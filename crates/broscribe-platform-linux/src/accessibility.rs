use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use std::time::{Duration, Instant};

use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::document::DocumentProxy;
use atspi::proxy::text::TextProxy;
use atspi::{Event, EventTypeProperties, ObjectRefOwned, Role, State};
use broscribe_platform::{
    AppUiReader, AxChange, AxChangeCallback, AxIncomplete, AxLimits, AxNode, AxObservation,
    AxScope, AxScopePolicy, AxTree, DocumentContext, Error, Result, ax_transient,
};
use futures::channel::{mpsc as asynchronous, oneshot};
use futures::{FutureExt, StreamExt};

#[derive(Default)]
pub struct LinuxAppUiReader;
type Callback = Rc<dyn Fn(AxChange) + Send>;
struct CallbackEntry {
    callback: Callback,
    pending: Arc<AtomicU8>,
}
struct Bridge {
    next: u64,
    callbacks: HashMap<u64, CallbackEntry>,
    sender: asynchronous::UnboundedSender<(u64, AxChange)>,
    receiver: Option<asynchronous::UnboundedReceiver<(u64, AxChange)>>,
}
impl Default for Bridge {
    fn default() -> Self {
        let (sender, receiver) = asynchronous::unbounded();
        Self {
            next: 0,
            callbacks: HashMap::new(),
            sender,
            receiver: Some(receiver),
        }
    }
}
thread_local! { static BRIDGE: RefCell<Bridge> = RefCell::new(Bridge::default()); }
struct Dispatcher(asynchronous::UnboundedReceiver<(u64, AxChange)>);
impl Drop for Dispatcher {
    fn drop(&mut self) {
        // Preserve queued events if GPUI cancels/restarts its foreground task.
        let (_, empty) = asynchronous::unbounded();
        let receiver = std::mem::replace(&mut self.0, empty);
        BRIDGE.with(|bridge| bridge.borrow_mut().receiver = Some(receiver));
    }
}
impl LinuxAppUiReader {
    /// Spawn once on GPUI's foreground executor, on the same thread that calls
    /// observe. Only this future invokes callbacks; D-Bus I/O has no UI loop.
    pub async fn dispatch_events() {
        let Some(receiver) = BRIDGE.with(|bridge| bridge.borrow_mut().receiver.take()) else {
            return;
        };
        let mut dispatcher = Dispatcher(receiver);
        while let Some((id, change)) = dispatcher.0.next().await {
            let callback = BRIDGE.with(|bridge| {
                bridge.borrow().callbacks.get(&id).map(|entry| {
                    entry
                        .pending
                        .fetch_and(!change_bit(change), Ordering::AcqRel);
                    entry.callback.clone()
                })
            });
            if let Some(callback) = callback {
                callback(change);
            }
        }
    }
}
fn change_bit(change: AxChange) -> u8 {
    1 << change as u8
}

/// Per-call D-Bus timeout outside snapshots (observer setup, trust checks).
const METHOD_TIMEOUT: Duration = Duration::from_secs(2);
/// Whole observer or trust setup.
const SETUP_TIMEOUT: Duration = Duration::from_secs(8);
/// Characters of one text attribute kept; longer text is clipped and the
/// snapshot marked incomplete.
const MAX_TEXT: usize = 65536;

/// A D-Bus failure that may pass: the reply timed out.
fn transient_dbus(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::InputOutput(io) => io.kind() == std::io::ErrorKind::TimedOut,
        zbus::Error::MethodError(name, _, _) => matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.NoReply"
                | "org.freedesktop.DBus.Error.Timeout"
                | "org.freedesktop.DBus.Error.TimedOut"
        ),
        _ => false,
    }
}

fn dbus_error(context: &str, error: zbus::Error) -> Error {
    if transient_dbus(&error) {
        ax_transient(format!("{context}: {error}"))
    } else {
        crate::platform_error(context, error)
    }
}

async fn bounded_request<T>(limit: Duration, future: impl Future<Output = Result<T>>) -> Result<T> {
    let request = future.fuse();
    let timeout = FutureExt::fuse(async_io::Timer::after(limit));
    futures::pin_mut!(request, timeout);
    futures::select! {
        result = request => result,
        _ = timeout => Err(ax_transient("reading AT-SPI: accessibility service timed out")),
    }
}

async fn connect(method_timeout: Duration) -> Result<zbus::Connection> {
    let session = zbus::connection::Builder::session()
        .map_err(|e| crate::platform_error("opening AT-SPI session", e))?
        .method_timeout(method_timeout)
        .build()
        .await
        .map_err(|e| dbus_error("connecting AT-SPI session", e))?;
    let bus = atspi::proxy::bus::BusProxy::new(&session)
        .await
        .map_err(|e| dbus_error("finding AT-SPI bus", e))?;
    let address = bus
        .get_address()
        .await
        .map_err(|e| dbus_error("reading AT-SPI address", e))?;
    zbus::connection::Builder::address(address.as_str())
        .map_err(|e| crate::platform_error("parsing AT-SPI address", e))?
        .method_timeout(method_timeout)
        .build()
        .await
        .map_err(|e| dbus_error("connecting AT-SPI bus", e))
}
async fn app_for_pid(connection: &zbus::Connection, pid: u32) -> Result<ObjectRefOwned> {
    let root = AccessibleProxy::builder(connection)
        .destination("org.a11y.atspi.Registry")
        .map_err(|e| crate::platform_error("locating AT-SPI registry", e))?
        .path("/org/a11y/atspi/accessible/root")
        .map_err(|e| crate::platform_error("locating AT-SPI root", e))?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|e| dbus_error("opening AT-SPI root", e))?;
    let dbus = zbus::Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .map_err(|e| dbus_error("opening AT-SPI process resolver", e))?;
    let count = root
        .child_count()
        .await
        .map_err(|e| dbus_error("listing accessible applications", e))?
        .clamp(0, 1024);
    for index in 0..count {
        let app = root
            .get_child_at_index(index)
            .await
            .map_err(|e| dbus_error("reading accessible application", e))?;
        let Some(name) = app.name_as_str() else {
            continue;
        };
        let app_pid: zbus::Result<u32> = dbus.call("GetConnectionUnixProcessID", &(name,)).await;
        if app_pid.is_ok_and(|p| p == pid) {
            return Ok(app);
        }
    }
    Err(Error::NotFound(format!("AT-SPI application for pid {pid}")))
}
struct Budget {
    left: usize,
    max_depth: usize,
    incomplete: AxIncomplete,
    deadline: Instant,
    visited: HashSet<(String, String)>,
}

/// A text attribute, clipped at [`MAX_TEXT`] characters (marked).
fn bounded(text: String, incomplete: &mut AxIncomplete) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    if text.chars().count() > MAX_TEXT {
        incomplete.clipped = true;
        return Some(text.chars().take(MAX_TEXT).collect());
    }
    Some(text)
}

/// A field read: a failure leaves the snapshot incomplete.
fn read<T>(result: zbus::Result<T>, incomplete: &mut AxIncomplete) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(_) => {
            incomplete.read_failed = true;
            None
        }
    }
}

/// The D-Bus error a service answers for a method its object does not
/// implement.
const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";

/// `GetAttributes`: an object whose service does not implement it has no
/// attributes (AccessKit's application root answers `UnknownMethod`). Only
/// that remote reply is treated so; any other failure, including another
/// method error, leaves the snapshot incomplete as [`read`] does.
fn read_attributes(
    result: zbus::Result<HashMap<String, String>>,
    incomplete: &mut AxIncomplete,
) -> HashMap<String, String> {
    match result {
        Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == UNKNOWN_METHOD => {
            HashMap::new()
        }
        result => read(result, incomplete).unwrap_or_default(),
    }
}

/// AT-SPI roles onto the shared vocabulary ([`broscribe_platform::AX_ROLES`]).
fn role_name(role: Role) -> &'static str {
    match role {
        Role::Application => "AXApplication",
        Role::Frame | Role::Dialog | Role::Window => "AXWindow",
        Role::Label | Role::Static | Role::Paragraph | Role::Text | Role::Caption => "AXStaticText",
        Role::Entry | Role::PasswordText | Role::SpinButton => "AXTextField",
        Role::Button | Role::ToggleButton => "AXButton",
        Role::CheckBox => "AXCheckBox",
        Role::RadioButton | Role::PageTab => "AXRadioButton",
        Role::ComboBox => "AXComboBox",
        Role::Heading => "AXHeading",
        Role::List | Role::ListBox => "AXList",
        Role::ListItem | Role::TableRow | Role::TreeItem => "AXRow",
        Role::Table => "AXTable",
        Role::ColumnHeader => "AXColumn",
        Role::Tree | Role::TreeTable => "AXOutline",
        Role::Link => "AXLink",
        Role::Image | Role::Icon => "AXImage",
        Role::Menu => "AXMenu",
        Role::MenuItem | Role::CheckMenuItem | Role::RadioMenuItem => "AXMenuItem",
        Role::MenuBar => "AXMenuBar",
        Role::PageTabList => "AXTabGroup",
        Role::ToolBar => "AXToolbar",
        Role::ScrollPane => "AXScrollArea",
        Role::ScrollBar => "AXScrollBar",
        Role::Slider => "AXSlider",
        Role::Separator => "AXSplitter",
        Role::ProgressBar => "AXProgressIndicator",
        Role::DocumentWeb | Role::DocumentFrame => "AXWebArea",
        _ => "AXGroup",
    }
}

/// One AT-SPI object's fields onto an [`AxNode`], with macOS Chromium
/// semantics: the single accessible name is both `title` and
/// `description` (macOS splits it between them by where it came from),
/// static text without a text interface carries its name as `value`, the
/// DOM `id` and `class` object attributes are the identifier and classes.
/// The AT-SPI description (macOS `AXHelp`, never read there) is left out.
fn mapped(
    role: Role,
    name: Option<String>,
    text: Option<String>,
    attributes: &HashMap<String, String>,
) -> AxNode {
    let role = role_name(role);
    let value = text.or_else(|| (role == "AXStaticText").then(|| name.clone()).flatten());
    AxNode {
        role: role.into(),
        subrole: None,
        title: name.clone(),
        value,
        description: name,
        identifier: attributes
            .get("id")
            .or_else(|| attributes.get("accessible-id"))
            .filter(|id| !id.is_empty())
            .cloned(),
        classes: attributes
            .get("class")
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        document: None,
        children: Vec::new(),
    }
}

/// A document's URL from its AT-SPI document attributes (`DocURL`, `URI`).
fn document_url(attributes: &HashMap<String, String>) -> Option<String> {
    ["DocURL", "URI"]
        .iter()
        .find_map(|key| attributes.get(*key).filter(|url| !url.is_empty()).cloned())
}

/// Whether [`walk`] reads an object's text interface: never a password,
/// and a web document's whole text only when a rule reads it.
fn reads_text(role: Role, has_text: bool, scope: &AxScope) -> bool {
    role != Role::PasswordText
        && has_text
        && (role_name(role) != "AXWebArea" || scope.policy().document_text)
}

/// What one AT-SPI object answered, before mapping.
struct Native {
    role: Role,
    name: Option<String>,
    text: Option<String>,
    attributes: HashMap<String, String>,
    /// A window's iconified state (`None`: unknown, or not a window).
    iconified: Option<bool>,
    /// A document's URL.
    url: Option<String>,
}

/// One object placed in a snapshot.
struct Placed {
    node: AxNode,
    /// The minimized state its subtree inherits.
    hidden: Option<bool>,
    /// Whether its contents may be read: not a document of another origin.
    open: bool,
}

/// Maps one object ([`mapped`]) and places it in the snapshot at `depth`
/// under an ancestor window's `hidden` state. `token` (its bus name and
/// path) is computed for top-level windows and documents only.
fn place(
    native: Native,
    token: impl FnOnce() -> String,
    depth: usize,
    hidden: Option<bool>,
    scope: &AxScope,
) -> Placed {
    let mut node = mapped(native.role, native.name, native.text, &native.attributes);
    let hidden = if node.role == "AXWindow" {
        // Unknown state never makes a window count as visible.
        Some(hidden.unwrap_or(false) || native.iconified.unwrap_or(true))
    } else {
        hidden
    };
    if node.role == "AXWindow" && depth == 1 {
        node.document = Some(DocumentContext::new(
            None,
            Some(token()),
            hidden.unwrap_or(true),
        ));
    } else if node.role == "AXWebArea" {
        let context = DocumentContext::new(native.url, Some(token()), hidden.unwrap_or(true));
        let open = scope.permits(context.url());
        node.document = Some(context);
        return Placed { node, hidden, open };
    }
    Placed {
        node,
        hidden,
        open: true,
    }
}

async fn read_text(
    connection: &zbus::Connection,
    proxy: &AccessibleProxy<'_>,
    incomplete: &mut AxIncomplete,
) -> Option<String> {
    let text = async {
        let text = TextProxy::builder(connection)
            .destination(proxy.inner().destination().clone())?
            .path(proxy.inner().path().clone())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let characters = text.character_count().await?;
        let whole = text
            .get_text(0, characters.clamp(0, MAX_TEXT as i32))
            .await?;
        zbus::Result::Ok((whole, usize::try_from(characters).unwrap_or(0) > MAX_TEXT))
    };
    let (whole, clipped) = read(text.await, incomplete)?;
    if clipped {
        incomplete.clipped = true;
    }
    bounded(whole, incomplete)
}

async fn read_document_attributes(
    connection: &zbus::Connection,
    proxy: &AccessibleProxy<'_>,
) -> zbus::Result<HashMap<String, String>> {
    let document = DocumentProxy::builder(connection)
        .destination(proxy.inner().destination().clone())?
        .path(proxy.inner().path().clone())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    document.get_attributes().await
}

/// Reads `object` and, within the budget, its subtree. A failure below the
/// root never fails the snapshot: it marks it incomplete. `hidden`: the
/// ancestor window's minimized state, `None` above any window.
fn walk<'a>(
    connection: &'a zbus::Connection,
    scope: &'a AxScope,
    object: ObjectRefOwned,
    depth: usize,
    hidden: Option<bool>,
    budget: &'a mut Budget,
) -> Pin<Box<dyn Future<Output = Result<AxNode>> + 'a>> {
    Box::pin(async move {
        budget.left = budget.left.saturating_sub(1);
        let proxy = object
            .as_accessible_proxy(connection)
            .await
            .map_err(|e| crate::platform_error("opening accessible node", e))?;
        let role = proxy
            .get_role()
            .await
            .map_err(|e| dbus_error("reading accessible role", e))?;
        let ax_role = role_name(role);
        let incomplete = &mut budget.incomplete;
        let name = read(proxy.name().await, incomplete).and_then(|n| bounded(n, incomplete));
        let attributes = read_attributes(proxy.get_attributes().await, incomplete);
        let interfaces = read(proxy.get_interfaces().await, incomplete);
        let has = |interface| interfaces.is_some_and(|set| set.contains(interface));
        let text = if reads_text(role, has(atspi::Interface::Text), scope) {
            read_text(connection, &proxy, incomplete).await
        } else {
            None
        };
        let iconified = if ax_role == "AXWindow" {
            read(proxy.get_state().await, &mut budget.incomplete)
                .map(|states| states.contains(State::Iconified))
        } else {
            None
        };
        let url = if ax_role == "AXWebArea" && has(atspi::Interface::Document) {
            read(
                read_document_attributes(connection, &proxy).await,
                &mut budget.incomplete,
            )
            .and_then(|attributes| document_url(&attributes))
        } else {
            None
        };
        let Placed {
            mut node,
            hidden,
            open,
        } = place(
            Native {
                role,
                name,
                text,
                attributes,
                iconified,
                url,
            },
            || format!("{}{}", proxy.inner().destination(), proxy.inner().path()),
            depth,
            hidden,
            scope,
        );
        if !open {
            // Another origin: identity only, nothing inside is read.
            return Ok(node);
        }
        let count = read(proxy.child_count().await, &mut budget.incomplete)
            .unwrap_or(0)
            .max(0);
        for index in 0..count {
            if depth >= budget.max_depth {
                budget.incomplete.depth_cap = true;
                break;
            }
            if budget.left == 0 {
                budget.incomplete.node_cap = true;
                break;
            }
            if Instant::now() >= budget.deadline {
                budget.incomplete.time_budget = true;
                break;
            }
            // A child that could not be read leaves the snapshot partial
            // (marked incomplete), never silently whole.
            let child = match proxy.get_child_at_index(index).await {
                Ok(child) if !child.is_null() => child,
                _ => {
                    budget.incomplete.read_failed = true;
                    continue;
                }
            };
            let key = (
                child.name_as_str().unwrap_or_default().to_owned(),
                child.path_as_str().to_owned(),
            );
            if !budget.visited.insert(key) {
                budget.incomplete.read_failed = true;
                continue;
            }
            match walk(connection, scope, child, depth + 1, hidden, budget).await {
                Ok(child) => node.children.push(child),
                Err(_) => budget.incomplete.read_failed = true,
            }
        }
        Ok(node)
    })
}

impl LinuxAppUiReader {
    fn read_tree(&self, scope: &AxScope, limits: AxLimits) -> Result<AxTree> {
        limits.validate()?;
        // The walk stops at its budget; this bounds connecting and one
        // element still answering at the deadline.
        let whole = limits.budget + limits.element_timeout * 3;
        futures::executor::block_on(bounded_request(whole, async {
            let connection = connect(limits.element_timeout).await?;
            let app = app_for_pid(&connection, scope.pid()).await?;
            let mut budget = Budget {
                left: limits.max_nodes,
                max_depth: limits.max_depth,
                incomplete: AxIncomplete::default(),
                deadline: Instant::now() + limits.budget,
                visited: HashSet::new(),
            };
            budget.visited.insert((
                app.name_as_str().unwrap_or_default().to_owned(),
                app.path_as_str().to_owned(),
            ));
            let root = walk(&connection, scope, app, 0, None, &mut budget).await?;
            Ok(AxTree {
                root,
                incomplete: budget.incomplete,
            })
        }))
    }
}

/// Changes an observed application posts, in signal order; the stream ends
/// when its bus subscription does.
type ChangeStream = futures::stream::LocalBoxStream<'static, Vec<AxChange>>;

/// Connects, finds `pid`'s application and subscribes to its signals. The
/// stream owns the connection: dropping it disconnects, which unregisters
/// this client's subscriptions.
async fn subscribe(pid: u32) -> Result<ChangeStream> {
    let connection = connect(METHOD_TIMEOUT).await?;
    let app = app_for_pid(&connection, pid).await?;
    let name = app
        .name_as_str()
        .ok_or_else(|| Error::NotFound("AT-SPI application bus name".into()))?
        .to_owned();
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(name.as_str())
        .map_err(|e| crate::platform_error("matching accessible signals", e))?
        .build();
    let stream = zbus::MessageStream::for_match_rule(rule, &connection, Some(64))
        .await
        .map_err(|e| crate::platform_error("subscribing accessible signals", e))?;
    let registry = atspi::proxy::registry::RegistryProxy::new(&connection)
        .await
        .map_err(|e| crate::platform_error("opening accessibility registry", e))?;
    for event in [
        "object:property-change",
        "object:text-changed",
        "object:text-attributes-changed",
        "object:children-changed",
        "object:visible-data-changed",
        "object:announcement",
        "window:",
        "focus:",
    ] {
        registry
            .register_event(event)
            .await
            .map_err(|e| crate::platform_error("registering accessible observation", e))?;
    }
    let changes = futures::stream::unfold(
        (connection, stream),
        |(connection, mut stream)| async move {
            // A failed read ends the subscription; an unparsable signal is skipped.
            while let Some(Ok(message)) = stream.next().await {
                if let Ok(event) = Event::try_from(&message) {
                    return Some((event_changes(&event), (connection, stream)));
                }
            }
            None
        },
    );
    Ok(changes.boxed_local())
}

struct Observation {
    id: u64,
    stop: Option<oneshot::Sender<()>>,
    _main_thread: std::marker::PhantomData<Rc<()>>,
}
impl Drop for Observation {
    fn drop(&mut self) {
        // Erasing the callback here, on the dispatching thread, is what
        // ends the observation: a change queued earlier finds no callback.
        // The I/O thread is not joined, since it may still be connecting;
        // the stop signal cancels its setup or ends its subscription.
        BRIDGE.with(|bridge| {
            bridge.borrow_mut().callbacks.remove(&self.id);
        });
        if let Some(stop) = self.stop.take() {
            // An error means the I/O thread already ended.
            let _ = stop.send(());
        }
    }
}
impl AxObservation for Observation {}

/// Registers `callback` on this thread (GPUI's, which runs
/// [`LinuxAppUiReader::dispatch_events`]) and returns without waiting for
/// `setup`, which runs on its own I/O thread and then forwards the changes
/// in `changes` until the observation is dropped. A failed setup leaves the
/// observation silent, never faking a change: snapshots then run on the
/// reader host's heartbeat, as for an app posting no notifications (the
/// host discards observer errors either way).
fn observe_with<F, S>(
    changes: &[AxChange],
    callback: AxChangeCallback,
    setup: F,
) -> Result<Observation>
where
    F: FnOnce() -> S + Send + 'static,
    S: Future<Output = Result<ChangeStream>>,
{
    let pending = Arc::new(AtomicU8::new(0));
    let (id, sender) = BRIDGE.with(|bridge| {
        let mut bridge = bridge.borrow_mut();
        bridge.next += 1;
        let id = bridge.next;
        bridge.callbacks.insert(
            id,
            CallbackEntry {
                callback: Rc::from(callback),
                pending: pending.clone(),
            },
        );
        (id, bridge.sender.clone())
    });
    let (stop, stopped) = oneshot::channel();
    let changes = changes.to_vec();
    let io = move || {
        futures::executor::block_on(async move {
            let mut stopped = stopped.fuse();
            let setup = setup().fuse();
            futures::pin_mut!(setup);
            // Dropping the observation cancels a setup still connecting,
            // scanning or registering; its connection closes with it. Stop
            // wins over a setup or change ready at the same time.
            let mut stream = futures::select_biased! {
                _ = stopped => return,
                subscribed = setup => match subscribed {
                    Ok(stream) => stream,
                    // `observe` has returned: the observation stays silent.
                    Err(_) => return,
                },
            };
            loop {
                futures::select_biased! {
                    _ = stopped => break,
                    changed = stream.next().fuse() => {
                        let Some(changed) = changed else { break };
                        for change in changed.into_iter().filter(|c| changes.contains(c)) {
                            let bit = change_bit(change);
                            if pending.fetch_or(bit, Ordering::AcqRel) & bit == 0
                                && sender.unbounded_send((id, change)).is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        });
    };
    if let Err(e) = std::thread::Builder::new()
        .name("broscribe-atspi-io".into())
        .spawn(io)
    {
        BRIDGE.with(|bridge| {
            bridge.borrow_mut().callbacks.remove(&id);
        });
        return Err(Error::io("starting accessibility I/O", e));
    }
    Ok(Observation {
        id,
        stop: Some(stop),
        _main_thread: std::marker::PhantomData,
    })
}

fn event_changes(event: &Event) -> Vec<AxChange> {
    match event.member() {
        "TextChanged" | "TextAttributesChanged" => {
            vec![AxChange::ValueChanged, AxChange::LiveRegionChanged]
        }
        "Announcement" => vec![AxChange::LiveRegionChanged],
        "PropertyChange" => vec![AxChange::TitleChanged, AxChange::ValueChanged],
        "ChildrenChanged" => vec![
            AxChange::ElementCreated,
            AxChange::ElementDestroyed,
            AxChange::LayoutChanged,
        ],
        "Create" => vec![AxChange::WindowCreated],
        "Destroy" | "Close" => vec![AxChange::ElementDestroyed],
        "Activate" | "Focus" => vec![AxChange::FocusedWindowChanged],
        _ => vec![AxChange::LayoutChanged],
    }
}
impl AppUiReader for LinuxAppUiReader {
    fn is_trusted(&self) -> bool {
        futures::executor::block_on(bounded_request(SETUP_TIMEOUT, connect(METHOD_TIMEOUT))).is_ok()
    }
    fn request_trust(&self) -> bool {
        futures::executor::block_on(bounded_request(SETUP_TIMEOUT, async {
            atspi::connection::set_session_accessibility(true)
                .await
                .map_err(|e| crate::platform_error("enabling session accessibility", e))
        }))
        .is_ok()
            && self.is_trusted()
    }
    fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree> {
        self.read_tree(&AxScope::new(pid, AxScopePolicy::default()), limits)
    }
    /// Element tokens (bus name and object path) need no per-incarnation
    /// state; documents of other origins are skipped while walking.
    fn snapshot_in(&self, scope: &AxScope, limits: AxLimits) -> Result<AxTree> {
        self.read_tree(scope, limits)
    }
    fn observe(
        &self,
        pid: u32,
        changes: &[AxChange],
        callback: AxChangeCallback,
    ) -> Result<Box<dyn AxObservation>> {
        let observation = observe_with(changes, callback, move || {
            bounded_request(SETUP_TIMEOUT, subscribe(pid))
        })?;
        Ok(Box::new(observation))
    }
    fn manual_accessibility(&self, _pid: u32) -> Result<Option<bool>> {
        Ok(None)
    }
    fn set_manual_accessibility(&self, _pid: u32, _enabled: bool) -> Result<()> {
        Err(Error::Unsupported("AXManualAccessibility is a macOS-only Chromium attribute; Linux uses the AT-SPI accessibility bus".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::AX_ROLES;
    use broscribe_readers::rules::conformance::{self, Element, Kind, Mapper, Os};

    #[test]
    fn every_role_maps_into_and_covers_the_shared_vocabulary() {
        let emitted: HashSet<&str> = (0..=u32::from(u8::MAX))
            .filter_map(|n| Role::try_from(n).ok())
            .map(role_name)
            .collect();
        for ax in &emitted {
            assert!(AX_ROLES.contains(ax), "{ax}");
        }
        for ax in AX_ROLES {
            assert!(emitted.contains(ax), "no AT-SPI role maps to {ax}");
        }
    }

    #[test]
    fn get_attributes_unknown_method_means_none_and_every_other_failure_is_incomplete() {
        let call = zbus::Message::method_call("/org/a11y/atspi/accessible/root", "GetAttributes")
            .unwrap()
            .interface("org.a11y.atspi.Accessible")
            .unwrap()
            .build(&())
            .unwrap();
        let reply = |name: &str| -> zbus::Result<HashMap<String, String>> {
            let message = zbus::Message::error(&call.header(), name)
                .unwrap()
                .build(&("synthetic detail",))
                .unwrap();
            Err(zbus::Error::from(message))
        };

        let mut incomplete = AxIncomplete::default();
        assert!(read_attributes(reply(UNKNOWN_METHOD), &mut incomplete).is_empty());
        assert_eq!(
            incomplete,
            AxIncomplete::default(),
            "no attributes, not a failure"
        );

        let attributes = HashMap::from([("id".to_owned(), "synthetic".to_owned())]);
        assert_eq!(
            read_attributes(Ok(attributes.clone()), &mut incomplete),
            attributes
        );
        assert!(!incomplete.any());

        for failure in [
            reply("org.freedesktop.DBus.Error.UnknownObject"),
            reply("org.freedesktop.DBus.Error.AccessDenied"),
            Err(zbus::Error::InvalidReply),
            Err(zbus::Error::Failure(UNKNOWN_METHOD.into())),
        ] {
            let mut incomplete = AxIncomplete::default();
            assert!(read_attributes(failure, &mut incomplete).is_empty());
            assert!(incomplete.read_failed, "{incomplete:?}");
        }
    }

    /// Synthetic AT-SPI answers for a conformance element, through
    /// [`reads_text`] and [`place`]: windows are frames, web containers
    /// sections and native ones panels, static text answers through its
    /// text interface, DOM ids are `id` object attributes and native ones
    /// `accessible-id`, a document's URL is its `DocURL`, and each keyed
    /// element keeps its own object path.
    fn object(
        element: &Element,
        depth: usize,
        web: bool,
        hidden: Option<bool>,
        scope: &AxScope,
        incomplete: &mut AxIncomplete,
    ) -> AxNode {
        let web = web || element.kind == Kind::Document;
        let role = match element.kind {
            Kind::Window => Role::Frame,
            Kind::Document => Role::DocumentWeb,
            Kind::Group if web => Role::Section,
            Kind::Group => Role::Panel,
            Kind::Outline => Role::Tree,
            Kind::Row => Role::TreeItem,
            Kind::Button => Role::Button,
            Kind::Text => Role::Static,
            Kind::Heading => Role::Heading,
        };
        let answered = if element.unreadable {
            Err(zbus::Error::Failure("unanswered".into()))
        } else {
            Ok(element.name.clone().unwrap_or_default())
        };
        let name = read(answered, incomplete).and_then(|n| bounded(n, incomplete));
        let mut attributes = HashMap::new();
        if let Some(id) = &element.id {
            let key = if web { "id" } else { "accessible-id" };
            attributes.insert(key.to_owned(), id.clone());
        }
        if !element.classes.is_empty() {
            attributes.insert("class".to_owned(), element.classes.join(" "));
        }
        let has_text = matches!(element.kind, Kind::Text | Kind::Document);
        let text = if reads_text(role, has_text, scope) {
            element.text.clone().and_then(|t| bounded(t, incomplete))
        } else {
            None
        };
        let url = (element.kind == Kind::Document)
            .then(|| {
                document_url(&HashMap::from([(
                    "DocURL".to_owned(),
                    element.url.clone().unwrap_or_default(),
                )]))
            })
            .flatten();
        let iconified = if element.kind == Kind::Window {
            element.minimized
        } else {
            None
        };
        let key = element.key.as_deref().unwrap_or("anonymous");
        let placed = place(
            Native {
                role,
                name,
                text,
                attributes,
                iconified,
                url,
            },
            || format!(":1.42/org/a11y/atspi/accessible/{key}"),
            depth,
            hidden,
            scope,
        );
        let mut node = placed.node;
        if placed.open {
            node.children = element
                .children
                .iter()
                .map(|child| object(child, depth + 1, web, placed.hidden, scope, incomplete))
                .collect();
        }
        node
    }

    struct AtSpi;

    impl Mapper for AtSpi {
        fn os(&self) -> Os {
            Os::Linux
        }

        fn snapshot(&self, windows: &[Element], policy: &AxScopePolicy) -> AxTree {
            let scope = AxScope::new(4242, policy.clone());
            let mut incomplete = AxIncomplete::default();
            let application = Native {
                role: Role::Application,
                name: Some("Synthetic app".into()),
                text: None,
                attributes: HashMap::new(),
                iconified: None,
                url: None,
            };
            let mut root = place(application, String::new, 0, None, &scope).node;
            root.children = windows
                .iter()
                .map(|window| object(window, 1, false, None, &scope, &mut incomplete))
                .collect();
            AxTree { root, incomplete }
        }
    }

    #[test]
    fn every_shipped_rule_reads_through_the_at_spi_mapping() {
        conformance::check(&AtSpi);
    }

    #[test]
    fn fields_follow_macos_chromium_semantics() {
        let attributes = HashMap::from([
            ("id".to_owned(), "hangup-button".to_owned()),
            ("class".to_owned(), "control  leave".to_owned()),
        ]);
        let button = mapped(Role::Button, Some("Leave call".into()), None, &attributes);
        assert_eq!(button.role, "AXButton");
        assert_eq!(button.title.as_deref(), Some("Leave call"));
        assert_eq!(button.description.as_deref(), Some("Leave call"));
        assert_eq!(button.identifier.as_deref(), Some("hangup-button"));
        assert_eq!(button.classes, ["control", "leave"]);
        assert_eq!(button.value, None);
        assert_eq!(button.subrole, None, "no platform text in role slots");
        let label = mapped(
            Role::Static,
            Some("You left the meeting".into()),
            None,
            &HashMap::new(),
        );
        assert_eq!(label.value.as_deref(), Some("You left the meeting"));
        let text = mapped(
            Role::Paragraph,
            Some("name".into()),
            Some("content".into()),
            &HashMap::new(),
        );
        assert_eq!(text.value.as_deref(), Some("content"));
        let attrs = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        assert_eq!(
            document_url(&attrs(&[
                ("URI", "https://b.example"),
                ("DocURL", "https://a.example")
            ])),
            Some("https://a.example".into())
        );
        assert_eq!(
            document_url(&attrs(&[("DocURL", ""), ("URI", "https://b.example")])),
            Some("https://b.example".into())
        );
        assert_eq!(document_url(&attrs(&[("MimeType", "text/html")])), None);
    }

    #[test]
    fn clipped_text_and_failed_reads_mark_incomplete() {
        let mut incomplete = AxIncomplete::default();
        assert_eq!(bounded(String::new(), &mut incomplete), None);
        assert_eq!(
            bounded("x".repeat(MAX_TEXT), &mut incomplete).map(|t| t.len()),
            Some(MAX_TEXT)
        );
        assert!(!incomplete.any());
        let clipped = bounded("x".repeat(MAX_TEXT + 1), &mut incomplete).unwrap();
        assert_eq!(clipped.chars().count(), MAX_TEXT);
        assert!(incomplete.clipped && !incomplete.read_failed);
        let failed: zbus::Result<String> = Err(zbus::Error::Failure("gone".into()));
        assert_eq!(read(failed, &mut incomplete), None);
        assert!(incomplete.read_failed);
    }

    #[test]
    fn only_timeouts_are_transient() {
        let timeout =
            zbus::Error::InputOutput(Arc::new(std::io::Error::from(std::io::ErrorKind::TimedOut)));
        assert!(broscribe_platform::is_ax_transient(&dbus_error(
            "x", timeout
        )));
        let refused = zbus::Error::InputOutput(Arc::new(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused,
        )));
        assert!(!broscribe_platform::is_ax_transient(&dbus_error(
            "x", refused
        )));
        assert!(!broscribe_platform::is_ax_transient(&dbus_error(
            "x",
            zbus::Error::Failure("no".into())
        )));
    }

    /// Sends `passed` when dropped: a synthetic setup or change stream
    /// ended, and whether that setup got past its gate first.
    struct Ended {
        to: std::sync::mpsc::Sender<bool>,
        passed: bool,
    }
    impl Drop for Ended {
        fn drop(&mut self) {
            self.to.send(self.passed).unwrap();
        }
    }
    fn ended(to: std::sync::mpsc::Sender<bool>) -> Ended {
        Ended { to, passed: false }
    }

    /// A change stream posting `items`, reporting on `end` once exhausted.
    fn finite(items: Vec<Vec<AxChange>>, end: std::sync::mpsc::Sender<bool>) -> ChangeStream {
        futures::stream::unfold(
            (items.into_iter(), ended(end)),
            |(mut items, end)| async move { items.next().map(|item| (item, (items, end))) },
        )
        .boxed_local()
    }

    fn registered(id: u64) -> bool {
        BRIDGE.with(|bridge| bridge.borrow().callbacks.contains_key(&id))
    }

    /// Runs this thread's dispatcher (GPUI's foreground task) over the
    /// changes already queued for this thread's callbacks.
    fn dispatch_queued() {
        assert_eq!(LinuxAppUiReader::dispatch_events().now_or_never(), None);
    }

    #[test]
    fn observing_returns_before_setup_and_a_drop_during_setup_cancels_it() {
        let (release, gate) = oneshot::channel::<()>();
        // The gate's controller keeps it closed while this (owner) thread
        // acknowledges that `observe_with` returned, then until the owner
        // drops `ack`. Should observe block on its setup again, the
        // controller gives up after the setup bound itself and releases
        // the gate: the test then fails below instead of hanging.
        let (ack, acks) = std::sync::mpsc::channel::<()>();
        let controller = std::thread::spawn(move || {
            let prompt = acks.recv_timeout(SETUP_TIMEOUT).is_ok();
            if prompt {
                // Waits for the owner's release request: `ack` dropped.
                let _ = acks.recv();
            }
            drop(release);
            prompt
        });
        let (report, setup_ended) = std::sync::mpsc::channel();
        let (seen, changes) = std::sync::mpsc::channel();
        let observation = observe_with(
            &[AxChange::ValueChanged],
            Box::new(move |change| seen.send(change).unwrap()),
            move || {
                let mut setup = ended(report);
                async move {
                    // An abandoned gate opens too: a failing test leaves no
                    // setup waiting.
                    let _ = gate.await;
                    setup.passed = true;
                    drop(setup);
                    Ok::<ChangeStream, Error>(
                        futures::stream::iter([vec![AxChange::ValueChanged]]).boxed_local(),
                    )
                }
            },
        )
        .unwrap();
        // Acknowledged once returned; an error means the controller had
        // already given up, which the join below reports.
        let _ = ack.send(());
        // Returned with the setup gated, its callback registered.
        let id = observation.id;
        assert!(registered(id));
        drop(observation);
        // The drop returned with the setup still gated, and erased the callback.
        assert!(!registered(id));
        drop(ack);
        assert!(
            controller.join().unwrap(),
            "observe returned only after its setup was released"
        );
        // The stop, sent before the gate opened, cancelled the setup at it.
        assert_eq!(setup_ended.recv(), Ok(false));
        // A change that raced the drop finds no callback; the callback is gone.
        BRIDGE
            .with(|bridge| {
                bridge
                    .borrow()
                    .sender
                    .unbounded_send((id, AxChange::ValueChanged))
            })
            .unwrap();
        dispatch_queued();
        assert_eq!(
            changes.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );
    }

    #[test]
    fn requested_changes_reach_the_callback_on_the_dispatching_thread() {
        let (report, stream_ended) = std::sync::mpsc::channel();
        let (seen, changes) = std::sync::mpsc::channel();
        let observation = observe_with(
            &[AxChange::ValueChanged],
            Box::new(move |change| seen.send(change).unwrap()),
            move || async move {
                Ok::<ChangeStream, Error>(finite(
                    vec![vec![AxChange::LayoutChanged, AxChange::ValueChanged]],
                    report,
                ))
            },
        )
        .unwrap();
        // The I/O thread forwarded everything before its stream ended.
        stream_ended.recv().unwrap();
        assert_eq!(
            changes.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty),
            "callbacks run only on the dispatching thread"
        );
        dispatch_queued();
        assert_eq!(changes.try_recv(), Ok(AxChange::ValueChanged));
        assert_eq!(
            changes.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty),
            "an unrequested change is not delivered"
        );
        drop(observation);
        // Dropping erased the callback and the sender it held.
        assert_eq!(
            changes.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );
    }

    #[test]
    fn a_failed_setup_leaves_a_silent_observation() {
        let (report, setup_ended) = std::sync::mpsc::channel();
        let (seen, changes) = std::sync::mpsc::channel();
        let observation = observe_with(
            &AxChange::ALL,
            Box::new(move |change| seen.send(change).unwrap()),
            move || {
                let setup = ended(report);
                async move {
                    let _setup = setup;
                    Err::<ChangeStream, Error>(Error::NotFound("synthetic application".into()))
                }
            },
        )
        .unwrap();
        setup_ended.recv().unwrap();
        dispatch_queued();
        // No change is faked; the reader host keeps its heartbeat snapshots.
        assert_eq!(
            changes.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
        let id = observation.id;
        assert!(registered(id));
        drop(observation);
        assert!(!registered(id));
        assert_eq!(
            changes.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );
    }
}
