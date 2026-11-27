//! Windows, meeting-app UI text and the menu-bar status item.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use crate::{Error, Result};

/// Priority of a direct assistive-technology announcement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnouncementPriority {
    /// Polite status update (`Role::Status` / `Live::Polite`).
    Medium,
    /// Interrupting alert (`Role::Alert` / `Live::Assertive`).
    High,
}

/// Order of the safe (Cancel) and action buttons at the trailing end of a
/// confirmation dialog (SYS-151).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogButtonOrder {
    /// Cancel on the leading side, action on the trailing side (macOS, Linux).
    CancelFirst,
    /// Action on the leading side, Cancel on the trailing side (Windows).
    ActionFirst,
}

/// The system display settings BroScribe's theme follows (ACC-32).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DisplayPreferences {
    /// macOS Increase Contrast, or the XDG Settings portal's
    /// `org.freedesktop.appearance` `contrast` set to higher contrast.
    pub increase_contrast: bool,
    /// macOS Reduce Transparency: translucent fills are drawn opaque.
    pub reduce_transparency: bool,
    /// A Windows contrast theme is on (`SPI_GETHIGHCONTRAST` reports
    /// `HCF_HIGHCONTRASTON`).
    pub forced_colors: bool,
    /// The user's light-appearance accent as `0xRRGGBB` in sRGB.
    /// `None` keeps BroScribe's palette (such as macOS Multicolor).
    pub light_accent: Option<u32>,
    /// The user's dark-appearance accent in the same format.
    pub dark_accent: Option<u32>,
}

impl DisplayPreferences {
    pub fn accent(self, dark: bool) -> Option<u32> {
        if dark {
            self.dark_accent
        } else {
            self.light_accent
        }
    }

    /// Whether the "more contrast" theme variants apply: Increase Contrast
    /// or a Windows contrast theme.
    pub fn more_contrast(&self) -> bool {
        self.increase_contrast || self.forced_colors
    }

    /// Whether translucent overlays, drop targets and selections are drawn
    /// opaque: Reduce Transparency, or any of the contrast settings.
    pub fn opaque(&self) -> bool {
        self.reduce_transparency || self.more_contrast()
    }
}

/// Reads the system's display accessibility settings (ACC-32).
pub trait DisplayAccessibility: Send + Sync {
    /// Whether the snapshot must be read on the UI loop. The app reads other
    /// sources in the background because the Linux portal can block.
    fn requires_main_thread(&self) -> bool {
        false
    }

    /// macOS Increase Contrast; the Linux portal's higher-contrast setting.
    fn increase_contrast(&self) -> bool;
    /// macOS Reduce Transparency.
    fn reduce_transparency(&self) -> bool;
    /// A Windows contrast theme.
    fn forced_colors(&self) -> bool;
    /// The user's accent colour (`0xRRGGBB`, sRGB) when they chose one,
    /// `None` for Multicolor or where the system has no such setting.
    fn accent(&self, _dark: bool) -> Option<u32> {
        None
    }
    /// All the settings in one read.
    fn preferences(&self) -> DisplayPreferences {
        DisplayPreferences {
            increase_contrast: self.increase_contrast(),
            reduce_transparency: self.reduce_transparency(),
            forced_colors: self.forced_colors(),
            light_accent: self.accent(false),
            dark_accent: self.accent(true),
        }
    }
}

/// A platform without display accessibility settings: everything off.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDisplayAccessibility;

impl DisplayAccessibility for NoDisplayAccessibility {
    fn increase_contrast(&self) -> bool {
        false
    }
    fn reduce_transparency(&self) -> bool {
        false
    }
    fn forced_colors(&self) -> bool {
        false
    }
}

/// Announces live-region text changes directly to assistive technology on
/// platforms whose AccessKit adapter reads live announcements only from a
/// node's value (macOS: `NSAccessibilityAnnouncementRequested`), keeping the
/// label-only rule for `Status` and `Alert` nodes intact. Windows and Linux
/// announce live regions natively through AccessKit and use [`NoopAnnouncer`].
pub trait Announcer: Send + Sync {
    fn announce(&self, text: &str, priority: AnnouncementPriority);
}

/// No-op announcer for platforms where AccessKit handles live regions natively.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopAnnouncer;

impl Announcer for NoopAnnouncer {
    fn announce(&self, _text: &str, _priority: AnnouncementPriority) {}
}

/// One application window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowInfo {
    /// Owning process.
    pub pid: u32,
    /// Owning application's display name.
    pub app: String,
    /// Owning application's bundle id, when it has one.
    pub bundle_id: Option<String>,
    /// Window title; `None` when the platform withholds titles (macOS without
    /// the Screen Recording permission, which BroScribe never requests by
    /// default).
    pub title: Option<String>,
}

pub trait WindowInspector: Send + Sync {
    fn windows(&self) -> Result<Vec<WindowInfo>>;
}

/// The shared role vocabulary: every role a platform mapper emits for
/// Windows UI Automation and Linux AT-SPI, and every role a shipped reader
/// rule may name. macOS reports native AX roles, a superset.
pub const AX_ROLES: &[&str] = &[
    "AXApplication",
    "AXWindow",
    "AXWebArea",
    "AXGroup",
    "AXButton",
    "AXStaticText",
    "AXHeading",
    "AXTextField",
    "AXCheckBox",
    "AXRadioButton",
    "AXComboBox",
    "AXList",
    "AXRow",
    "AXTable",
    "AXColumn",
    "AXOutline",
    "AXLink",
    "AXImage",
    "AXMenu",
    "AXMenuItem",
    "AXMenuBar",
    "AXTabGroup",
    "AXToolbar",
    "AXScrollArea",
    "AXScrollBar",
    "AXSlider",
    "AXSplitter",
    "AXProgressIndicator",
];

/// One element of an application's accessibility tree (macOS AX, Windows UI
/// Automation, Linux AT-SPI), reduced to what meeting-app readers match on.
/// Roles use the macOS names ([`AX_ROLES`]); other platforms map their roles
/// onto them. Attributes follow macOS Chromium semantics: `title` and
/// `description` both carry the accessible name where a platform exposes a
/// single computed name, and static text carries its text as `value`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AxNode {
    pub role: String,
    pub subrole: Option<String>,
    pub title: Option<String>,
    /// Text values only (numbers, booleans and other value types are left out).
    pub value: Option<String>,
    pub description: Option<String>,
    /// The app's stable identifier (`AXIdentifier`; for web content the DOM
    /// `id`).
    pub identifier: Option<String>,
    /// Web content only: the DOM class list.
    pub classes: Vec<String>,
    /// Web documents (`AXWebArea`) and the application's top-level windows
    /// (`AXWindow` children of the root) only: which element this is
    /// (call-identity checks, plan H). Never selectable, printed or stored.
    pub document: Option<DocumentContext>,
    pub children: Vec<AxNode>,
}

impl AxNode {
    pub fn new(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            ..Self::default()
        }
    }

    /// Number of nodes in this subtree, itself included.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(AxNode::count).sum::<usize>()
    }
}

/// The identity of one web document or top-level window element, for
/// call-state checks only. Its URL and element token stay private: `Debug`
/// redacts them and the type is never serialized, so neither reaches a dump,
/// a log or the daemon.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DocumentContext {
    url: Option<String>,
    element: Option<String>,
    hidden: bool,
}

impl DocumentContext {
    /// `url`: the document URL (`None` for windows). `element`: an opaque
    /// token naming this very element for the reader incarnation's
    /// lifetime, never reused for another one (macOS: an id issued by the
    /// incarnation's [`AxScope`] registry comparing elements with
    /// `CFEqual`; Windows: the UI Automation runtime id of a document, the
    /// window handle and owner of a window; Linux: the AT-SPI bus name and
    /// object path); `None` when it cannot be established. `hidden`: its
    /// window is minimized, or that could not be established.
    pub fn new(url: Option<String>, element: Option<String>, hidden: bool) -> Self {
        Self {
            url: url.filter(|u| !u.is_empty()),
            element: element.filter(|e| !e.is_empty()),
            hidden,
        }
    }

    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    pub fn element(&self) -> Option<&str> {
        self.element.as_deref()
    }

    /// The window showing it is minimized: what it shows is not current.
    pub fn hidden(&self) -> bool {
        self.hidden
    }
}

impl std::fmt::Debug for DocumentContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocumentContext")
            .field("url", &self.url.as_ref().map(|_| "<redacted>"))
            .field("element", &self.element.as_ref().map(|_| "<redacted>"))
            .field("hidden", &self.hidden)
            .finish()
    }
}

/// The lowercase `scheme://host[:port]` origin of `url`; `None` when it has
/// no `scheme://` or an empty host.
pub fn url_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    (!scheme.is_empty() && !host.is_empty()).then(|| {
        format!(
            "{}://{}",
            scheme.to_ascii_lowercase(),
            host.to_ascii_lowercase()
        )
    })
}

/// Caps on one [`AppUiReader::snapshot`]: meeting apps (Electron, browsers)
/// expose very large trees, so a snapshot stops descending past `max_depth`,
/// stops adding nodes after `max_nodes`, gives one unresponsive element at
/// most `element_timeout` and stops reading once `budget` elapsed. Every cut
/// is reported in [`AxTree::incomplete`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxLimits {
    pub max_depth: usize,
    pub max_nodes: usize,
    pub element_timeout: Duration,
    pub budget: Duration,
}

impl Default for AxLimits {
    fn default() -> Self {
        Self {
            max_depth: 48,
            max_nodes: 6000,
            element_timeout: Duration::from_secs(1),
            budget: Duration::from_secs(3),
        }
    }
}

impl AxLimits {
    /// Refuses limits no snapshot can honour: no node budget, no time, or
    /// a per-element timeout longer than the whole budget.
    pub fn validate(&self) -> Result<()> {
        if self.max_nodes == 0 {
            return Err(Error::InvalidInput(
                "accessibility snapshot needs a nonzero node budget".into(),
            ));
        }
        if self.element_timeout.is_zero()
            || self.budget.is_zero()
            || self.element_timeout > self.budget
        {
            return Err(Error::InvalidInput(
                "accessibility snapshot needs a positive element timeout within its budget".into(),
            ));
        }
        Ok(())
    }
}

/// Why a snapshot is not the whole tree. Any of them makes the snapshot
/// incomplete: it may feed roster/caption reading, never an end of call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AxIncomplete {
    /// [`AxLimits::max_nodes`] was reached.
    pub node_cap: bool,
    /// [`AxLimits::max_depth`] was reached.
    pub depth_cap: bool,
    /// [`AxLimits::budget`] ran out.
    pub time_budget: bool,
    /// An element, its children or one of its attributes could not be read.
    pub read_failed: bool,
    /// A text attribute was longer than the platform reader keeps.
    pub clipped: bool,
    /// The incarnation's bounded window/document identity registry is full
    /// and refused an element this snapshot met: that element has no
    /// identity, so nothing about it can be tracked or proven.
    pub identity_cap: bool,
}

impl AxIncomplete {
    pub fn any(&self) -> bool {
        self.node_cap
            || self.depth_cap
            || self.time_budget
            || self.read_failed
            || self.clipped
            || self.identity_cap
    }

    /// The reasons that are set, by field name in declaration order (empty
    /// for a whole tree): the stable spelling `readers dump` reports.
    pub fn names(&self) -> Vec<&'static str> {
        // Destructured without `..`, so a new reason cannot be left out.
        let Self {
            node_cap,
            depth_cap,
            time_budget,
            read_failed,
            clipped,
            identity_cap,
        } = *self;
        [
            ("node_cap", node_cap),
            ("depth_cap", depth_cap),
            ("time_budget", time_budget),
            ("read_failed", read_failed),
            ("clipped", clipped),
            ("identity_cap", identity_cap),
        ]
        .into_iter()
        .filter_map(|(name, set)| set.then_some(name))
        .collect()
    }
}

/// A capped snapshot of one application's accessibility tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AxTree {
    /// The application element; its children are the app's windows.
    pub root: AxNode,
    /// What cut the snapshot short, if anything.
    pub incomplete: AxIncomplete,
}

impl AxTree {
    /// Whether the snapshot is not the whole tree.
    pub fn truncated(&self) -> bool {
        self.incomplete.any()
    }
}

/// What one reader incarnation lets a snapshot read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AxScopePolicy {
    /// Accepted `scheme://host` origins, ASCII case-insensitive. When not
    /// empty, a web document whose URL has another origin keeps only its
    /// identity: nothing inside it is read. A document whose URL cannot be
    /// read is read whole (it may be the call). Empty: no web filter.
    pub origins: Vec<String>,
    /// Read whole-document text (UI Automation text ranges, AT-SPI document
    /// text) only when a rule reads document values.
    pub document_text: bool,
}

/// One reader incarnation's view of one app: its [`AxScopePolicy`] and the
/// platform state that must live exactly as long as the incarnation (the
/// macOS element registry). Clones share that state; it is dropped with the
/// last clone, so a detached walk of an earlier incarnation can never feed
/// a newer one.
#[derive(Clone)]
pub struct AxScope {
    pid: u32,
    policy: Arc<AxScopePolicy>,
    state: Option<Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for AxScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AxScope")
            .field("pid", &self.pid)
            .field("origins", &self.policy.origins.len())
            .field("document_text", &self.policy.document_text)
            .field("state", &self.state.is_some())
            .finish()
    }
}

impl AxScope {
    /// A scope without platform state.
    pub fn new(pid: u32, policy: AxScopePolicy) -> Self {
        Self {
            pid,
            policy: Arc::new(policy),
            state: None,
        }
    }

    /// A scope carrying a platform reader's per-incarnation state.
    pub fn with_state(pid: u32, policy: AxScopePolicy, state: Arc<dyn Any + Send + Sync>) -> Self {
        Self {
            pid,
            policy: Arc::new(policy),
            state: Some(state),
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn policy(&self) -> &AxScopePolicy {
        &self.policy
    }

    /// The platform state, if this scope carries one of type `T`.
    pub fn state<T: Any>(&self) -> Option<&T> {
        self.state.as_deref()?.downcast_ref()
    }

    /// Whether the contents of a document at `url` may be read.
    pub fn permits(&self, url: Option<&str>) -> bool {
        let origins = &self.policy.origins;
        if origins.is_empty() {
            return true;
        }
        let Some(url) = url else {
            return true;
        };
        url_origin(url)
            .is_some_and(|origin| origins.iter().any(|o| o.eq_ignore_ascii_case(&origin)))
    }

    /// Whether `node` is a web document whose contents this scope skips.
    pub fn skips(&self, node: &AxNode) -> bool {
        node.role == "AXWebArea"
            && node
                .document
                .as_ref()
                .is_some_and(|document| !self.permits(document.url()))
    }

    /// Drops the contents of every document [`Self::skips`], for readers
    /// that cannot skip them while walking.
    pub fn prune(&self, tree: &mut AxTree) {
        fn walk(scope: &AxScope, node: &mut AxNode) {
            if scope.skips(node) {
                node.children.clear();
                return;
            }
            for child in &mut node.children {
                walk(scope, child);
            }
        }
        walk(self, &mut tree.root);
    }
}

/// Stable [`Error::Service`] code of an accessibility read that failed for
/// a passing reason (the app was busy, a request timed out): retry later.
pub const AX_TRANSIENT: &str = "accessibility_transient";

/// A transient accessibility failure ([`AX_TRANSIENT`]).
pub fn ax_transient(message: impl Into<String>) -> Error {
    Error::Service {
        code: AX_TRANSIENT,
        message: message.into(),
    }
}

pub fn is_ax_transient(error: &Error) -> bool {
    error.code() == AX_TRANSIENT
}

/// Accessibility notifications a reader listens to. Any of them means "the
/// UI changed, take a new snapshot".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AxChange {
    ValueChanged,
    TitleChanged,
    ElementCreated,
    ElementDestroyed,
    WindowCreated,
    FocusedWindowChanged,
    LayoutChanged,
    /// Web content: an ARIA live region (captions) changed.
    LiveRegionChanged,
}

impl AxChange {
    /// Every notification, in the order observers register them.
    pub const ALL: [Self; 8] = [
        Self::ValueChanged,
        Self::TitleChanged,
        Self::ElementCreated,
        Self::ElementDestroyed,
        Self::WindowCreated,
        Self::FocusedWindowChanged,
        Self::LayoutChanged,
        Self::LiveRegionChanged,
    ];
}

/// Receives the observed app's notifications, on the thread whose run loop
/// the observer was attached to (the UI loop's main thread).
pub type AxChangeCallback = Box<dyn Fn(AxChange) + Send>;

/// A running observation. Dropping it detaches the observer: no callback
/// runs after the drop returns. Main-thread only, like the observer itself.
pub trait AxObservation {}

/// Reads a meeting app's UI (roster, captions, active speaker) through the
/// platform accessibility API (design §9): no OCR, no screenshots, and it
/// works while the window sits behind others.
///
/// Snapshots may run on any thread; [`AppUiReader::observe`] attaches to the
/// calling thread's run loop and must be called on the UI loop's main thread
/// (one event-loop owner, AGENTS.md).
pub trait AppUiReader: Send + Sync {
    /// Whether this process may read other apps' UI (macOS: the Accessibility
    /// permission). Never prompts.
    fn is_trusted(&self) -> bool;

    /// Asks the system to show its permission prompt (setup step 2, after
    /// BroScribe has explained why) and returns the current state; the grant
    /// itself arrives later.
    fn request_trust(&self) -> bool;

    /// A one-off tree of `pid`'s application element, capped by `limits`
    /// (rule-authoring dumps). Element tokens are not comparable across
    /// one-off snapshots.
    fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree>;

    /// The scope of one reader incarnation of `pid`. Readers keeping no
    /// per-incarnation state need not override it.
    fn scope(&self, pid: u32, policy: AxScopePolicy) -> AxScope {
        AxScope::new(pid, policy)
    }

    /// A tree of the scope's app within its policy; element tokens stay
    /// comparable for the scope's lifetime. Readers that cannot skip
    /// documents while walking read whole and prune.
    fn snapshot_in(&self, scope: &AxScope, limits: AxLimits) -> Result<AxTree> {
        let mut tree = self.snapshot(scope.pid(), limits)?;
        scope.prune(&mut tree);
        Ok(tree)
    }

    /// Which of `elements` (tokens issued within `scope`) the platform
    /// verified destroyed: the element itself reports it no longer exists.
    /// Absent, hidden, on another Space, unanswered or unknown elements are
    /// never listed. Readers that cannot verify it list none.
    fn destroyed(&self, scope: &AxScope, elements: &[String]) -> Result<Vec<String>> {
        let _ = (scope, elements);
        Ok(Vec::new())
    }

    /// Calls `callback` for every [`AxChange`] in `changes` that `pid`
    /// posts, until the returned observation is dropped.
    fn observe(
        &self,
        pid: u32,
        changes: &[AxChange],
        callback: AxChangeCallback,
    ) -> Result<Box<dyn AxObservation>>;

    /// Chromium's and Electron's `AXManualAccessibility` on `pid`'s
    /// application element: `None` when the app does not support it.
    fn manual_accessibility(&self, pid: u32) -> Result<Option<bool>>;

    /// Sets `AXManualAccessibility`, which makes Chromium-based browsers and
    /// Electron apps build their web accessibility tree without a screen
    /// reader running.
    fn set_manual_accessibility(&self, pid: u32, enabled: bool) -> Result<()>;
}

/// Where the status item sends the [`MenuItemSpec::id`] of each clicked menu
/// item. The app drains the receiver on GPUI's foreground executor.
pub type StatusClickSender = futures::channel::mpsc::UnboundedSender<String>;

/// One entry of the status item's menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItemSpec {
    /// Sent on the click channel when the item is chosen.
    pub id: String,
    pub title: String,
    pub enabled: bool,
    /// Triggered when the status item itself is activated (Linux `Activate` /
    /// `SecondaryActivate`, Windows `NIN_SELECT` / `NIN_KEYSELECT`, and bolded
    /// in Windows popup menus). Never falls back to the first item when false.
    pub default: bool,
}

impl MenuItemSpec {
    pub const SEPARATOR_TITLE: &'static str = "-";

    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            enabled: true,
            default: false,
        }
    }

    /// A separator line (never clickable).
    pub fn separator() -> Self {
        Self {
            id: String::new(),
            title: Self::SEPARATOR_TITLE.into(),
            enabled: false,
            default: false,
        }
    }

    pub fn is_separator(&self) -> bool {
        self.id.is_empty() && self.title == Self::SEPARATOR_TITLE
    }
}

/// The enabled non-separator item marked [`MenuItemSpec::default`], if any.
/// Activation never falls back to the first enabled item.
pub fn default_menu_item(items: &[MenuItemSpec]) -> Option<&MenuItemSpec> {
    items
        .iter()
        .find(|item| item.default && item.enabled && !item.is_separator())
}

/// The status item's glyph: BroScribe's waveform, with a dot while audio is
/// captured or two bars while paused, in one ink: a template image in the
/// macOS menu bar, the taskbar's contrasting ink on Windows, light ink in a
/// dark ring on Linux panels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusIcon {
    #[default]
    Idle,
    Recording,
    Paused,
}

/// A taskbar button's optional recording-state overlay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskbarState {
    #[default]
    None,
    Recording,
    Paused,
}

/// The menu-bar item. Created and used on the main thread, inside the UI
/// event loop (GPUI's); implementations are `!Send`. Clicks arrive as item ids
/// on the [`StatusClickSender`] given at construction.
pub trait StatusItem {
    /// The item's full spoken name, state and counts in words (e.g.
    /// `"BroScribe · Recording · 3 speakers to review"`), used for tooltips,
    /// accessibility labels and text-based tray hosts.
    fn set_title(&mut self, title: &str) -> Result<()>;
    /// The glyph for the recording state. Platforms without a per-state
    /// glyph keep their icon; the title's words still carry the state.
    fn set_icon(&mut self, icon: StatusIcon) -> Result<()>;
    /// Replaces the whole menu.
    fn set_menu(&mut self, items: Vec<MenuItemSpec>) -> Result<()>;
    /// The number shown beside the glyph (nothing when 0). Only macOS
    /// draws it; Linux and Windows show no separate number, and on every
    /// platform the counts are said in words in the [title](Self::set_title).
    /// Counts pending Record/Ignore prompts only. Linux uses this count to
    /// request attention from the desktop host.
    fn set_badge_count(&mut self, n: usize) -> Result<()>;
    /// Applies a described state glyph to the button for this app window.
    fn set_taskbar_state(
        &mut self,
        _window: &dyn crate::overlay::HasWindowHandle,
        _state: TaskbarState,
    ) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod ax_tests {
    use super::*;

    fn document(url: Option<&str>, children: Vec<AxNode>) -> AxNode {
        AxNode {
            document: Some(DocumentContext::new(
                url.map(Into::into),
                Some("doc".into()),
                false,
            )),
            children,
            ..AxNode::new("AXWebArea")
        }
    }

    #[test]
    fn every_incomplete_reason_truncates_the_snapshot_and_is_named() {
        let reasons: [fn(&mut AxIncomplete); 6] = [
            |i| i.node_cap = true,
            |i| i.depth_cap = true,
            |i| i.time_budget = true,
            |i| i.read_failed = true,
            |i| i.clipped = true,
            |i| i.identity_cap = true,
        ];
        let names = [
            "node_cap",
            "depth_cap",
            "time_budget",
            "read_failed",
            "clipped",
            "identity_cap",
        ];
        assert!(!AxTree::default().truncated());
        assert!(AxIncomplete::default().names().is_empty());
        let mut all = AxIncomplete::default();
        for (set, name) in reasons.into_iter().zip(names) {
            let mut tree = AxTree::default();
            set(&mut tree.incomplete);
            assert!(tree.incomplete.any() && tree.truncated(), "{tree:?}");
            assert_eq!(tree.incomplete.names(), [name]);
            set(&mut all);
        }
        assert_eq!(all.names(), names);
    }

    #[test]
    fn scope_prunes_only_documents_of_other_known_origins() {
        let scope = AxScope::new(
            7,
            AxScopePolicy {
                origins: vec!["https://meet.google.com".into()],
                document_text: false,
            },
        );
        let text = || vec![AxNode::new("AXStaticText")];
        let mut tree = AxTree {
            root: AxNode {
                children: vec![
                    document(Some("https://MEET.google.com/abc-defg-hij?x"), text()),
                    document(Some("https://mail.example.com/inbox"), text()),
                    document(None, text()),
                    document(Some("not a url"), text()),
                ],
                ..AxNode::new("AXApplication")
            },
            incomplete: AxIncomplete::default(),
        };
        scope.prune(&mut tree);
        let kept: Vec<usize> = tree
            .root
            .children
            .iter()
            .map(|d| d.children.len())
            .collect();
        assert_eq!(kept, [1, 0, 1, 0]);
        assert!(!tree.truncated(), "policy pruning is not a cut");
        // No filter reads everything.
        assert!(AxScope::new(7, AxScopePolicy::default()).permits(Some("https://x.example")));
    }

    #[test]
    fn limits_validation_and_transient_errors() {
        assert!(AxLimits::default().validate().is_ok());
        for bad in [
            AxLimits {
                max_nodes: 0,
                ..AxLimits::default()
            },
            AxLimits {
                element_timeout: Duration::ZERO,
                ..AxLimits::default()
            },
            AxLimits {
                element_timeout: Duration::from_secs(5),
                budget: Duration::from_secs(1),
                ..AxLimits::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert!(is_ax_transient(&ax_transient("busy")));
        assert!(!is_ax_transient(&Error::NotFound("gone".into())));
        assert_eq!(
            url_origin("HTTPS://Meet.Google.com:443/x"),
            Some("https://meet.google.com:443".into())
        );
        assert_eq!(url_origin("about:blank"), None);
    }

    #[test]
    fn scope_state_is_shared_by_clones_and_dropped_with_the_last() {
        let state = Arc::new(5u32);
        let scope = AxScope::with_state(1, AxScopePolicy::default(), state.clone());
        let clone = scope.clone();
        assert_eq!(clone.state::<u32>(), Some(&5));
        assert_eq!(clone.state::<String>(), None);
        drop(scope);
        assert_eq!(Arc::strong_count(&state), 2);
        drop(clone);
        assert_eq!(Arc::strong_count(&state), 1);
        let debug = format!(
            "{:?}",
            AxScope::new(
                1,
                AxScopePolicy {
                    origins: vec!["https://private.example".into()],
                    document_text: false
                }
            )
        );
        assert!(!debug.contains("private"));
    }

    #[test]
    fn menu_item_default_is_false_and_activation_never_falls_back() {
        let first = MenuItemSpec::new("record-start", "Start recording");
        let sep = MenuItemSpec::separator();
        assert!(!first.default);
        assert!(!sep.default);
        assert_eq!(default_menu_item(&[first.clone(), sep.clone()]), None);

        let mut disabled_default = MenuItemSpec::new("open-app", "Open BroScribe");
        disabled_default.default = true;
        disabled_default.enabled = false;
        assert_eq!(
            default_menu_item(&[first.clone(), disabled_default]),
            None,
            "a disabled default item is not activated"
        );

        let mut open = MenuItemSpec::new("open-app", "Open BroScribe");
        open.default = true;
        let items = [first, sep, open];
        let chosen = default_menu_item(&items).unwrap();
        assert_eq!(chosen.id, "open-app");
    }

    #[test]
    fn display_preferences_pick_contrast_and_opacity() {
        let none = DisplayPreferences::default();
        assert_eq!(NoDisplayAccessibility.preferences(), none);
        assert!(!none.more_contrast() && !none.opaque());
        let transparency = DisplayPreferences {
            reduce_transparency: true,
            ..none
        };
        assert!(!transparency.more_contrast() && transparency.opaque());
        for contrast in [
            DisplayPreferences {
                increase_contrast: true,
                ..none
            },
            DisplayPreferences {
                forced_colors: true,
                ..none
            },
        ] {
            assert!(
                contrast.more_contrast() && contrast.opaque(),
                "{contrast:?}"
            );
        }
    }
}

/// Native path selection for import. The UI remains in charge of its event loop.
pub trait ImportPathPicker {
    fn pick(
        &self,
        request: ImportPathRequest,
    ) -> futures::future::LocalBoxFuture<
        'static,
        std::result::Result<Option<Vec<std::path::PathBuf>>, String>,
    >;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportPathRequest {
    pub files: bool,
    pub directories: bool,
    pub multiple: bool,
    pub prompt: Option<String>,
}

/// The file URLs/paths delivered by the platform's drag session.
pub trait ImportFileDrop {
    fn paths(&self) -> &[std::path::PathBuf];
}
