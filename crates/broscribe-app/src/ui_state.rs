//! Where the windows were left (SHL-39, PNL-56): `<data_dir>/ui-state.json`.
//!
//! This is UI state, not settings: it never goes in `config.toml`, it is not
//! part of the library and nothing is ever rebuilt from it. A missing,
//! unreadable or damaged file means "nothing remembered" and never stops a
//! launch. It holds:
//!
//! - the main window's bounds (windowed, maximized or full screen, with the
//!   windowed frame to return to), the display they were on, whether the
//!   sidebar shows and the last section (Library, Review queue or People);
//! - the About and Settings windows' positions and the last Settings pane;
//! - the recording controls' last frame per display
//!   (`recording_controls.frames`, keyed by [`display_key`]) and the display
//!   they were last left on.
//!
//! A window opens where it was left only when the display it was on is still
//! present and its frame overlaps that display by at least
//! [`MIN_OVERLAP`]×[`MIN_OVERLAP`] px; otherwise it opens at its default
//! place. Keys a later BroScribe adds (one-time tips, other windows) are
//! kept as they were.
//!
//! The app keeps one copy in memory ([`UiStateStore`]), updated as windows
//! move, resize and change section, and writes it atomically, off the UI
//! thread, when a window closes and when the app quits; the recording
//! controls' frame also once they stop moving ([`PANEL_SETTLE`]). Writes
//! land in the order they were made: an older copy never replaces a newer
//! one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    App, Bounds, DisplayId, Global, Pixels, PlatformDisplay, Task, Window, WindowBounds, point, px,
    size,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::state::{Route, SettingsPage};

/// The file's name in the data folder.
pub const FILE_NAME: &str = "ui-state.json";
/// How much of a remembered frame must lie on its display, each way, for it
/// to be used.
pub const MIN_OVERLAP: f32 = 100.;
/// The recording controls' smallest frame (PNL-37's 360×440, which the
/// cross-surface review chose over SYS-30's 360×420).
pub const PANEL_MIN: (f32, f32) = (360., 440.);
/// How long the recording controls must stay where they are before their
/// frame is written.
pub const PANEL_SETTLE: Duration = Duration::from_secs(1);

/// A rectangle in GPUI's window coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Frame {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Whether `other` and this share at least `min`×`min`.
    pub fn overlaps_by(&self, other: &Frame, min: f32) -> bool {
        let width = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let height = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        width >= min && height >= min
    }

    /// Whether every field is a finite number and the size is positive.
    fn is_usable(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.width > 0.
            && self.height > 0.
    }

    pub fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(
            point(px(self.x), px(self.y)),
            size(px(self.width), px(self.height)),
        )
    }
}

impl From<Bounds<Pixels>> for Frame {
    fn from(bounds: Bounds<Pixels>) -> Self {
        Self::new(
            f32::from(bounds.origin.x),
            f32::from(bounds.origin.y),
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
        )
    }
}

/// How a window was shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    Windowed,
    Maximized,
    Fullscreen,
}

/// A window as it was left.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedWindow {
    pub mode: WindowMode,
    /// The windowed frame (the one to return to when maximized or full
    /// screen).
    pub frame: Frame,
    /// The display it was on ([`display_key`]); absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

impl SavedWindow {
    pub fn window_bounds(&self) -> WindowBounds {
        let bounds = self.frame.bounds();
        match self.mode {
            WindowMode::Windowed => WindowBounds::Windowed(bounds),
            WindowMode::Maximized => WindowBounds::Maximized(bounds),
            WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
        }
    }
}

/// The main window's sidebar sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Library,
    ReviewQueue,
    People,
}

impl Section {
    const ALL: [Section; 3] = [Section::Library, Section::ReviewQueue, Section::People];

    pub fn key(self) -> &'static str {
        match self {
            Section::Library => "library",
            Section::ReviewQueue => "review_queue",
            Section::People => "people",
        }
    }

    /// The section a saved key names; `None` for a key this version
    /// doesn't know.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.key() == key)
    }

    /// The section the sidebar highlights for `route`.
    pub fn of(route: &Route) -> Self {
        match route {
            Route::Library { .. } | Route::LibraryPerson { .. } => Self::Library,
            Route::ReviewQueue => Self::ReviewQueue,
            Route::People { .. } => Self::People,
        }
    }

    pub fn route(self) -> Route {
        match self {
            Section::Library => Route::library(),
            Section::ReviewQueue => Route::ReviewQueue,
            Section::People => Route::people(),
        }
    }
}

fn shown() -> bool {
    true
}

/// The main window as it was left.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MainWindowState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<SavedWindow>,
    /// The user's sidebar preference. A narrow window may collapse it.
    #[serde(default = "shown")]
    pub sidebar_visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_width: Option<f32>,
    /// The last section, by [`Section::key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
}

impl Default for MainWindowState {
    fn default() -> Self {
        Self {
            window: None,
            sidebar_visible: true,
            sidebar_width: None,
            library_width: None,
            section: None,
        }
    }
}

impl MainWindowState {
    pub fn section(&self) -> Option<Section> {
        self.section.as_deref().and_then(Section::from_key)
    }
}

/// The recording controls' last frame on each display.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecordingControlsState {
    #[serde(default)]
    pub frames: BTreeMap<String, Frame>,
    /// The display they were last left on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_display: Option<String>,
}

impl RecordingControlsState {
    fn is_empty(&self) -> bool {
        self.frames.is_empty() && self.last_display.is_none()
    }
}

/// Everything in `ui-state.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UiState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_window: Option<MainWindowState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about_window: Option<SavedWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_window: Option<SavedWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_page: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seen_notices: Vec<String>,
    #[serde(default, skip_serializing_if = "RecordingControlsState::is_empty")]
    pub recording_controls: RecordingControlsState,
    /// Keys this version doesn't know, kept as they were.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

impl UiState {
    /// The file at `path`; nothing remembered when it is missing,
    /// unreadable or damaged (logged, never fatal).
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, "ignoring the damaged window state file");
                Self::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(%error, "cannot read the window state file");
                Self::default()
            }
        }
    }

    /// Replaces the file at `path` atomically.
    pub fn save(&self, path: &Path) -> broscribe_core::Result<()> {
        broscribe_core::fsutil::write_json_atomic(path, self)
    }
}

/// A display as the restore rules see it.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayFrame {
    /// [`display_key`].
    pub key: String,
    pub bounds: Frame,
    /// The part outside the menu bar, Dock or taskbar.
    pub visible: Frame,
}

/// The display a remembered window opens on (an index into `displays`):
/// the one it was on, when it is still present and the frame overlaps it by
/// at least [`MIN_OVERLAP`] each way. A window whose display wasn't known
/// opens on the first display its frame overlaps that much.
pub fn restorable(saved: &SavedWindow, displays: &[DisplayFrame]) -> Option<usize> {
    if !saved.frame.is_usable() {
        return None;
    }
    let fits = |display: &DisplayFrame| saved.frame.overlaps_by(&display.bounds, MIN_OVERLAP);
    match &saved.display {
        Some(key) => displays
            .iter()
            .position(|display| &display.key == key)
            .filter(|&index| fits(&displays[index])),
        None => displays.iter().position(fits),
    }
}

/// [`restorable`] for a window that remembers only its position: the frame
/// at the saved origin with `size`.
pub fn restorable_position(
    saved: &SavedWindow,
    size: (f32, f32),
    displays: &[DisplayFrame],
) -> Option<(usize, Frame)> {
    let moved = SavedWindow {
        mode: WindowMode::Windowed,
        frame: Frame::new(saved.frame.x, saved.frame.y, size.0, size.1),
        display: saved.display.clone(),
    };
    restorable(&moved, displays).map(|index| (index, moved.frame))
}

/// The recording controls' remembered frame for the display they open on:
/// the display they were last left on, else any other present display
/// with a frame of its own, in the system's order. A frame is used only
/// when its display is present and it overlaps that display's visible area
/// by at least [`MIN_OVERLAP`] each way; it is then held to [`PANEL_MIN`]
/// and to half the display's visible width. `None` means today's placement.
pub fn panel_frame(
    state: &RecordingControlsState,
    displays: &[DisplayFrame],
) -> Option<(usize, Frame)> {
    let candidates = state
        .last_display
        .iter()
        .chain(displays.iter().map(|display| &display.key));
    for key in candidates {
        let Some(frame) = state.frames.get(key).filter(|frame| frame.is_usable()) else {
            continue;
        };
        let Some(index) = displays.iter().position(|display| &display.key == key) else {
            continue;
        };
        let visible = &displays[index].visible;
        if frame.overlaps_by(visible, MIN_OVERLAP) {
            return Some((index, clamp_panel(*frame, visible)));
        }
    }
    None
}

/// At least [`PANEL_MIN`], at most half the visible width and the visible
/// height (never below the minimum).
fn clamp_panel(frame: Frame, visible: &Frame) -> Frame {
    let width = frame.width.min(visible.width / 2.).max(PANEL_MIN.0);
    let height = frame.height.min(visible.height).max(PANEL_MIN.1);
    Frame::new(frame.x, frame.y, width, height)
}

// ---- GPUI ----

/// A display's stable key: its UUID where the system reports one, else its
/// bounds (X11 reports a nil UUID for every display; *verify* that Windows
/// and Wayland keep theirs across restarts).
pub fn display_key(display: &dyn PlatformDisplay) -> String {
    match display.uuid() {
        Ok(uuid) if !uuid.is_nil() => format!("uuid:{uuid}"),
        _ => {
            let bounds = Frame::from(display.bounds());
            format!(
                "bounds:{},{},{}x{}",
                bounds.x, bounds.y, bounds.width, bounds.height
            )
        }
    }
}

/// The present displays, the primary one first.
pub fn displays(cx: &App) -> Vec<(DisplayFrame, DisplayId)> {
    let primary = cx.primary_display().map(|display| display.id());
    let mut displays: Vec<(DisplayFrame, DisplayId)> = cx
        .displays()
        .iter()
        .map(|display| {
            (
                DisplayFrame {
                    key: display_key(display.as_ref()),
                    bounds: display.bounds().into(),
                    visible: display.visible_bounds().into(),
                },
                display.id(),
            )
        })
        .collect();
    displays.sort_by_key(|(_, id)| Some(*id) != primary);
    displays
}

/// The app's copy of the file (installed at launch with [`install`]).
pub struct UiStateStore {
    path: PathBuf,
    state: UiState,
    /// Changed since the last write was started.
    dirty: bool,
    /// The number of the last copy handed to a write.
    generation: u64,
    /// The number of the last copy written; writes hold it while writing.
    written: Arc<Mutex<u64>>,
    /// The recording controls' write, waiting for them to settle.
    panel_write: Option<Task<()>>,
    uninstalling: bool,
}

impl Global for UiStateStore {}

/// Loads `<data_dir>/ui-state.json` and writes it again whenever a window
/// closes (when something changed).
pub fn install(data_dir: &Path, cx: &mut App) {
    let path = data_dir.join(FILE_NAME);
    let state = UiState::load(&path);
    cx.set_global(UiStateStore {
        path,
        state,
        dirty: false,
        generation: 0,
        written: Arc::default(),
        panel_write: None,
        uninstalling: false,
    });
    cx.on_window_closed(|cx, _| save(cx).detach()).detach();
}

/// The remembered state, when the store is installed.
pub fn current(cx: &App) -> Option<&UiState> {
    cx.try_global::<UiStateStore>().map(|store| &store.state)
}

fn update(cx: &mut App, change: impl FnOnce(&mut UiState)) {
    if !cx.has_global::<UiStateStore>() {
        return;
    }
    let store = cx.global_mut::<UiStateStore>();
    let before = store.state.clone();
    change(&mut store.state);
    if store.state != before {
        store.dirty = true;
    }
}

/// Writes the remembered state off the UI thread when it changed since
/// the last write; the task ends when it is written.
pub fn save(cx: &mut App) -> Task<()> {
    if !cx.has_global::<UiStateStore>() {
        return Task::ready(());
    }
    let store = cx.global_mut::<UiStateStore>();
    if store.uninstalling || !store.dirty {
        return Task::ready(());
    }
    store.dirty = false;
    store.generation += 1;
    let generation = store.generation;
    let state = store.state.clone();
    let path = store.path.clone();
    let written = store.written.clone();
    cx.background_executor()
        .spawn(async move { write_in_order(&path, &state, generation, &written) })
}

/// Wait for any geometry write already underway, then suppress later writes
/// so quitting from Settings cannot recreate a deleted data folder.
pub fn suspend_for_uninstall(cx: &mut App) -> Task<()> {
    if !cx.has_global::<UiStateStore>() {
        return Task::ready(());
    }
    let store = cx.global_mut::<UiStateStore>();
    store.uninstalling = true;
    store.panel_write.take();
    let generation = store.generation;
    let written = store.written.clone();
    cx.background_executor().spawn(async move {
        advance_write_floor(&written, generation);
    })
}

pub(crate) fn resume_after_failed_uninstall(cx: &mut App) {
    if let Some(store) = cx.try_global::<UiStateStore>()
        && store.uninstalling
    {
        let store = cx.global_mut::<UiStateStore>();
        store.uninstalling = false;
        store.dirty = true;
    }
}

fn advance_write_floor(written: &Mutex<u64>, generation: u64) {
    let mut last = written.lock().unwrap_or_else(|error| error.into_inner());
    *last = (*last).max(generation);
}

/// Writes copy number `generation` unless an equal or newer copy was written or retired.
fn write_in_order(path: &Path, state: &UiState, generation: u64, written: &Mutex<u64>) {
    let mut last = written.lock().unwrap_or_else(|e| e.into_inner());
    if generation <= *last {
        return;
    }
    match state.save(path) {
        Ok(()) => *last = generation,
        Err(error) => tracing::warn!(%error, "cannot save the window state"),
    }
}

fn saved_window(window: &Window, cx: &App) -> SavedWindow {
    let (mode, bounds) = match window.window_bounds() {
        WindowBounds::Windowed(bounds) => (WindowMode::Windowed, bounds),
        WindowBounds::Maximized(bounds) => (WindowMode::Maximized, bounds),
        WindowBounds::Fullscreen(bounds) => (WindowMode::Fullscreen, bounds),
    };
    SavedWindow {
        mode,
        frame: bounds.into(),
        display: window
            .display(cx)
            .map(|display| display_key(display.as_ref())),
    }
}

/// Where the main window reopens: where it was left, when that is still on
/// a present display.
pub fn main_window_bounds(cx: &App) -> Option<(WindowBounds, DisplayId)> {
    let saved = current(cx)?.main_window.as_ref()?.window.as_ref()?;
    let displays = displays(cx);
    let frames: Vec<DisplayFrame> = displays.iter().map(|(frame, _)| frame.clone()).collect();
    let index = restorable(saved, &frames)?;
    Some((saved.window_bounds(), displays[index].1))
}

/// The main window's section when the app was last left, if any.
pub fn main_window_section(cx: &App) -> Option<Section> {
    current(cx)?.main_window.as_ref()?.section()
}

/// Records the main window's bounds as they are now.
pub fn remember_main_window(window: &Window, cx: &mut App) {
    let saved = saved_window(window, cx);
    update(cx, |state| {
        state
            .main_window
            .get_or_insert_with(Default::default)
            .window = Some(saved);
    });
}

/// Records the section the main window shows.
pub fn remember_section(route: &Route, cx: &mut App) {
    let section = Section::of(route).key().to_owned();
    update(cx, |state| {
        let main = state.main_window.get_or_insert_with(Default::default);
        main.section = Some(section);
    });
}

pub fn main_panes(cx: &App) -> (bool, Option<f32>, Option<f32>) {
    current(cx)
        .and_then(|state| state.main_window.as_ref())
        .map_or((true, None, None), |main| {
            (main.sidebar_visible, main.sidebar_width, main.library_width)
        })
}

pub fn remember_sidebar(visible: bool, cx: &mut App) {
    update(cx, |state| {
        state
            .main_window
            .get_or_insert_with(Default::default)
            .sidebar_visible = visible
    });
}

pub fn remember_sidebar_width(width: f32, cx: &mut App) {
    update(cx, |state| {
        state
            .main_window
            .get_or_insert_with(Default::default)
            .sidebar_width = Some(width)
    });
}

pub fn remember_library_width(width: f32, cx: &mut App) {
    update(cx, |state| {
        state
            .main_window
            .get_or_insert_with(Default::default)
            .library_width = Some(width)
    });
}

pub fn settings_page(cx: &App) -> Option<SettingsPage> {
    let page = current(cx)?.settings_page.as_ref()?;
    serde_json::from_value(Value::String(page.clone())).ok()
}

pub fn remember_settings_page(page: SettingsPage, cx: &mut App) {
    let name = serde_json::to_value(page)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned));
    update(cx, |state| state.settings_page = name);
}

/// Records a once-only notice outside the application preferences.
pub fn claim_notice(id: &str, cx: &mut App) -> bool {
    let mut claimed = false;
    update(cx, |state| {
        if !state.seen_notices.iter().any(|seen| seen == id) {
            state.seen_notices.push(id.to_owned());
            claimed = true;
        }
    });
    if claimed {
        save(cx).detach();
    }
    claimed
}

/// Where About reopens: its default `size` at the position it was left at,
/// when that is still on a present display.
pub fn about_window_bounds(
    about_size: (f32, f32),
    cx: &App,
) -> Option<(Bounds<Pixels>, DisplayId)> {
    auxiliary_window_bounds(current(cx)?.about_window.as_ref()?, about_size, cx)
}

pub fn settings_window_bounds(size: (f32, f32), cx: &App) -> Option<(Bounds<Pixels>, DisplayId)> {
    auxiliary_window_bounds(current(cx)?.settings_window.as_ref()?, size, cx)
}

fn auxiliary_window_bounds(
    saved: &SavedWindow,
    size: (f32, f32),
    cx: &App,
) -> Option<(Bounds<Pixels>, DisplayId)> {
    let displays = displays(cx);
    let frames: Vec<DisplayFrame> = displays.iter().map(|(frame, _)| frame.clone()).collect();
    let (index, frame) = restorable_position(saved, size, &frames)?;
    Some((frame.bounds(), displays[index].1))
}

/// Records About's position as it is now.
pub fn remember_about_window(window: &Window, cx: &mut App) {
    let saved = SavedWindow {
        mode: WindowMode::Windowed,
        ..saved_window(window, cx)
    };
    update(cx, |state| state.about_window = Some(saved));
}

pub fn remember_settings_window(window: &Window, cx: &mut App) {
    let saved = saved_window(window, cx);
    update(cx, |state| state.settings_window = Some(saved));
}

/// Where the recording controls open: their remembered frame for the
/// display they open on ([`panel_frame`]); `None` for today's placement.
pub fn panel_bounds(cx: &App) -> Option<(Bounds<Pixels>, DisplayId)> {
    let state = &current(cx)?.recording_controls;
    let displays = displays(cx);
    let frames: Vec<DisplayFrame> = displays.iter().map(|(frame, _)| frame.clone()).collect();
    let (index, frame) = panel_frame(state, &frames)?;
    Some((frame.bounds(), displays[index].1))
}

/// Records the recording controls' frame on the display they are on, and
/// writes it once they stay there for [`PANEL_SETTLE`].
pub fn remember_panel(window: &Window, cx: &mut App) {
    let Some(display) = window.display(cx) else {
        return;
    };
    let key = display_key(display.as_ref());
    let frame = Frame::from(window.bounds());
    if !cx.has_global::<UiStateStore>() {
        return;
    }
    update(cx, |state| {
        let controls = &mut state.recording_controls;
        controls.frames.insert(key.clone(), frame);
        controls.last_display = Some(key);
    });
    let timer = cx.background_executor().timer(PANEL_SETTLE);
    // A later move replaces (cancels) this wait; the write itself, once
    // started, is never cancelled.
    let task = cx.spawn(async move |cx| {
        timer.await;
        cx.update(|cx| save(cx).detach());
    });
    cx.global_mut::<UiStateStore>().panel_write = Some(task);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(key: &str, x: f32, width: f32) -> DisplayFrame {
        DisplayFrame {
            key: key.into(),
            bounds: Frame::new(x, 0., width, 1000.),
            // A 25 px menu bar and a 75 px Dock.
            visible: Frame::new(x, 25., width, 900.),
        }
    }

    fn windowed(x: f32, y: f32, display: Option<&str>) -> SavedWindow {
        SavedWindow {
            mode: WindowMode::Windowed,
            frame: Frame::new(x, y, 1120., 720.),
            display: display.map(str::to_owned),
        }
    }

    #[test]
    fn the_file_round_trips_bounds_maximized_sidebar_section_and_panel_frames() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut state = UiState {
            main_window: Some(MainWindowState {
                window: Some(SavedWindow {
                    mode: WindowMode::Maximized,
                    frame: Frame::new(40., 60., 900., 640.),
                    display: Some("uuid:synthetic-1".into()),
                }),
                sidebar_visible: false,
                sidebar_width: Some(235.),
                library_width: Some(340.),
                section: Some(Section::People.key().into()),
            }),
            about_window: Some(windowed(10., 20., None)),
            settings_window: Some(windowed(200., 180., Some("uuid:synthetic-1"))),
            settings_page: Some("recording".into()),
            seen_notices: vec!["tray-tip".into()],
            recording_controls: RecordingControlsState::default(),
            other: Map::new(),
        };
        state.recording_controls.frames.insert(
            "uuid:synthetic-1".into(),
            Frame::new(1000., 100., 420., 680.),
        );
        state.recording_controls.last_display = Some("uuid:synthetic-1".into());
        state.save(&path).unwrap();
        let loaded = UiState::load(&path);
        assert_eq!(loaded, state);
        let main = loaded.main_window.unwrap();
        assert_eq!(main.section(), Some(Section::People));
        assert!(!main.sidebar_visible);
        assert!(matches!(
            main.window.unwrap().window_bounds(),
            WindowBounds::Maximized(_)
        ));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"recording_controls\""));
        assert!(text.contains("\"frames\""));
    }

    #[test]
    fn writes_are_atomic_and_an_older_copy_never_replaces_a_newer_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let written = Mutex::new(0);
        let older = UiState {
            about_window: Some(windowed(1., 1., None)),
            ..UiState::default()
        };
        let newer = UiState {
            about_window: Some(windowed(2., 2., None)),
            ..UiState::default()
        };
        write_in_order(&path, &newer, 2, &written);
        write_in_order(&path, &older, 1, &written);
        assert_eq!(UiState::load(&path), newer);
        // Nothing but the file itself is left behind.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [FILE_NAME]);
        let cleanup =
            broscribe_platform::profile_cleanup::clean(dir.path(), "data", false, &[], false)
                .unwrap();
        assert!(
            cleanup.kept.is_empty(),
            "the UI state writer must stay on the uninstall allowlist"
        );
    }

    #[test]
    fn a_failed_uninstall_can_admit_new_writes_without_replaying_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let written = Mutex::new(0);
        let state = UiState::default();
        advance_write_floor(&written, 2);
        write_in_order(&path, &state, 2, &written);
        assert!(!path.exists());
        write_in_order(&path, &state, 3, &written);
        assert!(path.exists());
        assert_eq!(*written.lock().unwrap(), 3);
    }

    #[test]
    fn a_missing_or_damaged_file_remembers_nothing_and_unknown_keys_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(UiState::load(&path), UiState::default());
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(UiState::load(&path), UiState::default());
        std::fs::write(
            &path,
            br#"{"main_window":{"section":"timeline"},"tips":{"tray_shown":true}}"#,
        )
        .unwrap();
        let loaded = UiState::load(&path);
        let main = loaded.main_window.clone().unwrap();
        assert_eq!(main.section(), None, "a section this version doesn't know");
        assert!(main.sidebar_visible, "shown unless saved hidden");
        loaded.save(&path).unwrap();
        let again: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(again["tips"]["tray_shown"], Value::Bool(true));
        assert_eq!(again["main_window"]["section"], "timeline");
    }

    #[test]
    fn sections_follow_the_sidebar() {
        assert_eq!(Section::of(&Route::library()), Section::Library);
        assert_eq!(
            Section::of(&Route::meeting(broscribe_core::MeetingId::random())),
            Section::Library
        );
        assert_eq!(Section::of(&Route::ReviewQueue), Section::ReviewQueue);
        assert_eq!(Section::of(&Route::people()), Section::People);
        for section in Section::ALL {
            assert_eq!(Section::from_key(section.key()), Some(section));
            assert_eq!(Section::of(&section.route()), section);
        }
    }

    #[test]
    fn a_window_reopens_only_on_its_display_and_only_when_it_overlaps_enough() {
        let displays = [
            display("uuid:a", 0., 1440.),
            display("uuid:b", 1440., 1920.),
        ];
        assert_eq!(
            restorable(&windowed(100., 100., Some("uuid:a")), &displays),
            Some(0)
        );
        assert_eq!(
            restorable(&windowed(1600., 100., Some("uuid:b")), &displays),
            Some(1)
        );
        // Its display is gone.
        assert_eq!(
            restorable(&windowed(100., 100., Some("uuid:gone")), &displays),
            None
        );
        // Only 99 px of it lies on its display, either way.
        assert_eq!(
            restorable(&windowed(1341., 100., Some("uuid:a")), &displays),
            None
        );
        assert_eq!(
            restorable(&windowed(100., 901., Some("uuid:a")), &displays),
            None
        );
        // Exactly 100×100 is enough.
        assert_eq!(
            restorable(&windowed(1340., 900., Some("uuid:a")), &displays),
            Some(0)
        );
        // Off every display.
        assert_eq!(
            restorable(&windowed(-5000., 100., Some("uuid:a")), &displays),
            None
        );
        // Its display wasn't known: the first one it overlaps enough.
        assert_eq!(restorable(&windowed(1600., 100., None), &displays), Some(1));
        assert_eq!(restorable(&windowed(9000., 100., None), &displays), None);
        // Nonsense never opens a window.
        let mut broken = windowed(100., 100., Some("uuid:a"));
        broken.frame.width = f32::NAN;
        assert_eq!(restorable(&broken, &displays), None);
        broken.frame.width = 0.;
        assert_eq!(restorable(&broken, &displays), None);
    }

    #[test]
    fn about_keeps_its_size_and_returns_to_its_position() {
        let displays = [display("uuid:a", 0., 1440.)];
        let saved = SavedWindow {
            mode: WindowMode::Windowed,
            frame: Frame::new(300., 200., 600., 900.),
            display: Some("uuid:a".into()),
        };
        assert_eq!(
            restorable_position(&saved, (520., 560.), &displays),
            Some((0, Frame::new(300., 200., 520., 560.)))
        );
        let gone = SavedWindow {
            display: Some("uuid:b".into()),
            ..saved
        };
        assert_eq!(restorable_position(&gone, (520., 560.), &displays), None);
    }

    #[test]
    fn the_panel_returns_per_display_and_only_where_it_still_fits() {
        let laptop = display("uuid:laptop", 0., 1440.);
        let external = display("uuid:external", 1440., 2560.);
        let mut state = RecordingControlsState::default();
        state
            .frames
            .insert("uuid:laptop".into(), Frame::new(1000., 100., 420., 680.));
        state
            .frames
            .insert("uuid:external".into(), Frame::new(3500., 50., 480., 800.));
        state.last_display = Some("uuid:external".into());

        // Both present: the display it was last left on.
        assert_eq!(
            panel_frame(&state, &[laptop.clone(), external.clone()]),
            Some((1, Frame::new(3500., 50., 480., 800.)))
        );
        // That display is gone: the laptop's own frame.
        assert_eq!(
            panel_frame(&state, std::slice::from_ref(&laptop)),
            Some((0, Frame::new(1000., 100., 420., 680.)))
        );
        // A frame for a display that is gone is never used on another one.
        let only_external = RecordingControlsState {
            frames: BTreeMap::from([("uuid:external".into(), Frame::new(3500., 50., 480., 800.))]),
            last_display: Some("uuid:external".into()),
        };
        assert_eq!(
            panel_frame(&only_external, std::slice::from_ref(&laptop)),
            None
        );
        // Less than 100×100 of it on the visible area: today's placement.
        let mut edge = RecordingControlsState::default();
        edge.frames
            .insert("uuid:laptop".into(), Frame::new(1341., 100., 420., 680.));
        assert_eq!(panel_frame(&edge, std::slice::from_ref(&laptop)), None);
        edge.frames
            .insert("uuid:laptop".into(), Frame::new(100., -656., 420., 680.));
        assert_eq!(
            panel_frame(&edge, std::slice::from_ref(&laptop)),
            None,
            "under the menu bar"
        );
        // Held to the minimum and to half the visible width.
        let mut odd = RecordingControlsState::default();
        odd.frames
            .insert("uuid:laptop".into(), Frame::new(100., 100., 200., 300.));
        assert_eq!(
            panel_frame(&odd, std::slice::from_ref(&laptop)),
            Some((0, Frame::new(100., 100., 360., 440.)))
        );
        odd.frames
            .insert("uuid:laptop".into(), Frame::new(100., 100., 1200., 2000.));
        assert_eq!(
            panel_frame(&odd, std::slice::from_ref(&laptop)),
            Some((0, Frame::new(100., 100., 720., 900.)))
        );
        // Nothing remembered.
        assert_eq!(
            panel_frame(&RecordingControlsState::default(), &[laptop, external]),
            None
        );
    }
}
