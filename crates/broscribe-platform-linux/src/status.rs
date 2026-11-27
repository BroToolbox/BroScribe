use std::collections::HashMap;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use broscribe_platform::{MenuItemSpec, Result, StatusClickSender, StatusIcon, StatusItem};
use futures::StreamExt;
use zbus::blocking::connection::Builder;
use zbus::zvariant::{ObjectPath, OwnedValue, Str, Value};

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const METHOD_TIMEOUT: Duration = Duration::from_secs(2);

type IconPixmap = (i32, i32, Vec<u8>);
type ToolTip = (String, Vec<IconPixmap>, String, String);

/// Whether a StatusNotifierWatcher runs on the session bus. Without one the
/// tray icon shows nowhere, so text must not send the user there (SHL-14).
/// A session bus that can't be asked counts as none.
pub fn status_notifier_watcher_present() -> bool {
    let Ok(connection) = tray_bus().and_then(Builder::build) else {
        return false;
    };
    let Ok(proxy) = zbus::blocking::fdo::DBusProxy::new(&connection) else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(WATCHER) else {
        return false;
    };
    proxy.name_has_owner(name).unwrap_or(false)
}

fn tray_bus() -> zbus::Result<Builder<'static>> {
    Ok(Builder::session()?.method_timeout(METHOD_TIMEOUT))
}

/// Whether a `NameOwnerChanged` signal announces a (re)started
/// StatusNotifierWatcher, which has forgotten every registered item.
fn watcher_appeared(name: &str, new_owner: Option<&str>) -> bool {
    name == WATCHER && new_owner.is_some_and(|owner| !owner.is_empty())
}

/// The status glyph per state, as PNGs at 22, 32, 44 and 64 px (the host
/// scales the closest): light ink inside a dark ring, which reads on light
/// and dark panels. Rendered with the app's brand files
/// (crates/broscribe-app/assets/brand,
/// `cargo run -p broscribe-app --example render_brand`).
fn tray_pngs(icon: StatusIcon) -> [&'static [u8]; 4] {
    macro_rules! pngs {
        ($state:literal) => {
            [
                include_bytes!(concat!(
                    "../../broscribe-app/assets/brand/linux/tray-",
                    $state,
                    "-22.png"
                )),
                include_bytes!(concat!(
                    "../../broscribe-app/assets/brand/linux/tray-",
                    $state,
                    "-32.png"
                )),
                include_bytes!(concat!(
                    "../../broscribe-app/assets/brand/linux/tray-",
                    $state,
                    "-44.png"
                )),
                include_bytes!(concat!(
                    "../../broscribe-app/assets/brand/linux/tray-",
                    $state,
                    "-64.png"
                )),
            ]
        };
    }
    match icon {
        StatusIcon::Idle => pngs!("idle"),
        StatusIcon::Recording => pngs!("recording"),
        StatusIcon::Paused => pngs!("paused"),
    }
}

/// A PNG as a StatusNotifierItem pixmap: big-endian ARGB32, as the
/// specification requires. `None` unless it is 8-bit RGBA.
fn decode(png: &[u8]) -> Option<IconPixmap> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .ok()?;
    let mut buffer = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buffer).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let pixels = info.width as usize * info.height as usize;
    let (rgba, _) = buffer.get(..pixels * 4)?.as_chunks::<4>();
    let argb = rgba.iter().flat_map(|&[r, g, b, a]| [a, r, g, b]).collect();
    Some((
        i32::try_from(info.width).ok()?,
        i32::try_from(info.height).ok()?,
        argb,
    ))
}

/// The symbolic glyph per state: an SVG that GTK and KDE hosts paint in the
/// panel's own text colour, as a macOS menu bar paints a template image.
fn symbolic(icon: StatusIcon) -> (&'static str, &'static [u8]) {
    macro_rules! svg {
        ($name:literal) => {
            (
                $name,
                include_bytes!(concat!(
                    "../../broscribe-app/assets/brand/linux/",
                    $name,
                    ".svg"
                )),
            )
        };
    }
    match icon {
        StatusIcon::Idle => svg!("broscribe-idle-symbolic"),
        StatusIcon::Recording => svg!("broscribe-recording-symbolic"),
        StatusIcon::Paused => svg!("broscribe-paused-symbolic"),
    }
}

/// Writes the symbolic glyphs into `$XDG_RUNTIME_DIR/broscribe/icons`, the
/// item's `IconThemePath`. `None` without a private runtime directory (as
/// for the agent's socket): the item then sends its pixmaps only.
fn install_symbolic_icons() -> Option<String> {
    install_symbolic_icons_in(std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from))
}

const SYMBOLIC_STATES: [StatusIcon; 3] =
    [StatusIcon::Idle, StatusIcon::Recording, StatusIcon::Paused];

pub(crate) fn symbolic_icon_names() -> impl Iterator<Item = &'static str> {
    SYMBOLIC_STATES.into_iter().map(|icon| symbolic(icon).0)
}

pub(crate) fn install_symbolic_icons_in(runtime: Option<std::path::PathBuf>) -> Option<String> {
    use std::os::unix::fs::DirBuilderExt as _;
    let runtime = runtime.filter(|dir| {
        dir.is_absolute() && broscribe_platform_unix::check_private_dir(dir).is_ok()
    })?;
    let dir = runtime.join("broscribe").join("icons");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()?;
    for icon in SYMBOLIC_STATES {
        let (name, svg) = symbolic(icon);
        let path = dir.join(format!("{name}.svg"));
        if std::fs::read(&path).is_ok_and(|current| current == svg) {
            continue;
        }
        // Whole files only: a panel may read while another profile writes.
        let partial = dir.join(format!(".{name}.svg.{}", std::process::id()));
        std::fs::write(&partial, svg).ok()?;
        std::fs::rename(&partial, &path).ok()?;
    }
    dir.into_os_string().into_string().ok()
}

/// `icon`'s pixmaps, decoded once.
fn pixmaps(icon: StatusIcon) -> Vec<IconPixmap> {
    static DECODED: LazyLock<[Vec<IconPixmap>; 3]> = LazyLock::new(|| {
        [StatusIcon::Idle, StatusIcon::Recording, StatusIcon::Paused].map(|icon| {
            tray_pngs(icon)
                .iter()
                .filter_map(|png| decode(png))
                .collect()
        })
    });
    let index = match icon {
        StatusIcon::Idle => 0,
        StatusIcon::Recording => 1,
        StatusIcon::Paused => 2,
    };
    DECODED[index].clone()
}

#[derive(Default)]
struct State {
    title: String,
    icon: StatusIcon,
    needs_attention: bool,
    revision: u32,
    /// Item numbers from an earlier layout must never identify another ask.
    menu_offset: i32,
    menu: Vec<MenuItemSpec>,
}

impl State {
    fn menu_item(&self, id: i32) -> Option<&MenuItemSpec> {
        let index = id.checked_sub(self.menu_offset)?.checked_sub(1)?;
        self.menu.get(usize::try_from(index).ok()?)
    }

    fn replace_menu(&mut self, items: Vec<MenuItemSpec>) -> Result<u32> {
        let next = self.menu_offset as usize + self.menu.len();
        if next + items.len() > i32::MAX as usize {
            return Err(crate::platform_error(
                "updating status menu",
                "menu IDs exhausted",
            ));
        }
        self.menu_offset = next as i32;
        self.menu = items;
        self.revision = self.revision.wrapping_add(1);
        Ok(self.revision)
    }
}
struct ItemService {
    state: Arc<Mutex<State>>,
    clicks: StatusClickSender,
    /// Where the symbolic glyphs are, if they could be written.
    icon_theme_path: Option<String>,
}
impl ItemService {
    /// The current state's symbolic name, or none (pixmaps only).
    fn current_icon_name(&self) -> String {
        match self.icon_theme_path {
            Some(_) => {
                let icon = self.state.lock().unwrap_or_else(|e| e.into_inner()).icon;
                symbolic(icon).0.to_owned()
            }
            None => String::new(),
        }
    }
}
struct MenuService {
    state: Arc<Mutex<State>>,
    clicks: StatusClickSender,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl ItemService {
    fn activate(&self, _x: i32, _y: i32) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(item) = broscribe_platform::ui::default_menu_item(&state.menu) {
            let _ = self.clicks.unbounded_send(item.id.clone());
        }
    }
    fn secondary_activate(&self, x: i32, y: i32) {
        self.activate(x, y);
    }
    // The host uses the exported DBusMenu; no private GUI/menu event loop.
    fn context_menu(&self, _x: i32, _y: i32) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "The desktop host renders the DBusMenu exported at /MenuBar".into(),
        ))
    }
    fn scroll(&self, _delta: i32, _orientation: &str) -> zbus::fdo::Result<()> {
        Err(zbus::fdo::Error::NotSupported(
            "BroScribe has no scroll action".into(),
        ))
    }
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }
    #[zbus(property)]
    fn id(&self) -> &str {
        "broscribe"
    }
    #[zbus(property)]
    fn title(&self) -> String {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.title.clone()
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        if self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .needs_attention
        {
            "NeedsAttention"
        } else {
            "Active"
        }
    }
    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }
    /// The current state's symbolic glyph, found in [`Self::icon_theme_path`];
    /// hosts that cannot load it draw [`Self::icon_pixmap`].
    #[zbus(property)]
    fn icon_name(&self) -> String {
        self.current_icon_name()
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> String {
        self.icon_theme_path.clone().unwrap_or_default()
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<IconPixmap> {
        pixmaps(self.state.lock().unwrap_or_else(|e| e.into_inner()).icon)
    }
    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        self.current_icon_name()
    }
    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Vec<IconPixmap> {
        self.icon_pixmap()
    }
    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }
    /// The item is more than its menu: [`Self::activate`] runs Open
    /// BroScribe. Which click the host turns into `Activate`, and which
    /// opens the menu exported at [`Self::menu`] instead, is the host's
    /// choice (design §4.3); a host told `ItemIsMenu` may open the menu on
    /// every click and never call `Activate`.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'_> {
        ObjectPath::from_static_str_unchecked("/MenuBar")
    }
    #[zbus(property)]
    fn tool_tip(&self) -> ToolTip {
        (
            self.current_icon_name(),
            self.icon_pixmap(),
            self.title(),
            "Local meeting recorder".into(),
        )
    }
    #[zbus(signal)]
    async fn new_title(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn new_icon(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn new_tool_tip(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

type Properties = HashMap<String, OwnedValue>;
type Layout = (i32, Properties, Vec<OwnedValue>);
fn string_value(s: &str) -> OwnedValue {
    OwnedValue::from(Str::from(s))
}
fn properties(item: &MenuItemSpec) -> Properties {
    let mut props = Properties::new();
    props.insert("label".into(), string_value(&item.title));
    props.insert("enabled".into(), OwnedValue::from(item.enabled));
    props.insert("visible".into(), OwnedValue::from(true));
    if item.is_separator() {
        props.insert("type".into(), string_value("separator"));
    }
    props
}
#[zbus::interface(name = "com.canonical.dbusmenu")]
impl MenuService {
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if parent_id > 0 {
            let item = state
                .menu_item(parent_id)
                .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown menu id".into()))?;
            return Ok((state.revision, (parent_id, properties(item), Vec::new())));
        }
        let children = if recursion_depth == 0 {
            Vec::new()
        } else {
            state
                .menu
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    OwnedValue::try_from(Value::from((
                        state.menu_offset + (i + 1) as i32,
                        properties(item),
                        Vec::<OwnedValue>::new(),
                    )))
                    .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
                })
                .collect::<zbus::fdo::Result<Vec<_>>>()?
        };
        Ok((state.revision, (0, Properties::new(), children)))
    }
    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, Properties)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .menu
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let id = state.menu_offset + (i + 1) as i32;
                (ids.is_empty() || ids.contains(&id)).then(|| (id, properties(item)))
            })
            .collect()
    }
    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedValue> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .menu_item(id)
            .and_then(|i| properties(i).remove(name))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown menu item/property".into()))
    }
    fn event(&self, id: i32, event_id: &str, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        if let Some(item) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .menu_item(id)
            .filter(|i| i.enabled && !i.is_separator())
        {
            let _ = self.clicks.unbounded_send(item.id.clone());
        }
    }
    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, event, data, time) in events {
            self.event(id, &event, data, time);
        }
        Vec::new()
    }
    fn about_to_show(&self, _id: i32) -> bool {
        false
    }
    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A D-Bus service with clicks bridged to GPUI's foreground channel. The
/// zbus transport executor handles D-Bus I/O only; it never owns a UI loop.
pub struct LinuxStatusItem {
    connection: zbus::blocking::Connection,
    state: Arc<Mutex<State>>,
    /// Registers the item again whenever the watcher (the panel or shell
    /// extension owning `WATCHER`) restarts; dropping it cancels the task.
    _watcher_restarts: zbus::Task<()>,
    _main_thread: PhantomData<Rc<()>>,
}
/// Asks the watcher to show the item served under `item` (a bus name).
async fn register(connection: &zbus::Connection, item: &str) -> zbus::Result<()> {
    connection
        .call_method(
            Some(WATCHER),
            WATCHER_PATH,
            Some(WATCHER),
            "RegisterStatusNotifierItem",
            &(item,),
        )
        .await
        .map(|_| ())
}
impl LinuxStatusItem {
    pub fn new(clicks: StatusClickSender) -> Result<Self> {
        let state = Arc::new(Mutex::new(State {
            title: "BroScribe".into(),
            ..State::default()
        }));
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let connection = tray_bus()
            .map_err(|e| crate::platform_error("connecting status item", e))?
            .name(name.as_str())
            .map_err(|e| crate::platform_error("naming status item", e))?
            .serve_at(
                "/StatusNotifierItem",
                ItemService {
                    state: state.clone(),
                    clicks: clicks.clone(),
                    icon_theme_path: install_symbolic_icons(),
                },
            )
            .map_err(|e| crate::platform_error("exporting status item", e))?
            .serve_at(
                "/MenuBar",
                MenuService {
                    state: state.clone(),
                    clicks,
                },
            )
            .map_err(|e| crate::platform_error("exporting status menu", e))?
            .build()
            .map_err(|e| crate::platform_error("starting status item", e))?;
        let bus = connection.inner().clone();
        // Subscribe before the first registration, so a watcher restarting
        // in between is still seen. The watcher keeps no state across a
        // restart: without registering again the icon would vanish for good.
        let mut restarts = futures::executor::block_on(async {
            zbus::fdo::DBusProxy::new(&bus)
                .await?
                .receive_name_owner_changed_with_args(&[(0, WATCHER)])
                .await
        })
        .map_err(|e| crate::platform_error("watching the status notifier watcher", e))?;
        futures::executor::block_on(register(&bus, &name)).map_err(|e| {
            crate::platform_error(
                "registering status item (desktop must support StatusNotifierItem)",
                e,
            )
        })?;
        let task_bus = bus.clone();
        let watcher_restarts = bus.executor().spawn(
            async move {
                while let Some(change) = restarts.next().await {
                    let Ok(args) = change.args() else {
                        continue;
                    };
                    let new_owner: &Option<zbus::names::UniqueName<'_>> = args.new_owner();
                    if watcher_appeared(
                        args.name().as_str(),
                        new_owner.as_ref().map(|owner| owner.as_str()),
                    ) {
                        // A refusal leaves the item unregistered until the
                        // watcher's next start, exactly as at login.
                        let _ = register(&task_bus, &name).await;
                    }
                }
            },
            "broscribe-status-watcher",
        );
        Ok(Self {
            connection,
            state,
            _watcher_restarts: watcher_restarts,
            _main_thread: PhantomData,
        })
    }
    fn changed(&self) -> Result<()> {
        self.connection
            .emit_signal(
                None::<&str>,
                "/StatusNotifierItem",
                "org.kde.StatusNotifierItem",
                "NewTitle",
                &(),
            )
            .map_err(|e| crate::platform_error("updating status title", e))?;
        self.connection
            .emit_signal(
                None::<&str>,
                "/StatusNotifierItem",
                "org.kde.StatusNotifierItem",
                "NewToolTip",
                &(),
            )
            .map_err(|e| crate::platform_error("updating status tooltip", e))?;
        Ok(())
    }
}
impl StatusItem for LinuxStatusItem {
    fn set_title(&mut self, title: &str) -> Result<()> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).title = title.into();
        self.changed()
    }
    /// The waveform glyph for `icon`; the title says the state in words.
    fn set_icon(&mut self, icon: StatusIcon) -> Result<()> {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.icon == icon {
                return Ok(());
            }
            state.icon = icon;
        }
        self.connection
            .emit_signal(
                None::<&str>,
                "/StatusNotifierItem",
                "org.kde.StatusNotifierItem",
                "NewIcon",
                &(),
            )
            .map_err(|e| crate::platform_error("updating the system tray item", e))?;
        self.changed()
    }
    fn set_badge_count(&mut self, n: usize) -> Result<()> {
        let needs_attention = n > 0;
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.needs_attention == needs_attention {
                return Ok(());
            }
            state.needs_attention = needs_attention;
        }
        self.connection
            .emit_signal(
                None::<&str>,
                "/StatusNotifierItem",
                "org.kde.StatusNotifierItem",
                "NewStatus",
                &(if needs_attention {
                    "NeedsAttention"
                } else {
                    "Active"
                },),
            )
            .map_err(|e| crate::platform_error("updating system tray attention", e))
    }
    fn set_menu(&mut self, items: Vec<MenuItemSpec>) -> Result<()> {
        let revision = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.replace_menu(items)?
        };
        self.connection
            .emit_signal(
                None::<&str>,
                "/MenuBar",
                "com.canonical.dbusmenu",
                "LayoutUpdated",
                &(revision, 0i32),
            )
            .map_err(|e| crate::platform_error("updating status menu", e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "run with dbus-run-session to use an isolated session bus"]
    fn tray_connections_bound_dbus_method_calls() {
        let connection = tray_bus().and_then(Builder::build).unwrap();
        let timeout = connection.inner().method_timeout().unwrap();
        assert!((Duration::from_secs(1)..=Duration::from_secs(2)).contains(&timeout));
    }

    #[test]
    fn late_menu_click_never_answers_a_replacement_prompt() {
        let mut model = State::default();
        model
            .replace_menu(vec![MenuItemSpec::new(
                "prompt:ask-synthetic-1:record",
                "Record",
            )])
            .unwrap();
        let old = model.menu_offset + 1;
        model
            .replace_menu(vec![MenuItemSpec::new(
                "prompt:ask-synthetic-2:record",
                "Record",
            )])
            .unwrap();
        let current = model.menu_offset + 1;
        let (clicks, mut received) = futures::channel::mpsc::unbounded();
        let service = MenuService {
            state: Arc::new(Mutex::new(model)),
            clicks,
        };
        let data = || OwnedValue::from(0u32);
        service.event(old, "clicked", data(), 0);
        assert!(received.try_recv().is_err());
        service.event(current, "clicked", data(), 0);
        assert_eq!(
            received.try_recv().unwrap(),
            "prompt:ask-synthetic-2:record"
        );
    }

    /// A watcher restart is a `NameOwnerChanged` for its well-known name
    /// with a new owner: the item registers again. Its disappearance, or
    /// another name changing owner, registers nothing.
    #[test]
    fn a_restarted_watcher_triggers_registration_again() {
        assert!(watcher_appeared(WATCHER, Some(":1.42")));
        assert!(!watcher_appeared(WATCHER, None));
        assert!(!watcher_appeared(WATCHER, Some("")));
        assert!(!watcher_appeared(
            "org.kde.StatusNotifierHost-7",
            Some(":1.43")
        ));
        assert!(!watcher_appeared(
            "org.freedesktop.Notifications",
            Some(":1.44")
        ));
    }

    fn pixel(pixmap: &IconPixmap, x: usize, y: usize) -> [u8; 4] {
        let start = (y * pixmap.0 as usize + x) * 4;
        pixmap.2[start..start + 4].try_into().unwrap()
    }

    /// Every state has square big-endian ARGB32 pixmaps at 22, 32, 44 and
    /// 64 px: a transparent margin, light ink inside a darker ring, and a
    /// different drawing per state.
    #[test]
    fn every_state_has_ringed_argb_pixmaps() {
        let mut drawings = Vec::new();
        for icon in [StatusIcon::Idle, StatusIcon::Recording, StatusIcon::Paused] {
            let pixmaps = pixmaps(icon);
            let sizes: Vec<(i32, i32)> = pixmaps.iter().map(|p| (p.0, p.1)).collect();
            assert_eq!(sizes, [(22, 22), (32, 32), (44, 44), (64, 64)], "{icon:?}");
            for pixmap in &pixmaps {
                let side = pixmap.0 as usize;
                assert_eq!(pixmap.2.len(), side * side * 4);
                for corner in [(0, 0), (side - 1, 0), (0, side - 1), (side - 1, side - 1)] {
                    assert_eq!(pixel(pixmap, corner.0, corner.1)[0], 0, "{icon:?} {side}");
                }
                // Alpha first: the opaque pixels are the light ink and the
                // dark ring around it.
                let opaque: Vec<[u8; 4]> = pixmap
                    .2
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|p| p[0] == 0xff)
                    .copied()
                    .collect();
                let luma = |p: &[u8; 4]| p[1..].iter().map(|&c| u32::from(c)).sum::<u32>();
                assert!(
                    opaque.iter().any(|p| luma(p) > 600),
                    "{icon:?} {side}: no light ink"
                );
                assert!(
                    opaque.iter().any(|p| luma(p) < 150),
                    "{icon:?} {side}: no dark ring"
                );
            }
            drawings.push(pixmaps[0].2.clone());
        }
        assert_ne!(drawings[0], drawings[1]);
        assert_ne!(drawings[1], drawings[2]);
        assert_ne!(drawings[0], drawings[2]);
    }

    /// Each state's symbolic icon is a `-symbolic` SVG drawn only with
    /// filled shapes in `currentColor` (what GTK recolours) and KDE's
    /// colour-scheme class.
    #[test]
    fn every_state_has_a_recolourable_symbolic_svg() {
        let mut names = Vec::new();
        for icon in [StatusIcon::Idle, StatusIcon::Recording, StatusIcon::Paused] {
            let (name, svg) = symbolic(icon);
            let svg = std::str::from_utf8(svg).unwrap();
            assert!(name.starts_with("broscribe-") && name.ends_with("-symbolic"));
            assert!(svg.starts_with("<svg") && svg.contains(r#"viewBox="0 0 16 16""#));
            assert!(svg.contains(r#"fill="currentColor""#), "{name}");
            assert!(svg.contains("ColorScheme-Text"), "{name}");
            assert!(!svg.contains("stroke"), "{name}: GTK recolours fills only");
            names.push(name);
        }
        names.dedup();
        assert_eq!(names.len(), 3);
    }

    /// The glyphs land in the runtime directory whole and stay put when
    /// they are already current; without a private runtime directory there
    /// is no theme path.
    #[test]
    fn symbolic_icons_are_written_to_the_private_runtime_dir() {
        let runtime = tempfile::tempdir().unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(runtime.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        let first = install_symbolic_icons_in(Some(runtime.path().to_owned()));
        let second = install_symbolic_icons_in(Some(runtime.path().to_owned()));
        assert_eq!(install_symbolic_icons_in(Some("relative/dir".into())), None);
        assert_eq!(install_symbolic_icons_in(None), None);
        let dir = runtime.path().join("broscribe").join("icons");
        assert_eq!(first.as_deref(), dir.to_str());
        assert_eq!(second, first);
        let mut files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(
            files,
            [
                "broscribe-idle-symbolic.svg",
                "broscribe-paused-symbolic.svg",
                "broscribe-recording-symbolic.svg"
            ]
        );
        assert_eq!(
            std::fs::read(dir.join("broscribe-idle-symbolic.svg")).unwrap(),
            symbolic(StatusIcon::Idle).1
        );
    }

    #[test]
    fn activation_sends_only_the_enabled_default_item() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let state = Arc::new(Mutex::new(State {
            title: "BroScribe · Recording · 3 speakers to review".into(),
            ..Default::default()
        }));
        let service = ItemService {
            state: state.clone(),
            clicks: tx,
            icon_theme_path: None,
        };
        assert_eq!(
            service.title(),
            "BroScribe · Recording · 3 speakers to review"
        );
        assert_eq!(
            service.tool_tip().2,
            "BroScribe · Recording · 3 speakers to review"
        );
        // Hosts call Activate on a primary click only when the item is not
        // menu-only; the context menu stays exported.
        assert!(!service.item_is_menu());
        assert_eq!(service.menu().as_str(), "/MenuBar");

        let mut open = MenuItemSpec::new("open-app", "Open BroScribe");
        open.default = true;
        let disabled = |id: &str, title: &str| {
            let mut item = MenuItemSpec::new(id, title);
            item.enabled = false;
            item
        };
        let states = [
            // Idle: record-start is the first enabled item.
            vec![
                disabled("recording-status", "Not recording"),
                MenuItemSpec::new("record-start", "Start recording"),
                disabled("record-stop", "Stop and save recording"),
                MenuItemSpec::new("open-panel", "Open recording panel"),
                open.clone(),
            ],
            // Recording: record-stop is the first enabled item.
            vec![
                disabled("recording-status", "Recording"),
                disabled("record-start", "Start recording"),
                MenuItemSpec::new("record-stop", "Stop and save recording"),
                MenuItemSpec::new("open-panel", "Open recording panel"),
                open.clone(),
            ],
            // Paused: record-stop is the first enabled item.
            vec![
                disabled("recording-status", "Paused"),
                disabled("record-start", "Start recording"),
                MenuItemSpec::new("record-stop", "Stop and save recording"),
                MenuItemSpec::new("open-panel", "Open recording panel"),
                open.clone(),
            ],
            // Agent down: agent-start is the first enabled item.
            vec![
                disabled("recording-status", "Recording agent not connected"),
                MenuItemSpec::new("agent-start", "Start recording agent"),
                disabled("record-start", "Start recording"),
                disabled("record-stop", "Stop and save recording"),
                MenuItemSpec::new("open-panel", "Open recording panel"),
                open.clone(),
            ],
            // Agent starting: open-panel is the first enabled item.
            vec![
                disabled("recording-status", "Recording agent not connected"),
                disabled("agent-start", "Starting recording agent…"),
                disabled("record-start", "Start recording"),
                disabled("record-stop", "Stop and save recording"),
                MenuItemSpec::new("open-panel", "Open recording panel"),
                open,
            ],
        ];
        for menu in states {
            state.lock().unwrap().menu = menu;
            service.activate(0, 0);
            assert_eq!(rx.try_recv().unwrap(), "open-app");
            service.secondary_activate(0, 0);
            assert_eq!(rx.try_recv().unwrap(), "open-app");
            assert!(rx.try_recv().is_err());
        }

        // Without an enabled default item, activation never falls back to the first item.
        state.lock().unwrap().menu = vec![
            MenuItemSpec::new("record-start", "Start recording"),
            MenuItemSpec::new("record-stop", "Stop and save recording"),
        ];
        service.activate(0, 0);
        service.secondary_activate(0, 0);
        assert!(rx.try_recv().is_err());

        let mut disabled_default = MenuItemSpec::new("open-app", "Open BroScribe");
        disabled_default.default = true;
        disabled_default.enabled = false;
        state.lock().unwrap().menu = vec![
            MenuItemSpec::new("record-start", "Start recording"),
            disabled_default,
        ];
        service.activate(0, 0);
        service.secondary_activate(0, 0);
        assert!(rx.try_recv().is_err());
    }
}
