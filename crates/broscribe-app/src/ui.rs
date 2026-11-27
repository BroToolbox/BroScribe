//! The main window shell: a sidebar (Library, Review queue (N), People,
//! and a Settings button), the main area for the current [`Route`], a job-progress footer
//! and a drop target for files anywhere on the window; and the window a
//! launch that cannot start shows instead ([`open_startup_error_window`]).
//!
//! View lifetimes: the library view lives as long as the window; People,
//! and Review queue are created on first visit
//! and kept; the meeting view and the import sheet are created for their
//! route and dropped when it changes. On first run the setup assistant
//! opens in a separate window. Each view's
//! root is the window's one `Role::Main` region.
//!
//! The shell repaints only when what it shows itself (route, review count,
//! footer lines) changes; views follow the state on their own.
//!
//! Settings changed by another process (`broscribe config …`) are read again
//! off the UI thread whenever the window becomes active
//! ([`reload_config`]); the state keeps only a newer revision. The saved
//! reading size scales gpui-component's theme text sizes
//! ([`follow_reading_size`]): every rem-based size in every window follows,
//! the theme's colours and mode stay as they are.
//!
//! The BroScribe theme itself (light and dark, following the system
//! appearance) is installed by [`theme::init`] before the first window.

pub mod assets;
pub mod brand;
pub mod chrome;
pub mod fonts;
pub mod format;
pub(crate) mod icons;
pub(crate) mod keymap;
pub mod kit;
pub(crate) mod once;
// Written by `docs/redesign/tools/palette.py --theme-json`; rustfmt would
// make it differ from what `--check-generated` expects.
#[rustfmt::skip]
mod palette_gen;
mod record_control;
pub(crate) mod rich_text;
pub(crate) mod select;
pub mod settings_window;
pub(crate) mod setup_window;
pub(crate) mod terms;
pub mod type_scale;
use type_scale::{TextRole, TextRoleExt as _, tabular};
pub mod theme;
#[cfg(test)]
mod transient_editors;
#[cfg(test)]
mod u56_tests;
pub mod windows;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use broscribe_core::MeetingId;
use broscribe_core::config::{Config, ReadingSize};
use broscribe_core::learning::REIDENTIFY_KIND;
use broscribe_core::queue::kind_label;
use broscribe_platform::{AnnouncementPriority, Announcer, HostPlatform, StatusIconPlace};
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::menu::PopupMenu;
use gpui::component::popover::Popover;
use gpui::component::resizable::{h_resizable, resizable_panel};
use gpui::component::{
    ActiveTheme as _, Disableable as _, Root, Sizable as _, Theme, WindowExt as _,
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Bounds, ClipboardItem, Context, Div, Element, ElementId,
    Entity, EntityId, ExternalPaths, Focusable as _, Global, GlobalElementId, InspectorElementId,
    InteractiveElement as _, IntoElement, KeyDownEvent, LayoutId, ParentElement, Pixels, Position,
    Render, Role, SharedString, Stateful, StatefulInteractiveElement, Style, Styled as _,
    Subscription, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowId, WindowOptions,
    div, px, size,
};

use crate::jobs::{FailedJob, WorkerState, core_user_text, failure_text, stored_error};
use crate::launch::StartupFailure;
use crate::state::{AppState, Route};
use crate::views::import_sheet::{
    ImportRuns, ImportSheet, accepts_drop, dropped_paths, file_label,
};
use crate::views::library::{LibraryEvent, LibraryView};
use crate::views::meeting::MeetingView;
use crate::views::people::PeopleView;
use crate::views::review::ReviewView;
use crate::views::setup::{self, SetupDeps};

const SIDEBAR_WIDTH: f32 = 200.;
/// The Library list pane: 300 px of list plus the desk around its card
/// (24 px leading, 8 px trailing).
const LIST_WIDTH: f32 = 332.;
const LIST_MIN_WIDTH: f32 = 292.;
const LIST_MAX_WIDTH: f32 = 472.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneLayout {
    Single,
    Split,
    Wide,
}

impl PaneLayout {
    pub(crate) fn for_window(window: &Window) -> Self {
        Self::at_width(window.viewport_size().width / window.rem_size())
    }

    fn at_width(rems: f32) -> Self {
        if rems < 47.5 {
            Self::Single
        } else if rems < 62.5 {
            Self::Split
        } else {
            Self::Wide
        }
    }
}

/// One row in the background-processing footer: a live status/alert label
/// (the job's name and state) and optional non-live trailing counts and
/// detail (the percentage) that never trigger live announcements (ACC-18).
#[derive(Clone, Debug, PartialEq, Eq)]
struct JobLine {
    id: SharedString,
    text: String,
    /// A running job's progress text when it counts something (the speaker
    /// recheck's committed meetings), shown after the name.
    counts: Option<String>,
    percent: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WorkerNotice {
    Restarting { attempt: u32 },
    Unavailable { crashes: Option<u32> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct JobFailureLine {
    job_id: String,
    meeting: Option<MeetingId>,
    text: String,
    needs_model: bool,
}

/// What the shell itself shows from the app state.
#[derive(Clone, Debug, PartialEq)]
struct ShellSnapshot {
    route: Route,
    review_count: Option<usize>,
    review_count_error: Option<String>,
    running_jobs: Vec<JobLine>,
    worker_notice: Option<WorkerNotice>,
    job_failure: Option<JobFailureLine>,
    setup_incomplete: bool,
    setup_progress: Option<usize>,
}

impl ShellSnapshot {
    fn of(state: &AppState) -> Self {
        Self {
            route: state.route().clone(),
            review_count: state.review_count(),
            review_count_error: state.review_count_error().map(str::to_owned),
            running_jobs: running_jobs(state),
            worker_notice: match state.jobs().worker {
                WorkerState::Restarting { crashes } => Some(WorkerNotice::Restarting {
                    attempt: crashes + 1,
                }),
                WorkerState::Unavailable { crashes, .. } => {
                    Some(WorkerNotice::Unavailable { crashes })
                }
                _ => None,
            },
            job_failure: state.shown_job_failure().map(|failed| JobFailureLine {
                job_id: failed.job_id.clone(),
                meeting: failed
                    .meeting_id
                    .clone()
                    .filter(|id| state.job_meeting_title(id).is_some()),
                text: failed_job_text(state, failed),
                needs_model: matches!(
                    failed.message.as_deref().and_then(stored_error),
                    Some(broscribe_core::Error::ModelMissing(_))
                ),
            }),
            setup_progress: state
                .config()
                .setup
                .defer_assistant
                .then(|| setup::unfinished_essentials(state.config()))
                .flatten(),
            setup_incomplete: !state
                .config()
                .setup
                .completed_steps
                .iter()
                .any(|step| step == setup::ASSISTANT_DONE),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolbarChrome {
    Unified,
    Native,
    Client,
}

impl ToolbarChrome {
    fn for_window(host: HostPlatform, client_decorated: bool) -> Self {
        if client_decorated {
            Self::Client
        } else if host == HostPlatform::MacOs {
            Self::Unified
        } else {
            Self::Native
        }
    }
}

struct OpenMainMenu {
    view: Entity<PopupMenu>,
    return_focus: gpui::FocusHandle,
    _dismiss: Subscription,
}

pub struct MainWindow {
    state: Entity<AppState>,
    deps: SetupDeps,
    library: Entity<LibraryView>,
    meeting: Option<(MeetingId, Entity<MeetingView>)>,
    meeting_title: Option<Subscription>,
    imports: Entity<ImportRuns>,
    import: Option<gpui::WeakEntity<ImportSheet>>,
    import_drop: Option<bool>,
    import_drag_paths: Vec<PathBuf>,
    people: Option<Entity<PeopleView>>,
    review: Option<Entity<ReviewView>>,
    last_route: Option<Route>,
    back: VecDeque<Route>,
    forward: VecDeque<Route>,
    history_target: Option<Route>,
    list_selection: bool,
    keep_sidebar_focus: bool,
    sidebar_visible: bool,
    sidebar_open_narrow: bool,
    sidebar_cursor: usize,
    sidebar_rows: [gpui::FocusHandle; 3],
    sidebar_toggle_focus: gpui::FocusHandle,
    pane_layout: PaneLayout,
    toolbar_focus: gpui::FocusHandle,
    recording_toolbar_focus: gpui::FocusHandle,
    recording_control: Entity<record_control::RecordControl>,
    host: HostPlatform,
    main_menu_focus: gpui::FocusHandle,
    main_menu: Option<OpenMainMenu>,
    search_expanded: bool,
    sidebar_focus: gpui::FocusHandle,
    content_focus: gpui::FocusHandle,
    status_focus: gpui::FocusHandle,
    last_area_focus: [Option<gpui::WeakFocusHandle>; 5],
    tray_notice: Option<(&'static str, &'static str)>,
    tray_place: StatusIconPlace,
    /// What was last rendered from the state.
    shown: ShellSnapshot,
    _subscriptions: Vec<Subscription>,
}

impl MainWindow {
    pub fn new(
        state: Entity<AppState>,
        deps: SetupDeps,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        follow_reading_size(&state, cx);
        let library = cx.new(|cx| {
            let mut library = LibraryView::new(state.clone(), window, cx);
            library.embed();
            library
        });
        let imports = ImportRuns::global(cx);
        let updates = crate::views::update::UpdateView::shared(&state, cx);
        let update_banner = std::cell::RefCell::new(updates.read(cx).banner_state(cx));
        let downloads = state.read(cx).model_downloads().clone();
        let subscriptions = vec![
            cx.observe(&downloads, |_, _, cx| cx.notify()),
            cx.observe(&updates, move |_, updates, cx| {
                let next = updates.read(cx).banner_state(cx);
                if next != *update_banner.borrow() {
                    update_banner.replace(next);
                    cx.notify();
                }
            }),
            cx.observe_in(&imports, window, |this, _, window, cx| {
                this.sync_import_dialog(window, cx);
                cx.notify();
            }),
            // Follow route changes; repaint only when what the shell shows
            // changed (progress that changes no line repaints nothing).
            cx.observe_in(&state, window, |this, state, window, cx| {
                this.sync_route(window, cx);
                let shown = ShellSnapshot::of(state.read(cx));
                if shown != this.shown {
                    if this.tray_place == StatusIconPlace::NotificationArea
                        && !shown.setup_incomplete
                        && this.shown.setup_incomplete
                    {
                        this.tray_notice = tray_notice(
                            this.tray_place,
                            true,
                            &state.read(cx).paths().data_dir,
                            cx,
                        );
                    }
                    if shown.route != this.shown.route {
                        crate::ui_state::remember_section(&shown.route, cx);
                    }
                    this.shown = shown;
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &state,
                window,
                |this, _, event: &crate::state::AppEvent, window, cx| {
                    if let crate::state::AppEvent::MeetingDeleted(id) = event
                        && this.state.read(cx).route().selected_meeting() == Some(id)
                    {
                        let next = this.library.update(cx, |library, cx| {
                            library.select_after_deleting(id, window, cx)
                        });
                        this.list_selection = true;
                        let route = if this.pane_layout == PaneLayout::Single {
                            Route::library()
                        } else {
                            next.map_or_else(Route::library, Route::meeting)
                        };
                        this.go(route, cx);
                        this.sync_route(window, cx);
                        this.library
                            .update(cx, |library, cx| library.focus_list(window, cx));
                    }
                    if matches!(event, crate::state::AppEvent::MeetingDeleted(_)) {
                        this.library.update(cx, |_, cx| cx.notify());
                    }
                },
            ),
            // Where it was left, for the next time it opens (SHL-39); kept
            // in memory and written when a window closes or the app quits.
            cx.observe_window_bounds(window, |this, window, cx| {
                crate::ui_state::remember_main_window(window, cx);
                this.sync_pane_layout(window, cx);
            }),
            cx.subscribe_in(
                &library,
                window,
                |this, _, event: &LibraryEvent, window, cx| {
                    let LibraryEvent::SelectMeeting {
                        id,
                        enter,
                        preview,
                        segment,
                        action,
                    } = event;
                    let single = this.pane_layout == PaneLayout::Single;
                    if !single || !preview {
                        let route = Route::meeting(id.clone());
                        if this.state.read(cx).route() != &route {
                            this.list_selection = true;
                            this.go(route, cx);
                            this.sync_route(window, cx);
                        }
                        if let Some((shown, meeting)) = &this.meeting
                            && shown == id
                        {
                            if let Some(action) = action {
                                meeting.update(cx, |view, cx| {
                                    view.open_library_command(*action, window, cx)
                                });
                            } else if let Some(segment) = segment {
                                let snippet = this
                                    .state
                                    .read(cx)
                                    .search()
                                    .transcript_match(id)
                                    .map(|hit| hit.snippet.clone())
                                    .unwrap_or_default();
                                meeting.update(cx, |view, cx| {
                                    view.open_library_match(
                                        *segment,
                                        &snippet,
                                        *enter || single,
                                        window,
                                        cx,
                                    )
                                });
                            }
                        }
                        if action.is_none() && segment.is_none() && (*enter || single) {
                            this.focus_detail_next_frame(window, cx);
                        }
                    }
                },
            ),
            // Files and settings may have changed while another app was in
            // front.
            cx.observe_window_activation(window, |this, window, cx| {
                crate::menu::window_activation_changed(cx);
                if window.is_window_active() {
                    let state = this.state.clone();
                    if let Some(daemon) = state.read(cx).daemon() {
                        daemon.read(cx).refresh_capture_access(cx);
                    }
                    if let Some(agent) = state.read(cx).agent_starter() {
                        agent.update(cx, |agent, cx| agent.refresh_service(cx));
                    }
                    state.update(cx, |state, cx| state.window_activated(cx));
                    crate::views::update::UpdateView::shared(&state, cx)
                        .update(cx, |updates, cx| updates.refresh(cx));
                    reload_config(&state, cx);
                }
            }),
        ];
        let shown = ShellSnapshot::of(state.read(cx));
        let tray_place = status_icon_place(cx);
        let tray_notice = tray_notice(
            tray_place,
            !shown.setup_incomplete,
            &state.read(cx).paths().data_dir,
            cx,
        );
        let (sidebar_visible, _, _) = crate::ui_state::main_panes(cx);
        let mut this = Self {
            state: state.clone(),
            deps,
            library,
            meeting: None,
            meeting_title: None,
            imports,
            import: None,
            import_drop: None,
            import_drag_paths: Vec::new(),
            people: None,
            review: None,
            last_route: None,
            back: VecDeque::new(),
            forward: VecDeque::new(),
            history_target: None,
            list_selection: false,
            keep_sidebar_focus: false,
            sidebar_visible,
            sidebar_open_narrow: false,
            sidebar_cursor: 0,
            sidebar_rows: std::array::from_fn(|_| cx.focus_handle()),
            sidebar_toggle_focus: cx.focus_handle(),
            pane_layout: PaneLayout::for_window(window),
            toolbar_focus: cx.focus_handle(),
            recording_toolbar_focus: cx.focus_handle(),
            recording_control: cx.new(|cx| record_control::RecordControl::new(state.clone(), cx)),
            host: broscribe_platform_native::NativePlatform::host_platform(),
            main_menu_focus: cx.focus_handle(),
            main_menu: None,
            search_expanded: false,
            sidebar_focus: cx.focus_handle(),
            content_focus: cx.focus_handle(),
            status_focus: cx.focus_handle(),
            last_area_focus: std::array::from_fn(|_| None),
            tray_notice,
            tray_place,
            shown,
            _subscriptions: subscriptions,
        };
        this.sync_route(window, cx);
        this
    }

    /// The mounted meeting view, if any.
    #[cfg(test)]
    pub(crate) fn meeting_view(&self) -> Option<&Entity<MeetingView>> {
        self.meeting.as_ref().map(|(_, view)| view)
    }

    /// The embedded Library list.
    #[cfg(test)]
    pub(crate) fn library_view(&self) -> &Entity<LibraryView> {
        &self.library
    }

    /// The review queue's view, once it was opened.
    #[cfg(test)]
    pub(crate) fn review_view(&self) -> Option<&Entity<ReviewView>> {
        self.review.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn open_import_for_test(&self) -> Option<Entity<ImportSheet>> {
        self.import.as_ref().and_then(|view| view.upgrade())
    }

    fn sync_import_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.imports.read(cx).has_dialog_request() || window.root::<Root>().flatten().is_none()
        {
            return;
        }
        let paths = self
            .imports
            .update(cx, |runs, _| runs.take_dialog_request());
        if let Some(form) = self.import.as_ref().and_then(|form| form.upgrade())
            && window.has_active_dialog(cx)
        {
            form.update(cx, |form, cx| form.append_paths(paths, window, cx));
        } else if !window.has_active_dialog(cx) {
            let state = self.state.clone();
            let form = cx.new(|cx| ImportSheet::new(state, paths, window, cx));
            let first = std::cell::Cell::new(true);
            self.import = Some(form.downgrade());
            window.open_dialog(cx, move |dialog, window, cx| {
                // BroScribe's dialogs stand on the popover ground, not the desk.
                let dialog = dialog.bg(cx.theme().popover);
                if first.replace(false) {
                    let focus = form.read(cx).initial_focus(cx);
                    focus.focus(window, cx);
                }
                let title = form.read(cx).heading();
                dialog
                    .width(px(560.))
                    .entrance(gpui::component::dialog::DialogEntrance::None)
                    .overlay_closable(false)
                    .close_button(false)
                    .title(crate::ui::title("import-dialog-title", title))
                    .child(
                        div()
                            .h(window.viewport_size().height * 0.7)
                            .child(form.clone()),
                    )
            });
        }
    }

    fn update_import_drag(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        if self.import_drag_paths != paths || self.import_drop.is_none() {
            self.import_drag_paths = paths.to_vec();
            self.set_import_drop(Some(accepts_drop(paths)), cx);
        }
    }

    fn set_import_drop(&mut self, target: Option<bool>, cx: &mut Context<Self>) {
        if target.is_none() {
            self.import_drag_paths.clear();
        }
        if self.import_drop != target {
            self.import_drop = target;
            cx.notify();
        }
    }

    /// Creates the view for the current route and drops route-scoped views.
    fn sync_route(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let route = self.state.read(cx).route().clone();
        if self.last_route.as_ref() == Some(&route) {
            return;
        }
        let previous = self.last_route.clone();
        let traversing = self.history_target.take().as_ref() == Some(&route);
        let selecting = std::mem::take(&mut self.list_selection);
        let keep_sidebar_focus = std::mem::take(&mut self.keep_sidebar_focus);
        if !traversing && let Some(previous) = &previous {
            let replace = selecting
                && self.pane_layout != PaneLayout::Single
                && matches!(
                    previous,
                    Route::Library { .. } | Route::LibraryPerson { .. }
                );
            if !replace {
                let previous = if matches!(previous, Route::People { .. }) {
                    Route::People {
                        person: self
                            .people
                            .as_ref()
                            .and_then(|people| people.read(cx).selected_person().cloned()),
                    }
                } else {
                    previous.clone()
                };
                push_history(&mut self.back, previous);
            }
            self.forward.clear();
        }
        self.sidebar_cursor = match route {
            Route::ReviewQueue => 1,
            Route::People { .. } => 2,
            _ => 0,
        };
        self.last_route = Some(route.clone());
        let state = self.state.clone();
        if !matches!(&route, Route::Library { .. } | Route::LibraryPerson { .. }) {
            self.library
                .update(cx, |library, _| library.cancel_pending_selection());
        }
        if !matches!(&route, Route::Library { selected: Some(id) } if self.meeting.as_ref().is_some_and(|(m, _)| m == id))
        {
            self.meeting = None;
            self.meeting_title = None;
        }
        if route.selected_meeting().is_none() {
            window.set_window_title(&windows::title_for(self.host, history_destination(&route)));
        }
        match route {
            Route::Library { selected: Some(id) } => {
                if !selecting {
                    self.library
                        .update(cx, |view, cx| view.reveal_meeting(&id, window, cx));
                    if !keep_sidebar_focus && previous.is_some() {
                        self.focus_detail_next_frame(window, cx);
                    }
                }
                if self.meeting.is_none() {
                    let view =
                        cx.new(|cx| MeetingView::new(state, id.clone(), window, cx).embedded());
                    window.set_window_title(&windows::title_for(
                        self.host,
                        &view.read(cx).window_title(),
                    ));
                    self.meeting_title =
                        Some(cx.observe_in(&view, window, |this, view, window, cx| {
                            let title =
                                windows::title_for(this.host, &view.read(cx).window_title());
                            if window.window_title() != title {
                                window.set_window_title(&title);
                            }
                            this.publish_menu(window, cx);
                        }));
                    self.meeting = Some((id, view));
                }
            }
            Route::People { person } => {
                if self.people.is_none() {
                    let deps = self.deps.clone();
                    let people = cx.new(|cx| PeopleView::new(state, window, cx).embedded(deps));
                    let mut pane = None;
                    self._subscriptions.push(cx.observe_in(
                        &people,
                        window,
                        move |this, _, window, cx| {
                            this.publish_menu(window, cx);
                            let next = this.people_history_target(cx);
                            if next != pane {
                                pane = next;
                                cx.notify();
                            }
                        },
                    ));
                    self.people = Some(people);
                }
                if let (Some(id), Some(people)) = (person, &self.people) {
                    people.update(cx, |view, cx| {
                        view.request_selection(id, true, window, cx);
                    });
                } else if !keep_sidebar_focus
                    && previous.is_some()
                    && let Some(people) = &self.people
                {
                    people.focus_handle(cx).focus(window, cx);
                }
            }
            Route::ReviewQueue => {
                let review = match &self.review {
                    Some(review) => review.clone(),
                    None => {
                        let review = cx.new(|cx| ReviewView::new(state, window, cx).embedded());
                        self._subscriptions.push(cx.observe_in(
                            &review,
                            window,
                            |this, _, window, cx| this.publish_menu(window, cx),
                        ));
                        self.review = Some(review.clone());
                        review
                    }
                };
                if !keep_sidebar_focus && previous.is_some() {
                    review.focus_handle(cx).focus(window, cx);
                }
            }
            Route::LibraryPerson { person } => {
                self.library.update(cx, |view, cx| {
                    view.show_person_meetings(&person, window, cx);
                    view.focus_list(window, cx);
                });
            }
            Route::Library { selected: None } => {
                if !keep_sidebar_focus
                    && previous.is_some()
                    && !self.library.read(cx).search_has_focus(window, cx)
                {
                    self.library
                        .update(cx, |view, cx| view.focus_list(window, cx));
                }
            }
        }
    }

    fn focus_detail_next_frame(&self, window: &mut Window, cx: &mut Context<Self>) {
        let shell = cx.weak_entity();
        window.defer(cx, move |window, cx| {
            let _ = shell.update(cx, |this, cx| {
                if let Some((_, meeting)) = &this.meeting {
                    let focus = meeting.read(cx).detail_focus().clone();
                    focus.focus(window, cx);
                }
            });
        });
    }

    fn focus_on_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.shown.route {
            Route::Library { .. } | Route::LibraryPerson { .. } => {
                if self.pane_layout == PaneLayout::Single && self.meeting.is_some() {
                    self.focus_detail_next_frame(window, cx);
                } else if self.library.read(cx).has_meetings()
                    || self.state.read(cx).search().loading().is_some()
                {
                    self.library
                        .update(cx, |view, cx| view.focus_list(window, cx));
                } else if self.show_sidebar(self.pane_layout != PaneLayout::Wide) {
                    self.sidebar_rows[self.sidebar_cursor].focus(window, cx);
                } else {
                    self.sidebar_toggle_focus.focus(window, cx);
                }
            }
            Route::ReviewQueue | Route::People { .. } => {
                self.focus_next_area(false, window, cx);
            }
        }
    }

    fn sync_pane_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let layout = PaneLayout::for_window(window);
        if self.pane_layout != layout {
            let sidebar_focused = self.sidebar_focus.contains_focused(window, cx);
            self.pane_layout = layout;
            self.sidebar_open_narrow = false;
            if sidebar_focused && layout != PaneLayout::Wide {
                self.sidebar_toggle_focus.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn go(&mut self, route: Route, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| state.navigate(route, cx));
    }

    fn focus_library_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_expanded = true;
        self.library
            .update(cx, |view, cx| view.focus_search(window, cx));
        let library = self.library.clone();
        window.on_next_frame(move |window, cx| {
            library.update(cx, |view, cx| view.focus_search(window, cx));
        });
        cx.notify();
    }

    fn history_destination(&self, forward: bool, cx: &App) -> Option<&'static str> {
        if !forward
            && self.pane_layout == PaneLayout::Single
            && self.shown.route.selected_meeting().is_some()
            && !matches!(
                self.back.back(),
                Some(Route::People { .. } | Route::LibraryPerson { .. })
            )
        {
            Some("Library")
        } else if self.people_history_target(cx) == Some(!forward) {
            Some("People")
        } else {
            let routes = if forward { &self.forward } else { &self.back };
            routes.back().map(history_destination)
        }
    }

    fn publish_menu(&self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.meeting.as_ref().map(|(_, meeting)| meeting.read(cx));
        let dialog = view.is_some_and(MeetingView::dialog_open);
        let meeting = view.and_then(|view| view.menu_state_for_focus(window, cx));
        let menu = crate::menu::MainMenuState {
            back: self.history_destination(false, cx).is_some(),
            forward: self.history_destination(true, cx).is_some(),
            sidebar: self.show_sidebar(self.pane_layout != PaneLayout::Wide),
            fullscreen: window.is_fullscreen(),
            meeting,
            review_undo: self.shown.route == Route::ReviewQueue
                && self
                    .review
                    .as_ref()
                    .is_some_and(|view| view.read(cx).can_undo_for_focus(window, cx)),
            people_undo: matches!(self.shown.route, Route::People { .. })
                && self
                    .people
                    .as_ref()
                    .is_some_and(|people| people.read(cx).can_undo_for_focus(window, cx)),
            find: meeting.is_some()
                || matches!(
                    self.shown.route,
                    Route::Library { .. } | Route::LibraryPerson { .. } | Route::People { .. }
                ),
            matches: meeting.is_some_and(|meeting| meeting.find_matches),
            dialog,
        };
        crate::menu::publish_main_menu(window.window_handle().window_id(), menu, cx);
    }

    fn meeting_command<A: gpui::Action>(
        cx: &Context<Self>,
        run: impl Fn(&mut MeetingView, &mut Window, &mut Context<MeetingView>) + 'static,
    ) -> impl Fn(&A, &mut Window, &mut App) + 'static {
        cx.listener(move |this, _: &A, window, cx| {
            if let Some((_, meeting)) = &this.meeting
                && !(window.root::<Root>().flatten().is_some() && window.has_active_dialog(cx))
            {
                meeting.update(cx, |view, cx| run(view, window, cx));
                cx.stop_propagation();
            }
        })
    }

    fn people_history_target(&self, cx: &App) -> Option<bool> {
        (self.pane_layout == PaneLayout::Single
            && matches!(self.shown.route, Route::People { person: None }))
        .then_some(self.people.as_ref())
        .flatten()
        .and_then(|people| {
            let people = people.read(cx);
            people
                .can_switch_single_pane()
                .then(|| people.detail_is_visible(self.pane_layout))
        })
    }

    fn navigate_history(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.people_history_target(cx) == Some(!forward)
            && let Some(people) = &self.people
        {
            people.update(cx, |people, cx| {
                people.show_single_detail(forward, window, cx)
            });
            cx.notify();
            return;
        }
        if self.pane_layout == PaneLayout::Single
            && self.shown.route.selected_meeting().is_some()
            && !forward
            && !matches!(
                self.back.back(),
                Some(
                    Route::Library { selected: None }
                        | Route::LibraryPerson { .. }
                        | Route::People { .. }
                )
            )
        {
            push_history(&mut self.forward, self.shown.route.clone());
            let route = Route::library();
            self.history_target = Some(route.clone());
            self.go(route, cx);
            return;
        }
        let library = self.state.read(cx).library();
        let (source, destination) = if forward {
            (&mut self.forward, &mut self.back)
        } else {
            (&mut self.back, &mut self.forward)
        };
        while let Some(route) = source.pop_back() {
            if route
                .selected_meeting()
                .is_some_and(|id| matches!(library.meeting_exists(id), Ok(false)))
            {
                continue;
            }
            push_history(destination, self.shown.route.clone());
            self.history_target = Some(route.clone());
            self.go(route, cx);
            return;
        }
        cx.notify();
    }

    fn show_sidebar(&self, narrow: bool) -> bool {
        if narrow {
            self.sidebar_open_narrow
        } else {
            self.sidebar_visible
        }
    }

    fn toggle_sidebar(&mut self, narrow: bool, window: &mut Window, cx: &mut Context<Self>) {
        let visible = !self.show_sidebar(narrow);
        if narrow {
            self.sidebar_open_narrow = visible;
            if visible {
                let focus = self.sidebar_rows[self.sidebar_cursor].clone();
                window.on_next_frame(move |window, cx| focus.focus(window, cx));
            }
        } else {
            self.sidebar_visible = visible;
            crate::ui_state::remember_sidebar(visible, cx);
        }
        if !visible && self.sidebar_focus.contains_focused(window, cx) {
            self.sidebar_toggle_focus.focus(window, cx);
        }
        cx.notify();
    }

    fn choose_section(
        &mut self,
        index: usize,
        arrow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let route = match index {
            0 => self
                .library
                .read(cx)
                .selected_meeting()
                .cloned()
                .map_or_else(Route::library, |id| {
                    if self.pane_layout == PaneLayout::Single {
                        Route::library()
                    } else {
                        Route::meeting(id)
                    }
                }),
            1 => Route::ReviewQueue,
            _ => Route::people(),
        };
        let closing = self.sidebar_open_narrow && !arrow;
        if closing {
            self.toggle_sidebar(true, window, cx);
            self.sidebar_toggle_focus.focus(window, cx);
        } else if arrow {
            self.sidebar_rows[index].focus(window, cx);
        }
        if self.state.read(cx).route() != &route {
            self.keep_sidebar_focus = arrow || closing;
            self.go(route, cx);
        }
    }

    fn sidebar_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified()
            || !self.sidebar_rows.iter().any(|row| row.is_focused(window))
        {
            return;
        }
        let (index, arrow) = match event.keystroke.key.as_str() {
            "up" => ((self.sidebar_cursor + 2) % 3, true),
            "down" => ((self.sidebar_cursor + 1) % 3, true),
            "home" => (0, true),
            "end" => (2, true),
            "enter" | "space" => (self.sidebar_cursor, false),
            _ => return,
        };
        self.choose_section(index, arrow, window, cx);
        cx.stop_propagation();
        window.prevent_default();
        cx.notify();
    }

    fn focus_next_area(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut areas = vec![(
            0,
            self.toolbar_focus.clone(),
            Some(self.sidebar_toggle_focus.clone()),
        )];
        if self.show_sidebar(self.pane_layout != PaneLayout::Wide) {
            areas.push((
                1,
                self.sidebar_focus.clone(),
                Some(self.sidebar_rows[self.sidebar_cursor].clone()),
            ));
        }
        match self.shown.route {
            Route::Library { .. } | Route::LibraryPerson { .. } => {
                if (self.pane_layout != PaneLayout::Single || self.meeting.is_none())
                    && self.library.read(cx).has_meetings()
                {
                    areas.push((2, self.library.focus_handle(cx), None));
                }
                if let Some((_, meeting)) = &self.meeting {
                    areas.push((
                        3,
                        self.content_focus.clone(),
                        Some(meeting.read(cx).detail_focus().clone()),
                    ));
                }
            }
            Route::People { .. } => {
                if let Some(people) = &self.people {
                    let people = people.read(cx);
                    if people.list_is_visible(self.pane_layout) {
                        areas.push((2, people.focus_handle(cx), None));
                    }
                    if people.detail_is_visible(self.pane_layout) {
                        areas.push((3, people.detail_focus().clone(), None));
                    }
                }
            }
            Route::ReviewQueue => {
                if let Some(review) = &self.review {
                    areas.push((2, self.content_focus.clone(), Some(review.focus_handle(cx))));
                }
            }
        }
        if !self.shown.running_jobs.is_empty()
            || self.shown.job_failure.is_some()
            || self.imports.read(cx).progress().is_some()
        {
            areas.push((4, self.status_focus.clone(), None));
        }
        let current = areas
            .iter()
            .position(|(_, focus, _)| focus.contains_focused(window, cx))
            .or_else(|| {
                self.recording_toolbar_focus
                    .contains_focused(window, cx)
                    .then_some(0)
            });
        if let Some(index) = current {
            self.last_area_focus[areas[index].0] =
                window.focused(cx).map(|focus| focus.downgrade());
        }
        let next = match (current, backwards) {
            (Some(index), false) => (index + 1) % areas.len(),
            (Some(index), true) => (index + areas.len() - 1) % areas.len(),
            (None, false) => 0,
            (None, true) => areas.len() - 1,
        };
        let (area, root, target) = &areas[next];
        let last = self.last_area_focus[*area]
            .as_ref()
            .and_then(gpui::WeakFocusHandle::upgrade)
            .filter(|last| root == last || root.contains(last, window));
        last.as_ref()
            .or(target.as_ref())
            .unwrap_or(root)
            .focus(window, cx);
    }

    fn nav_item(
        &self,
        id: &'static str,
        label: SharedString,
        index: usize,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let selected = index == self.sidebar_cursor;
        let focused = selected && self.sidebar_rows[index].is_focused(window);
        let (spoken, badge, error) = match (
            index,
            self.shown.review_count,
            &self.shown.review_count_error,
        ) {
            (1, _, Some(error)) => (
                "Review queue, count unavailable".into(),
                None,
                Some(format!(
                    "Couldn't count speakers to review. {}",
                    crate::jobs::sentence(error)
                )),
            ),
            (1, Some(count), _) if count > 0 => (
                format!(
                    "Review queue, {} to review",
                    crate::ui::format::count(count as u64, "speaker", "speakers")
                )
                .into(),
                Some(count),
                None,
            ),
            _ => (label.clone(), None, None),
        };
        let icon = [icons::LIBRARY, icons::REVIEW_QUEUE, icons::PEOPLE][index];
        let row = div()
            .id(id)
            .w_full()
            .min_h(gpui::rems(2.))
            .flex()
            .gap(kit::GROUP_GAP);
        kit::sidebar_item(row, selected, focused, cx)
            .role(Role::ListItem)
            .aria_label(spoken)
            .aria_position_in_set(index + 1)
            .aria_size_of_set(3)
            .track_focus(
                &self.sidebar_rows[index]
                    .clone()
                    .tab_index(if selected { 0 } else { -1 })
                    .tab_stop(selected),
            )
            .min_h(gpui::rems(2.))
            .flex_none()
            .py_1()
            .px_3()
            .flex()
            .items_center()
            .text_role(TextRole::Label)
            .on_click(
                cx.listener(move |this, _, window, cx| {
                    this.choose_section(index, false, window, cx)
                }),
            )
            .gap(px(8.))
            .child(
                div()
                    .debug_selector(move || format!("{id}-icon"))
                    .size(px(16.))
                    .flex_none()
                    .child(
                        icons::icon(icon)
                            .size(px(16.))
                            .text_color(kit::sidebar_icon_colour(selected, cx)),
                    ),
            )
            .child(div().flex_1().min_w_0().whitespace_normal().child(label))
            .when_some(badge, |el, count| {
                el.child(kit::count_badge(count, cx).debug_selector(|| "nav-review-badge".into()))
            })
            .when_some(error, |el, error| {
                el.tooltip(kit::title_tooltip(error))
                    .child(kit::glyph(assets::STATUS_FAILED, theme.danger))
            })
    }

    fn sidebar(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .id("sidebar")
            .debug_selector(|| "main-sidebar".into())
            .role(Role::Navigation)
            .aria_label("Sections")
            .track_focus(&self.sidebar_focus)
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.sidebar_key(event, window, cx)
            }))
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .h_full()
            .p_2()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .id("sidebar-sections")
                    .role(Role::List)
                    .aria_label("Main sections")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(self.nav_item("nav-library", "Library".into(), 0, window, cx))
                    .child(self.nav_item("nav-review", "Review queue".into(), 1, window, cx))
                    .child(self.nav_item("nav-people", "People".into(), 2, window, cx)),
            )
            .child(div().flex_1())
            .when_some(self.shown.setup_progress, |el, completed| {
                el.child(
                    div()
                        .debug_selector(|| "finish-setup-hit-area".into())
                        .child(
                            Button::new("finish-setup")
                                .tab_stop(true)
                                .w_full()
                                .label(format!(
                                    "Finish setup · {completed} of {}",
                                    setup::ESSENTIAL_SETUP_COUNT
                                ))
                                .ghost()
                                .child(kit::glyph(assets::STATUS_PENDING, theme.muted_foreground))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Err(error) =
                                        setup_window::show(&this.state, &this.deps, cx)
                                    {
                                        tracing::error!(%error, "cannot open the setup assistant");
                                    }
                                })),
                        ),
                )
            })
            .child(
                Button::new("nav-settings")
                    .tab_stop(true)
                    .label("Settings")
                    .icon(gpui::component::IconName::Settings)
                    .ghost()
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(crate::menu::OpenSettings), cx)
                    }),
            )
    }

    fn toggle_main_menu(&mut self, keyboard: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.main_menu.is_some() {
            self.close_main_menu(window, cx);
        } else if !crate::menu::has_global_menu_bar(self.host) && !window.has_active_dialog(cx) {
            self.open_main_menu(keyboard, window, cx);
        }
    }

    fn close_main_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.main_menu.take() {
            if open.view.focus_handle(cx).contains_focused(window, cx) {
                open.return_focus.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn open_main_menu(&mut self, keyboard: bool, window: &mut Window, cx: &mut Context<Self>) {
        let return_focus = if keyboard {
            window
                .focused(cx)
                .unwrap_or_else(|| self.main_menu_focus.clone())
        } else {
            self.main_menu_focus.clone()
        };
        let reading = self.state.read(cx).config().appearance.reading_size;
        let updates = crate::views::update::UpdateNotices::global(cx);
        let updates = updates
            .read(cx)
            .menu_state(self.recording_control.read(cx).in_capture());
        let can_import = !window.has_active_dialog(cx);
        let menu = PopupMenu::build(window, cx, |menu, _, _| {
            crate::menu::toolbar_menu(
                menu.action_context(return_focus.clone()),
                reading,
                can_import,
                updates,
            )
        });
        let dismiss = cx.subscribe_in(
            &menu,
            window,
            |this, _, _: &gpui::DismissEvent, window, cx| {
                this.close_main_menu(window, cx);
            },
        );
        let focus = menu.focus_handle(cx);
        self.main_menu = Some(OpenMainMenu {
            view: menu,
            return_focus,
            _dismiss: dismiss,
        });
        if keyboard {
            window.on_next_frame(move |window, cx| {
                if focus.contains_focused(window, cx) {
                    focus.dispatch_action(&gpui::base::actions::SelectDown, window, cx);
                }
            });
        }
        cx.notify();
    }

    fn search_is_compact(window: &Window) -> bool {
        window.viewport_size().width < window.rem_size() * 58.
    }

    fn recording_controls_need_row(window: &Window) -> bool {
        window.viewport_size().width < window.rem_size() * 30.
    }

    fn recording_toolbar_row(&self) -> impl IntoElement {
        div()
            .id("recording-toolbar-row")
            .track_focus(&self.recording_toolbar_focus)
            .w_full()
            .px_2()
            .py_1()
            .child(self.recording_control.clone())
    }

    fn toolbar(
        &self,
        narrow: bool,
        chrome: ToolbarChrome,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let width = window.viewport_size().width;
        let separate_recording_controls = Self::recording_controls_need_row(window);
        let search_collapsed = Self::search_is_compact(window);
        let hide_import = width < window.rem_size() * 45.;
        let sidebar_label = if self.show_sidebar(narrow) {
            "Hide sidebar"
        } else {
            "Show sidebar"
        };
        let section = crate::ui_state::Section::of(&self.shown.route);
        let mut bar = div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_1()
            .items_center()
            .gap_1()
            .px_2()
            .when(chrome == ToolbarChrome::Native, |bar| {
                bar.h(chrome::TOOLBAR_HEIGHT)
                    .flex_none()
                    .border_b_1()
                    .border_color(cx.theme().title_bar_border)
                    .bg(cx.theme().title_bar)
            })
            .child(
                Button::new("toggle-sidebar")
                    .track_focus(&self.sidebar_toggle_focus)
                    .icon(icons::SIDEBAR)
                    .accessibility_label(sidebar_label)
                    .tooltip_with_action(
                        sidebar_label,
                        &crate::menu::ToggleSidebar,
                        Some("BroScribeMain"),
                    )
                    .ghost()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_sidebar(narrow, window, cx)
                    })),
            )
            .child(self.history_button(false, cx))
            .child(self.history_button(true, cx))
            .when(chrome != ToolbarChrome::Client, |bar| {
                let label = history_destination(&section.route());
                bar.child(
                    heading("toolbar-title", label)
                        .aria_level(1)
                        .min_w_0()
                        .truncate()
                        .tooltip(kit::title_tooltip(label)),
                )
            })
            .when(search_collapsed && !self.search_expanded, |bar| {
                bar.child(
                    Button::new("toolbar-search")
                        .icon(icons::SEARCH)
                        .accessibility_label("Search meetings")
                        .tooltip_with_action("Search meetings", &crate::menu::SearchMeetings, None)
                        .ghost()
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(crate::menu::SearchMeetings), cx)
                        }),
                )
            })
            .child(crate::views::library::LibraryToolbar {
                library: self.library.clone(),
                hide_import,
                hide_search: search_collapsed,
            });
        if !crate::menu::has_global_menu_bar(self.host) {
            let main = cx.weak_entity();
            let open = self.main_menu.as_ref().map(|open| open.view.clone());
            let mut popover = Popover::new("toolbar-main-menu")
                .anchor(gpui::Anchor::TopRight)
                .appearance(false)
                .open(open.is_some())
                .trigger(
                    Button::new("toolbar-more")
                        .icon(icons::MAIN_MENU)
                        .ghost()
                        .track_focus(&self.main_menu_focus)
                        .accessibility_label("Main menu")
                        .tooltip("Main menu (F10)"),
                )
                .on_open_change(move |open, window, cx| {
                    let _ = main.update(cx, |this, cx| {
                        if *open {
                            this.open_main_menu(false, window, cx);
                        } else {
                            this.close_main_menu(window, cx);
                        }
                    });
                });
            if let Some(open) = open {
                popover = popover
                    .track_focus(&open.focus_handle(cx))
                    .content(move |_, _, _| open.clone());
            }
            bar = bar.child(popover);
        }
        let bar = bar.when(!separate_recording_controls, |bar| {
            bar.child(self.recording_control.clone())
        });
        let navigation = if chrome == ToolbarChrome::Unified {
            gpui::component::TitleBar::new()
                .h(gpui::rems(3.25))
                .child(bar)
                .into_any_element()
        } else {
            bar.into_any_element()
        };
        div()
            .id("main-toolbar")
            .role(Role::Toolbar)
            .aria_label("BroScribe controls")
            .track_focus(&self.toolbar_focus)
            .flex()
            .flex_col()
            .when(chrome == ToolbarChrome::Client, |bar| {
                bar.flex_1().min_w_0()
            })
            .when(chrome != ToolbarChrome::Client, |bar| {
                bar.flex_none().w_full()
            })
            .child(navigation)
            .when(
                separate_recording_controls && chrome != ToolbarChrome::Client,
                |bar| bar.child(self.recording_toolbar_row()),
            )
            .into_any_element()
    }

    fn history_button(&self, forward: bool, cx: &Context<Self>) -> Button {
        let (id, label) = if forward {
            ("history-forward", "Forward")
        } else {
            ("history-back", "Back")
        };
        let target = self.history_destination(forward, cx);
        let label =
            target.map_or_else(|| label.to_owned(), |target| format!("{label} to {target}"));
        Button::new(id)
            .icon(if forward {
                gpui::assets::IconName::ArrowRight
            } else {
                gpui::assets::IconName::ArrowLeft
            })
            .accessibility_label(label.clone())
            .ghost()
            .disabled(target.is_none())
            .when(target.is_some(), |button| button.tooltip(label))
            .on_click(
                cx.listener(move |this, _, window, cx| this.navigate_history(forward, window, cx)),
            )
    }

    fn content(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let view = match &self.shown.route {
            Route::Library { .. } | Route::LibraryPerson { .. } => {
                // The meetings list is a card on the desk; the meeting beside
                // it puts its own header on the desk and its content in a card.
                let split = self.pane_layout != PaneLayout::Single;
                let list = div()
                    .id("library-desk")
                    .size_full()
                    .flex()
                    .flex_col()
                    .pl(kit::DESK_PADDING)
                    .py(kit::DESK_PADDING)
                    .map(|desk| {
                        if split {
                            desk.pr_2()
                        } else {
                            desk.pr(kit::DESK_PADDING)
                        }
                    })
                    .child(
                        kit::desk_card("library-card", cx)
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.library.clone()),
                    )
                    .into_any_element();
                let detail = self.meeting.as_ref().map_or_else(
                    || {
                        kit::empty_state(
                            "meeting-detail-empty",
                            "No meeting selected",
                            text_line(
                                "meeting-detail-guidance",
                                Role::Label,
                                "Select a meeting to see its transcript and summary.",
                            ),
                            false,
                            None,
                            None,
                            cx,
                        )
                        .into_any_element()
                    },
                    |(_, view)| view.clone().into_any_element(),
                );
                let detail = div()
                    .id("meeting-detail-pane")
                    .debug_selector(|| "main-detail-pane".into())
                    .track_focus(&self.content_focus)
                    .size_full()
                    .child(detail);
                Some(if self.pane_layout != PaneLayout::Single {
                    content_split("library-split", list, detail.into_any_element(), window, cx)
                } else if self.meeting.is_some() {
                    detail.into_any_element()
                } else {
                    list
                })
            }
            Route::People { .. } => self.people.clone().map(|v| v.into_any_element()),
            Route::ReviewQueue => self.review.clone().map(|v| {
                div()
                    .id("review-content")
                    .track_focus(&self.content_focus)
                    .size_full()
                    .child(v)
                    .into_any_element()
            }),
        };
        // `sync_route` runs before the repaint that shows a new route.
        view.unwrap_or_else(|| {
            div()
                .id("main")
                .role(Role::Main)
                .flex_1()
                .into_any_element()
        })
    }

    fn processing_notice(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let (kind, role, text, retry) = match self.shown.worker_notice.as_ref()? {
            WorkerNotice::Restarting { attempt } => (
                kit::StatusKind::Attention,
                Role::Status,
                format!("Background jobs stopped unexpectedly. Retrying · attempt {attempt}"),
                false,
            ),
            WorkerNotice::Unavailable { crashes } => (
                kit::StatusKind::Failed,
                Role::Alert,
                match crashes {
                    Some(crashes) => format!(
                        "Background jobs can't run. They stopped unexpectedly {crashes} times in a row."
                    ),
                    None => {
                        "Background jobs can't run. BroScribe's command-line tool couldn't start."
                            .into()
                    }
                },
                true,
            ),
        };
        Some(
            kit::banner("jobs-worker", kind, role, text, cx)
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .when(retry, |actions| {
                            actions.child(
                                Button::new("jobs-worker-retry")
                                    .label("Try again")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.state.update(cx, |state, cx| state.kick_jobs(cx));
                                    })),
                            )
                        })
                        .when(!retry, |actions| {
                            actions.child(
                                Button::new("jobs-worker-diagnostics")
                                    .label("Diagnostics")
                                    .ghost()
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(crate::menu::ShowAbout), cx)
                                    }),
                            )
                        })
                        .child(
                            Button::new("jobs-worker-copy")
                                .label("Copy details")
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let jobs = this.state.read(cx).jobs();
                                    let detail = match &jobs.worker {
                                        WorkerState::Unavailable { detail, .. } => Some(detail),
                                        _ => jobs.last_crash.as_ref(),
                                    };
                                    if let Some(detail) = detail {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            detail.clone(),
                                        ));
                                    }
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn processing_status(&self, cx: &Context<Self>) -> impl IntoElement {
        use gpui::component::status_bar::StatusBar;
        let running = &self.shown.running_jobs;
        let mut status = StatusBar::new()
            .text_role(TextRole::Caption)
            .h(gpui::rems(1.75))
            .py_0()
            .overflow_hidden();
        let downloads = self.state.read(cx).model_downloads();
        if let Some(percent) = downloads.read(cx).percent() {
            let download = downloads.clone();
            status = status
                .left(kit::status_line(
                    "main-model-download",
                    kit::StatusKind::Pending,
                    format!("Downloading speech models · {}", format::percent(percent)),
                    cx,
                ))
                .left(
                    Button::new("main-model-download-cancel")
                        .xsmall()
                        .ghost()
                        .label("Cancel")
                        .disabled(downloads.read(cx).stopping())
                        .on_click(move |_, _, cx| {
                            download.update(cx, setup::ModelDownloads::cancel)
                        }),
                );
        }
        if let Some((position, total, run)) = self.imports.read(cx).progress() {
            let words = if run.is_manifest() {
                "The manifest’s meetings are queued.".to_owned()
            } else {
                format!(
                    "Importing {position} of {total} · {}",
                    file_label(&run.path)
                )
            };
            status = status
                .left(kit::status_line(
                    "main-import-status",
                    kit::StatusKind::Pending,
                    words,
                    cx,
                ))
                .left(
                    Button::new("main-show-import")
                        .label("Show")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.go(Route::library(), cx);
                            this.library
                                .update(cx, |view, cx| view.focus_imports(window, cx));
                        })),
                );
        }
        if let Some(first) = running.first() {
            status = status.left(job_status_line(first, true, cx));
        }
        if running.len() > 1 {
            let all_jobs = running.clone();
            status = status.left(
                Popover::new("jobs-popover")
                    .anchor(gpui::Anchor::BottomRight)
                    .trigger(
                        Button::new("jobs-more")
                            .label(format!("+{} more", running.len() - 1))
                            .xsmall()
                            .ghost(),
                    )
                    .content(move |_, _, cx| {
                        div()
                            .id("jobs")
                            .role(Role::Group)
                            .aria_label("Background jobs")
                            .max_w(gpui::rems(26.))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(all_jobs.iter().map(|job| job_status_line(job, false, cx)))
                    }),
            );
        }
        if let Some(failure) = &self.shown.job_failure {
            let id = failure.job_id.clone();
            status = status.right(
                div()
                    .flex()
                    .min_w_0()
                    .items_center()
                    .gap_1()
                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().child(
                        kit::status_line(
                            "jobs-failure",
                            kit::StatusKind::Failed,
                            failure.text.clone(),
                            cx,
                        ),
                    ))
                    .when_some(failure.meeting.clone(), |row, id| {
                        row.child(
                            Button::new("jobs-failure-open")
                                .label("Open meeting")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.go(Route::meeting(id.clone()), cx)
                                })),
                        )
                    })
                    .when(failure.needs_model, |row| {
                        row.child(
                            Button::new("jobs-failure-models")
                                .label("Open model settings")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.state.update(cx, |_, cx| {
                                        cx.emit(crate::state::AppEvent::OpenSettings(Some(
                                            crate::state::SettingsPage::Transcription,
                                        )));
                                    })
                                })),
                        )
                    })
                    .child(
                        Button::new("jobs-failure-dismiss")
                            .label("Dismiss")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.state.update(cx, |state, cx| {
                                    state.dismiss_job_failure(id.clone(), cx);
                                })
                            })),
                    ),
            );
        }
        div()
            .id("status-area")
            .role(Role::Group)
            .aria_label("Background jobs")
            .debug_selector(|| "main-status-bar".into())
            .track_focus(&self.status_focus)
            .flex_none()
            .child(status)
    }
}

fn push_history(history: &mut VecDeque<Route>, route: Route) {
    if history.back() != Some(&route) {
        if history.len() == 50 {
            history.pop_front();
        }
        history.push_back(route);
    }
}

fn history_destination(route: &Route) -> &'static str {
    match route {
        Route::Library { selected: None } | Route::LibraryPerson { .. } => "Library",
        Route::ReviewQueue => "Review queue",
        Route::People { .. } => "People",
        Route::Library { selected: Some(_) } => "the meeting",
    }
}

/// The Library and People share the same saved list width, in Standard points.
pub(crate) fn content_split(
    id: &'static str,
    list: AnyElement,
    detail: AnyElement,
    window: &Window,
    cx: &App,
) -> AnyElement {
    let scale = f32::from(window.rem_size()) / 16.;
    let (_, _, saved_width) = crate::ui_state::main_panes(cx);
    h_resizable((id, f32::from(window.rem_size()).to_bits() as u64))
        .child(
            resizable_panel()
                .size(px(saved_width
                    .unwrap_or(LIST_WIDTH)
                    .clamp(LIST_MIN_WIDTH, LIST_MAX_WIDTH)
                    * scale))
                .size_range(px(LIST_MIN_WIDTH * scale)..px(LIST_MAX_WIDTH * scale))
                .fixed()
                .child(list),
        )
        .child(
            resizable_panel()
                .size_range(px(420. * scale)..px(f32::MAX))
                .child(detail),
        )
        .on_resize(move |state, _, cx| {
            if let Some(width) = state.read(cx).sizes().first() {
                crate::ui_state::remember_library_width(f32::from(*width) / scale, cx);
            }
        })
        .into_any_element()
}

/// Reads the saved settings again on the background executor and gives them
/// to the state, which applies only a revision newer than its own (another
/// process, e.g. `broscribe config`, may have saved them).
pub fn reload_config(state: &Entity<AppState>, cx: &mut App) {
    let data_dir = state.read(cx).paths().data_dir.clone();
    let load =
        state.read(cx).cli().spawn(
            cx.background_executor(),
            async move { Config::load(&data_dir) },
        );
    let state = state.downgrade();
    cx.spawn(async move |cx| match load.await {
        Ok(config) => {
            state
                .update(cx, |state, cx| state.set_config(config, cx))
                .ok();
        }
        Err(error) => tracing::warn!(code = error.code(), "reading the settings again"),
    })
    .detach();
}

/// The theme's text sizes before a reading size scaled them, and the sizes
/// last set; a theme loaded since (other sizes) becomes the new base.
struct ReadingBase {
    base: (Pixels, Pixels),
    applied: (Pixels, Pixels),
}

impl Global for ReadingBase {}

/// The state whose saved reading size the theme follows.
struct ReadingFollower {
    state: EntityId,
    _subscriptions: [Subscription; 2],
}

impl Global for ReadingFollower {}

/// Scales the theme's base and monospace text sizes (gpui-component sets
/// every window's rem size from them) by `size`. Colours, fonts and mode are
/// untouched; windows repaint only when a size changes.
pub fn apply_reading_size(size: ReadingSize, cx: &mut App) {
    if !cx.has_global::<Theme>() {
        return;
    }
    let current = {
        let theme = cx.theme();
        (theme.font_size, theme.mono_font_size)
    };
    let base = match cx.try_global::<ReadingBase>() {
        Some(scale) if scale.applied == current => scale.base,
        _ => current,
    };
    let target = (base.0 * size.scale(), base.1 * size.scale());
    if target != current {
        Theme::update(cx, |theme| {
            theme.font_size = target.0;
            theme.mono_font_size = target.1;
        });
    }
    cx.set_global(ReadingBase {
        base,
        applied: target,
    });
}

/// Applies `state`'s saved reading size and Appearance (light, dark or the
/// system's) now and whenever its config (applied in revision order) or the
/// theme changes. Once per state: a reopened
/// window does not follow twice.
pub fn follow_reading_size(state: &Entity<AppState>, cx: &mut App) {
    let id = state.entity_id();
    if cx
        .try_global::<ReadingFollower>()
        .is_some_and(|follower| follower.state == id)
    {
        return;
    }
    let shown_now = saved_reading_size(state, cx);
    apply_reading_size(shown_now, cx);
    theme::apply_theme_choice(state.read(cx).config().appearance.theme, cx);
    let mut shown = shown_now;
    let on_config = cx.observe(state, move |state, cx| {
        let now = saved_reading_size(&state, cx);
        if now != shown {
            shown = now;
            apply_reading_size(now, cx);
        }
        // Light, dark or the system's appearance (a repeat costs nothing).
        theme::apply_theme_choice(state.read(cx).config().appearance.theme, cx);
    });
    // A theme loaded over the scaled sizes is scaled again.
    let weak = state.downgrade();
    let on_theme = cx.observe_global::<Theme>(move |cx| {
        if let Some(state) = weak.upgrade() {
            let now = saved_reading_size(&state, cx);
            apply_reading_size(now, cx);
        }
    });
    cx.set_global(ReadingFollower {
        state: id,
        _subscriptions: [on_config, on_theme],
    });
}

fn saved_reading_size(state: &Entity<AppState>, cx: &App) -> ReadingSize {
    state.read(cx).config().appearance.reading_size
}

/// What the footer calls a job of `kind`: the core's label, naming the
/// template of a template job the user started from here (progress events
/// carry only the kind).
pub fn job_label(state: &AppState, job_id: &str, kind: Option<&str>) -> String {
    let Some(kind) = kind else {
        return "Background job".to_owned();
    };
    let input = match state.generations().template_name(job_id) {
        Some(name) => serde_json::json!({ "template_name": name }),
        None => serde_json::Value::Null,
    };
    kind_label(kind, &input)
}

fn failed_job_text(state: &AppState, failed: &FailedJob) -> String {
    let label = job_label(state, &failed.job_id, failed.kind.as_deref());
    let title = failed
        .meeting_id
        .as_ref()
        .and_then(|id| state.job_meeting_title(id));
    let what = match title {
        Some(title) if label != "Background job" => {
            let subject = match failed.kind.as_deref() {
                Some(broscribe_core::queue::ENRICH_KIND) => "generating the summary".into(),
                _ => {
                    let mut label = label;
                    if let Some(first) = label.get_mut(..1) {
                        first.make_ascii_lowercase();
                    }
                    label
                }
            };
            format!("Couldn't finish {subject} for “{title}”.")
        }
        _ => "Couldn't finish a background job.".to_owned(),
    };
    let error = failed.message.as_deref().and_then(stored_error);
    let cause = error
        .as_ref()
        .map(core_user_text)
        .unwrap_or_else(|| "Something went wrong.".into());
    failure_text(&what, &cause, "")
}

/// The bar shows running jobs only. A job's key follows it when a different
/// job finishes, and progress numbers have their own non-live node.
fn running_jobs(state: &AppState) -> Vec<JobLine> {
    let jobs = state.jobs();
    let mut lines: Vec<_> = jobs
        .active
        .iter()
        .map(|job| {
            let mut text = job_label(state, &job.job_id, job.kind.as_deref());
            if let Some(title) = job
                .meeting_id
                .as_ref()
                .and_then(|id| state.job_meeting_title(id))
            {
                text.push_str(" · ");
                text.push_str(title);
            }
            let counting = job.kind.as_deref() == Some(REIDENTIFY_KIND);
            let counts = job.message.clone().filter(|_| counting);
            if let Some(message) = job
                .message
                .as_deref()
                .filter(|message| !counting && !message.trim().is_empty())
            {
                text.push_str(" · ");
                text.push_str(message.trim());
            }
            let percent = job.fraction.and_then(format::progress_value);
            if !counting && percent.is_none() && !text.contains(" · ") {
                text.push('…');
            }
            JobLine {
                id: format!("jobs-active-{}", job.job_id).into(),
                text,
                counts,
                percent,
            }
        })
        .collect();
    if jobs.worker == WorkerState::Running && lines.is_empty() {
        lines.push(JobLine {
            id: "jobs-worker-working".into(),
            text: "Starting background jobs…".into(),
            counts: None,
            percent: None,
        });
    }
    lines
}

fn job_status_line(item: &JobLine, one_line: bool, cx: &App) -> Div {
    let kind = kit::StatusKind::Pending;
    let status = kit::status_without_wait_clock(item.id.clone(), kind, item.text.clone(), cx);
    div()
        .flex()
        .when_else(
            one_line,
            |row| row.min_w_0().overflow_hidden().whitespace_nowrap(),
            |row| row.flex_wrap(),
        )
        .items_center()
        .gap_1()
        .child(status)
        .when_some(item.counts.as_deref(), |el, counts| {
            el.child(tabular(text_line(
                SharedString::from(format!("{}-counts", item.id)),
                Role::Label,
                format!("· {counts}"),
            )))
        })
        .when_some(item.percent, |el, percent| {
            el.child(tabular(text_line(
                format!("{}-detail", item.id),
                Role::Label,
                format!("· {}", format::percent(percent)),
            )))
        })
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Reading size can change without a window resize.
        self.sync_pane_layout(window, cx);
        self.publish_menu(window, cx);
        let narrow = self.pane_layout != PaneLayout::Wide;
        let (_, saved_sidebar, _) = crate::ui_state::main_panes(cx);
        // Persist widths in Standard points so an existing layout follows
        // Reading size without changing the remembered proportions.
        let scale = f32::from(window.rem_size()) / 16.;
        let scaled = move |points| px(points * scale);
        let sidebar_width = scaled(saved_sidebar.unwrap_or(SIDEBAR_WIDTH).clamp(180., 280.));
        self.library.update(cx, |library, cx| {
            library.set_compact(self.pane_layout != PaneLayout::Single, cx)
        });
        // A platform drag end invalidates the window even when the pointer is
        // no longer over a hitbox (for example, another app took the drag).
        if self.import_drop.is_some() && !cx.has_active_drag() {
            self.import_drop = None;
            self.import_drag_paths.clear();
        }
        let theme = cx.theme();
        let content = self.content(window, cx);
        let body = div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .min_w_0()
            .children(self.processing_notice(cx))
            .child(
                div()
                    .id("content-area")
                    .when(
                        self.pane_layout == PaneLayout::Single && self.meeting.is_some(),
                        |area| area.role(Role::Main).aria_label("Library"),
                    )
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(content)
                    .when_some(self.import_drop, |area, accepted| {
                        area.child(
                            div()
                                .absolute()
                                .inset_2()
                                .flex()
                                .justify_center()
                                .items_center()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .gap_1()
                                        .p_4()
                                        .rounded_lg()
                                        .border_2()
                                        .border_color(theme.foreground)
                                        .bg(theme.popover)
                                        .child(if accepted {
                                            text_line(
                                                "drop-hint-title",
                                                Role::Status,
                                                "Drop to import",
                                            )
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                        } else {
                                            kit::status_line(
                                                "drop-hint-title",
                                                kit::StatusKind::Failed,
                                                "BroScribe can't import these files.",
                                                cx,
                                            )
                                        })
                                        .when(accepted, |card| {
                                            card.child(
                                                text_line(
                                                    "drop-hint-body",
                                                    Role::Label,
                                                    "Recordings, folders or an import manifest",
                                                )
                                                .text_role(TextRole::Label)
                                                .text_color(theme.muted_foreground),
                                            )
                                        }),
                                ),
                        )
                    }),
            )
            .child(self.processing_status(cx));
        let panes = if !narrow && self.show_sidebar(false) {
            h_resizable(("main-panes", f32::from(window.rem_size()).to_bits() as u64))
                .child(
                    resizable_panel()
                        .size(sidebar_width)
                        .size_range(scaled(180.)..scaled(280.))
                        .fixed()
                        .child(self.sidebar(window, cx)),
                )
                .child(resizable_panel().child(body))
                .on_resize(move |state, _, cx| {
                    if let Some(width) = state.read(cx).sizes().first() {
                        crate::ui_state::remember_sidebar_width(f32::from(*width) / scale, cx);
                    }
                })
                .into_any_element()
        } else {
            div()
                .id("main-content-with-sidebar-overlay")
                .relative()
                .size_full()
                .child(body)
                .when(narrow && self.sidebar_open_narrow, |body| {
                    body.child(
                        div()
                            .id("sidebar-overlay")
                            .absolute()
                            .top_0()
                            .left_0()
                            .bottom_0()
                            .w(sidebar_width)
                            .shadow_lg()
                            .child(self.sidebar(window, cx)),
                    )
                })
                .into_any_element()
        };
        let shell = div()
            .id("main-window")
            .text_role(TextRole::Body)
            .w_full()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_row()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(panes);
        let chrome = ToolbarChrome::for_window(self.host, chrome::client_decorated(window));
        let toolbar = self.toolbar(narrow, chrome, window, cx);
        use crate::menu::*;
        let layout = div()
            .size_full()
            .flex()
            .flex_col()
            .on_drag_move(cx.listener(
                |this, event: &gpui::DragMoveEvent<ExternalPaths>, window, cx| {
                    if window.root::<Root>().flatten().is_some()
                        && !window.has_active_dialog(cx)
                        && event.bounds.contains(&event.event.position)
                    {
                        if let Some(paths) = event.dragged_item().downcast_ref::<ExternalPaths>() {
                            this.update_import_drag(paths.paths(), cx);
                        }
                    } else {
                        this.set_import_drop(None, cx);
                    }
                },
            ))
            .on_file_drop_exit(cx.listener(|this, _, _, cx| this.set_import_drop(None, cx)))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.set_import_drop(None, cx);
                }),
            )
            .can_drop({
                let drag = self.import_drop;
                let paths = self.import_drag_paths.clone();
                move |payload, window, cx| {
                    payload
                        .downcast_ref::<ExternalPaths>()
                        .is_some_and(|external| {
                            window.root::<Root>().flatten().is_some()
                                && !window.has_active_dialog(cx)
                                && drag
                                    .filter(|_| paths == external.paths())
                                    .unwrap_or_else(|| accepts_drop(external.paths()))
                        })
                }
            })
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.set_import_drop(None, cx);
                ImportRuns::request_dialog(dropped_paths(paths), cx);
            }))
            .key_context("BroScribeMain")
            .on_action(Self::meeting_command::<ExportMeeting>(
                cx,
                MeetingView::present_export,
            ))
            .on_action(Self::meeting_command::<RenameMeeting>(
                cx,
                MeetingView::start_rename,
            ))
            .on_action(Self::meeting_command::<CopyTranscript>(
                cx,
                |view, _, cx| view.copy_transcript(cx),
            ))
            .on_action(Self::meeting_command::<RevealRecording>(
                cx,
                |view, _, cx| view.reveal_recording(cx),
            ))
            .on_action(Self::meeting_command::<ToggleMeetingPlayback>(
                cx,
                MeetingView::toggle_playback,
            ))
            .on_action(Self::meeting_command::<BackFifteenSeconds>(
                cx,
                |view, window, cx| view.seek_relative(-15., window, cx),
            ))
            .on_action(Self::meeting_command::<ForwardFifteenSeconds>(
                cx,
                |view, window, cx| view.seek_relative(15., window, cx),
            ))
            .on_action(Self::meeting_command::<GenerateMeetingSummary>(
                cx,
                MeetingView::show_summary_controls,
            ))
            .on_action(Self::meeting_command::<CopyMeetingSummary>(
                cx,
                |view, _, cx| view.copy_summary(cx),
            ))
            .on_action(Self::meeting_command::<DeleteMeeting>(
                cx,
                MeetingView::present_delete,
            ))
            .on_action(Self::meeting_command::<FindNext>(cx, |view, window, cx| {
                view.step_find_command(true, window, cx)
            }))
            .on_action(Self::meeting_command::<FindPrevious>(
                cx,
                |view, window, cx| view.step_find_command(false, window, cx),
            ))
            .on_mouse_up(
                gpui::MouseButton::Navigate(gpui::NavigationDirection::Back),
                cx.listener(|this, _, window, cx| this.navigate_history(false, window, cx)),
            )
            .on_mouse_up(
                gpui::MouseButton::Navigate(gpui::NavigationDirection::Forward),
                cx.listener(|this, _, window, cx| this.navigate_history(true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &crate::menu::NavigateBack, window, cx| {
                    if window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("Input"))
                    {
                        cx.propagate();
                        return;
                    }
                    this.navigate_history(false, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::menu::FindInView, window, cx| {
                    if matches!(this.shown.route, Route::People { .. })
                        && let Some(people) = &this.people
                    {
                        people.update(cx, |people, cx| people.focus_search(window, cx));
                    } else if this
                        .library
                        .read(cx)
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                    {
                        this.focus_library_search(window, cx);
                    } else if let Some((_, meeting)) = &this.meeting {
                        meeting.update(cx, |view, cx| view.open_find(window, cx));
                    } else if matches!(
                        this.shown.route,
                        Route::Library { .. } | Route::LibraryPerson { .. }
                    ) {
                        this.focus_library_search(window, cx);
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::menu::SearchMeetings, window, cx| {
                    this.go(Route::library(), cx);
                    this.focus_library_search(window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(cx.listener(
                |_: &mut Self, _: &crate::menu::ToggleFullScreen, window, cx| {
                    window.toggle_fullscreen();
                    window.refresh();
                    cx.stop_propagation();
                },
            ))
            .on_action(
                cx.listener(|this, _: &crate::menu::NavigateForward, window, cx| {
                    if window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("Input"))
                    {
                        cx.propagate();
                        return;
                    }
                    this.navigate_history(true, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(move |this, _: &crate::menu::ToggleSidebar, window, cx| {
                    this.toggle_sidebar(narrow, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::menu::ShowMainMenu, window, cx| {
                    this.toggle_main_menu(true, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::menu::FocusNextArea, window, cx| {
                    this.focus_next_area(false, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::menu::FocusPreviousArea, window, cx| {
                    this.focus_next_area(true, window, cx);
                    cx.stop_propagation();
                }),
            )
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if !modifiers.modified()
                    && !crate::menu::has_global_menu_bar(this.host)
                    && this.main_menu.is_none()
                    && this.main_menu_focus.is_focused(window)
                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                {
                    this.toggle_main_menu(true, window, cx);
                    cx.stop_propagation();
                    window.prevent_default();
                    return;
                }
                if event.keystroke.key == "escape" && this.main_menu.is_none() {
                    if this.sidebar_open_narrow {
                        this.toggle_sidebar(true, window, cx);
                        this.sidebar_toggle_focus.focus(window, cx);
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if this.search_expanded
                        && !this.library.read(cx).search_has_focus(window, cx)
                        && !this
                            .library
                            .read(cx)
                            .focus_handle(cx)
                            .contains_focused(window, cx)
                    {
                        this.search_expanded = false;
                        cx.notify();
                    }
                }
            }));
        let body =
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .when(
                    chrome == ToolbarChrome::Client && Self::recording_controls_need_row(window),
                    |body| body.child(self.recording_toolbar_row()),
                )
                .when(
                    Self::search_is_compact(window) && self.search_expanded,
                    |body| {
                        body.child(div().px_3().py_1().child(
                            crate::views::library::LibraryToolbar {
                                library: self.library.clone(),
                                hide_import: true,
                                hide_search: false,
                            },
                        ))
                    },
                )
                .child(crate::views::update::UpdateBanner(
                    crate::views::update::UpdateView::shared(&self.state, cx),
                ))
                .when_some(self.tray_notice, |shell, (id, text)| {
                    shell.child(
                        kit::banner(id, kit::StatusKind::Info, Role::Note, text, cx).child(
                            Button::new("dismiss-tray-notice")
                                .label("Dismiss")
                                .ghost()
                                .self_start()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.tray_notice = None;
                                    cx.notify();
                                })),
                        ),
                    )
                })
                .child(shell);
        if chrome == ToolbarChrome::Client {
            layout.child(chrome::with_toolbar(
                history_destination(&crate::ui_state::Section::of(&self.shown.route).route())
                    .into(),
                toolbar,
                body,
            ))
        } else {
            layout.child(toolbar).child(body)
        }
    }
}

pub const NO_TRAY_TEXT: &str =
    "Your desktop doesn't show tray icons. Recording controls are in this window's toolbar.";
pub(crate) const TRAY_TIP: &str = "BroScribe's icon is under Show hidden icons (^) on the taskbar. To keep it in view, drag it onto the taskbar.";

struct LaunchStatusIconPlace(StatusIconPlace);
impl gpui::Global for LaunchStatusIconPlace {}

pub fn remember_status_icon_place(place: StatusIconPlace, cx: &mut App) {
    cx.set_global(LaunchStatusIconPlace(place));
}

pub(crate) fn status_icon_place(cx: &App) -> StatusIconPlace {
    cx.try_global::<LaunchStatusIconPlace>().map_or_else(
        broscribe_platform_native::NativePlatform::status_icon_place,
        |place| place.0,
    )
}

fn tray_notice(
    place: StatusIconPlace,
    setup_complete: bool,
    data_dir: &std::path::Path,
    cx: &App,
) -> Option<(&'static str, &'static str)> {
    let notice = match place {
        StatusIconPlace::NotificationArea if setup_complete => ("tray-tip", TRAY_TIP),
        StatusIconPlace::NoTrayHost => ("no-tray", NO_TRAY_TEXT),
        _ => return None,
    };
    let already_seen = crate::ui_state::current(cx)
        .is_some_and(|state| state.seen_notices.iter().any(|id| id == notice.0));
    (!already_seen && once::claim(data_dir, notice.0)).then_some(notice)
}

/// The window a launch that cannot start shows (an app opened from the
/// Finder has no visible stderr): what failed, the file or folder involved,
/// what to do, and Quit.
pub struct StartupErrorView {
    failure: StartupFailure,
    on_quit: Rc<dyn Fn(&mut App)>,
    retry_focus: gpui::FocusHandle,
    copied: bool,
}

impl StartupErrorView {
    pub fn new(failure: StartupFailure, on_quit: Rc<dyn Fn(&mut App)>, cx: &App) -> Self {
        Self {
            failure,
            on_quit,
            retry_focus: cx.focus_handle(),
            copied: false,
        }
    }

    pub fn failure(&self) -> &StartupFailure {
        &self.failure
    }

    pub fn copy_details(&self, cx: &mut App) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.failure.support_report()));
    }

    pub fn quit(&self, cx: &mut App) {
        (self.on_quit)(cx);
    }
}

impl Render for StartupErrorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let path = self.failure.path.clone();
        let view = div()
            .id("startup-error")
            .key_context(keymap::STARTUP_ERROR)
            .text_role(TextRole::Body)
            .role(Role::Main)
            .aria_label("BroScribe startup problem")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        gpui::svg()
                            .path(assets::WAVEFORM)
                            .size(gpui::rems(1.25))
                            .text_color(theme.foreground),
                    )
                    .child(title("startup-error-title", self.failure.title())),
            )
            .child(
                // The note is a card on the window's desk.
                kit::card("startup-error-alert", cx)
                    .spoken(
                        Role::Alert,
                        format!("{} {}", self.failure.detail(), self.failure.guidance()),
                    )
                    .child(text_line(
                        "startup-error-details",
                        Role::Label,
                        format!("Details: {}", self.failure.detail()),
                    ))
                    .child(text_line(
                        "startup-error-guidance",
                        Role::Label,
                        self.failure.guidance(),
                    )),
            )
            .when_some(path.clone(), |el, path| {
                el.child(
                    text_line(
                        "startup-error-location",
                        Role::Label,
                        format!("Location: {}", path.display()),
                    )
                    .text_role(TextRole::Mono)
                    .font_family(theme.mono_font_family.clone()),
                )
            })
            .on_action(|_: &keymap::RetryStartup, _, cx| cx.restart())
            .on_action(cx.listener(|this, _: &keymap::QuitFailedStartup, _, cx| this.quit(cx)))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("startup-error-copy")
                            .label("Copy details")
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.copy_details(cx);
                                this.copied = true;
                                cx.notify();
                            })),
                    )
                    .when_some(path, |el, path| {
                        let label = broscribe_platform_native::NativePlatform::host_platform()
                            .reveal_label();
                        el.child(
                            Button::new("startup-error-reveal")
                                .label(label)
                                .outline()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copied = false;
                                    cx.reveal_path(&path);
                                    cx.notify();
                                })),
                        )
                    })
                    .child(kit::confirm_actions(
                        Button::new("startup-error-quit")
                            .label("Quit BroScribe")
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| this.quit(cx))),
                        Button::new("startup-error-retry")
                            .label("Try again")
                            .track_focus(&self.retry_focus)
                            .map(|button| kit::primary(button, false, cx))
                            .on_click(|_, _, cx| cx.restart()),
                        cx,
                    )),
            )
            .when(self.copied, |view| {
                view.child(kit::status_line(
                    "startup-error-copied",
                    kit::StatusKind::Done,
                    "Details copied.",
                    cx,
                ))
            });
        chrome::framed(windows::STARTUP_TITLE, view, window)
    }
}

/// Informational maintenance screen. No normal state, data inventory or
/// operational controls are mounted here.
struct MaintenanceView {
    data_dir: PathBuf,
    copied: bool,
}

impl Render for MaintenanceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        const SUMMARY: &str = "BroScribe is in maintenance mode for this data folder, so the library and recording are unavailable. BroScribe hasn't checked the library, recordings or queue.";
        const RESUME: &str = "To leave maintenance mode, quit BroScribe and BroScribe Agent, then run this command for this data folder:";
        const COMMAND: &str = "broscribe maintenance disable";
        let folder: SharedString = format!("Data folder: {}", self.data_dir.display()).into();
        let view = div()
            .id("maintenance")
            .text_role(TextRole::Body)
            .role(Role::Main)
            .aria_label("BroScribe maintenance")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(title("maintenance-heading", "Maintenance mode"))
            .child(
                // The note is a card on the window's desk.
                kit::card("maintenance-note", cx)
                    .gap_3()
                    .child(text_line("maintenance-summary", Role::Label, SUMMARY))
                    .child(
                        text_line("maintenance-folder", Role::Label, folder)
                            .text_role(TextRole::Mono),
                    )
                    .child(text_line("maintenance-resume", Role::Label, RESUME)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        text_line("maintenance-command", Role::Label, COMMAND)
                            .text_role(TextRole::Mono)
                            .bg(cx.theme().muted)
                            .px_2()
                            .py_1(),
                    )
                    .child(
                        Button::new("maintenance-copy")
                            .label("Copy command")
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    COMMAND.to_owned(),
                                ));
                                this.copied = true;
                                cx.notify();
                            })),
                    ),
            )
            .when(self.copied, |view| {
                view.child(kit::status_line(
                    "maintenance-copied",
                    kit::StatusKind::Done,
                    "Command copied.",
                    cx,
                ))
            })
            .child(text_line(
                "maintenance-after",
                Role::Label,
                "Leaving maintenance mode starts nothing. Open BroScribe again afterwards.",
            ))
            .child(
                Button::new("maintenance-quit")
                    .label("Quit BroScribe")
                    .map(|button| kit::primary(button, false, cx))
                    .on_click(|_, _, cx| cx.quit()),
            );
        chrome::framed(
            windows::title(windows::maintenance_title(
                broscribe_platform_native::NativePlatform::host_platform(),
            )),
            view,
            window,
        )
    }
}

/// The title of the window a second launch shows when the running app
/// didn't come forward (SHL-14).
pub const ALREADY_RUNNING_TITLE: &str = "BroScribe is already running";

/// What that window says: where the running app's icon is, in the
/// platform's own words, or, on a Linux desktop without a tray, what to do
/// instead.
pub fn already_running_text(place: StatusIconPlace) -> &'static str {
    match place {
        StatusIconPlace::MenuBar => {
            "BroScribe couldn't bring its window forward. Open it from the BroScribe icon in the \
             menu bar, or quit BroScribe there and open it again."
        }
        StatusIconPlace::NotificationArea => {
            "BroScribe couldn't bring its window forward. Open it from the BroScribe icon in the \
             notification area (under ^ on the taskbar), or quit BroScribe there and open it \
             again."
        }
        StatusIconPlace::SystemTray => {
            "BroScribe couldn't bring its window forward. Open it from the BroScribe icon in the \
             system tray, or quit BroScribe there and open it again."
        }
        StatusIconPlace::NoTrayHost => {
            "BroScribe is running but didn't respond. Wait a moment, then open BroScribe again. \
             If it still doesn't appear, end broscribe-app in your system monitor, then open \
             BroScribe."
        }
    }
}

/// The notice a second launch shows when the single-instance handoff
/// failed: what happened and one Close button, which quits this launch.
pub struct AlreadyRunningView {
    place: StatusIconPlace,
    on_close: Rc<dyn Fn(&mut App)>,
}

impl AlreadyRunningView {
    pub fn text(&self) -> &'static str {
        already_running_text(self.place)
    }

    pub fn close(&self, cx: &mut App) {
        (self.on_close)(cx);
    }
}

impl Render for AlreadyRunningView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = div()
            .id("already-running")
            .role(Role::Main)
            .aria_label(ALREADY_RUNNING_TITLE)
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(title("already-running-heading", ALREADY_RUNNING_TITLE))
            .child(kit::card("already-running-note", cx).child(text_line(
                "already-running-text",
                Role::Label,
                self.text(),
            )))
            .child(
                div().flex().justify_end().child(
                    Button::new("already-running-close")
                        .label("Close")
                        .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                ),
            );
        chrome::framed(ALREADY_RUNNING_TITLE, view, window)
    }
}

/// Opens the notice a second launch shows when the running app didn't come
/// forward (480×240); `on_close` runs for its Close button.
pub fn open_already_running_window(
    place: StatusIconPlace,
    on_close: impl Fn(&mut App) + 'static,
    cx: &mut App,
) -> gpui::Result<WindowHandle<Root>> {
    let on_close: Rc<dyn Fn(&mut App)> = Rc::new(on_close);
    let handle = cx.open_window(
        fixed_window_options(
            ALREADY_RUNNING_TITLE,
            windows::NOTICE_SIZE,
            windows::NOTICE_SIZE,
            cx,
        ),
        move |window, cx| {
            theme::follow_appearance(window, cx);
            let view = cx.new(|_| AlreadyRunningView { place, on_close });
            cx.new(|cx| {
                cx.observe_window_activation(window, |_, _, cx| {
                    crate::menu::window_activation_changed(cx)
                })
                .detach();
                Root::new(view, window, cx)
            })
        },
    )?;
    windows::track_regular_window(handle.window_id(), cx);
    Ok(handle)
}

/// A launch window that remembers nothing: centred at `default`, at least
/// `min`.
fn fixed_window_options(
    title: &'static str,
    default: (f32, f32),
    min: (f32, f32),
    cx: &App,
) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            windows::px_size(default),
            cx,
        ))),
        window_min_size: Some(windows::px_size(min)),
        titlebar: Some(TitlebarOptions {
            title: Some(windows::title(title).into()),
            ..Default::default()
        }),
        ..crate::ui::assets::window_options()
    }
}

/// Uses the same lightweight startup window as startup failures.
pub fn open_maintenance_window(
    data_dir: PathBuf,
    cx: &mut App,
) -> gpui::Result<WindowHandle<Root>> {
    open_startup_window(
        windows::maintenance_title(broscribe_platform_native::NativePlatform::host_platform()),
        move |_, cx| {
            cx.new(|_| MaintenanceView {
                data_dir,
                copied: false,
            })
        },
        cx,
    )
}

/// Opens the startup error window; `on_quit` runs for its Quit button.
pub fn open_startup_error_window(
    failure: StartupFailure,
    on_quit: impl Fn(&mut App) + 'static,
    cx: &mut App,
) -> gpui::Result<WindowHandle<Root>> {
    keymap::bind_startup_error(cx);
    let on_quit: Rc<dyn Fn(&mut App)> = Rc::new(on_quit);
    open_startup_window(
        windows::STARTUP_TITLE,
        move |window, cx| {
            cx.new(|cx| {
                let view = StartupErrorView::new(failure, on_quit, cx);
                view.retry_focus.focus(window, cx);
                view
            })
        },
        cx,
    )
}

fn open_startup_window<V: Render + 'static>(
    title: &'static str,
    view: impl FnOnce(&mut Window, &mut App) -> Entity<V> + 'static,
    cx: &mut App,
) -> gpui::Result<WindowHandle<Root>> {
    let handle = cx.open_window(
        fixed_window_options(title, windows::STARTUP_SIZE, windows::STARTUP_MIN, cx),
        |window, cx| {
            theme::follow_appearance(window, cx);
            window.set_window_title(&windows::title(title));
            let view = view(window, cx);
            cx.new(|cx| {
                cx.observe_window_activation(window, |_, _, cx| {
                    crate::menu::window_activation_changed(cx)
                })
                .detach();
                Root::new(view, window, cx)
            })
        },
    )?;
    windows::track_regular_window(handle.window_id(), cx);
    Ok(handle)
}

/// Opens the main window (wrapped in gpui-component's [`Root`], which hosts
/// dialogs and sheets).
///
/// It opens where it was left when that is still on a present display
/// (SHL-39), otherwise centred at [`windows::main_default_size`]; it can't
/// be made smaller than [`windows::MAIN_MIN`] (SHL-11).
pub fn open_main_window(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) -> gpui::Result<WindowHandle<Root>> {
    open_main_window_with_focus(state, deps, true, cx)
}

pub fn open_main_window_at_login(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) -> gpui::Result<WindowHandle<Root>> {
    open_main_window_with_focus(state, deps, false, cx)
}

fn open_main_window_with_focus(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    focus: bool,
    cx: &mut App,
) -> gpui::Result<WindowHandle<Root>> {
    let main_state = state.clone();
    let main_deps = deps.clone();
    let mut options = main_window_options(cx);
    options.focus = focus;
    let handle = cx.open_window(options, |window, cx| {
        theme::follow_appearance(window, cx);
        let view = cx.new(|cx| MainWindow::new(main_state, main_deps, window, cx));
        if focus {
            let view = view.downgrade();
            window.on_next_frame(move |window, cx| {
                let _ = view.update(cx, |view, cx| {
                    view.focus_on_open(window, cx);
                    view.sync_import_dialog(window, cx);
                });
            });
        }
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    windows::track_regular_window(handle.window_id(), cx);
    let first_launch =
        crate::ui_state::current(cx).is_none() || crate::ui_state::claim_notice("first-launch", cx);
    if first_launch && setup::should_show_first_run(state.read(cx).config()) {
        let result = if focus {
            setup_window::show(state, deps, cx)
        } else {
            setup_window::show_unfocused(state, deps, cx)
        };
        if let Err(error) = result {
            tracing::error!(%error, "cannot open the setup assistant");
        }
    }
    Ok(handle)
}

pub fn show_setup_window(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) -> gpui::Result<()> {
    setup_window::show(state, deps, cx).map(|_| ())
}

pub fn show_agent_setup_window(
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) -> gpui::Result<()> {
    setup_window::show(state, deps, cx)?;
    setup_window::request_step(setup::Step::Permissions, cx);
    Ok(())
}

/// Where and how the main window opens.
fn main_window_options(cx: &App) -> WindowOptions {
    let unified = broscribe_platform_native::NativePlatform::host_platform() == HostPlatform::MacOs;
    let (window_bounds, display_id) = match crate::ui_state::main_window_bounds(cx) {
        Some((bounds, display)) => (bounds, Some(display)),
        None => (
            WindowBounds::Windowed(Bounds::centered(
                None,
                windows::main_default_size_here(cx),
                cx,
            )),
            None,
        ),
    };
    WindowOptions {
        window_bounds: Some(window_bounds),
        display_id,
        window_min_size: Some(windows::px_size(windows::MAIN_MIN)),
        app_owns_titlebar_drag: unified,
        titlebar: Some(TitlebarOptions {
            title: Some(windows::title("Library").into()),
            appears_transparent: unified,
            traffic_light_position: unified.then(|| gpui::point(px(20.), px(18.))),
        }),
        ..crate::ui::assets::window_options()
    }
}

/// Brings the main window forward, reopening it if the user closed it. A
/// deliberate open: an accessory launch gets its Dock icon first (SHL-15).
pub fn show_main_window(
    window: &mut Option<WindowHandle<Root>>,
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) {
    windows::activate_for_regular_window(cx);
    let reused = window.as_ref().is_some_and(|w| {
        w.update(cx, |_, window, _| window.activate_window())
            .is_ok()
    });
    if !reused {
        match open_main_window(state, deps, cx) {
            Ok(handle) => *window = Some(handle),
            Err(e) => {
                tracing::error!("cannot open the main window: {e}");
                windows::settle_presence(cx);
            }
        }
    }
}

pub fn focus_main_content(main: WindowHandle<Root>, cx: &mut App) {
    cx.defer(move |cx| {
        let _ = main.update(cx, |root, window, cx| {
            if let Ok(view) = root.view().clone().downcast::<MainWindow>() {
                view.update(cx, |view, cx| view.focus_on_open(window, cx));
            }
        });
    });
}

pub fn main_window_command(
    main: WindowHandle<Root>,
    command: crate::menu::AppCommand,
    cx: &mut App,
) {
    use crate::menu::AppCommand;
    cx.defer(move |cx| {
        let _ = gpui::AnyWindowHandle::from(main).update(cx, |root, window, cx| {
            let Ok(root) = root.downcast::<Root>() else {
                return;
            };
            let Ok(view) = root.read(cx).view().clone().downcast::<MainWindow>() else {
                return;
            };
            view.update(cx, |this, cx| match command {
                AppCommand::Search => {
                    this.go(Route::library(), cx);
                    this.focus_library_search(window, cx);
                }
                AppCommand::Import { folder } if !window.has_active_dialog(cx) => {
                    this.library.update(cx, |library, cx| {
                        if folder {
                            library.choose_folder(cx);
                        } else {
                            library.choose_recordings(cx);
                        }
                    });
                }
                AppCommand::Library | AppCommand::ReviewQueue | AppCommand::People => {
                    let index = match command {
                        AppCommand::Library => 0,
                        AppCommand::ReviewQueue => 1,
                        _ => 2,
                    };
                    this.choose_section(index, false, window, cx);
                    let view = cx.weak_entity();
                    window.on_next_frame(move |window, cx| {
                        let _ = view.update(cx, |this, cx| match index {
                            0 => this.focus_on_open(window, cx),
                            1 => {
                                if let Some(review) = &this.review {
                                    review.focus_handle(cx).focus(window, cx);
                                }
                            }
                            _ => {
                                if let Some(people) = &this.people {
                                    people.focus_handle(cx).focus(window, cx);
                                }
                            }
                        });
                    });
                }
                _ => {}
            });
        });
    });
}

/// Installs the platform [`Announcer`] used to speak live `Status` and `Alert`
/// changes per window (Q1 Option B), and the [`Root`] plugin that marks every
/// draw of every window opened afterwards. Call it before opening windows.
pub fn install_announcer(announcer: Arc<dyn Announcer>, cx: &mut App) {
    cx.set_global(live_announcer::LiveAnnouncer::new(announcer));
    gpui::base::Root::register_plugin::<live_announcer::LiveFrame>(cx, |_, _| {
        live_announcer::LiveFrame
    });
}

/// Speaks the live nodes that changed between two draws of a window.
///
/// Each draw of a window is one comparison: the `Status` and `Alert` nodes
/// drawn in it against those drawn in the window's previous draw. A node
/// whose text is new or changed is announced once; a node that is no longer
/// drawn is forgotten, so the same text appearing again is announced again.
/// The first draw of a window is its baseline and announces nothing.
///
/// Every window opened through [`Root`] after [`install_announcer`] draws a
/// zero-size frame marker (the `LiveFrame` root plugin), so a draw with no
/// live node at all still counts: it clears the previous nodes, and a window
/// whose first draws had none announces its first alert. Nothing here
/// notifies a view or requests a frame; it only follows draws that happen
/// anyway. A window without the marker (a test window not wrapped in `Root`)
/// is compared only on the draws that contain a live node.
///
/// Nodes are keyed by their probe's [`GlobalElementId`], the path of every
/// identified ancestor (views included), so two live nodes sharing a local id
/// under different identified parents keep separate entries. Limits: two live
/// nodes with the same id under the same identified parent share one entry
/// (GPUI's accessibility tree rejects that case too; tests assert, and the
/// app logs a warning, when their words differ in one draw); a node whose id
/// path
/// changes (an index-based id that shifts) counts as a new node; and a node
/// that is not drawn in a draw (scrolled out of a `list`, or inside a
/// `.cached()` view that was reused) counts as gone, so it is announced again
/// when it is next drawn.
mod live_announcer {
    use gpui::BorrowAppContext as _;

    use super::*;

    /// One live node as drawn: its key, priority and text.
    type LiveNode = (GlobalElementId, AnnouncementPriority, SharedString);

    #[derive(Default)]
    struct WindowLiveState {
        /// The window was compared once: later draws announce changes.
        initialized: bool,
        /// Each live node's text as of the window's last comparison.
        previous: HashMap<GlobalElementId, SharedString>,
        /// The live nodes drawn since the last comparison.
        current: Vec<LiveNode>,
        flush_scheduled: bool,
    }

    pub(super) struct LiveAnnouncer {
        announcer: Arc<dyn Announcer>,
        windows: HashMap<WindowId, WindowLiveState>,
    }

    impl Global for LiveAnnouncer {}

    impl LiveAnnouncer {
        pub(super) fn new(announcer: Arc<dyn Announcer>) -> Self {
            Self {
                announcer,
                windows: HashMap::new(),
            }
        }
    }

    /// The [`Root`] plugin whose overlay draws a [`LiveMarker::Frame`] in
    /// every draw of its window.
    pub(super) struct LiveFrame;

    impl Render for LiveFrame {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            LiveMarker::Frame
        }
    }

    impl gpui::base::RootPlugin for LiveFrame {}

    /// A zero-size element that records, while it is prepainted, either one
    /// draw of its window or one live node's text in that draw.
    pub(super) enum LiveMarker {
        Frame,
        Node {
            priority: AnnouncementPriority,
            text: SharedString,
        },
    }

    impl LiveMarker {
        pub(super) fn node(priority: AnnouncementPriority, text: SharedString) -> Self {
            Self::Node { priority, text }
        }
    }

    impl IntoElement for LiveMarker {
        type Element = Self;

        fn into_element(self) -> Self::Element {
            self
        }
    }

    impl Element for LiveMarker {
        type RequestLayoutState = ();
        type PrepaintState = ();

        /// A node's probe is identified so its global id, under the live
        /// element's own id, keys it.
        fn id(&self) -> Option<ElementId> {
            match self {
                Self::Frame => None,
                Self::Node { .. } => Some("live-region".into()),
            }
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(
            &mut self,
            _id: Option<&GlobalElementId>,
            _inspector_id: Option<&InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (LayoutId, Self::RequestLayoutState) {
            let style = Style {
                position: Position::Absolute,
                size: size(px(0.).into(), px(0.).into()),
                ..Default::default()
            };
            (window.request_layout(style, [], cx), ())
        }

        fn prepaint(
            &mut self,
            id: Option<&GlobalElementId>,
            _inspector_id: Option<&InspectorElementId>,
            _bounds: Bounds<Pixels>,
            _request_layout: &mut Self::RequestLayoutState,
            window: &mut Window,
            cx: &mut App,
        ) -> Self::PrepaintState {
            let node = match self {
                Self::Frame => None,
                Self::Node { priority, text } => id.map(|id| (id.clone(), *priority, text.clone())),
            };
            record(window.window_handle().window_id(), node, cx);
        }

        fn paint(
            &mut self,
            _id: Option<&GlobalElementId>,
            _inspector_id: Option<&InspectorElementId>,
            _bounds: Bounds<Pixels>,
            _request_layout: &mut Self::RequestLayoutState,
            _prepaint: &mut Self::PrepaintState,
            _window: &mut Window,
            _cx: &mut App,
        ) {
        }
    }

    /// Records one draw of `window_id` (and, for a probe, its live node) and
    /// schedules that draw's comparison for after it.
    fn record(window_id: WindowId, node: Option<LiveNode>, cx: &mut App) {
        if !cx.has_global::<LiveAnnouncer>() {
            return;
        }
        let schedule = cx.update_global::<LiveAnnouncer, bool>(|state, _| {
            let entry = state.windows.entry(window_id).or_default();
            entry.current.extend(node);
            if entry.flush_scheduled {
                false
            } else {
                entry.flush_scheduled = true;
                true
            }
        });
        if schedule {
            cx.defer(move |cx| flush_window(window_id, cx));
        }
    }

    type PendingAnnouncements = (
        Arc<dyn Announcer>,
        Vec<(SharedString, AnnouncementPriority)>,
    );

    fn flush_window(window_id: WindowId, cx: &mut App) {
        if !cx.has_global::<LiveAnnouncer>() {
            return;
        }
        let open_windows: Vec<WindowId> = cx.windows().into_iter().map(|w| w.window_id()).collect();
        let to_announce: Option<PendingAnnouncements> =
            cx.update_global::<LiveAnnouncer, _>(|state, _| {
                state.windows.retain(|id, _| open_windows.contains(id));
                let entry = state.windows.get_mut(&window_id)?;
                entry.flush_scheduled = false;
                let current = std::mem::take(&mut entry.current);
                let mut next: HashMap<GlobalElementId, SharedString> =
                    HashMap::with_capacity(current.len());
                let mut announcements = Vec::new();
                // One key, one text per draw. Two live nodes with one local
                // id under one identified parent share a key; with different
                // words, every draw would speak both (the later one wins the
                // entry), so they need distinct ids. The same node recorded
                // twice with the same words is fine. Tests assert it; the
                // app only logs it (the element path, never the words), so
                // a debug build never panics in a draw.
                let mut drawn = HashMap::with_capacity(current.len());
                for (id, _, text) in &current {
                    let clash = drawn
                        .insert(id, text)
                        .is_some_and(|earlier| earlier != text);
                    #[cfg(test)]
                    debug_assert!(
                        !clash,
                        "two live nodes share {id:?} with different text in one draw"
                    );
                    #[cfg(not(test))]
                    if clash {
                        tracing::warn!(
                            "two live nodes share {id:?} with different text in one draw"
                        );
                    }
                }
                if entry.initialized {
                    for (id, priority, text) in current {
                        let unchanged = entry.previous.get(&id) == Some(&text);
                        let duplicate_in_frame = next.get(&id) == Some(&text);
                        if !unchanged && !duplicate_in_frame && !text.trim().is_empty() {
                            announcements.push((text.clone(), priority));
                        }
                        next.insert(id, text);
                    }
                } else {
                    for (id, _, text) in current {
                        next.insert(id, text);
                    }
                    entry.initialized = true;
                }
                entry.previous = next;
                Some((state.announcer.clone(), announcements))
            });
        if let Some((announcer, announcements)) = to_announce {
            for (text, priority) in announcements {
                announcer.announce(&text, priority);
            }
        }
    }
}

/// Gives an element its accessible role and the name assistive technology
/// reads for it; the spoken text may differ from what the element shows.
///
/// Name rule: a `Role::Label` carries its text as its `value` only (it is
/// static text: AT-SPI and UI Automation name it from its value, and macOS
/// reads it as AXValue, so a label as well would be read twice); every
/// other role (heading, status, alert, note, …) carries it as its `label`
/// only, which is all those roles are named from.
///
/// Live rule (ACC-01): `Role::Status` sets `Live::Polite` and `Role::Alert`
/// sets `Live::Assertive`; no other role is live. Changes to a live node's
/// text between draws of a window are also forwarded to the platform
/// [`Announcer`] (macOS posts `NSAccessibilityAnnouncementRequested`, while
/// Windows and Linux rely on AccessKit's live-region events); see
/// `live_announcer` for how nodes are compared and its limits.
///
/// GPUI writes a node only for an element with both an ID and a role, so
/// this is for identified elements (a live one needs a non-empty ID); never
/// override the role afterwards.
pub(crate) trait Spoken: StatefulInteractiveElement + ParentElement {
    fn spoken(mut self, role: Role, text: impl Into<SharedString>) -> Self {
        let text = text.into();
        debug_assert!(
            !matches!(role, Role::Status | Role::Alert)
                || self
                    .interactivity()
                    .element_id
                    .as_ref()
                    .is_some_and(|id| !id.to_string().is_empty()),
            "a live {role:?} node needs a non-empty element id"
        );
        let element = self.role(role);
        #[cfg(test)]
        let element = spoken_log::record(element, role, &text);
        let element = match role {
            Role::Status => element.aria_live(gpui::accesskit::Live::Polite),
            Role::Alert => element.aria_live(gpui::accesskit::Live::Assertive),
            _ => element,
        };
        let element = if role == Role::Label {
            element.aria_value(text.clone())
        } else {
            element.aria_label(text.clone())
        };
        match role {
            Role::Status => element.child(live_announcer::LiveMarker::node(
                AnnouncementPriority::Medium,
                text,
            )),
            Role::Alert => element.child(live_announcer::LiveMarker::node(
                AnnouncementPriority::High,
                text,
            )),
            _ => element,
        }
    }
}

impl<E: StatefulInteractiveElement + ParentElement> Spoken for E {}

/// A line of text exposed as itself: `role` named by exactly the text shown.
pub(crate) fn text_line(
    id: impl Into<ElementId>,
    role: Role,
    text: impl Into<SharedString>,
) -> Stateful<Div> {
    let text = text.into();
    div().id(id).spoken(role, text.clone()).child(text)
}

/// A non-live timer (`Role::Timer`) with a stable accessible name (`label`),
/// a changing accessible value (`spoken_value`, e.g. `"8 seconds"`), and
/// visible `shown` text (e.g. `"Saving in 8 s"`). Never live (ACC-18).
pub(crate) fn timer_line(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    spoken_value: impl Into<SharedString>,
    shown: impl Into<SharedString>,
) -> Stateful<Div> {
    let label = label.into();
    let spoken_value = spoken_value.into();
    let shown = shown.into();
    let element = tabular(div())
        .id(id)
        .role(Role::Timer)
        .aria_label(label)
        .child(shown);
    let element = if broscribe_platform_native::NativePlatform::timer_value_uses_description() {
        element.aria_description(spoken_value.clone())
    } else {
        element.aria_value(spoken_value.clone())
    };
    #[cfg(test)]
    let element = spoken_log::record(element, Role::Timer, &spoken_value);
    element
}

pub use format::format_clock;
pub(crate) use format::{spoken_duration, spoken_seconds};

/// A level-two section heading named by its text.
pub(crate) fn heading(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    heading_at(id, text, 2, TextRole::Section)
}

/// An in-section heading, in the smaller Label style.
pub(crate) fn subheading(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    heading_at(id, text, 3, TextRole::Label)
}

fn heading_at(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    level: usize,
    text_role: TextRole,
) -> Stateful<Div> {
    text_line(id, Role::Heading, text)
        .aria_level(level)
        .text_role(text_role)
}

/// A page or window title: a [`heading`] in the display family (Manrope
/// SemiBold) at the largest text size, one per view.
pub(crate) fn title(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Stateful<Div> {
    heading_at(id, text, 1, TextRole::Title)
}

/// What assistive technology gets for `element`, through GPUI's own node
/// writer: its ID, its role and the name it is read by (the `value` of a
/// `Role::Label`, the `label` of every other role); `None` when it gets no
/// node (no ID or no role).
#[cfg(test)]
pub(crate) fn a11y_name<E: gpui::Element>(element: &E) -> Option<(String, Role, String)> {
    let (id, node) = a11y_written(element)?;
    let name = if node.role() == Role::Label {
        node.value()
    } else {
        node.label()
    };
    Some((id, node.role(), name.unwrap_or_default().to_owned()))
}

/// The description GPUI writes for `element` (empty when none). Only the
/// Unix update tests read it.
#[cfg(all(test, unix))]
pub(crate) fn a11y_description<E: gpui::Element>(element: &E) -> String {
    a11y_written(element)
        .and_then(|(_, node)| node.description().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
fn a11y_written<E: gpui::Element>(element: &E) -> Option<(String, gpui::accesskit::Node)> {
    let role = gpui::Element::a11y_role(element)?;
    let id = gpui::Element::id(element)?;
    let mut node = gpui::accesskit::Node::new(role);
    gpui::Element::write_a11y_info(element, &mut node);
    Some((id.to_string(), node))
}

/// A window's accessibility tree as GPUI's node writer built it for its
/// last frame, read through `Window::debug_a11y_tree_json` after
/// `Window::activate_a11y_for_tests` (test windows connect no assistive
/// technology, so they build no tree otherwise). Unlike [`spoken_log`] it
/// holds every node, `.role().aria_label()` ones and gpui-component's
/// included, with its parent.
#[cfg(test)]
pub(crate) mod a11y_tree {
    use gpui::Window;

    /// One node: the element that wrote it, its role (`Debug` of
    /// `accesskit::Role`), the name it is read by and its parent.
    #[derive(Clone, Debug)]
    pub(crate) struct Node {
        pub element_id: Option<String>,
        pub role: String,
        /// Its `label`, else its `value` (a `Role::Label`'s text), else
        /// empty.
        pub name: String,
        pub description: String,
        pub level: Option<u64>,
        /// The actions it supports (`Debug` of `accesskit::Action`).
        pub actions: Vec<String>,
        pub parent: Option<usize>,
        pub children: Vec<usize>,
    }

    pub(crate) struct Tree {
        pub nodes: Vec<Node>,
    }

    /// An element ID as the dump writes it (`Name("tag")`,
    /// `NamedInteger("tag", 0)`) as the app writes it (`tag`, `tag-0`).
    fn shown_id(debug: &str) -> String {
        let quoted = |s: &str| s.trim().trim_matches('"').to_owned();
        if let Some(inner) = debug
            .strip_prefix("Name(")
            .and_then(|s| s.strip_suffix(')'))
        {
            quoted(inner)
        } else if let Some(inner) = debug
            .strip_prefix("NamedInteger(")
            .and_then(|s| s.strip_suffix(')'))
            && let Some((name, ix)) = inner.rsplit_once(',')
        {
            format!("{}-{}", quoted(name), ix.trim())
        } else if let Some(inner) = debug
            .strip_prefix("Integer(")
            .and_then(|s| s.strip_suffix(')'))
        {
            inner.to_owned()
        } else {
            debug.to_owned()
        }
    }

    /// Draws `window` with its accessibility tree built and reads the tree.
    pub(crate) fn draw_and_read(window: &mut Window, cx: &mut gpui::App) -> Tree {
        window.activate_a11y_for_tests();
        window.draw(cx).clear(cx);
        let json = window
            .debug_a11y_tree_json()
            .expect("the frame built an accessibility tree");
        Tree::parse(&json)
    }

    impl Tree {
        pub(crate) fn assert_title(&self, expected: &str) {
            let titles: Vec<_> = self
                .nodes
                .iter()
                .filter(|node| node.level == Some(1))
                .map(|node| (node.role.as_str(), node.element_id.as_deref()))
                .collect();
            assert_eq!(titles, [("Heading", Some(expected))]);
        }

        pub(crate) fn assert_heading(&self, id: &str, level: u64) {
            let node = &self.nodes[self.find(id)];
            assert_eq!(node.role, "Heading");
            assert_eq!(node.level, Some(level), "{id}");
            assert!(!node.name.is_empty(), "{id}");
        }

        fn parse(json: &str) -> Self {
            let value: serde_json::Value = serde_json::from_str(json).expect("tree JSON");
            let map = value["nodes"].as_object().expect("nodes");
            let keys: Vec<&String> = map.keys().collect();
            let index = |key: &str| keys.iter().position(|k| *k == key);
            let mut nodes: Vec<Node> = map
                .values()
                .map(|node| {
                    let aria = &node["aria"];
                    let text = |field: &str| aria[field].as_str().unwrap_or_default().to_owned();
                    let label = text("label");
                    Node {
                        element_id: node["element_id"].as_str().map(shown_id),
                        role: text("role"),
                        name: if label.is_empty() {
                            text("value")
                        } else {
                            label
                        },
                        description: text("description"),
                        level: aria["level"].as_u64(),
                        actions: aria["on_action"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|action| action.as_str().map(str::to_owned))
                            .collect(),
                        parent: None,
                        children: Vec::new(),
                    }
                })
                .collect();
            for (ix, node) in map.values().enumerate() {
                for child in node["children"].as_array().into_iter().flatten() {
                    let child = index(child.as_str().expect("child key")).expect("child node");
                    nodes[child].parent = Some(ix);
                    nodes[ix].children.push(child);
                }
            }
            Self { nodes }
        }

        /// The nodes written by the element `id` (several when views
        /// repeat an ID).
        pub(crate) fn all(&self, id: &str) -> Vec<usize> {
            (0..self.nodes.len())
                .filter(|&ix| self.nodes[ix].element_id.as_deref() == Some(id))
                .collect()
        }

        /// The one node written by the element `id`.
        pub(crate) fn find(&self, id: &str) -> usize {
            match self.all(id).as_slice() {
                [ix] => *ix,
                found => panic!("{} nodes {id}: {self:?}", found.len()),
            }
        }

        /// `(role, name)` of node `ix`.
        pub(crate) fn read(&self, ix: usize) -> (&str, &str) {
            (&self.nodes[ix].role, &self.nodes[ix].name)
        }

        pub(crate) fn parent(&self, ix: usize) -> Option<usize> {
            self.nodes[ix].parent
        }

        /// Node `ix` and every node below it.
        pub(crate) fn subtree(&self, ix: usize) -> Vec<usize> {
            (0..self.nodes.len())
                .filter(|&n| {
                    std::iter::successors(Some(n), |&a| self.nodes[a].parent).any(|a| a == ix)
                })
                .collect()
        }

        /// Nodes under `ix` (not `ix` itself) that have no name. A text
        /// field's own text run is its value and is empty while the field
        /// is; the field is named.
        pub(crate) fn unnamed_under(&self, ix: usize) -> Vec<&Node> {
            self.subtree(ix)
                .into_iter()
                .filter(|&n| n != ix)
                .map(|n| &self.nodes[n])
                .filter(|n| n.name.trim().is_empty())
                .filter(|n| {
                    !(n.role == "TextRun"
                        && n.parent.is_some_and(|p| self.nodes[p].role == "TextInput"))
                })
                .collect()
        }

        /// `(node, ancestor)` pairs under `ix` where a node has the name of
        /// one of the nodes it sits in, which assistive technology would
        /// read twice.
        pub(crate) fn names_repeated_from_ancestors(&self, ix: usize) -> Vec<(&Node, &Node)> {
            let mut out = Vec::new();
            for n in self.subtree(ix) {
                let node = &self.nodes[n];
                if node.name.trim().is_empty() {
                    continue;
                }
                let ancestors = std::iter::successors(node.parent, |&a| self.nodes[a].parent);
                for a in ancestors {
                    if self.nodes[a].name == node.name {
                        out.push((node, &self.nodes[a]));
                    }
                }
            }
            out
        }
    }

    /// The tree in reading order, one indented line per node.
    impl std::fmt::Debug for Tree {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            writeln!(f)?;
            let mut stack: Vec<(usize, usize)> = (0..self.nodes.len())
                .rev()
                .filter(|&ix| self.nodes[ix].parent.is_none())
                .map(|ix| (ix, 0))
                .collect();
            while let Some((ix, depth)) = stack.pop() {
                let node = &self.nodes[ix];
                writeln!(
                    f,
                    "{:indent$}{} {} {:?}",
                    "",
                    node.role,
                    node.element_id.as_deref().unwrap_or("-"),
                    node.name,
                    indent = depth * 2
                )?;
                stack.extend(node.children.iter().rev().map(|&c| (c, depth + 1)));
            }
            Ok(())
        }
    }
}

/// Test-only record of the nodes [`Spoken::spoken`] names while a view
/// builds its elements, so a whole render can be checked for what
/// assistive technology hears (test windows build no accessibility tree).
#[cfg(test)]
pub(crate) mod spoken_log {
    use std::cell::RefCell;

    use gpui::{InteractiveElement, Role, SharedString};

    /// One named node: its ID, role and name.
    pub(crate) type Spoken = (String, Role, String);

    thread_local! {
        static LOG: RefCell<Option<Vec<Spoken>>> = const { RefCell::new(None) };
    }

    pub(super) fn record<E: InteractiveElement>(
        mut element: E,
        role: Role,
        text: &SharedString,
    ) -> E {
        let id = element
            .interactivity()
            .element_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        LOG.with(|log| {
            if let Some(log) = log.borrow_mut().as_mut() {
                log.push((id, role, text.to_string()));
            }
        });
        element
    }

    /// Runs `build` and returns what it built with every node it named.
    pub(crate) fn while_building<R>(build: impl FnOnce() -> R) -> (R, Vec<Spoken>) {
        LOG.with(|log| *log.borrow_mut() = Some(Vec::new()));
        let built = build();
        let named = LOG.with(|log| log.borrow_mut().take()).unwrap_or_default();
        (built, named)
    }

    /// Checks that every node in `named` has an ID and a name, that no two
    /// share an ID, so each text is addressed by its own stable node, and
    /// that no two carry the same text (U08's rule), so none nests a node
    /// with its own words.
    ///
    /// The one exception is a field repeated across rows. `rows` lists the
    /// render's row fields as ID patterns, each a fixed start, the row key
    /// and a fixed end (which may be empty). The row key is `*`, any
    /// non-empty key (`provider-state-*`, `template-*-state`), or the
    /// render's whole row keys listed in braces
    /// (`provider-state-{synthetic|synthetic-agent}`); any other pattern
    /// fails the check. Two nodes may carry the same text only when they
    /// have the same role and one pattern matches both IDs with different
    /// row keys: `provider-state-remote` and `provider-state-synthetic-agent`
    /// may both read "off". As `*` can't tell a row key with `-` parts from
    /// a row key followed by another field, under `*` neither ID may extend
    /// the other with a `-` part (`calendar-google` and
    /// `calendar-google-heading`, even under `calendar-*`); listed keys are
    /// whole, so `provider-state-synthetic` and
    /// `provider-state-synthetic-agent` may share a text under
    /// `provider-state-{synthetic|synthetic-agent}`. Two fields of one row
    /// (`template-1-name`, `template-1-description`), IDs that match no
    /// pattern (`settings-mic-title`, `settings-retention-note`), an empty
    /// row key and nodes of different roles never may.
    pub(crate) fn assert_distinct(named: &[Spoken], rows: &[&str]) {
        if let Err(why) = check_distinct(named, rows) {
            panic!("{why}: {named:?}");
        }
    }

    /// [`assert_distinct`]'s check: what is wrong first, if anything.
    fn check_distinct(named: &[Spoken], rows: &[&str]) -> Result<(), String> {
        let rows = rows
            .iter()
            .map(|pattern| {
                RowPattern::parse(pattern).ok_or_else(|| {
                    format!(
                        "row pattern {pattern} needs a fixed start and one row key, \
                         `*` or `{{key|key}}`"
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut ids = std::collections::BTreeSet::new();
        for (ix, (id, role, name)) in named.iter().enumerate() {
            if id.is_empty() {
                return Err(format!("a {role:?} node without an ID"));
            }
            if name.trim().is_empty() {
                return Err(format!("{id} has no name"));
            }
            if !ids.insert(id.as_str()) {
                return Err(format!("{id} is used twice"));
            }
            for (other, other_role, _) in named[..ix].iter().filter(|(_, _, n)| n == name) {
                if other_role != role || !same_field_in_two_rows(&rows, id, other) {
                    return Err(format!(
                        "{other} ({other_role:?}) and {id} ({role:?}) both read {name:?}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether `id` is `base` followed by more `-` parts.
    fn extends(id: &str, base: &str) -> bool {
        id.strip_prefix(base)
            .is_some_and(|rest| rest.starts_with('-'))
    }

    /// A row field's ID pattern: a fixed start, the row key, a fixed end.
    struct RowPattern<'a> {
        start: &'a str,
        /// The listed whole row keys; `None` for `*`, any non-empty key.
        keys: Option<Vec<&'a str>>,
        end: &'a str,
    }

    impl<'a> RowPattern<'a> {
        /// `start*end` or `start{key|key…}end`, with a non-empty start and
        /// no other `*`, `{`, `}` or `|`.
        fn parse(pattern: &'a str) -> Option<Self> {
            let plain = |s: &str| !s.contains(['*', '{', '}', '|']);
            let (start, keys, end) = match pattern.split_once('{') {
                Some((start, rest)) => {
                    let (keys, end) = rest.split_once('}')?;
                    let keys: Vec<&str> = keys.split('|').collect();
                    if keys.iter().any(|k| k.is_empty() || !plain(k)) {
                        return None;
                    }
                    (start, Some(keys), end)
                }
                None => {
                    let (start, end) = pattern.split_once('*')?;
                    (start, None, end)
                }
            };
            (!start.is_empty() && plain(start) && plain(end)).then_some(Self { start, keys, end })
        }

        /// The row key of `id`, when `id` matches: a non-empty key, and
        /// one of the listed ones when the pattern lists them.
        fn row_key<'id>(&self, id: &'id str) -> Option<&'id str> {
            let key = id
                .strip_prefix(self.start)?
                .strip_suffix(self.end)
                .filter(|key| !key.is_empty())?;
            match &self.keys {
                Some(keys) => keys.contains(&key).then_some(key),
                None => Some(key),
            }
        }
    }

    /// Whether one pattern of `rows` matches both `a` and `b` with different
    /// row keys: the same field in two rows. Under `*`, neither may extend
    /// the other (see [`assert_distinct`]).
    fn same_field_in_two_rows(rows: &[RowPattern<'_>], a: &str, b: &str) -> bool {
        rows.iter().any(|pattern| {
            let whole = pattern.keys.is_some() || !(extends(a, b) || extends(b, a));
            whole
                && matches!(
                    (pattern.row_key(a), pattern.row_key(b)),
                    (Some(x), Some(y)) if x != y
                )
        })
    }

    fn label(id: &str, name: &str) -> Spoken {
        (id.to_owned(), Role::Label, name.to_owned())
    }

    #[test]
    fn the_same_field_may_repeat_its_text_across_rows() {
        assert_distinct(
            &[
                label("provider-state-synthetic-agent", "off"),
                label("provider-state-remote", "off"),
                label("template-1-state", "Generated"),
                label("template-12-state", "Generated"),
            ],
            &["provider-state-*", "template-*-state"],
        );
    }

    /// Even a pattern matching both (`calendar-*`, row keys `google` and
    /// `google-heading`) never lets a node repeat the text of the node its
    /// ID extends.
    #[test]
    #[should_panic(expected = "both read")]
    fn a_node_never_repeats_the_text_of_the_node_it_extends() {
        assert_distinct(
            &[
                label("calendar-google", "Google"),
                label("calendar-google-heading", "Google"),
            ],
            &["calendar-*"],
        );
    }

    /// Listed row keys are whole: provider IDs with `-` parts, one
    /// extending the other, may share a field's text. A key the pattern
    /// doesn't list matches nothing.
    #[test]
    fn listed_row_keys_are_whole() {
        let rows = ["provider-state-{synthetic|synthetic-agent}"];
        let pair = [
            label("provider-state-synthetic", "off"),
            label("provider-state-synthetic-agent", "off"),
        ];
        assert_eq!(check_distinct(&pair, &rows), Ok(()));
        // Under `*` the same pair can't be told from a row and a field
        // after it, and is refused.
        let why = check_distinct(&pair, &["provider-state-*"]).unwrap_err();
        assert!(why.contains("both read"), "{why}");
        // An unlisted key: no match, refused.
        let why = check_distinct(
            &[
                label("provider-state-synthetic", "off"),
                label("provider-state-synthetic-heading", "off"),
            ],
            &rows,
        )
        .unwrap_err();
        assert!(why.contains("both read"), "{why}");
        // Two fields of one listed row, as with `*`.
        let why = check_distinct(
            &[
                label("provider-state-synthetic", "off"),
                label("provider-key-state-synthetic", "off"),
            ],
            &[
                "provider-state-{synthetic|synthetic-agent}",
                "provider-key-state-{synthetic|synthetic-agent}",
            ],
        )
        .unwrap_err();
        assert!(why.contains("both read"), "{why}");
    }

    /// `*` stands for a non-empty row key: an ID with nothing in its place
    /// is no row of that field.
    #[test]
    fn an_empty_row_key_is_no_row() {
        for (pattern, empty, keyed) in [
            (
                "provider-state-*",
                "provider-state-",
                "provider-state-remote",
            ),
            ("template-*-state", "template--state", "template-1-state"),
        ] {
            let why = check_distinct(&[label(empty, "off"), label(keyed, "off")], &[pattern])
                .unwrap_err();
            assert!(why.contains("both read"), "{pattern}: {why}");
        }
    }

    #[test]
    fn only_one_field_in_two_rows_may_share_a_text() {
        let rows = [
            "template-*-name",
            "template-*-description",
            "provider-name-*",
            "provider-state-*",
        ];
        let refused = |a: Spoken, b: Spoken| {
            let why = check_distinct(&[a, b], &rows).unwrap_err();
            assert!(why.contains("both read"), "{why}");
        };
        // Two fields of the same row.
        refused(
            label("template-1-name", "Synthetic"),
            label("template-1-description", "Synthetic"),
        );
        refused(
            label("provider-name-x", "x"),
            label("provider-state-x", "x"),
        );
        // Unrelated IDs, which no pattern matches.
        refused(
            label("settings-mic-title", "Microphone"),
            label("settings-retention-note", "Microphone"),
        );
        // Without the pattern, the same field across rows is refused too.
        assert!(
            check_distinct(
                &[
                    label("provider-key-state-a", "no API key"),
                    label("provider-key-state-b", "no API key"),
                ],
                &rows,
            )
            .is_err()
        );
        // The same field across rows, but in another role.
        refused(
            label("provider-state-a", "off"),
            ("provider-state-b".into(), Role::Status, "off".into()),
        );
        // A pattern needs a fixed start and exactly one row key, `*` or a
        // list of non-empty keys.
        for bad in [
            "*-state",
            "provider-state",
            "provider-*-state-*",
            "{a|b}-state",
            "provider-state-{}",
            "provider-state-{a|}",
            "provider-state-{a",
            "provider-state-{a}}",
            "provider-*-{a|b}",
            "provider-{a|b}-*",
            "provider-state-a|b",
        ] {
            assert!(check_distinct(&[], &[bad]).is_err(), "{bad}");
        }
        for good in [
            "provider-state-*",
            "template-*-state",
            "provider-{a|b-c}-state",
        ] {
            assert_eq!(check_distinct(&[], &[good]), Ok(()), "{good}");
        }
    }

    /// The role and name of the node `id` in `named`; panics when it is
    /// missing.
    pub(crate) fn node<'a>(named: &'a [Spoken], id: &str) -> (Role, &'a str) {
        named
            .iter()
            .find(|(i, ..)| i == id)
            .map(|(_, role, name)| (*role, name.as_str()))
            .unwrap_or_else(|| panic!("no node {id} in {named:?}"))
    }

    /// Whether `named` has a node `id`.
    pub(crate) fn has(named: &[Spoken], id: &str) -> bool {
        named.iter().any(|(i, ..)| i == id)
    }

    /// Checks that every string in `shown` is exactly the name of a node in
    /// `named`, so each text a render shows is read as written.
    pub(crate) fn assert_shown(named: &[Spoken], shown: &[&str]) {
        for text in shown {
            assert!(
                named.iter().any(|(_, _, name)| name == text),
                "{text:?} is not a node's name: {named:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    mod copy_gates;

    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use broscribe_core::Library;
    use broscribe_core::config::Config;
    use broscribe_platform::fakes::{FakeMicCapture, FakePaths, FakeSystemInfo};
    use broscribe_platform::resolve_paths;
    use gpui::TestAppContext;

    use super::*;

    use crate::views::setup::ASSISTANT_DONE;

    #[gpui::test]
    fn leaving_a_meeting_drops_its_view(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("lib")).unwrap();
        let mut config = Config::default();
        config.setup.completed_steps.push(ASSISTANT_DONE.into());
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    config,
                    library,
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| MainWindow::new(state.clone(), deps, window, cx));
        let id = MeetingId::parse("m-0000a1").unwrap();
        state.update(cx, |s, cx| s.navigate(Route::meeting(id.clone()), cx));
        cx.run_until_parked();
        let weak = window
            .update(cx, |this, _, cx| {
                assert_eq!(cx.windows().len(), 1);
                this.meeting_view().unwrap().downgrade()
            })
            .unwrap();
        state.update(cx, |s, cx| s.navigate(Route::library(), cx));
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| assert!(this.meeting_view().is_none()))
            .unwrap();
        assert!(
            weak.upgrade().is_none(),
            "the meeting view outlived its route"
        );
    }

    #[gpui::test]
    fn footer_names_jobs_by_what_they_do(cx: &mut TestAppContext) {
        use crate::generation::{GenerationTarget, WatchedJob};
        let dir = tempfile::tempdir().unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        state.update(cx, |s, cx| s.set_local_jobs_allowed(false, cx));
        let m = MeetingId::parse("m-0000a1").unwrap();
        let target = GenerationTarget::Template {
            id: "synthetic-notes".into(),
            name: "Synthetic notes".into(),
        };
        let job = target.job_id(&m);
        state.update(cx, |s, cx| {
            s.watch_generation(
                job.clone(),
                WatchedJob {
                    generation_run: None,
                    meeting_id: m.clone(),
                    target,
                    baseline: None,
                    failures_before: 0,
                },
                cx,
            )
        });
        state.read_with(cx, |s, _| {
            assert_eq!(job_label(s, "j", Some("enrich")), "Generating summary");
            assert_eq!(
                job_label(s, "j", Some("refresh-vectors")),
                "Refreshing search"
            );
            assert_eq!(
                job_label(s, job.as_str(), Some("template")),
                "Running Synthetic notes"
            );
            assert_eq!(job_label(s, "other", Some("template")), "Running template");
            assert_eq!(job_label(s, "j", None), "Background job");
        });
    }

    #[gpui::test]
    fn the_shell_repaints_only_when_what_it_shows_changes(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.setup.completed_steps.push(ASSISTANT_DONE.into());
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    config,
                    Library::open(dir.path().join("lib")).unwrap(),
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| MainWindow::new(state.clone(), deps, window, cx));
        let shell = window.update(cx, |_, _, cx| cx.entity()).unwrap();
        let repaints = Rc::new(std::cell::Cell::new(0));
        let _watch = cx.update(|cx| {
            let repaints = repaints.clone();
            cx.observe(&shell, move |_, _| repaints.set(repaints.get() + 1))
        });
        cx.run_until_parked();
        repaints.set(0);

        // A state change the shell does not show (as a progress event that
        // changes no line) repaints nothing.
        state.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(repaints.get(), 0);

        state.update(cx, |s, cx| s.navigate(Route::people(), cx));
        cx.run_until_parked();
        assert!(repaints.get() > 0, "a new route is shown");
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.shown.route, Route::people())
            })
            .unwrap();
    }

    fn bounds_of(window: WindowHandle<Root>, cx: &mut TestAppContext) -> WindowBounds {
        window
            .update(cx, |_, window, _| window.window_bounds())
            .unwrap()
    }

    /// SHL-11, SHL-39: the main window opens centred at its default size,
    /// can't be made smaller than 600×420, and next time opens where it was
    /// left (bounds and section), unless that is no longer on a display.
    #[gpui::test]
    fn the_main_window_opens_where_it_was_left_or_at_its_default(cx: &mut TestAppContext) {
        use crate::ui_state::{FILE_NAME, Section, UiState};

        let dir = tempfile::tempdir().unwrap();
        cx.update(gpui::init);
        let state = shell_state(cx, dir.path());
        let data_dir = state.read_with(cx, |state, _| state.paths().data_dir.clone());
        std::fs::create_dir_all(&data_dir).unwrap();
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(|cx| crate::ui_state::install(&data_dir, cx));

        // Nothing remembered: 1120×720 (the 1920×1080 test display is big
        // enough), centred, with its minimum.
        let options = cx.update(|cx| main_window_options(cx));
        assert_eq!(options.window_min_size, Some(size(px(600.), px(420.))));
        let default = Bounds::new(gpui::point(px(400.), px(180.)), size(px(1120.), px(720.)));
        assert_eq!(options.window_bounds, Some(WindowBounds::Windowed(default)));
        let window = cx.update(|cx| open_main_window(&state, &deps, cx)).unwrap();
        assert_eq!(bounds_of(window, cx), WindowBounds::Windowed(default));

        // Resized, on People, then closed: written for the next launch.
        cx.simulate_window_resize(window.into(), size(px(900.), px(600.)));
        state.update(cx, |state, cx| state.navigate(Route::people(), cx));
        cx.run_until_parked();
        window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        let saved = UiState::load(&data_dir.join(FILE_NAME))
            .main_window
            .expect("the main window was remembered");
        assert_eq!(saved.section(), Some(Section::People));
        assert!(saved.sidebar_visible);
        let left = Bounds::new(gpui::point(px(400.), px(180.)), size(px(900.), px(600.)));
        assert_eq!(
            saved.window.as_ref().unwrap().window_bounds(),
            WindowBounds::Windowed(left)
        );

        // The next launch reads it back: the same bounds and section.
        cx.update(|cx| crate::ui_state::install(&data_dir, cx));
        assert_eq!(
            cx.update(|cx| crate::ui_state::main_window_section(cx)),
            Some(Section::People)
        );
        let window = cx.update(|cx| open_main_window(&state, &deps, cx)).unwrap();
        assert_eq!(bounds_of(window, cx), WindowBounds::Windowed(left));
        window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();

        // Left where no display is now, or on a display that is gone: the
        // default again.
        let key = cx.update(|cx| crate::ui_state::displays(cx)[0].0.key.clone());
        for (x, display) in [(1821., key.as_str()), (400., "uuid:synthetic-gone")] {
            std::fs::write(
                data_dir.join(FILE_NAME),
                format!(
                    r#"{{"main_window":{{"window":{{"mode":"windowed","frame":{{"x":{x},"y":180,"width":900,"height":600}},"display":"{display}"}}}}}}"#
                ),
            )
            .unwrap();
            cx.update(|cx| crate::ui_state::install(&data_dir, cx));
            let window = cx.update(|cx| open_main_window(&state, &deps, cx)).unwrap();
            assert_eq!(bounds_of(window, cx), WindowBounds::Windowed(default));
            window
                .update(cx, |_, window, _| window.remove_window())
                .unwrap();
            cx.run_until_parked();
        }
    }

    /// SHL-11: the startup error and maintenance windows open at 560×320
    /// and keep at least 480×280; the already-running notice is 480×240.
    #[gpui::test]
    fn launch_windows_have_their_default_and_minimum_sizes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let startup = fixed_window_options(
                windows::STARTUP_TITLE,
                windows::STARTUP_SIZE,
                windows::STARTUP_MIN,
                cx,
            );
            assert_eq!(startup.window_min_size, Some(size(px(480.), px(280.))));
            assert!(matches!(
                startup.window_bounds,
                Some(WindowBounds::Windowed(bounds)) if bounds.size == size(px(560.), px(320.))
            ));
            let notice = fixed_window_options(
                ALREADY_RUNNING_TITLE,
                windows::NOTICE_SIZE,
                windows::NOTICE_SIZE,
                cx,
            );
            assert_eq!(notice.window_min_size, Some(size(px(480.), px(240.))));
            assert!(matches!(
                notice.window_bounds,
                Some(WindowBounds::Windowed(bounds)) if bounds.size == size(px(480.), px(240.))
            ));
            assert_eq!(
                notice
                    .titlebar
                    .and_then(|titlebar| titlebar.title)
                    .as_deref(),
                Some(windows::title(ALREADY_RUNNING_TITLE).as_str())
            );
        });
    }

    /// SHL-14: when the handoff fails, the second launch says where the
    /// running app's icon is, in each platform's words, or what to do on a
    /// Linux desktop without a tray; Close ends that launch.
    #[gpui::test]
    fn the_already_running_notice_says_where_the_app_is(cx: &mut TestAppContext) {
        for (place, says) in [
            (StatusIconPlace::MenuBar, "in the menu bar"),
            (
                StatusIconPlace::NotificationArea,
                "in the notification area (under ^ on the taskbar)",
            ),
            (StatusIconPlace::SystemTray, "in the system tray"),
        ] {
            let text = already_running_text(place);
            assert!(
                text.starts_with(
                    "BroScribe couldn't bring its window forward. Open it from the BroScribe icon "
                ),
                "{text}"
            );
            assert!(text.contains(says), "{text}");
            assert!(text.ends_with("or quit BroScribe there and open it again."));
            for word in ["tray icon", "status icon"] {
                assert!(!text.contains(word), "{text}");
            }
        }
        assert_eq!(
            already_running_text(StatusIconPlace::NoTrayHost),
            "BroScribe is running but didn't respond. Wait a moment, then open BroScribe again. \
             If it still doesn't appear, end broscribe-app in your system monitor, then open \
             BroScribe."
        );

        cx.update(gpui::init);
        let closed = Rc::new(std::cell::Cell::new(0));
        let window = cx
            .update(|cx| {
                let closed = closed.clone();
                open_already_running_window(
                    StatusIconPlace::SystemTray,
                    move |_| closed.set(closed.get() + 1),
                    cx,
                )
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            bounds_of(window, cx),
            WindowBounds::Windowed(Bounds::new(
                gpui::point(px(720.), px(420.)),
                size(px(480.), px(240.))
            ))
        );
        let notice = window
            .update(cx, |root, _, _| {
                root.view()
                    .clone()
                    .downcast::<AlreadyRunningView>()
                    .unwrap()
            })
            .unwrap();
        notice.read_with(cx, |notice, _| {
            assert_eq!(
                notice.text(),
                already_running_text(StatusIconPlace::SystemTray)
            )
        });
        notice.update(cx, |notice, cx| notice.close(cx));
        assert_eq!(closed.get(), 1);
    }

    #[gpui::test]
    fn startup_actions_follow_platform_order_and_the_default_and_escape_keys(
        cx: &mut TestAppContext,
    ) {
        use crate::launch::StartupStage;
        use broscribe_platform::{DialogButtonOrder, fakes::FakeDialogButtonOrder};

        cx.update(theme::init);
        for order in [
            DialogButtonOrder::CancelFirst,
            DialogButtonOrder::ActionFirst,
        ] {
            let override_order = FakeDialogButtonOrder::new(order);
            let private =
                "/synthetic/Example Person/test@example.invalid/token-synthetic/config.toml";
            let failure = StartupFailure::new(StartupStage::Config, Some(private.into()));
            let quits = Rc::new(std::cell::Cell::new(0));
            let window = cx.update(|cx| {
                let quits = quits.clone();
                open_startup_error_window(failure, move |_| quits.set(quits.get() + 1), cx).unwrap()
            });
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            visual.run_until_parked();
            visual.update(|window, cx| {
                let tree = a11y_tree::draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("startup-error-location")).1,
                    format!("Location: {private}")
                );
                assert!(
                    tree.read(tree.find("startup-error-details"))
                        .1
                        .starts_with("Details: The settings file")
                );
            });
            let copy = job_button(&mut visual, "Copy details").unwrap();
            let quit = job_button(&mut visual, "Quit BroScribe").unwrap();
            let retry = job_button(&mut visual, "Try again").unwrap();
            assert!(copy.origin.x < quit.origin.x.min(retry.origin.x));
            assert_eq!(
                retry.origin.x < quit.origin.x,
                order == DialogButtonOrder::ActionFirst
            );
            visual.simulate_click(copy.center(), Default::default());
            assert!(job_button(&mut visual, "Copy details").is_some());
            visual.update(|window, cx| {
                let tree = a11y_tree::draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("startup-error-copied")),
                    ("Status", "Details copied.")
                );
            });
            let copied = visual
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap();
            assert!(copied.starts_with("Couldn't read BroScribe's settings."));
            assert!(copied.contains("Location: Settings file"));
            for value in [
                "Example Person",
                "test@example.invalid",
                "token-synthetic",
                "/synthetic/",
            ] {
                assert!(!copied.contains(value));
            }

            let mut clicked = cx.expect_restart();
            visual.simulate_click(retry.center(), Default::default());
            assert!(
                clicked.try_recv().unwrap().is_some(),
                "primary button requests restart"
            );
            let mut entered = cx.expect_restart();
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(
                    window.focused(cx).is_some(),
                    "the startup window gives its default action focus"
                );
                window.focus_next(cx);
                window.draw(cx).clear(cx);
            });
            visual.simulate_keystrokes("enter");
            assert!(
                entered.try_recv().unwrap().is_some(),
                "Enter requests restart with a button focused"
            );
            visual.simulate_keystrokes("escape");
            assert_eq!(quits.get(), 1);
            drop(override_order);
        }
    }

    #[gpui::test]
    fn maintenance_copies_the_command_without_the_explanatory_text(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            theme::init(cx);
            open_maintenance_window("/synthetic/data".into(), cx).unwrap()
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("maintenance-command")),
                ("Label", "broscribe maintenance disable")
            );
            assert!(
                tree.read(tree.find("maintenance-resume"))
                    .1
                    .ends_with("run this command for this data folder:")
            );
            assert!(
                !tree
                    .read(tree.find("maintenance-summary"))
                    .1
                    .contains("broscribe maintenance disable")
            );
        });
        let button = job_button(&mut visual, "Copy command").unwrap();
        visual.simulate_click(button.center(), Default::default());
        assert_eq!(
            visual
                .read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("broscribe maintenance disable")
        );
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("maintenance-copied")),
                ("Status", "Command copied.")
            );
        });
    }

    pub(super) fn shell_state(cx: &mut TestAppContext, dir: &std::path::Path) -> Entity<AppState> {
        let mut config = Config::default();
        config.setup.completed_steps.push(ASSISTANT_DONE.into());
        cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir)).unwrap(),
                    config,
                    Library::open(dir.join("lib")).unwrap(),
                    dir.join("no-such-cli"),
                ),
                cx,
            )
        })
    }

    #[gpui::test]
    fn tray_banners_follow_the_watcher_and_setup_and_stay_dismissed_across_windows(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::fakes::FakeStatusIconPlace;

        cx.update(theme::init);
        for (place, id, copy) in [
            (StatusIconPlace::NotificationArea, "tray-tip", TRAY_TIP),
            (StatusIconPlace::SystemTray, "no-tray", NO_TRAY_TEXT),
            (StatusIconPlace::NoTrayHost, "no-tray", NO_TRAY_TEXT),
            (StatusIconPlace::MenuBar, "no-tray", NO_TRAY_TEXT),
        ] {
            let _place = FakeStatusIconPlace::new(place);
            let dir = tempfile::tempdir().unwrap();
            let state = shell_state(cx, dir.path());
            let data = state.read_with(cx, |state, _| state.paths().data_dir.clone());
            let (config, ()) = Config::update(&data, |config| {
                config.setup.defer_assistant = true;
                Ok(())
            })
            .unwrap();
            state.update(cx, |state, cx| state.set_config(config, cx));
            let deps = SetupDeps {
                mic: Arc::new(FakeMicCapture::new(16_000)),
                system: Arc::new(FakeSystemInfo::default()),
            };
            let open = |cx: &mut TestAppContext| {
                cx.add_window(|window, cx| MainWindow::new(state.clone(), deps.clone(), window, cx))
            };
            let window = open(cx);
            let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
            visual.update(|window, cx| {
                let tree = a11y_tree::draw_and_read(window, cx);
                assert_eq!(
                    !tree.all(id).is_empty(),
                    place == StatusIconPlace::NoTrayHost
                );
            });
            let (config, ()) = Config::update(&data, |config| {
                config.setup.completed_steps.push(ASSISTANT_DONE.into());
                Ok(())
            })
            .unwrap();
            state.update(cx, |state, cx| state.set_config(config, cx));
            visual.run_until_parked();
            let visible = matches!(
                place,
                StatusIconPlace::NotificationArea | StatusIconPlace::NoTrayHost
            );
            visual.update(|window, cx| {
                let tree = a11y_tree::draw_and_read(window, cx);
                if visible {
                    assert_eq!(tree.read(tree.find(id)), ("Note", copy));
                } else {
                    assert!(tree.all(id).is_empty());
                }
            });
            if visible {
                let dismiss = job_button(&mut visual, "Dismiss").unwrap();
                visual.simulate_click(dismiss.center(), Default::default());
                visual.update(|window, cx| {
                    assert!(a11y_tree::draw_and_read(window, cx).all(id).is_empty())
                });
                assert!(data.join("state").join(format!("{id}-shown")).is_file());
                let reopened = open(cx);
                cx.update_window(reopened.into(), |_, window, cx| {
                    assert!(a11y_tree::draw_and_read(window, cx).all(id).is_empty())
                })
                .unwrap();
            }
        }
    }

    #[gpui::test]
    fn shell_toolbar_and_library_each_paint_their_own_primary(cx: &mut TestAppContext) {
        cx.update(crate::ui::theme::init);
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let window = cx.add_window(|window, cx| MainWindow::new(state, deps, window, cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                view.recording_control
                    .update(cx, |control, cx| control.mark_connected(cx));
            })
            .unwrap();
        let mut painted = cx
            .update_window(window.into(), |_, window, cx| {
                kit::painted_primary_actions(window, cx)
            })
            .unwrap();
        painted.sort();
        assert_eq!(painted, ["library-choose", "toolbar-start"]);
    }

    #[gpui::test]
    fn toolbar_main_menu_uses_f10_and_restores_focus(cx: &mut TestAppContext) {
        toolbar_main_menu_for_host(cx, HostPlatform::Windows, gpui::OperatingSystem::Windows);
    }

    #[gpui::test]
    fn linux_main_menu_uses_f10_and_restores_focus(cx: &mut TestAppContext) {
        toolbar_main_menu_for_host(cx, HostPlatform::Linux, gpui::OperatingSystem::Linux);
    }

    fn toolbar_main_menu_for_host(
        cx: &mut TestAppContext,
        host: HostPlatform,
        os: gpui::OperatingSystem,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        cx.set_platform_info(gpui::PlatformInfo {
            os,
            target: gpui::PlatformTarget::Native,
        });
        cx.update(|cx| {
            crate::ui::theme::init(cx);
            cx.bind_keys(crate::ui::keymap::key_bindings_for(host, cx));
        });
        let main_slot: Rc<std::cell::RefCell<Option<Entity<MainWindow>>>> = Rc::default();
        let slot = main_slot.clone();
        let (_, visual) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| {
                let mut main = MainWindow::new(
                    state,
                    SetupDeps {
                        mic: Arc::new(FakeMicCapture::new(16_000)),
                        system: Arc::new(FakeSystemInfo::default()),
                    },
                    window,
                    cx,
                );
                main.host = host;
                main
            });
            *slot.borrow_mut() = Some(view.clone());
            Root::new(view, window, cx)
        });
        let main = main_slot.borrow().clone().unwrap();
        let focus = visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("main-toolbar")),
                ("Toolbar", "BroScribe controls")
            );
            let menu = gpui::base::test_support::snapshots(window);
            assert!(menu.iter().any(|node| node.label() == Some("Main menu")));
            let focus = main.read(cx).toolbar_focus.clone();
            focus.focus(window, cx);
            focus
        });
        visual.simulate_keystrokes("f10");
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            for label in [
                "Import recordings…",
                "Import folder…",
                "Search meetings",
                "Show recording controls",
                "Settings",
                "Setup assistant",
                "Check for updates",
                "Larger text",
                "Smaller text",
                "Standard text size",
                "Keyboard shortcuts",
                "About BroScribe",
                "Quit BroScribe",
            ] {
                assert!(tree.nodes.iter().any(|node| node.name == label), "{label}");
            }
            assert!(!focus.is_focused(window));
        });
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            assert!(!tree.nodes.iter().any(|node| node.name == "Quit BroScribe"));
            assert!(focus.is_focused(window));
        });
        visual.simulate_keystrokes("f10 f10");
        visual.update(|window, _| assert!(focus.is_focused(window)));
        visual.simulate_keystrokes("f10 ctrl-.");
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            assert!(!tree.nodes.iter().any(|node| node.name == "Quit BroScribe"));
            assert!(focus.is_focused(window));
        });
    }

    #[gpui::test]
    fn search_meetings_opens_library_search_from_other_routes_at_both_widths(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        split_meeting(&state, "Synthetic calendar review", cx);
        let main = cx.update(|cx| {
            crate::ui::theme::init(cx);
            open_main_window_at_login(
                &state,
                &SetupDeps {
                    mic: Arc::new(FakeMicCapture::new(16_000)),
                    system: Arc::new(FakeSystemInfo::default()),
                },
                cx,
            )
            .unwrap()
        });
        for (width, from) in [(700., Route::people()), (1120., Route::ReviewQueue)] {
            cx.simulate_window_resize(main.into(), size(px(width), px(720.)));
            state.update(cx, |state, cx| state.navigate(from, cx));
            cx.run_until_parked();
            cx.update(|cx| main_window_command(main, crate::menu::AppCommand::Search, cx));
            cx.run_until_parked();
            assert_eq!(
                state.read_with(cx, |state, _| state.route().clone()),
                Route::library()
            );
            cx.update_window(main.into(), |root, window, cx| {
                let root = root.downcast::<Root>().unwrap();
                let view = root
                    .read(cx)
                    .view()
                    .clone()
                    .downcast::<MainWindow>()
                    .unwrap();
                assert!(view.read(cx).search_expanded);
                window.draw(cx).clear(cx);
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(main.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                assert!(
                    window
                        .context_stack()
                        .iter()
                        .any(|context| context.contains("Input")),
                    "the search input has focus at {width}"
                );
            })
            .unwrap();
        }
    }

    #[gpui::test]
    fn sidebar_names_its_selected_section_and_offers_setup_above_settings(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let data = state.read_with(cx, |state, _| state.paths().data_dir.clone());
        let config = Config::update(&data, |config| {
            config.setup.defer_assistant = true;
            config.setup.completed_steps =
                vec!["languages".into(), "models".into(), "permissions".into()];
            Ok(())
        })
        .unwrap()
        .0;
        state.update(cx, |state, cx| state.set_config(config, cx));
        cx.update(crate::ui::theme::init);
        let (main, visual) = cx.add_window_view(|window, cx| {
            MainWindow::new(
                state.clone(),
                SetupDeps {
                    mic: Arc::new(FakeMicCapture::new(16_000)),
                    system: Arc::new(FakeSystemInfo::default()),
                },
                window,
                cx,
            )
        });
        let names = visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            let sidebar = tree.find("sidebar");
            let sections = tree.find("sidebar-sections");
            assert_eq!(tree.parent(sections), Some(sidebar));
            assert_eq!(tree.nodes[sections].name, "Main sections");
            tree.nodes[sections]
                .children
                .iter()
                .chain(
                    tree.nodes[sidebar]
                        .children
                        .iter()
                        .filter(|index| **index != sections),
                )
                .map(|&index| tree.nodes[index].name.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            names,
            [
                "Library",
                "Review queue",
                "People",
                "Finish setup · 2 of 4",
                "Settings"
            ]
        );
        for id in ["nav-library-icon", "nav-review-icon", "nav-people-icon"] {
            let icon = visual.debug_bounds(id).expect(id);
            assert_eq!(icon.size, size(px(16.), px(16.)));
        }
        visual.update(|window, cx| {
            main.update(cx, |main, cx| main.sidebar_rows[0].focus(window, cx));
        });
        visual.simulate_keystrokes("down end home");
        let setup = visual.debug_bounds("finish-setup-hit-area").unwrap();
        visual.simulate_click(setup.center(), gpui::Modifiers::default());
        visual.run_until_parked();
        assert!(!Config::load(&data).unwrap().setup.defer_assistant);
        assert!(main.read_with(visual, |main, _| main.shown.setup_progress.is_none()));
    }

    pub(super) fn split_shell<'a>(
        state: &Entity<AppState>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<MainWindow>, &'a mut gpui::VisualTestContext) {
        let state = state.clone();
        cx.update(|cx| {
            theme::init(cx);
            cx.bind_keys(crate::menu::key_bindings(cx));
        });
        cx.add_window_view(move |window, cx| {
            MainWindow::new(
                state,
                SetupDeps {
                    mic: Arc::new(FakeMicCapture::new(16_000)),
                    system: Arc::new(FakeSystemInfo::default()),
                },
                window,
                cx,
            )
        })
    }

    pub(super) fn split_meeting(
        state: &Entity<AppState>,
        title: &str,
        cx: &TestAppContext,
    ) -> MeetingId {
        let library = state.read_with(cx, |state, _| state.library().clone());
        let mut meeting = broscribe_core::schema::Meeting::new(
            MeetingId::random(),
            broscribe_core::schema::Source::Import,
            broscribe_core::schema::now(),
        );
        meeting.title = Some(title.into());
        broscribe_core::ops::with_index(&library, |tx, index| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)?;
            index.upsert_meeting(tx, &meeting)
        })
        .unwrap();
        meeting.id
    }

    #[gpui::test]
    fn library_selection_and_links_reveal_the_same_meeting_after_reload(cx: &mut TestAppContext) {
        use broscribe_core::schema::Source;
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let id = split_meeting(&state, "Synthetic orbit review", cx);
        let (main, visual) = split_shell(&state, cx);
        visual.simulate_resize(size(px(1200.), px(760.)));
        visual.run_until_parked();
        let library = main.read_with(visual, |main, _| main.library.clone());
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            tree.find("library-meetings");
            tree.find("meeting-detail-empty");
            library.update(cx, |view, cx| view.focus_list(window, cx));
        });
        visual.simulate_keystrokes("down");
        let executor = visual.update(|_, cx| cx.background_executor().clone());
        executor.advance_clock(std::time::Duration::from_millis(150));
        visual.run_until_parked();
        visual.update(|window, cx| {
            assert_eq!(state.read(cx).route().selected_meeting(), Some(&id));
            assert!(library.focus_handle(cx).is_focused(window));
            assert!(main.read(cx).back.is_empty());
            library.update(cx, |view, cx| {
                view.set_source_filter(Some(Source::Recording), cx)
            });
        });
        visual.run_until_parked();
        state.update(visual, |state, cx| state.navigate(Route::ReviewQueue, cx));
        state.update(visual, |state, cx| {
            state.navigate(Route::meeting(id.clone()), cx)
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("library-filters-cleared")),
                ("Status", "Filters cleared to show this meeting.")
            );
            assert_eq!(library.read(cx).selected_meeting(), Some(&id));
            assert_eq!(state.read(cx).route().selected_meeting(), Some(&id));
            assert!(main.read(cx).content_focus.contains_focused(window, cx));
            state.update(cx, |state, cx| state.rerun_search(cx));
        });
        visual.run_until_parked();
        assert_eq!(
            library.read_with(visual, |view, _| view.selected_meeting().cloned()),
            Some(id)
        );
    }

    #[gpui::test]
    fn split_breakpoints_overlay_history_and_area_keys_preserve_focus(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let id = split_meeting(&state, "Synthetic planning", cx);
        let (main, visual) = split_shell(&state, cx);
        visual.simulate_resize(size(px(1000.), px(760.)));
        visual.run_until_parked();
        visual.update(|window, cx| {
            let shell = main.read(cx);
            assert_eq!(shell.pane_layout, PaneLayout::Wide);
            shell.sidebar_toggle_focus.clone().focus(window, cx);
        });
        visual.simulate_keystrokes("f6");
        visual.update(|window, cx| assert!(main.read(cx).sidebar_rows[0].is_focused(window)));
        visual.simulate_keystrokes("f6");
        visual.update(|window, cx| {
            assert!(main.read(cx).library.focus_handle(cx).is_focused(window))
        });
        visual.simulate_keystrokes("shift-f6 end");
        visual.update(|window, cx| {
            assert!(main.read(cx).sidebar_rows[2].is_focused(window));
            assert_eq!(state.read(cx).route(), &Route::people());
        });
        visual.simulate_keystrokes("home");
        visual.simulate_resize(size(px(999.), px(760.)));
        visual.run_until_parked();
        visual.update(|window, cx| {
            let shell = main.read(cx);
            assert_eq!(shell.pane_layout, PaneLayout::Split);
            assert!(!shell.show_sidebar(true));
            assert!(shell.sidebar_toggle_focus.is_focused(window));
        });
        visual.dispatch_action(crate::menu::ToggleSidebar);
        visual.run_until_parked();
        assert!(visual.debug_bounds("main-sidebar").is_some());
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| assert!(main.read(cx).sidebar_toggle_focus.is_focused(window)));
        visual.simulate_resize(size(px(759.), px(760.)));
        visual.run_until_parked();
        visual.update(|window, cx| {
            assert_eq!(main.read(cx).pane_layout, PaneLayout::Single);
            let library = main.read(cx).library.clone();
            library.update(cx, |library, cx| library.focus_list(window, cx));
        });
        visual.simulate_keystrokes("down enter");
        visual.run_until_parked();
        assert!(visual.debug_bounds("main-detail-pane").is_some());
        visual.dispatch_action(crate::menu::NavigateBack);
        visual.run_until_parked();
        visual.update(|window, cx| {
            let shell = main.read(cx);
            assert!(shell.library.focus_handle(cx).is_focused(window));
            assert_eq!(shell.library.read(cx).selected_meeting(), Some(&id));
            assert_eq!(state.read(cx).route(), &Route::library());
        });
        visual.dispatch_action(crate::menu::NavigateForward);
        assert_eq!(
            state.read_with(visual, |state, _| state.route().selected_meeting().cloned()),
            Some(id)
        );
        assert_eq!(PaneLayout::at_width(760. / 16.), PaneLayout::Split);
        assert_eq!(PaneLayout::at_width(1000. / 24.), PaneLayout::Single);
    }

    #[gpui::test]
    fn history_caps_both_stacks_and_a_new_visit_clears_forward(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let (main, visual) = split_shell(&state, cx);
        let read_history = |window: &mut Window, cx: &mut App| {
            let tree = a11y_tree::draw_and_read(window, cx);
            tree.read(tree.find("history-back")).1.to_owned()
        };
        assert_eq!(visual.update(read_history), "Back");
        for _ in 0..60 {
            state.update(visual, |state, cx| state.navigate(Route::ReviewQueue, cx));
            state.update(visual, |state, cx| state.navigate(Route::people(), cx));
        }
        assert_eq!(visual.update(read_history), "Back to Review queue");
        assert_eq!(main.read_with(visual, |main, _| main.back.len()), 50);
        for _ in 0..50 {
            visual.update(|window, cx| {
                main.update(cx, |main, cx| main.navigate_history(false, window, cx))
            });
        }
        assert_eq!(
            main.read_with(visual, |main, _| (main.back.len(), main.forward.len())),
            (0, 50)
        );
        state.update(visual, |state, cx| state.navigate(Route::library(), cx));
        assert!(main.read_with(visual, |main, _| main.forward.is_empty()));
    }

    #[gpui::test]
    fn review_count_has_words_and_a_badge_only_when_nonzero(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let (main, visual) = split_shell(&state, cx);
        visual.simulate_resize(size(px(1200.), px(760.)));
        let review_label = |window: &mut Window, cx: &mut App| {
            let tree = a11y_tree::draw_and_read(window, cx);
            let (role, words) = tree.read(tree.find("nav-review"));
            assert_eq!(role, "ListItem");
            words.to_owned()
        };
        assert_eq!(visual.update(review_label), "Review queue");
        assert!(visual.debug_bounds("nav-review-badge").is_none());
        visual.update(|_, cx| {
            main.update(cx, |main, cx| {
                main.shown.review_count = Some(3);
                cx.notify();
            })
        });
        assert_eq!(
            visual.update(review_label),
            "Review queue, 3 speakers to review"
        );
        // The count is amber, because it counts decisions waiting.
        let badge = visual.debug_bounds("nav-review-badge").unwrap();
        visual.update(|window, cx| {
            let amber = crate::ui::brand::brand(cx).badge;
            let scale = window.scale_factor();
            assert!(window.painted_quads().iter().any(|quad| {
                quad.bounds == badge.scale(scale) && quad.background == amber.into()
            }));
        });
        visual.update(|_, cx| {
            main.update(cx, |main, cx| {
                main.shown.review_count_error = Some("The index is unavailable".into());
                cx.notify();
            })
        });
        assert_eq!(
            visual.update(review_label),
            "Review queue, count unavailable"
        );
        assert!(visual.debug_bounds("nav-review-badge").is_none());
    }

    #[gpui::test]
    fn people_route_links_ask_before_replacing_a_dirty_person_editor(cx: &mut TestAppContext) {
        use crate::ui::kit::dialog_gate::click;
        use broscribe_core::{PersonId, schema::Person};

        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let library = state.read_with(cx, |state, _| state.library().clone());
        let first = Person::new(PersonId::random(), "Aster Example");
        let second = Person::new(PersonId::random(), "Briar Example");
        library
            .write(|tx| {
                tx.save_person(&first)?;
                tx.save_person(&second)
            })
            .unwrap();
        let (main, visual) = split_shell(&state, cx);
        visual.simulate_resize(size(px(1250.), px(900.)));
        let navigate = |person, visual: &mut gpui::VisualTestContext| {
            state.update(visual, |state, cx| {
                state.navigate(
                    Route::People {
                        person: Some(person),
                    },
                    cx,
                );
            });
            visual.run_until_parked();
        };
        navigate(first.id.clone(), visual);
        click(visual, "person-edit", Role::Button);
        visual.simulate_input(" Draft");
        navigate(second.id.clone(), visual);
        let selected_person = |visual: &gpui::VisualTestContext| {
            main.read_with(visual, |main, cx| {
                main.people
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .selected_person()
                    .cloned()
            })
        };
        assert_eq!(selected_person(visual), Some(first.id.clone()));
        click(visual, "person-keep-editing", Role::Button);
        assert_eq!(selected_person(visual), Some(first.id.clone()));
        navigate(second.id.clone(), visual);
        click(visual, "person-discard-changes", Role::Button);
        assert_eq!(selected_person(visual), Some(second.id.clone()));
        assert_eq!(library.load_person(&first.id).unwrap().name, first.name);
    }

    #[gpui::test]
    fn dragged_pane_widths_round_trip_and_restore_after_reopen(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        cx.update(|cx| crate::ui_state::install(dir.path(), cx));
        let (_, visual) = split_shell(&state, cx);
        visual.simulate_resize(size(px(1250.), px(720.)));
        visual.run_until_parked();
        let drag = |visual: &mut gpui::VisualTestContext, x: Pixels, change: f32| {
            let from = gpui::point(x - px(2.), px(500.));
            let to = gpui::point(x + px(change), from.y);
            let button = gpui::MouseButton::Left;
            visual.simulate_mouse_move(from, None, gpui::Modifiers::default());
            visual.simulate_mouse_down(from, button, gpui::Modifiers::default());
            visual.simulate_mouse_move(
                gpui::point(x + px(10.), from.y),
                button,
                gpui::Modifiers::default(),
            );
            visual.simulate_mouse_move(to, button, gpui::Modifiers::default());
            visual.simulate_mouse_up(to, button, gpui::Modifiers::default());
            visual.run_until_parked();
        };
        let sidebar = visual.debug_bounds("main-sidebar").unwrap();
        let detail = visual.debug_bounds("main-detail-pane").unwrap();
        assert_eq!(sidebar.size.width, px(200.));
        assert_eq!(detail.origin.x - sidebar.right(), px(LIST_WIDTH));
        drag(visual, sidebar.right(), 36.);
        let detail = visual.debug_bounds("main-detail-pane").unwrap();
        drag(visual, detail.origin.x, 500.);
        let (sidebar, list) = visual.update(|_, cx| {
            let (_, sidebar, list) = crate::ui_state::main_panes(cx);
            (sidebar.unwrap(), list.unwrap())
        });
        assert!((230. ..=240.).contains(&sidebar), "{sidebar}");
        assert_eq!(list, LIST_MAX_WIDTH);
        visual.update(|window, _| window.remove_window());
        cx.run_until_parked();
        let path = dir.path().join("ui-state.json");
        let saved = crate::ui_state::UiState::load(&path).main_window.unwrap();
        assert_eq!(
            (saved.sidebar_width, saved.library_width),
            (Some(sidebar), Some(list))
        );
        cx.update(|cx| crate::ui_state::install(dir.path(), cx));
        let (_, visual) = split_shell(&state, cx);
        visual.simulate_resize(size(px(1250.), px(720.)));
        visual.run_until_parked();
        let reopened = visual.debug_bounds("main-sidebar").unwrap();
        let detail = visual.debug_bounds("main-detail-pane").unwrap();
        assert_eq!(reopened.size.width, px(sidebar));
        assert_eq!(detail.origin.x - reopened.right(), px(list));
    }

    #[gpui::test]
    fn window_titles_follow_sections_meetings_and_launch_states(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let meeting = split_meeting(&state, "Synthetic lunar planning", cx);
        let untitled = split_meeting(&state, "", cx);
        let (main, visual) = split_shell(&state, cx);
        for host in [
            HostPlatform::MacOs,
            HostPlatform::Windows,
            HostPlatform::Linux,
        ] {
            visual.update(|_, cx| main.update(cx, |main, _| main.host = host));
            for (route, title) in [
                (Route::ReviewQueue, "Review queue"),
                (Route::people(), "People"),
                (Route::library(), "Library"),
                (Route::meeting(meeting.clone()), "Synthetic lunar planning"),
                (Route::meeting(untitled.clone()), "Untitled meeting"),
            ] {
                state.update(visual, |state, cx| state.navigate(route, cx));
                visual.run_until_parked();
                let expected = if host == HostPlatform::MacOs {
                    title.into()
                } else {
                    format!("{title} - BroScribe")
                };
                assert_eq!(visual.window_title(), Some(expected));
            }
            assert_eq!(
                windows::title_for(host, windows::STARTUP_TITLE),
                "BroScribe can't start"
            );
            assert_eq!(
                windows::title_for(host, windows::maintenance_title(host)),
                if host == HostPlatform::MacOs {
                    "Maintenance Mode"
                } else {
                    "Maintenance mode - BroScribe"
                }
            );
        }
        let (error, maintenance) = cx.update(|cx| {
            let failure = StartupFailure::new(crate::launch::StartupStage::Config, None);
            (
                open_startup_error_window(failure, |_| {}, cx).unwrap(),
                open_maintenance_window(dir.path().into(), cx).unwrap(),
            )
        });
        for (window, title) in [
            (error, windows::STARTUP_TITLE),
            (
                maintenance,
                windows::maintenance_title(
                    broscribe_platform_native::NativePlatform::host_platform(),
                ),
            ),
        ] {
            assert_eq!(
                gpui::VisualTestContext::from_window(window.into(), cx).window_title(),
                Some(windows::title(title))
            );
        }
    }

    fn job_button(visual: &mut gpui::VisualTestContext, label: &str) -> Option<Bounds<Pixels>> {
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            gpui::base::test_support::snapshots(window)
                .iter()
                .find(|node| node.label() == Some(label))
                .map(|node| node.bounds())
        })
    }

    #[gpui::test]
    fn status_bar_stays_one_line_and_more_opens_every_job(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let window = cx.update(|cx| {
            theme::init(cx);
            open_main_window(&state, &deps, cx).unwrap()
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.run_until_parked();
        let idle = visual.debug_bounds("main-status-bar").unwrap();
        assert_eq!(idle.size.height, px(28.));
        assert!(job_button(&mut visual, "+2 more").is_none());
        for (idx, kind) in ["enrich", "refresh-vectors", "record-final"]
            .into_iter()
            .enumerate()
        {
            state.update(&mut visual, |state, cx| {
                state.daemon_progress(
                    &broscribe_ipc::JobProgress {
                        job_id: format!("synthetic-{idx}"),
                        kind: kind.into(),
                        phase: "progress".into(),
                        meeting_id: None,
                        fraction: Some(0.42),
                        message: None,
                    },
                    cx,
                )
            });
        }
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("jobs-active-synthetic-0")),
                ("Status", "Generating summary")
            );
            assert!(tree.all("jobs-active-synthetic-1").is_empty());
        });
        assert_eq!(visual.debug_bounds("main-status-bar").unwrap(), idle);
        let more = job_button(&mut visual, "+2 more").unwrap();
        visual.simulate_click(more.center(), gpui::Modifiers::default());
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            assert_eq!(tree.all("jobs-active-synthetic-0").len(), 2);
            for (idx, label) in [(1, "Refreshing search"), (2, "Transcribing a recording")] {
                assert_eq!(
                    tree.read(tree.find(&format!("jobs-active-synthetic-{idx}"))),
                    ("Status", label)
                );
            }
        });
    }

    #[gpui::test]
    fn job_lines_use_middle_dots_for_reported_facts(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let id = split_meeting(&state, "Synthetic orbit check", cx);
        let (main, visual) = split_shell(&state, cx);
        let show =
            |visual: &mut gpui::VisualTestContext, meeting_id, message: Option<&str>, fraction| {
                state.update(visual, |state, cx| {
                    state.daemon_progress(
                        &broscribe_ipc::JobProgress {
                            job_id: "format-test".into(),
                            kind: "record-final".into(),
                            meeting_id,
                            phase: "started".into(),
                            fraction,
                            message: message.map(str::to_owned),
                        },
                        cx,
                    )
                });
                visual.run_until_parked();
                main.read_with(visual, |main, _| main.shown.running_jobs[0].clone())
            };
        let bare = show(visual, None, None, None);
        assert_eq!(bare.text, "Transcribing a recording…");
        assert_eq!(bare.percent, None);
        let step = show(visual, None, Some("chunk 3 of 9"), None);
        assert_eq!(step.text, "Transcribing a recording · chunk 3 of 9");
        assert_eq!(step.percent, None);
        let titled = show(visual, Some(id), None, Some(0.42));
        assert_eq!(
            titled.text,
            "Transcribing a recording · Synthetic orbit check"
        );
        assert_eq!(titled.percent, Some(42));
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("jobs-active-format-test-detail")),
                ("Label", "· 42%")
            );
        });
        state.update(visual, |state, cx| {
            state.set_worker_state(WorkerState::Running);
            state.daemon_progress(
                &broscribe_ipc::JobProgress {
                    job_id: "format-test".into(),
                    kind: "record-final".into(),
                    meeting_id: None,
                    phase: "done".into(),
                    fraction: None,
                    message: None,
                },
                cx,
            );
        });
        assert_eq!(
            main.read_with(visual, |main, _| main.shown.running_jobs[0].text.clone()),
            "Starting background jobs…"
        );
    }

    #[gpui::test]
    fn job_failure_opens_its_meeting_and_stays_dismissed_across_windows(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let id = split_meeting(&state, "Synthetic telescope review", cx);
        let (_, visual) = split_shell(&state, cx);
        let event = broscribe_ipc::JobProgress {
            job_id: "synthetic-failure".into(),
            kind: "enrich".into(),
            meeting_id: Some(id.clone()),
            phase: "failed".into(),
            fraction: None,
            message: Some("inference error: /synthetic/private/response".into()),
        };
        state.update(visual, |state, cx| state.daemon_progress(&event, cx));
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = a11y_tree::draw_and_read(window, cx);
            let alert = tree.find("jobs-failure");
            let (role, text) = tree.read(alert);
            assert_eq!(role, "Alert");
            assert!(text.starts_with(
                "Couldn't finish generating the summary for “Synthetic telescope review”."
            ));
            assert!(!text.contains("/synthetic/private"));
            assert!(tree.subtree(tree.find("status-area")).contains(&alert));
        });
        kit::dialog_gate::click(visual, "jobs-failure-open", Role::Button);
        assert_eq!(
            state.read_with(visual, |state, _| state.route().selected_meeting().cloned()),
            Some(id)
        );
        // A Linux session without a tray can also show a Dismiss button.
        kit::dialog_gate::click(visual, "jobs-failure-dismiss", Role::Button);
        assert!(state.read_with(visual, |state, _| state.shown_job_failure().is_none()));
        state.update(visual, |state, cx| state.daemon_progress(&event, cx));
        visual.run_until_parked();
        let assert_dismissed = |visual: &mut gpui::VisualTestContext| {
            visual.update(|window, cx| {
                let tree = a11y_tree::draw_and_read(window, cx);
                assert!(tree.all("jobs-failure-dismiss").is_empty());
            });
        };
        assert_dismissed(visual);
        let (_, visual) = split_shell(&state, cx);
        assert_dismissed(visual);
    }

    #[gpui::test]
    fn worker_notices_use_top_banners_and_keep_raw_details_for_copy(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let (_, visual) = split_shell(&state, cx);
        let notice = |window: &mut Window, cx: &mut App| {
            let tree = a11y_tree::draw_and_read(window, cx);
            let notice = tree.find("jobs-worker");
            assert!(notice < tree.find("library"));
            assert!(notice < tree.find("status-area"));
            let (role, text) = tree.read(notice);
            (role.to_owned(), text.to_owned())
        };
        for (worker, role, text, button) in [
            (
                WorkerState::Restarting { crashes: 1 },
                "Status",
                "Background jobs stopped unexpectedly. Retrying · attempt 2",
                "Diagnostics",
            ),
            (
                WorkerState::Unavailable {
                    detail: "/synthetic/private".into(),
                    crashes: None,
                },
                "Alert",
                "Background jobs can't run. BroScribe's command-line tool couldn't start.",
                "Try again",
            ),
            (
                WorkerState::Unavailable {
                    detail: "/synthetic/private".into(),
                    crashes: Some(5),
                },
                "Alert",
                "Background jobs can't run. They stopped unexpectedly 5 times in a row.",
                "Try again",
            ),
        ] {
            state.update(visual, |state, cx| {
                state.set_worker_state(worker);
                cx.notify();
            });
            visual.run_until_parked();
            assert_eq!(visual.update(notice), (role.into(), text.into()));
            assert!(job_button(visual, button).is_some());
        }
        let copy = job_button(visual, "Copy details").unwrap();
        visual.simulate_click(copy.center(), gpui::Modifiers::default());
        assert_eq!(
            visual
                .read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("/synthetic/private")
        );
    }

    #[test]
    fn toolbar_chrome_uses_the_host_and_the_compositor() {
        assert_eq!(
            ToolbarChrome::for_window(HostPlatform::MacOs, false),
            ToolbarChrome::Unified
        );
        assert_eq!(
            ToolbarChrome::for_window(HostPlatform::Windows, false),
            ToolbarChrome::Native
        );
        assert_eq!(
            ToolbarChrome::for_window(HostPlatform::Linux, false),
            ToolbarChrome::Native
        );
        assert_eq!(
            ToolbarChrome::for_window(HostPlatform::Linux, true),
            ToolbarChrome::Client
        );
        assert!(crate::menu::has_global_menu_bar(HostPlatform::MacOs));
        assert!(!crate::menu::has_global_menu_bar(HostPlatform::Windows));
        assert!(!crate::menu::has_global_menu_bar(HostPlatform::Linux));
    }

    /// The main window, the startup error and the maintenance window, as a
    /// launch opens them (without gpui-component's `Root`, which adds no
    /// title bar).
    fn launch_windows(
        cx: &mut TestAppContext,
        dir: &std::path::Path,
    ) -> (
        gpui::WindowHandle<MainWindow>,
        gpui::WindowHandle<StartupErrorView>,
        gpui::WindowHandle<MaintenanceView>,
    ) {
        use crate::launch::StartupStage;

        let state = shell_state(cx, dir);
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let main = cx.add_window(|window, cx| MainWindow::new(state, deps, window, cx));
        let failure = StartupFailure::new(
            StartupStage::Config,
            Some(PathBuf::from("/synthetic/config.toml")),
        );
        let error = cx
            .add_window(move |_, cx| StartupErrorView::new(failure, Rc::new(|_: &mut App| {}), cx));
        let data_dir = dir.join("data");
        let maintenance = cx.add_window(move |_, _| MaintenanceView {
            data_dir,
            copied: false,
        });
        (main, error, maintenance)
    }

    /// SHL-03: macOS, Windows and a server-decorated Linux window report
    /// server-side decorations, and no window gains anything.
    #[gpui::test]
    fn server_side_decorations_add_no_title_bar_to_any_launch_window(cx: &mut TestAppContext) {
        use chrome::testing::{chrome, title_heading};

        let dir = tempfile::tempdir().unwrap();
        cx.update(gpui::init);
        let (main, error, maintenance) = launch_windows(cx, dir.path());
        for window in [main.into(), error.into(), maintenance.into()] {
            chrome(window, cx).assert_none();
        }
        assert_eq!(title_heading(main, cx), None);
        assert_eq!(title_heading(error, cx), None);
        assert_eq!(title_heading(maintenance, cx), None);
        for (window, title) in [
            (main.into(), "toolbar-title"),
            (error.into(), "startup-error-title"),
            (maintenance.into(), "maintenance-heading"),
        ] {
            cx.update_window(window, |_, window, cx| {
                a11y_tree::draw_and_read(window, cx).assert_title(title);
            })
            .unwrap();
        }
    }

    /// SHL-03: where the compositor leaves decorations to the app, each
    /// window's first row is BroScribe's title bar: the window's title as a
    /// heading and Minimize, Maximize and Close as named buttons. Opening
    /// the window focuses nothing in it, and Close closes the window.
    #[gpui::test]
    fn client_side_decorations_give_every_launch_window_a_title_bar(cx: &mut TestAppContext) {
        use chrome::testing::{chrome, report_client_decorations, title_heading};
        use chrome::{CLOSE_LABEL, MAXIMIZE_LABEL, MINIMIZE_LABEL};

        let dir = tempfile::tempdir().unwrap();
        cx.update(gpui::init);
        // What each window focuses on open without a bar.
        let (main, error, maintenance) = launch_windows(cx, &dir.path().join("server"));
        let focused_without = [main.into(), error.into(), maintenance.into()]
            .map(|window| chrome(window, cx).focused);

        report_client_decorations(cx);
        let (main, error, maintenance) = launch_windows(cx, &dir.path().join("client"));
        let windows = [main.into(), error.into(), maintenance.into()];
        assert_eq!(title_heading(main, cx).as_deref(), Some("Library"));
        assert_eq!(
            title_heading(error, cx).as_deref(),
            Some("BroScribe can't start")
        );
        let host = broscribe_platform_native::NativePlatform::host_platform();
        assert_eq!(
            title_heading(maintenance, cx),
            Some(windows::title_for(host, windows::maintenance_title(host)))
        );
        for (window, focused_without) in windows.into_iter().zip(focused_without) {
            let shown = chrome(window, cx);
            if window == main.into() {
                shown.assert_first_with_height(chrome::TOOLBAR_HEIGHT);
            } else {
                shown.assert_first();
            }
            // The recording controls' Close-only bar is theirs alone.
            assert_eq!(
                shown.control_labels(),
                [MINIMIZE_LABEL, MAXIMIZE_LABEL, CLOSE_LABEL]
            );
            assert_eq!(
                shown.focused, focused_without,
                "the title bar changed what the window focuses on open"
            );

            let close = shown.control(CLOSE_LABEL).bounds.center();
            gpui::VisualTestContext::from_window(window, cx)
                .simulate_click(close, gpui::Modifiers::default());
            cx.run_until_parked();
            assert!(
                window.update(cx, |_, _, _| ()).is_err(),
                "Close left the window open"
            );
        }
    }

    /// Saves `size` the way Settings and the View menu do (a new revision
    /// under the config lock) and returns the saved config.
    fn save_reading_size(data_dir: &std::path::Path, size: ReadingSize) -> Config {
        Config::update(data_dir, |config| {
            config.appearance.reading_size = size;
            Ok(())
        })
        .map(|(config, ())| config)
        .unwrap()
    }

    #[gpui::test]
    fn the_saved_reading_size_scales_theme_text_and_keeps_the_theme(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let data_dir = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        std::fs::create_dir_all(&data_dir).unwrap();
        cx.update(gpui::init);
        let theme = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                let theme = cx.theme();
                (
                    theme.font_size,
                    theme.mono_font_size,
                    theme.background,
                    theme.mode,
                )
            })
        };
        let (base, mono, background, mode) = theme(cx);
        // Following twice (a reopened window) scales once.
        cx.update(|cx| {
            follow_reading_size(&state, cx);
            follow_reading_size(&state, cx);
        });
        let mut older = None;
        let steps = ReadingSize::ALL.into_iter().rev().chain(ReadingSize::ALL);
        for (i, size) in steps.enumerate() {
            let saved = save_reading_size(&data_dir, size);
            if i == 1 {
                // Larger, saved before the final Largest.
                older = Some(saved.clone());
            }
            state.update(cx, |s, cx| s.set_config(saved, cx));
            cx.run_until_parked();
            let (font, mono_font, bg, now_mode) = theme(cx);
            assert_eq!(font, base * size.scale(), "{size:?}");
            assert_eq!(mono_font, mono * size.scale(), "{size:?}");
            assert_eq!((bg, now_mode), (background, mode), "colours and mode stay");
        }
        // An older revision arriving late changes nothing.
        state.update(cx, |s, cx| s.set_config(older.unwrap(), cx));
        cx.run_until_parked();
        assert_eq!(theme(cx).0, base * ReadingSize::Largest.scale());

        // Another process saves a size: the next activation reads it back.
        save_reading_size(&data_dir, ReadingSize::Small);
        cx.update(|cx| reload_config(&state, cx));
        cx.run_until_parked();
        state.read_with(cx, |s, _| {
            assert_eq!(s.config().appearance.reading_size, ReadingSize::Small)
        });
        assert_eq!(theme(cx).0, base * ReadingSize::Small.scale());
    }

    #[gpui::test]
    fn switching_appearance_keeps_the_reading_size(cx: &mut TestAppContext) {
        use gpui::component::ThemeMode;
        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        let data_dir = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        std::fs::create_dir_all(&data_dir).unwrap();
        cx.update(|cx| {
            theme::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
        });
        let base = cx.update(|cx| cx.theme().font_size);
        let saved = save_reading_size(&data_dir, ReadingSize::Larger);
        state.update(cx, |s, cx| s.set_config(saved, cx));
        cx.update(|cx| follow_reading_size(&state, cx));
        cx.run_until_parked();
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::Dark] {
            cx.update(|cx| Theme::change(mode, None, cx));
            cx.run_until_parked();
            cx.update(|cx| {
                let name = if mode.is_dark() {
                    theme::DARK
                } else {
                    theme::LIGHT
                };
                assert_eq!(cx.theme().theme_name().as_ref(), name);
                assert_eq!(cx.theme().font_size, base * ReadingSize::Larger.scale());
            });
        }
    }

    /// The name and live-region rules: a `Label` is named by its value and
    /// carries no label; every other role is named by its label and carries no
    /// value, so no platform reads the text twice. `Status` is `Live::Polite`,
    /// `Alert` is `Live::Assertive`, and no other role (including `Label`,
    /// `Heading`, `Timer`, `Meter`) is live (ACC-01 / ACC-18).
    #[test]
    fn spoken_names_a_label_by_its_value_and_other_roles_by_their_label() {
        use gpui::accesskit::Live;

        const TEXT: &str = "Synthetic line";
        for (role, expected_live) in [
            (Role::Label, None),
            (Role::Heading, None),
            (Role::Status, Some(Live::Polite)),
            (Role::Alert, Some(Live::Assertive)),
            (Role::Note, None),
            (Role::Timer, None),
            (Role::Meter, None),
        ] {
            let line = text_line("synthetic-line", role, TEXT);
            let (_, node) = a11y_written(&line).expect("an identified node");
            if role == Role::Label {
                assert_eq!((node.value(), node.label()), (Some(TEXT), None));
            } else {
                assert_eq!((node.label(), node.value()), (Some(TEXT), None), "{role:?}");
            }
            assert_eq!(node.live(), expected_live, "{role:?}");
            assert_eq!(
                a11y_name(&line),
                Some(("synthetic-line".to_owned(), role, TEXT.to_owned()))
            );
        }
        let (_, node) = a11y_written(&heading("synthetic-heading", TEXT)).unwrap();
        assert_eq!((node.role(), node.label()), (Role::Heading, Some(TEXT)));
        assert_eq!(node.level(), Some(2));
        assert_eq!(node.live(), None);
        let (_, title_node) = a11y_written(&title("synthetic-title", TEXT)).unwrap();
        assert_eq!(title_node.level(), Some(1));
        let (_, subheading_node) = a11y_written(&subheading("synthetic-subheading", TEXT)).unwrap();
        assert_eq!(subheading_node.level(), Some(3));
        for (mut element, role) in [
            (title("title", TEXT), TextRole::Title),
            (heading("heading", TEXT), TextRole::Section),
            (subheading("subheading", TEXT), TextRole::Label),
        ] {
            assert_eq!(element.text_style(), div().text_role(role).text_style());
        }

        let mut timer = timer_line("synthetic-timer", "Saving in", "8 seconds", "Saving in 8 s");
        type_scale::tests::assert_tabular(&mut timer);
        let (timer_id, timer_node) = a11y_written(&timer).expect("timer node");
        assert_eq!(timer_id, "synthetic-timer");
        assert_eq!(timer_node.role(), Role::Timer);
        assert_eq!(timer_node.label(), Some("Saving in"));
        if broscribe_platform_native::NativePlatform::timer_value_uses_description() {
            assert_eq!(timer_node.description(), Some("8 seconds"));
            assert_eq!(timer_node.value(), None);
        } else {
            assert_eq!(timer_node.value(), Some("8 seconds"));
            assert_eq!(timer_node.description(), None);
        }
        assert_eq!(timer_node.live(), None);

        // Spoken words may differ from the visible child; the rule holds.
        let fact = div()
            .id("synthetic-fact")
            .spoken(Role::Label, "Field: value")
            .child("value");
        assert_eq!(
            a11y_name(&fact),
            Some((
                "synthetic-fact".to_owned(),
                Role::Label,
                "Field: value".to_owned()
            ))
        );
        let (_, fact_node) = a11y_written(&fact).unwrap();
        assert_eq!(fact_node.label(), None);
        assert_eq!(fact_node.live(), None);
    }

    #[test]
    fn clock_and_spoken_duration_helpers_format_cleanly() {
        assert_eq!(format_clock(0.0), "0:00");
        assert_eq!(format_clock(83.0), "1:23");
        assert_eq!(format_clock(427.0), "7:07");
        assert_eq!(format_clock(3725.0), "1:02:05");
        assert_eq!(format_clock(-3.0), "0:00");
        assert_eq!(format_clock(f64::NAN), "0:00");
        assert_eq!(spoken_seconds(0), "0 seconds");
        assert_eq!(spoken_seconds(1), "1 second");
        assert_eq!(spoken_seconds(8), "8 seconds");
        assert_eq!(spoken_duration(83), "1 minute 23 seconds");
        assert_eq!(spoken_duration(3725), "1 hour 2 minutes 5 seconds");
    }

    #[gpui::test]
    fn live_announcer_speaks_once_per_state_change_and_ignores_timer_ticks(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::fakes::FakeAnnouncer;

        struct ProbeView {
            status: Option<String>,
            alert: Option<String>,
            countdown: u64,
            meter_dbfs: i32,
        }

        impl Render for ProbeView {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                div()
                    .id("probe-root")
                    .children(
                        self.status
                            .clone()
                            .map(|s| text_line("probe-status", Role::Status, s)),
                    )
                    .children(
                        self.alert
                            .clone()
                            .map(|a| text_line("probe-alert", Role::Alert, a)),
                    )
                    .child(timer_line(
                        "probe-countdown",
                        "Saving in",
                        spoken_seconds(self.countdown),
                        format!("Saving in {} s", self.countdown),
                    ))
                    .child(text_line(
                        "probe-meter",
                        Role::Label,
                        format!("Me: {} dBFS", self.meter_dbfs),
                    ))
            }
        }

        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| {
            gpui::init(cx);
            install_announcer(announcer.clone(), cx);
        });
        let window = cx.add_window(|_, _| ProbeView {
            status: Some("Idle".into()),
            alert: None,
            countdown: 10,
            meter_dbfs: -40,
        });
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
        };
        draw(cx);
        // Initial frame establishes baseline; nothing announced yet.
        assert!(announcer.take().is_empty());

        // Countdown and meter ticks redraw the window, but the live status
        // did not change so nothing is announced.
        for (countdown, dbfs) in [(9, -24), (8, -18), (7, -30)] {
            window
                .update(cx, |v, _, cx| {
                    v.countdown = countdown;
                    v.meter_dbfs = dbfs;
                    cx.notify();
                })
                .unwrap();
            draw(cx);
        }
        assert!(
            announcer.take().is_empty(),
            "timer and label ticks must not trigger announcements"
        );

        // Status changes once: announced at Medium priority.
        window
            .update(cx, |v, _, cx| {
                v.status = Some("Saving…".into());
                cx.notify();
            })
            .unwrap();
        draw(cx);
        assert_eq!(
            announcer.take(),
            vec![("Saving…".to_owned(), AnnouncementPriority::Medium)]
        );

        // Alert appears: announced at High priority, once.
        window
            .update(cx, |v, _, cx| {
                v.alert = Some("Disk space is low.".into());
                cx.notify();
            })
            .unwrap();
        draw(cx);
        assert_eq!(
            announcer.take(),
            vec![("Disk space is low.".to_owned(), AnnouncementPriority::High)]
        );

        // Another countdown tick with the same status and alert: silent.
        window
            .update(cx, |v, _, cx| {
                v.countdown = 6;
                cx.notify();
            })
            .unwrap();
        draw(cx);
        assert!(announcer.take().is_empty());
    }

    /// A window wrapped in `Root`, as every app window is, showing an
    /// optional alert and whatever pairs of `(parent id, live text)` it is
    /// given, each as a Status with the same local id.
    struct LiveWindow {
        alert: Option<&'static str>,
        statuses: Vec<(&'static str, &'static str)>,
        renders: usize,
    }

    impl Render for LiveWindow {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.renders += 1;
            div()
                .id("live-window")
                .child(text_line("live-window-label", Role::Label, "Not live"))
                .children(
                    self.alert
                        .map(|alert| text_line("live-window-alert", Role::Alert, alert)),
                )
                .children(self.statuses.iter().map(|(parent, text)| {
                    div()
                        .id(*parent)
                        .child(text_line("status", Role::Status, *text))
                }))
        }
    }

    fn live_window(
        cx: &mut TestAppContext,
        statuses: Vec<(&'static str, &'static str)>,
    ) -> (
        Arc<broscribe_platform::fakes::FakeAnnouncer>,
        gpui::AnyWindowHandle,
        Entity<LiveWindow>,
    ) {
        let announcer = Arc::new(broscribe_platform::fakes::FakeAnnouncer::new());
        cx.update(|cx| {
            gpui::init(cx);
            install_announcer(announcer.clone(), cx);
        });
        let view = Rc::new(std::cell::RefCell::new(None));
        let window = cx.add_window({
            let view = view.clone();
            move |window, cx| {
                let live = cx.new(|_| LiveWindow {
                    alert: None,
                    statuses,
                    renders: 0,
                });
                *view.borrow_mut() = Some(live.clone());
                Root::new(live, window, cx)
            }
        });
        cx.run_until_parked();
        let view = view.borrow().clone().expect("the window's view");
        (announcer, window.into(), view)
    }

    fn draw_live(window: gpui::AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        cx.run_until_parked();
    }

    /// Every draw of a `Root` window is compared, an empty one too: a first
    /// alert in a window that never had a live node is announced, and an
    /// alert that went away and comes back with the same words is announced
    /// again. Following draws never asks for one.
    #[gpui::test]
    fn every_draw_counts_so_a_returning_alert_speaks_again(cx: &mut TestAppContext) {
        const FAILURE: &str = "Couldn't save the title. Synthetic cause.";
        let (announcer, window, view) = live_window(cx, Vec::new());
        let set_alert = |alert: Option<&'static str>, cx: &mut TestAppContext| {
            view.update(cx, |v, cx| {
                v.alert = alert;
                cx.notify();
            });
        };

        // Baseline, then more draws with no live node at all.
        draw_live(window, cx);
        draw_live(window, cx);
        assert!(announcer.take().is_empty());

        // The window's first alert speaks, once.
        set_alert(Some(FAILURE), cx);
        draw_live(window, cx);
        draw_live(window, cx);
        // Gone: the empty draw forgets it.
        set_alert(None, cx);
        draw_live(window, cx);
        // Back with the same words: it speaks again.
        set_alert(Some(FAILURE), cx);
        draw_live(window, cx);
        assert_eq!(
            announcer.take(),
            vec![
                (FAILURE.to_owned(), AnnouncementPriority::High),
                (FAILURE.to_owned(), AnnouncementPriority::High),
            ]
        );

        // One render per draw: the announcer never notifies or asks for a
        // frame of its own. The test platform does not expose the window's
        // dirty flag, so check what would set it or deliver a frame: no
        // notification of the view, no next-frame callback queued.
        let notified = Rc::new(std::cell::Cell::new(0));
        let _watch = cx.update(|cx| {
            let notified = notified.clone();
            cx.observe(&view, move |_, _| notified.set(notified.get() + 1))
        });
        let renders = view.read_with(cx, |v, _| v.renders);
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |v, _| v.renders), renders);
        draw_live(window, cx);
        assert_eq!(view.read_with(cx, |v, _| v.renders), renders + 1);
        assert!(announcer.take().is_empty());
        let queued_frames = cx
            .update_window(window, |_, window, cx| window.simulate_next_frame(cx))
            .unwrap();
        assert_eq!(queued_frames, 0, "no frame requested after the flush");
        assert_eq!(notified.get(), 0, "the view was never notified");
    }

    /// Two live nodes with the same local id under different parents keep
    /// their own entries: redraws stay quiet, and a change speaks once.
    #[gpui::test]
    fn live_nodes_sharing_a_local_id_keep_their_own_entries(cx: &mut TestAppContext) {
        let (announcer, window, view) = live_window(
            cx,
            vec![("first", "Synthetic one"), ("second", "Synthetic two")],
        );
        for _ in 0..3 {
            view.update(cx, |_, cx| cx.notify());
            draw_live(window, cx);
        }
        assert!(announcer.take().is_empty(), "no fight between the two");

        view.update(cx, |v, cx| {
            v.statuses[1].1 = "Synthetic three";
            cx.notify();
        });
        draw_live(window, cx);
        draw_live(window, cx);
        assert_eq!(
            announcer.take(),
            vec![("Synthetic three".to_owned(), AnnouncementPriority::Medium)]
        );
    }

    /// Speaker rechecks while their meeting's Speakers tab is open: the
    /// footer names the job live once; its counts there and every line of
    /// the tab's recheck box are non-live, so further count updates speak
    /// nothing on either surface. When the job succeeds the footer drops
    /// its line without a word and the tab's one Status says the recheck
    /// finished, once on both surfaces together; redraws say nothing. For a
    /// later recheck, a failed read (the tab's Alert) and the job's failure
    /// (the footer's Alert) each speak once.
    #[gpui::test]
    fn recheck_counts_stay_quiet_in_the_footer_and_the_speakers_tab(cx: &mut TestAppContext) {
        use broscribe_core::learning;

        let SpeakersTab {
            _dir,
            lib,
            s,
            model,
            state,
            announcer,
            window,
            shell,
            meeting,
        } = speakers_tab(cx);
        let request = lib
            .write(|tx| learning::request(tx, &model, Some(&s.meeting)))
            .unwrap();
        state.update(cx, |st, cx| st.recheck_requested(request.clone(), cx));
        cx.run_until_parked();

        let progress = |phase: &str, message: Option<&str>, cx: &mut TestAppContext| {
            state.update(cx, |st, cx| {
                st.daemon_progress(
                    &broscribe_ipc::JobProgress {
                        job_id: request.job_id.to_string(),
                        kind: REIDENTIFY_KIND.into(),
                        meeting_id: None,
                        phase: phase.into(),
                        fraction: None,
                        message: message.map(Into::into),
                    },
                    cx,
                )
            });
            // The recheck's new status is read before the next draw.
            cx.run_until_parked();
        };
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
            announcer.take()
        };
        // The nodes each surface names, as (id, role, text).
        let named = |cx: &mut TestAppContext| {
            window
                .update(cx, |_, window, cx| {
                    let footer = shell.update(cx, |view, cx| {
                        spoken_log::while_building(|| Render::render(view, window, cx)).1
                    });
                    let tab = meeting.update(cx, |view, cx| {
                        spoken_log::while_building(|| Render::render(view, window, cx)).1
                    });
                    let keep = |nodes: Vec<(String, Role, String)>, prefix: &str| {
                        nodes
                            .into_iter()
                            .filter(|(id, _, _)| id.starts_with(prefix))
                            .collect::<Vec<_>>()
                    };
                    (keep(footer, "jobs-"), keep(tab, "speaker-recheck"))
                })
                .unwrap()
        };
        let node = |id: &str, role: Role, text: &str| (id.to_owned(), role, text.to_owned());
        let counts = |n: u32| {
            format!(
                "{n} meeting(s) updated: {n} named, 0 replaced, 0 demoted, 0 suggested, 0 \
                 cleared; 0 waiting"
            )
        };

        draw(cx);
        let (footer, tab) = named(cx);
        assert!(footer.is_empty(), "{footer:?}");
        assert_eq!(
            tab,
            [node(
                "speaker-recheck-line-0",
                Role::Label,
                "Checking other speakers, this meeting first…"
            )]
        );

        // The job starts and counts its first meeting: the footer names it
        // once; the counts are labels on both surfaces.
        progress("started", None, cx);
        progress("progress", Some(&counts(1)), cx);
        assert_eq!(
            draw(cx),
            [(
                "Checking other speakers".to_owned(),
                AnnouncementPriority::Medium
            )]
        );
        // Two more count updates: nothing is spoken anywhere.
        for n in [2, 3] {
            progress("progress", Some(&counts(n)), cx);
            assert!(draw(cx).is_empty(), "counts {n} stay quiet");
            let (footer, tab) = named(cx);
            assert_eq!(
                footer,
                [
                    node(
                        &format!("jobs-active-{}", request.job_id),
                        Role::Status,
                        "Checking other speakers"
                    ),
                    node(
                        &format!("jobs-active-{}-counts", request.job_id),
                        Role::Label,
                        &format!("· {}", counts(n))
                    ),
                ]
            );
            assert_eq!(
                tab,
                [node(
                    "speaker-recheck-line-0",
                    Role::Label,
                    &format!("Checking other speakers, this meeting first: {}", counts(n))
                )]
            );
        }

        // The job succeeds, as the queue worker would leave it: every
        // request processed, a report with nothing changed.
        let job_file = finish_recheck(&lib, &request.job_id, Vec::new());
        // The footer's line goes without a word; the tab says the end once.
        progress("done", Some("synthetic summary"), cx);
        let finished = crate::learning::FINISHED;
        assert_eq!(
            draw(cx),
            [(finished.to_owned(), AnnouncementPriority::Medium)]
        );
        let (footer, tab) = named(cx);
        assert!(footer.is_empty(), "{footer:?}");
        assert_eq!(
            tab,
            [
                node("speaker-recheck-outcome", Role::Status, finished),
                node(
                    "speaker-recheck-line-0",
                    Role::Label,
                    "This meeting: no other speaker changed."
                ),
                node(
                    "speaker-recheck-line-1",
                    Role::Label,
                    "Other meetings: nothing changed."
                ),
            ]
        );
        // Redraws, with or without a notification, say nothing more.
        assert!(draw(cx).is_empty());
        state.update(cx, |_, cx| cx.notify());
        meeting.update(cx, |_, cx| cx.notify());
        assert!(draw(cx).is_empty());

        // A later naming asks again: the box goes back to checking without
        // a word, and the footer names the job again once it starts.
        let again = lib
            .write(|tx| learning::request(tx, &model, Some(&s.meeting)))
            .unwrap();
        assert_eq!(again.job_id, request.job_id);
        state.update(cx, |st, cx| st.recheck_requested(again, cx));
        cx.run_until_parked();
        assert!(draw(cx).is_empty());
        let (_, tab) = named(cx);
        assert_eq!(
            tab,
            [node(
                "speaker-recheck-line-0",
                Role::Label,
                "Checking other speakers, this meeting first…"
            )]
        );
        progress("started", None, cx);
        assert_eq!(
            draw(cx),
            [(
                "Checking other speakers".to_owned(),
                AnnouncementPriority::Medium
            )]
        );

        // The recheck can no longer be read: the tab's alert speaks once,
        // and a later count update with the same failure says nothing.
        std::fs::write(&job_file, b"{ not json").unwrap();
        progress("progress", Some(&counts(4)), cx);
        let spoken = draw(cx);
        assert_eq!(spoken.len(), 1, "{spoken:?}");
        assert_eq!(spoken[0].1, AnnouncementPriority::High);
        assert!(
            spoken[0]
                .0
                .starts_with("Couldn't update the speaker recheck. "),
            "{spoken:?}"
        );
        let (_, tab) = named(cx);
        let live: Vec<_> = tab
            .iter()
            .filter(|(_, role, _)| *role != Role::Label)
            .collect();
        assert_eq!(
            live,
            [&(
                "speaker-recheck-error".to_owned(),
                Role::Alert,
                spoken[0].0.clone()
            )]
        );
        progress("progress", Some(&counts(5)), cx);
        assert!(draw(cx).is_empty());

        // The job fails: the footer's alert speaks once.
        progress("failed", Some("synthetic failure"), cx);
        assert_eq!(
            draw(cx),
            [(
                "Couldn't finish a background job. Something went wrong.".to_owned(),
                AnnouncementPriority::High
            )]
        );
        assert!(draw(cx).is_empty());
    }

    #[gpui::test]
    fn missing_recording_model_failure_points_to_settings(cx: &mut TestAppContext) {
        let fixture = speakers_tab(cx);
        let failed = FailedJob {
            job_id: "synthetic-job".into(),
            kind: Some(broscribe_core::queue::RECORD_FINAL_KIND.into()),
            meeting_id: None,
            message: Some("model missing: synthetic-model is not installed".into()),
        };
        fixture.state.read_with(cx, |state, _| {
            assert_eq!(
                failed_job_text(state, &failed),
                "Couldn't finish a background job. A model it needs isn't installed."
            );
        });
    }

    /// The main window with a synthetic meeting's Speakers tab open, a
    /// fake announcer and no local worker: the speaker recheck tests' setup.
    struct SpeakersTab {
        _dir: tempfile::TempDir,
        lib: Library,
        s: crate::views::person_picker::fixtures::Synthetic,
        /// The active speaker-embedding model.
        model: String,
        state: Entity<AppState>,
        announcer: Arc<broscribe_platform::fakes::FakeAnnouncer>,
        window: gpui::WindowHandle<Root>,
        shell: Entity<MainWindow>,
        meeting: Entity<MeetingView>,
    }

    fn speakers_tab(cx: &mut TestAppContext) -> SpeakersTab {
        use broscribe_platform::fakes::FakeAnnouncer;

        use crate::views::meeting::MeetingTab;
        use crate::views::person_picker::fixtures;

        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("lib")).unwrap();
        let s = fixtures::seed(&library);
        let lib = library.clone();
        let mut config = Config::default();
        config.setup.completed_steps.push(ASSISTANT_DONE.into());
        let model = config.profile.speaker_embedding.clone();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    config,
                    library,
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        state.update(cx, |s, cx| s.set_local_jobs_allowed(false, cx));
        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| {
            gpui::init(cx);
            install_announcer(announcer.clone(), cx);
        });
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let shell = Rc::new(std::cell::RefCell::new(None));
        let window = cx.add_window({
            let shell = shell.clone();
            let state = state.clone();
            move |window, cx| {
                let main = cx.new(|cx| MainWindow::new(state, deps, window, cx));
                *shell.borrow_mut() = Some(main.clone());
                Root::new(main, window, cx)
            }
        });
        let shell = shell.borrow().clone().expect("the main window's view");
        state.update(cx, |st, cx| {
            st.navigate(Route::meeting(s.meeting.clone()), cx)
        });
        cx.run_until_parked();
        let meeting = shell.read_with(cx, |this, _| this.meeting_view().unwrap().clone());
        meeting.update(cx, |view, cx| view.set_tab(MeetingTab::Speakers, cx));
        SpeakersTab {
            _dir: dir,
            lib,
            s,
            model,
            state,
            announcer,
            window,
            shell,
            meeting,
        }
    }

    /// Leaves the speaker recheck `job_id` as the queue worker leaves it
    /// after a complete pass: every request processed, no meeting left to
    /// check first, a report of `meetings` for the newest request, and the
    /// job done. Returns the job's file.
    fn finish_recheck(
        lib: &Library,
        job_id: &broscribe_core::queue::JobId,
        meetings: Vec<broscribe_core::learning::AffectedMeeting>,
    ) -> PathBuf {
        use broscribe_core::learning::{LearningReport, ReidentifyInput};
        use broscribe_core::queue::{JOB_JSON, Job, JobState};

        let job_file = lib
            .root()
            .join(broscribe_core::paths::QUEUE_DIR)
            .join(job_id.as_str())
            .join(JOB_JSON);
        let mut job: Job = serde_json::from_slice(&std::fs::read(&job_file).unwrap()).unwrap();
        let mut input = ReidentifyInput::of(&job).unwrap();
        input.processed = input.requested;
        input.source_meetings.clear();
        input.report = Some(LearningReport {
            generation: input.requested,
            newly_named: meetings.iter().map(|m| m.newly_named).sum(),
            meetings,
            ..LearningReport::default()
        });
        job.input = input.to_value().unwrap();
        job.state = JobState::Done;
        std::fs::write(&job_file, serde_json::to_vec(&job).unwrap()).unwrap();
        job_file
    }

    /// Every recheck of a model reuses one job; the generation tells them
    /// apart. This app's request (generation 1) finishes and its end is
    /// said once; then a request made elsewhere (the CLI, simulated by
    /// writing its request and its finished state directly) runs and
    /// finishes as generation 2. Neither the open Speakers tab nor the
    /// review queue opened afterwards shows a live end Status for it or
    /// says anything; both still show its counts as non-live labels.
    #[gpui::test]
    fn a_recheck_requested_elsewhere_is_never_announced_as_this_apps(cx: &mut TestAppContext) {
        use broscribe_core::learning::{self, AffectedMeeting};
        use broscribe_core::{MeetingId, queue::JobState};

        let SpeakersTab {
            _dir,
            lib,
            s,
            model,
            state,
            announcer,
            window,
            shell,
            meeting,
        } = speakers_tab(cx);
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
            announcer.take()
        };
        // The recheck nodes of the Speakers tab and, once opened, of the
        // review queue, as (id, role, text).
        let named = |cx: &mut TestAppContext| {
            window
                .update(cx, |_, window, cx| {
                    let keep = |nodes: Vec<(String, Role, String)>, prefix: &str| {
                        nodes
                            .into_iter()
                            .filter(|(id, _, _)| id.starts_with(prefix))
                            .collect::<Vec<_>>()
                    };
                    let tab = meeting.update(cx, |view, cx| {
                        spoken_log::while_building(|| Render::render(view, window, cx)).1
                    });
                    let review = shell.read(cx).review_view().cloned().map(|review| {
                        review.update(cx, |view, cx| {
                            spoken_log::while_building(|| Render::render(view, window, cx)).1
                        })
                    });
                    (
                        keep(tab, "speaker-recheck"),
                        review.map(|nodes| keep(nodes, "review-recheck")),
                    )
                })
                .unwrap()
        };
        let node = |id: &str, role: Role, text: &str| (id.to_owned(), role, text.to_owned());
        // The library watch saw the queue change.
        let queue_changed = |cx: &mut TestAppContext| {
            state.update(cx, |st, cx| {
                st.library_changed(
                    &crate::watch::Changes {
                        queue: true,
                        ..Default::default()
                    },
                    cx,
                )
            });
            cx.run_until_parked();
        };

        // This app asks (generation 1) and the recheck finishes: the tab
        // says so once.
        let request = lib
            .write(|tx| learning::request(tx, &model, Some(&s.meeting)))
            .unwrap();
        assert_eq!(request.generation, 1);
        state.update(cx, |st, cx| st.recheck_requested(request.clone(), cx));
        cx.run_until_parked();
        draw(cx);
        finish_recheck(&lib, &request.job_id, Vec::new());
        queue_changed(cx);
        assert_eq!(
            draw(cx),
            [(
                crate::learning::FINISHED.to_owned(),
                AnnouncementPriority::Medium
            )]
        );

        // The CLI asks again (generation 2, the same job) and that recheck
        // runs: the tab goes back to checking without a word.
        let elsewhere = lib.write(|tx| learning::request(tx, &model, None)).unwrap();
        assert_eq!(elsewhere.job_id, request.job_id);
        assert_eq!(elsewhere.generation, 2);
        queue_changed(cx);
        assert!(draw(cx).is_empty());

        // It finishes, having named a speaker in another meeting.
        let other = MeetingId::random();
        finish_recheck(
            &lib,
            &request.job_id,
            vec![AffectedMeeting {
                meeting_id: other,
                source: false,
                newly_named: 1,
                replaced: 0,
                demoted: 0,
                suggested: 0,
                cleared: 0,
                changes: Vec::new(),
            }],
        );
        queue_changed(cx);
        let status = state.read_with(cx, |st, _| st.recheck().status.clone().unwrap());
        assert_eq!((status.requested, status.processed), (2, 2));
        assert_eq!(status.state, JobState::Done);
        // Not this app's request: nothing is said, and the tab shows the
        // counts only, as labels.
        assert!(draw(cx).is_empty());
        let (tab, review) = named(cx);
        assert!(review.is_none(), "Review is not open yet");
        assert_eq!(
            tab,
            [
                node(
                    "speaker-recheck-line-0",
                    Role::Label,
                    "This meeting: no other speaker changed."
                ),
                node(
                    "speaker-recheck-line-1",
                    Role::Label,
                    "Other meetings: 1 named automatically."
                ),
            ]
        );

        // Opening Review: no end Status there either and nothing about the
        // recheck is said, only the queue's own loading and waiting-count
        // lines, as on any opening; the counts are shown.
        state.update(cx, |st, cx| st.navigate(Route::ReviewQueue, cx));
        cx.run_until_parked();
        assert_eq!(
            draw(cx),
            [
                (
                    "Reading the review queue…".to_owned(),
                    AnnouncementPriority::Medium
                ),
                (
                    "3 speakers to review".to_owned(),
                    AnnouncementPriority::Medium
                ),
            ]
        );
        assert_eq!(
            named(cx).1.expect("Review is open"),
            [node(
                "review-recheck-line-0",
                Role::Label,
                "Other meetings: 1 named automatically."
            )]
        );
        assert!(draw(cx).is_empty());
    }

    /// Any other job's footer line is its name and step, live, with its
    /// percentage in a non-live label beside it: a moving percentage says
    /// nothing, and the line speaks again only when the step changes.
    #[gpui::test]
    fn a_job_speaks_again_for_a_new_step_not_for_its_percentage(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeAnnouncer;

        let dir = tempfile::tempdir().unwrap();
        let state = shell_state(cx, dir.path());
        state.update(cx, |s, cx| s.set_local_jobs_allowed(false, cx));
        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| {
            gpui::init(cx);
            install_announcer(announcer.clone(), cx);
        });
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let shell = Rc::new(std::cell::RefCell::new(None));
        let window = cx.add_window({
            let shell = shell.clone();
            let state = state.clone();
            move |window, cx| {
                let main = cx.new(|cx| MainWindow::new(state, deps, window, cx));
                *shell.borrow_mut() = Some(main.clone());
                Root::new(main, window, cx)
            }
        });
        let shell = shell.borrow().clone().expect("the main window's view");
        cx.run_until_parked();
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
            announcer.take()
        };
        let step = |message: &str, fraction: f64, cx: &mut TestAppContext| {
            state.update(cx, |st, cx| {
                st.daemon_progress(
                    &broscribe_ipc::JobProgress {
                        job_id: "j-synthetic".into(),
                        kind: "enrich".into(),
                        meeting_id: None,
                        phase: "progress".into(),
                        fraction: Some(fraction),
                        message: Some(message.into()),
                    },
                    cx,
                )
            });
            cx.run_until_parked();
            draw(cx)
        };
        let spoken = |text: &str| vec![(text.to_owned(), AnnouncementPriority::Medium)];
        // The library's own status settles first.
        draw(cx);
        assert!(draw(cx).is_empty());

        assert_eq!(
            step("Reading the transcript", 0.1, cx),
            spoken("Generating summary · Reading the transcript")
        );
        for fraction in [0.4, 0.7] {
            assert!(
                step("Reading the transcript", fraction, cx).is_empty(),
                "{fraction} stays quiet"
            );
        }
        let footer = window
            .update(cx, |_, window, cx| {
                shell.update(cx, |view, cx| {
                    spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
            })
            .unwrap();
        let footer: Vec<_> = footer
            .into_iter()
            .filter(|(id, _, _)| id.starts_with("jobs-"))
            .collect();
        assert_eq!(
            footer,
            [
                (
                    "jobs-active-j-synthetic".to_owned(),
                    Role::Status,
                    "Generating summary · Reading the transcript".to_owned()
                ),
                (
                    "jobs-active-j-synthetic-detail".to_owned(),
                    Role::Label,
                    "· 70%".to_owned()
                ),
            ]
        );
        assert_eq!(
            step("Writing the summary", 0.8, cx),
            spoken("Generating summary · Writing the summary")
        );
        assert!(step("Writing the summary", 0.95, cx).is_empty());
        assert!(draw(cx).is_empty());
    }

    /// Every Rust source file that can contain shipping UI, with its path under `src`.
    fn interface_sources() -> Vec<(String, String)> {
        rust_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"))
    }

    fn rust_sources(src: &Path) -> Vec<(String, String)> {
        fn collect(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(&path, found);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    found.push(path);
                }
            }
        }
        let mut files = Vec::new();
        collect(src, &mut files);
        files.sort();
        files
            .into_iter()
            .filter_map(|path| {
                let name = path
                    .strip_prefix(src)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if matches!(
                    name.as_str(),
                    "test_support.rs"
                        | "ui/tests/copy_gates.rs"
                        | "ui/transient_editors.rs"
                        | "ui/u56_tests.rs"
                ) {
                    return None;
                }
                Some((name, std::fs::read_to_string(&path).unwrap()))
            })
            .collect()
    }

    #[test]
    fn action_buttons_use_the_shared_component_and_only_links_use_a_hand_cursor() {
        for (name, source) in interface_sources() {
            let code = strip_comments_and_strings(&source);
            for retired in [
                "control_chrome(",
                "action_button(",
                "fn action_button<",
                "generate::control",
            ] {
                assert!(!code.contains(retired), "{retired} in {name}");
            }
            if name != "ui/rich_text.rs" {
                assert!(!code.contains(".cursor_pointer("), "hand cursor in {name}");
            }
        }
    }

    #[test]
    fn choice_labels_do_not_draw_radio_or_checkbox_glyphs() {
        let forbidden = ['\u{2611}', '\u{2610}', '\u{25c9}', '\u{25cb}'];
        for (path, source) in interface_sources() {
            assert!(
                !source
                    .chars()
                    .any(|character| forbidden.contains(&character)),
                "{path}"
            );
        }
    }

    #[test]
    fn custom_focus_styles_come_from_the_kit() {
        for (name, source) in interface_sources() {
            let code: String = strip_comments_and_strings(&shipping_text(&source))
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect();
            for method in [".focus(", ".focus_visible(", ".focus_within("] {
                for tail in code.split(method).skip(1) {
                    // FocusHandle::focus moves focus. Builder calls style it.
                    let moves_focus = !tail.starts_with('|')
                        && !tail.starts_with("move|")
                        && method == ".focus("
                        && !tail.starts_with("crate::ui::kit::");
                    assert!(
                        moves_focus
                            || tail.starts_with("crate::ui::kit::focus_halo(")
                            || (name == "ui/kit.rs" && tail.starts_with("focus_halo(")),
                        "{name}: {method}{}",
                        &tail[..tail.len().min(70)]
                    );
                }
            }
        }
    }

    #[test]
    fn only_region_actions_use_primary_and_destructive_openers_use_outline() {
        for (name, source) in interface_sources() {
            let production: String = strip_comments_and_strings(&shipping_text(&source))
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect();
            let expected = match name.as_str() {
                "ui.rs" | "views/update.rs" => 2,
                "views/integrations.rs" => 1,
                "views/panel/view.rs" => 9,
                "ui/record_control.rs" | "views/import_sheet.rs" => 1,
                // Setup's footer, discard confirmation and mutually exclusive capture actions.
                "views/setup.rs" => 8,
                // The Settings regions each have their own affirmative action.
                "views/settings.rs" => 5,
                "views/providers.rs" | "views/templates.rs" => 1,
                // A confirmation's affirmative action and an empty state's action.
                "ui/kit.rs" => 2,
                // Export confirms the modal, and Save confirms the title editor.
                "views/export.rs" | "views/meeting/header.rs" => 1,
                // The selected speaker has one primary decision.
                "views/meeting/speakers.rs" | "views/review.rs" => 1,
                // Save is the affirmative action of the due-date editor.
                "views/meeting/overview.rs" => 1,
                // Save in the editor and Keep editing in its discard prompt.
                "views/people.rs" => 2,
                _ => 0,
            };
            // Primary actions go through `kit::primary`, which mutes a
            // disabled one and gives the rest the primary shadow; only that
            // helper calls the component's own `.primary()`.
            let raw = production.matches(".primary()").count();
            assert_eq!(
                raw,
                usize::from(name == "ui/kit.rs"),
                "{name}: raw .primary()"
            );
            let primaries = production.matches("kit::primary(").count()
                + production.matches("self::primary(").count();
            assert_eq!(primaries, expected, "{name}");
            if name != "ui/kit.rs" && name != "views/update.rs" {
                assert!(
                    production
                        .split(".danger()")
                        .skip(1)
                        .all(|tail| tail.starts_with(".outline()")),
                    "{name}"
                );
            }
        }
    }

    /// Blanks `slice` byte for byte (a space per byte, `\n` kept), so
    /// offsets and line numbers of the result match the original.
    fn push_blanked(out: &mut String, slice: &str) {
        for ch in slice.chars() {
            if ch == '\n' {
                out.push('\n');
            } else {
                out.extend(std::iter::repeat_n(' ', ch.len_utf8()));
            }
        }
    }

    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    fn raw_string_prefix(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
        if i > 0 && is_ident_byte(bytes[i - 1]) {
            return None;
        }
        let head = match bytes.get(i..) {
            Some([b'b', b'r', ..]) | Some([b'c', b'r', ..]) => 2,
            Some([b'r', ..]) => 1,
            _ => return None,
        };
        let hashes = bytes[i + head..].iter().take_while(|&&b| b == b'#').count();
        (bytes.get(i + head + hashes) == Some(&b'"')).then_some((head + hashes + 1, hashes))
    }

    fn string_prefix(bytes: &[u8], i: usize) -> Option<usize> {
        if bytes[i] == b'"' {
            return Some(1);
        }
        if (i == 0 || !is_ident_byte(bytes[i - 1]))
            && matches!(bytes.get(i..), Some([b'b' | b'c', b'"', ..]))
        {
            return Some(2);
        }
        None
    }

    fn char_literal_len(bytes: &[u8], i: usize) -> Option<usize> {
        let head = if bytes[i] == b'\'' {
            1
        } else if (i == 0 || !is_ident_byte(bytes[i - 1]))
            && matches!(bytes.get(i..), Some([b'b', b'\'', ..]))
        {
            2
        } else {
            return None;
        };
        let rest = &bytes[i + head..];
        if rest.first() == Some(&b'\\') {
            let mut j = 1;
            while j < rest.len() && rest[j] != b'\n' && j <= 16 {
                if rest[j] == b'\'' && !(j == 1) {
                    return Some(head + j + 1);
                }
                if j == 1 && rest[j] == b'\\' && rest.get(j + 1) == Some(&b'\'') {
                    return Some(head + 3);
                }
                j += 1;
            }
            return None;
        }
        let s = std::str::from_utf8(rest).ok()?;
        let ch = s.chars().next()?;
        if ch != '\'' && ch != '\n' && rest.get(ch.len_utf8()) == Some(&b'\'') {
            Some(head + ch.len_utf8() + 1)
        } else {
            None
        }
    }

    /// Replaces comments, strings and char literals with spaces while keeping
    /// every `\n` so line numbers match the original file.
    fn strip_comments_and_strings(source: &str) -> String {
        strip_source(source, false, |_| {})
    }

    /// Replaces comments with spaces; strings and char literals stay.
    fn strip_comments(source: &str) -> String {
        strip_source(source, true, |_| {})
    }

    fn strip_source(
        source: &str,
        keep_literals: bool,
        mut on_string: impl FnMut(std::ops::Range<usize>),
    ) -> String {
        let keep = |out: &mut String, slice: &str| {
            if keep_literals {
                out.push_str(slice);
            } else {
                push_blanked(out, slice);
            }
        };
        let bytes = source.as_bytes();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i..].starts_with(b"//") {
                let end = bytes[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |off| i + off);
                push_blanked(&mut out, &source[i..end]);
                i = end;
            } else if bytes[i..].starts_with(b"/*") {
                let start = i;
                let mut depth = 1usize;
                i += 2;
                while i < bytes.len() && depth > 0 {
                    if bytes[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        let ch = source[i..].chars().next().unwrap();
                        i += ch.len_utf8();
                    }
                }
                push_blanked(&mut out, &source[start..i]);
            } else if let Some((prefix, hashes)) = raw_string_prefix(bytes, i) {
                let start = i;
                i += prefix;
                while i < bytes.len() {
                    if bytes[i] == b'"'
                        && bytes.len() >= i + 1 + hashes
                        && bytes[i + 1..i + 1 + hashes].iter().all(|&b| b == b'#')
                    {
                        i += 1 + hashes;
                        break;
                    }
                    let ch = source[i..].chars().next().unwrap();
                    i += ch.len_utf8();
                }
                on_string(start + prefix..i - hashes - 1);
                keep(&mut out, &source[start..i]);
            } else if let Some(prefix) = string_prefix(bytes, i) {
                let start = i;
                i += prefix;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 1;
                        if i < bytes.len() {
                            let ch = source[i..].chars().next().unwrap();
                            i += ch.len_utf8();
                        }
                    } else if bytes[i] == b'"' {
                        i += 1;
                        break;
                    } else {
                        let ch = source[i..].chars().next().unwrap();
                        i += ch.len_utf8();
                    }
                }
                on_string(start + prefix..i - 1);
                keep(&mut out, &source[start..i]);
            } else if let Some(len) = char_literal_len(bytes, i) {
                keep(&mut out, &source[i..i + len]);
                i += len;
            } else {
                let ch = source[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        out
    }

    fn cfg_tokens(s: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut chars = s.chars().peekable();
        while let Some(&ch) = chars.peek() {
            if ch.is_ascii_whitespace() {
                chars.next();
            } else if ch.is_ascii_alphanumeric() || ch == '_' {
                let mut ident = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        ident.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(ident);
            } else {
                tokens.push(ch.to_string());
                chars.next();
            }
        }
        tokens
    }

    fn split_cfg_args(tokens: &[String]) -> Vec<&[String]> {
        let mut groups = Vec::new();
        let mut depth = 0usize;
        let mut start = 0usize;
        for (idx, tok) in tokens.iter().enumerate() {
            match tok.as_str() {
                "(" => depth += 1,
                ")" => depth = depth.saturating_sub(1),
                "," if depth == 0 => {
                    groups.push(&tokens[start..idx]);
                    start = idx + 1;
                }
                _ => {}
            }
        }
        if start < tokens.len() {
            groups.push(&tokens[start..]);
        }
        groups
    }

    fn is_cfg_test_predicate(tokens: &[String]) -> bool {
        if tokens == ["test"] {
            return true;
        }
        if tokens.len() >= 4
            && tokens[0] == "all"
            && tokens[1] == "("
            && tokens.last().map(String::as_str) == Some(")")
        {
            return split_cfg_args(&tokens[2..tokens.len() - 1])
                .into_iter()
                .any(is_cfg_test_predicate);
        }
        if tokens.len() >= 4
            && tokens[0] == "any"
            && tokens[1] == "("
            && tokens.last().map(String::as_str) == Some(")")
        {
            let args = split_cfg_args(&tokens[2..tokens.len() - 1]);
            return !args.is_empty() && args.into_iter().all(is_cfg_test_predicate);
        }
        false
    }

    fn is_cfg_test_attr(attr_inner: &str) -> bool {
        let tokens = cfg_tokens(attr_inner);
        tokens.len() >= 4
            && tokens[0] == "cfg"
            && tokens[1] == "("
            && tokens.last().map(String::as_str) == Some(")")
            && is_cfg_test_predicate(&tokens[2..tokens.len() - 1])
    }

    fn parse_outer_attr(bytes: &[u8], hash_pos: usize) -> Option<(usize, &str)> {
        let mut j = hash_pos + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if bytes.get(j) != Some(&b'[') {
            return None;
        }
        let open = j;
        let mut depth = 1usize;
        j += 1;
        while j < bytes.len() && depth > 0 {
            match bytes[j] {
                b'[' => depth += 1,
                b']' => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        if depth != 0 {
            return None;
        }
        let inner = std::str::from_utf8(&bytes[open + 1..j - 1]).ok()?;
        Some((j, inner))
    }

    fn find_attributed_item_end(bytes: &[u8], mut j: usize) -> usize {
        loop {
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'#')
                && let Some((after, _)) = parse_outer_attr(bytes, j)
            {
                j = after;
                continue;
            }
            break;
        }
        let mut paren = 0usize;
        let mut bracket = 0usize;
        let mut brace = 0usize;
        let mut saw_brace = false;
        let mut saw_top_eq = false;
        while j < bytes.len() {
            match bytes[j] {
                b'(' => paren += 1,
                b')' => paren = paren.saturating_sub(1),
                b'[' => bracket += 1,
                b']' => bracket = bracket.saturating_sub(1),
                b'=' if paren == 0 && bracket == 0 && brace == 0 => {
                    let prev = j.checked_sub(1).and_then(|k| bytes.get(k).copied());
                    let next = bytes.get(j + 1).copied();
                    if !matches!(prev, Some(b'=' | b'!' | b'<' | b'>'))
                        && !matches!(next, Some(b'=' | b'>'))
                    {
                        saw_top_eq = true;
                    }
                }
                b'{' if paren == 0 && bracket == 0 => {
                    brace += 1;
                    saw_brace = true;
                }
                b'}' if paren == 0 && bracket == 0 => {
                    brace = brace.saturating_sub(1);
                    if brace == 0 && saw_brace && !saw_top_eq {
                        return j + 1;
                    }
                }
                b';' if paren == 0 && bracket == 0 && brace == 0 => {
                    return j + 1;
                }
                _ => {}
            }
            j += 1;
        }
        bytes.len()
    }

    mod motion_scan {
        use std::collections::HashMap;

        use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
        use syn::ext::IdentExt as _;
        use syn::parse::ParseStream;
        use syn::punctuated::Punctuated;
        use syn::visit::{self, Visit};
        use syn::visit_mut::{self, VisitMut};
        use syn::{
            Attribute, Block, Expr, FnArg, ForeignItem, GenericParam, Ident, ImplItem, Item, Lit,
            LitBool, Pat, Path, QSelf, Stmt, Token, TraitItem, Type, UseTree, parenthesized, token,
        };

        /// The motion in an app source that ships: animated loaders,
        /// animations, direct frame requests and reduced-motion bypasses, and,
        /// unless `theme` (the file is `ui/theme.rs`), GPUI or its motion set
        /// up outside `ui::theme::init`. Each finding is a line and the needle
        /// found there, once per line.
        ///
        /// The file is parsed with `syn`, and every node whose `#[cfg(…)]` can
        /// only hold when `test` is set is left out: items (a `mod tests`
        /// included, or the whole file under `#![cfg(test)]`), impl, trait and
        /// foreign items, struct and enum fields and variants, generic, fn and
        /// closure parameters, statements, match arms, struct-literal and pattern
        /// fields, arguments and array or tuple elements. A test-only expression
        /// anywhere else is emptied.
        ///
        /// What is left is read as a syntax tree (see `Finder`), never as
        /// spelled:
        /// - Every path (in an expression, call, type, pattern, struct literal,
        ///   bound, macro or attribute name) is read as its names joined by
        ///   `::`. Generic arguments are left out, and a qualified self stands
        ///   for the type it names (`<Spinner>::new` and `<Spinner as T>::new`
        ///   read `Spinner::new`).
        /// - A path whose first name the file imports is also read as the path
        ///   that name stands for, and after each glob the file imports
        ///   (`init` after `use gpui::*` reads `gpui::init`). Imports are
        ///   walked whole, grouped, nested, with `self` and renamed (`use
        ///   gpui::{App, init as start}`), and a type alias counts as one. Each
        ///   imported path is read too, on the line of its last name.
        /// - A method call is read by its name, turbofish left out, and a
        ///   needle that is one name (`Shimmer`, `set_reduce_motion`) by any
        ///   identifier that contains it.
        /// - Tokens `syn` keeps unparsed (macro bodies, attribute arguments) are
        ///   printed back on their source lines with every literal as `0`, and
        ///   the needles are matched on that text. A macro body that parses as
        ///   expressions or statements is also read as code.
        ///
        /// So comments, doc comments and strings never match, and a finding's
        /// line is the line it has in the file.
        ///
        /// Limits:
        /// - A `#[cfg(test)]` inside a macro body (`vec![…]`, `div! { … }`) is
        ///   not stripped, and what it gates is flagged: a false alarm at
        ///   worst, never production code skipped.
        /// - A cfg is test-only when it is false with `test` off whatever the
        ///   other options are (`test`, `all(test, …)`, `any(test, all(test,
        ///   …))`, `not(not(test))`, `any()`); anything less certain is scanned.
        ///   `cfg_attr` is not expanded.
        /// - A file whose `mod` declaration is test-only is still scanned unless
        ///   `interface_sources` leaves it out, as it does `test_support.rs`.
        /// - Names are followed by name, within the file and without scopes:
        ///   an import, alias or glob anywhere in it applies to every path in
        ///   it (a false alarm at worst). `Self`, an import or alias in another
        ///   file (`pub use … Spinner as Wait` there) and a name a macro
        ///   builds are not followed.
        ///
        /// Fails if the file doesn't parse.
        pub(super) fn motion(source: &str, theme: bool) -> syn::Result<Vec<(usize, &'static str)>> {
            let mut file = syn::parse_file(source)?;
            LeaveOutTestOnly.visit_file_mut(&mut file);
            let mut imports = Imports::default();
            imports.visit_file(&file);
            let imported = std::mem::take(&mut imports.paths);
            let mut finder = Finder {
                imports,
                needles: PATHS
                    .into_iter()
                    .chain(if theme { [].iter() } else { THEME_PATHS.iter() }.copied())
                    .collect(),
                found: Vec::new(),
            };
            for (names, line) in &imported {
                finder.path(names, *line);
            }
            finder.visit_file(&file);
            finder.found.sort_unstable();
            finder.found.dedup();
            Ok(finder.found)
        }

        /// Parses a cfg predicate (`test`, `unix`, `feature = "x"`, `true`,
        /// `false`, `all(…)`, `any(…)`, `not(…)`) and says what it is with
        /// `test` off: `Some(false)` when it is false whatever the other
        /// options are, `Some(true)` when it is true whatever they are, `None`
        /// when it depends on them. That is Kleene's three-valued logic, so a
        /// `Some` is never wrong.
        fn holds_without_test(input: ParseStream) -> syn::Result<Option<bool>> {
            if input.peek(LitBool) {
                return Ok(Some(input.parse::<LitBool>()?.value));
            }
            let name = input.call(Ident::parse_any)?;
            if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                input.parse::<Lit>()?;
                return Ok(None);
            }
            if !input.peek(token::Paren) {
                return Ok((name == "test").then_some(false));
            }
            let args;
            parenthesized!(args in input);
            let values: Vec<Option<bool>> =
                Punctuated::<_, Token![,]>::parse_terminated_with(&args, holds_without_test)?
                    .into_iter()
                    .collect();
            let every = |value| values.iter().all(|&v| v == Some(value));
            Ok(if name == "all" {
                if values.contains(&Some(false)) {
                    Some(false)
                } else {
                    every(true).then_some(true)
                }
            } else if name == "any" {
                if values.contains(&Some(true)) {
                    Some(true)
                } else {
                    every(false).then_some(false)
                }
            } else if name == "not" && values.len() == 1 {
                values[0].map(|value| !value)
            } else {
                None
            })
        }

        /// Whether `attrs` hold a `#[cfg(…)]` (or `#![cfg(…)]`) that can only
        /// hold when `test` is set. A cfg that doesn't parse may ship.
        fn test_only(attrs: &[Attribute]) -> bool {
            attrs.iter().any(|attr| {
                attr.path().is_ident("cfg")
                    && attr.parse_args_with(holds_without_test).ok() == Some(Some(false))
            })
        }

        /// The attributes of whichever variant `$node` is; none for a
        /// variant `syn` keeps as raw tokens (`Verbatim`), which the
        /// exhaustive enums don't have.
        macro_rules! attrs_of {
            ($node:expr, $kind:ident: $($variant:ident)*) => {
                match $node {
                    $($kind::$variant(node) => &node.attrs[..],)*
                    #[allow(unreachable_patterns)]
                    _ => &[],
                }
            };
        }

        fn item_attrs(item: &Item) -> &[Attribute] {
            attrs_of!(item, Item: Const Enum ExternCrate Fn ForeignMod Impl Macro Mod Static
                Struct Trait TraitAlias Type Union Use)
        }

        fn expr_attrs(expr: &Expr) -> &[Attribute] {
            attrs_of!(expr, Expr: Array Assign Async Await Binary Block Break Call Cast Closure
                Const Continue Field ForLoop Group If Index Infer Let Lit Loop Macro Match
                MethodCall Paren Path Range RawAddr Reference Repeat Return Struct Try TryBlock
                Tuple Unary Unsafe While Yield)
        }

        fn pat_attrs(pat: &Pat) -> &[Attribute] {
            attrs_of!(pat, Pat: Const Ident Lit Macro Or Paren Path Range Reference Rest Slice
                Struct Tuple TupleStruct Type Wild)
        }

        /// Whether an expression is test-only. `syn` puts the attributes of
        /// an expression statement or list element on its leftmost operand
        /// (`#[cfg(test)] a + b` on `a`), so they are looked for down that
        /// side.
        fn test_only_expr(mut expr: &Expr) -> bool {
            loop {
                if test_only(expr_attrs(expr)) {
                    return true;
                }
                expr = match expr {
                    Expr::Assign(e) => &e.left,
                    Expr::Binary(e) => &e.left,
                    Expr::Cast(e) => &e.expr,
                    // `..` parses as a binary operator too: the attribute
                    // of `#[cfg(test)] a..b` sits on the start `a`, not on
                    // the range. (`syn` rejects one before a range with no
                    // start, `#[cfg(test)] ..b`.)
                    Expr::Range(syn::ExprRange {
                        start: Some(start), ..
                    }) => start,
                    _ => return false,
                };
            }
        }

        fn test_only_stmt(stmt: &Stmt) -> bool {
            match stmt {
                Stmt::Local(local) => test_only(&local.attrs),
                Stmt::Item(item) => test_only(item_attrs(item)),
                Stmt::Expr(expr, _) => test_only_expr(expr),
                Stmt::Macro(mac) => test_only(&mac.attrs),
            }
        }

        fn retain<T, P>(list: &mut Punctuated<T, P>, test_only: impl Fn(&T) -> bool) {
            *list = std::mem::take(list)
                .into_pairs()
                .filter(|pair| !test_only(pair.value()))
                .collect();
        }

        /// Removes each test-only node from the list that holds it, then
        /// visits what is left.
        struct LeaveOutTestOnly;

        impl VisitMut for LeaveOutTestOnly {
            fn visit_file_mut(&mut self, file: &mut syn::File) {
                if test_only(&file.attrs) {
                    file.items.clear();
                }
                file.items.retain(|item| !test_only(item_attrs(item)));
                visit_mut::visit_file_mut(self, file);
            }

            fn visit_item_mod_mut(&mut self, module: &mut syn::ItemMod) {
                if let Some((_, items)) = &mut module.content {
                    items.retain(|item| !test_only(item_attrs(item)));
                }
                visit_mut::visit_item_mod_mut(self, module);
            }

            fn visit_item_impl_mut(&mut self, block: &mut syn::ItemImpl) {
                block
                    .items
                    .retain(|item| !test_only(attrs_of!(item, ImplItem: Const Fn Type Macro)));
                visit_mut::visit_item_impl_mut(self, block);
            }

            fn visit_item_trait_mut(&mut self, block: &mut syn::ItemTrait) {
                block
                    .items
                    .retain(|item| !test_only(attrs_of!(item, TraitItem: Const Fn Type Macro)));
                visit_mut::visit_item_trait_mut(self, block);
            }

            fn visit_item_foreign_mod_mut(&mut self, block: &mut syn::ItemForeignMod) {
                block
                    .items
                    .retain(|item| !test_only(attrs_of!(item, ForeignItem: Fn Static Type Macro)));
                visit_mut::visit_item_foreign_mod_mut(self, block);
            }

            fn visit_block_mut(&mut self, block: &mut syn::Block) {
                block.stmts.retain(|stmt| !test_only_stmt(stmt));
                visit_mut::visit_block_mut(self, block);
            }

            fn visit_fields_named_mut(&mut self, fields: &mut syn::FieldsNamed) {
                retain(&mut fields.named, |field| test_only(&field.attrs));
                visit_mut::visit_fields_named_mut(self, fields);
            }

            fn visit_fields_unnamed_mut(&mut self, fields: &mut syn::FieldsUnnamed) {
                retain(&mut fields.unnamed, |field| test_only(&field.attrs));
                visit_mut::visit_fields_unnamed_mut(self, fields);
            }

            fn visit_item_enum_mut(&mut self, item: &mut syn::ItemEnum) {
                retain(&mut item.variants, |variant| test_only(&variant.attrs));
                visit_mut::visit_item_enum_mut(self, item);
            }

            fn visit_generics_mut(&mut self, generics: &mut syn::Generics) {
                retain(&mut generics.params, |param| {
                    test_only(attrs_of!(param, GenericParam: Type Lifetime Const))
                });
                visit_mut::visit_generics_mut(self, generics);
            }

            fn visit_bound_lifetimes_mut(&mut self, bound: &mut syn::BoundLifetimes) {
                retain(&mut bound.lifetimes, |param| {
                    test_only(attrs_of!(param, GenericParam: Type Lifetime Const))
                });
                visit_mut::visit_bound_lifetimes_mut(self, bound);
            }

            fn visit_signature_mut(&mut self, signature: &mut syn::Signature) {
                retain(&mut signature.inputs, |arg| {
                    test_only(attrs_of!(arg, FnArg: Receiver Typed))
                });
                if signature
                    .variadic
                    .as_ref()
                    .is_some_and(|variadic| test_only(&variadic.attrs))
                {
                    signature.variadic = None;
                }
                visit_mut::visit_signature_mut(self, signature);
            }

            fn visit_type_bare_fn_mut(&mut self, ty: &mut syn::TypeBareFn) {
                retain(&mut ty.inputs, |arg| test_only(&arg.attrs));
                if ty
                    .variadic
                    .as_ref()
                    .is_some_and(|variadic| test_only(&variadic.attrs))
                {
                    ty.variadic = None;
                }
                visit_mut::visit_type_bare_fn_mut(self, ty);
            }

            fn visit_expr_closure_mut(&mut self, closure: &mut syn::ExprClosure) {
                retain(&mut closure.inputs, |pat| test_only(pat_attrs(pat)));
                visit_mut::visit_expr_closure_mut(self, closure);
            }

            fn visit_expr_match_mut(&mut self, expr: &mut syn::ExprMatch) {
                expr.arms.retain(|arm| !test_only(&arm.attrs));
                visit_mut::visit_expr_match_mut(self, expr);
            }

            fn visit_expr_struct_mut(&mut self, expr: &mut syn::ExprStruct) {
                retain(&mut expr.fields, |field| test_only(&field.attrs));
                visit_mut::visit_expr_struct_mut(self, expr);
            }

            fn visit_expr_call_mut(&mut self, expr: &mut syn::ExprCall) {
                retain(&mut expr.args, test_only_expr);
                visit_mut::visit_expr_call_mut(self, expr);
            }

            fn visit_expr_method_call_mut(&mut self, expr: &mut syn::ExprMethodCall) {
                retain(&mut expr.args, test_only_expr);
                visit_mut::visit_expr_method_call_mut(self, expr);
            }

            fn visit_expr_array_mut(&mut self, expr: &mut syn::ExprArray) {
                retain(&mut expr.elems, test_only_expr);
                visit_mut::visit_expr_array_mut(self, expr);
            }

            fn visit_expr_tuple_mut(&mut self, expr: &mut syn::ExprTuple) {
                retain(&mut expr.elems, test_only_expr);
                visit_mut::visit_expr_tuple_mut(self, expr);
            }

            fn visit_expr_mut(&mut self, expr: &mut Expr) {
                if test_only(expr_attrs(expr)) {
                    *expr = Expr::Verbatim(TokenStream::new());
                } else {
                    visit_mut::visit_expr_mut(self, expr);
                }
            }

            fn visit_pat_struct_mut(&mut self, pat: &mut syn::PatStruct) {
                retain(&mut pat.fields, |field| test_only(&field.attrs));
                visit_mut::visit_pat_struct_mut(self, pat);
            }

            fn visit_pat_tuple_mut(&mut self, pat: &mut syn::PatTuple) {
                retain(&mut pat.elems, |pat| test_only(pat_attrs(pat)));
                visit_mut::visit_pat_tuple_mut(self, pat);
            }

            fn visit_pat_tuple_struct_mut(&mut self, pat: &mut syn::PatTupleStruct) {
                retain(&mut pat.elems, |pat| test_only(pat_attrs(pat)));
                visit_mut::visit_pat_tuple_struct_mut(self, pat);
            }

            fn visit_pat_slice_mut(&mut self, pat: &mut syn::PatSlice) {
                retain(&mut pat.elems, |pat| test_only(pat_attrs(pat)));
                visit_mut::visit_pat_slice_mut(self, pat);
            }
        }

        /// Loaders, animations and reduced-motion bypasses, matched as text
        /// in a path's names joined by `::`; a needle that is one name is
        /// also matched in every identifier.
        const PATHS: [&str; 5] = [
            "Spinner::new",
            "Skeleton::new",
            "Shimmer",
            "Animation::new",
            "ReducedMotion::Continue",
        ];

        /// GPUI and motion set up, allowed only in `ui/theme.rs`; matched as
        /// `PATHS` are.
        const THEME_PATHS: [&str; 3] = ["gpui::init", "gpui_kit::init", "set_reduce_motion"];

        /// Methods and functions that animate or request frames, called or
        /// named, whatever their arguments, with the needle reported.
        const CALLS: [(&str, &str); 4] = [
            ("with_animation", "with_animation("),
            ("with_animations", "with_animations("),
            ("with_spring", "with_spring("),
            ("request_animation_frame", "request_animation_frame("),
        ];

        /// The needles for `loading`, which shows a loader when it takes an
        /// argument and reads a state when it doesn't (`search.loading()`).
        const LOADING_METHOD: &str = ".loading(";
        const LOADING_FN: &str = "::loading(";

        /// The names of `path`, generic arguments left out. With a qualified
        /// self, the names of its type come first and the trait's are left
        /// out (`<Spinner as T>::new` is `Spinner`, `new`).
        fn path_names(qself: Option<&QSelf>, path: &Path) -> Vec<String> {
            let mut names = Vec::new();
            let mut skip = 0;
            if let Some(qself) = qself {
                let mut ty = &*qself.ty;
                while let Type::Paren(syn::TypeParen { elem, .. })
                | Type::Group(syn::TypeGroup { elem, .. }) = ty
                {
                    ty = elem;
                }
                if let Type::Path(ty) = ty {
                    names = path_names(ty.qself.as_ref(), &ty.path);
                }
                skip = qself.position;
            }
            names.extend(
                path.segments
                    .iter()
                    .skip(skip)
                    .map(|segment| segment.ident.to_string()),
            );
            names
        }

        /// The line of `path`'s last name.
        fn line_of(path: &Path) -> usize {
            path.segments
                .last()
                .map_or(0, |segment| segment.ident.span().start().line)
        }

        /// What the names a file imports stand for, wherever the `use` or
        /// type alias is in it.
        #[derive(Default)]
        struct Imports {
            /// Each name, with the paths (names joined by `::`) it stands for.
            names: HashMap<String, Vec<String>>,
            /// The path before each glob (`gpui` for `use gpui::*`).
            globs: Vec<String>,
            /// Each imported path, with the line of its last name.
            paths: Vec<(Vec<String>, usize)>,
        }

        impl Imports {
            fn add(&mut self, tree: &UseTree, prefix: &mut Vec<String>) {
                match tree {
                    UseTree::Path(tree) => {
                        prefix.push(tree.ident.to_string());
                        self.add(&tree.tree, prefix);
                        prefix.pop();
                    }
                    UseTree::Name(tree) => self.add_name(prefix, &tree.ident, &tree.ident),
                    UseTree::Rename(tree) => self.add_name(prefix, &tree.ident, &tree.rename),
                    UseTree::Glob(_) => self.globs.push(prefix.join("::")),
                    UseTree::Group(group) => {
                        for tree in &group.items {
                            self.add(tree, prefix);
                        }
                    }
                }
            }

            /// `item` imported from `prefix` as `name` (`self` is `prefix`
            /// itself, and `_` names nothing).
            fn add_name(&mut self, prefix: &[String], item: &Ident, name: &Ident) {
                let mut path = prefix.to_vec();
                if item != "self" {
                    path.push(item.to_string());
                }
                let name = if name == "self" {
                    prefix.last().cloned()
                } else {
                    Some(name.to_string())
                };
                if let Some(name) = name.filter(|name| name != "_") {
                    self.names.entry(name).or_default().push(path.join("::"));
                }
                self.paths.push((path, item.span().start().line));
            }

            /// The paths `names` may stand for: as written, through the
            /// import of its first name, and after each glob.
            fn resolve(&self, names: &[String]) -> Vec<String> {
                let path = names.join("::");
                let mut paths = Vec::new();
                if let Some((first, rest)) = names.split_first() {
                    for imported in self.names.get(first).into_iter().flatten() {
                        paths.push(if rest.is_empty() {
                            imported.clone()
                        } else {
                            format!("{imported}::{}", rest.join("::"))
                        });
                    }
                }
                paths.extend(self.globs.iter().map(|glob| format!("{glob}::{path}")));
                paths.push(path);
                paths
            }
        }

        impl<'ast> Visit<'ast> for Imports {
            fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
                self.add(&item.tree, &mut Vec::new());
            }

            fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
                if let Type::Path(ty) = &*item.ty {
                    let path = path_names(ty.qself.as_ref(), &ty.path).join("::");
                    self.names
                        .entry(item.ident.to_string())
                        .or_default()
                        .push(path);
                }
                visit::visit_item_type(self, item);
            }
        }

        /// Finds the needles in what a file ships, on its syntax tree (see
        /// `motion`).
        struct Finder {
            imports: Imports,
            /// `PATHS`, and `THEME_PATHS` outside `ui/theme.rs`.
            needles: Vec<&'static str>,
            found: Vec<(usize, &'static str)>,
        }

        impl Finder {
            /// Matches a path, given as its names, on `line`.
            fn path(&mut self, names: &[String], line: usize) {
                let mut found = Vec::new();
                for path in self.imports.resolve(names) {
                    let needles = self.needles.iter().copied();
                    found.extend(needles.filter(|&needle| path.contains(needle)));
                    let last = path.rsplit("::").next();
                    found.extend(
                        CALLS
                            .into_iter()
                            .filter(|&(name, _)| Some(name) == last)
                            .map(|(_, needle)| needle),
                    );
                }
                self.found
                    .extend(found.into_iter().map(|needle| (line, needle)));
            }

            /// A path with a qualified self (`<Spinner>::new`), which is also
            /// read as written (`visit_path`).
            fn qualified_path(&mut self, qself: Option<&QSelf>, path: &Path) {
                if qself.is_some() {
                    self.path(&path_names(qself, path), line_of(path));
                }
            }

            /// Matches the needles as text on `tokens` printed back (see
            /// `print`), for what `syn` doesn't parse.
            fn text(&mut self, tokens: TokenStream) {
                let Some(first) = tokens.clone().into_iter().next() else {
                    return;
                };
                let first_line = first.span().start().line;
                let mut text = String::new();
                print(tokens, &mut text, &mut first_line.clone());
                let line = |at: usize| first_line + text[..at].matches('\n').count();
                let needles = self.needles.iter().copied();
                for needle in needles.chain(CALLS.map(|(_, needle)| needle)) {
                    for (at, _) in text.match_indices(needle) {
                        self.found.push((line(at), needle));
                    }
                }
                for needle in [LOADING_METHOD, LOADING_FN] {
                    for (at, _) in text.match_indices(needle) {
                        let reads_a_state = text[at + needle.len()..].trim_start().starts_with(')');
                        if !reads_a_state {
                            self.found.push((line(at), needle));
                        }
                    }
                }
            }
        }

        impl<'ast> Visit<'ast> for Finder {
            fn visit_path(&mut self, path: &'ast Path) {
                self.path(&path_names(None, path), line_of(path));
                visit::visit_path(self, path);
            }

            fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
                self.qualified_path(expr.qself.as_ref(), &expr.path);
                visit::visit_expr_path(self, expr);
            }

            fn visit_type_path(&mut self, ty: &'ast syn::TypePath) {
                self.qualified_path(ty.qself.as_ref(), &ty.path);
                visit::visit_type_path(self, ty);
            }

            fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
                self.qualified_path(expr.qself.as_ref(), &expr.path);
                visit::visit_expr_struct(self, expr);
            }

            fn visit_pat_struct(&mut self, pat: &'ast syn::PatStruct) {
                self.qualified_path(pat.qself.as_ref(), &pat.path);
                visit::visit_pat_struct(self, pat);
            }

            fn visit_pat_tuple_struct(&mut self, pat: &'ast syn::PatTupleStruct) {
                self.qualified_path(pat.qself.as_ref(), &pat.path);
                visit::visit_pat_tuple_struct(self, pat);
            }

            fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
                let line = call.method.span().start().line;
                for (name, needle) in CALLS {
                    if call.method == name {
                        self.found.push((line, needle));
                    }
                }
                if call.method == "loading" && !call.args.is_empty() {
                    self.found.push((line, LOADING_METHOD));
                }
                visit::visit_expr_method_call(self, call);
            }

            fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
                let mut callee = &*call.func;
                while let Expr::Paren(syn::ExprParen { expr, .. })
                | Expr::Group(syn::ExprGroup { expr, .. }) = callee
                {
                    callee = expr;
                }
                if let Expr::Path(callee) = callee
                    && !call.args.is_empty()
                    && self
                        .imports
                        .resolve(&path_names(callee.qself.as_ref(), &callee.path))
                        .iter()
                        .any(|path| path.ends_with("::loading"))
                {
                    self.found.push((line_of(&callee.path), LOADING_FN));
                }
                visit::visit_expr_call(self, call);
            }

            fn visit_ident(&mut self, ident: &'ast Ident) {
                let name = ident.to_string();
                let line = ident.span().start().line;
                for &needle in &self.needles {
                    if !needle.contains("::") && name.contains(needle) {
                        self.found.push((line, needle));
                    }
                }
            }

            fn visit_macro(&mut self, mac: &'ast syn::Macro) {
                // Its tokens are matched as text (`visit_token_stream`); a
                // body that parses is read as code too.
                if let Ok(exprs) =
                    mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
                {
                    for expr in &exprs {
                        visit::visit_expr(self, expr);
                    }
                } else if let Ok(stmts) = mac.parse_body_with(Block::parse_within) {
                    for stmt in &stmts {
                        visit::visit_stmt(self, stmt);
                    }
                }
                visit::visit_macro(self, mac);
            }

            /// A macro body, attribute arguments, or syntax `syn` keeps as
            /// raw tokens (`Verbatim`).
            fn visit_token_stream(&mut self, tokens: &'ast TokenStream) {
                self.text(tokens.clone());
            }
        }

        /// Prints `tokens` into `out`, each on the line where it starts in
        /// the source (`line` is the line `out` ends on): words are kept
        /// apart by a space, everything else is glued (`Spinner::new(`), and
        /// a literal prints as `0`.
        fn print(tokens: TokenStream, out: &mut String, line: &mut usize) {
            for token in tokens {
                match token {
                    TokenTree::Group(group) => {
                        let (open, close) = match group.delimiter() {
                            Delimiter::Parenthesis => ("(", ")"),
                            Delimiter::Brace => ("{", "}"),
                            Delimiter::Bracket => ("[", "]"),
                            Delimiter::None => ("", ""),
                        };
                        print_at(out, line, group.span_open(), open);
                        print(group.stream(), out, line);
                        print_at(out, line, group.span_close(), close);
                    }
                    TokenTree::Ident(ident) => {
                        print_at(out, line, ident.span(), &ident.to_string())
                    }
                    TokenTree::Punct(punct) => {
                        print_at(
                            out,
                            line,
                            punct.span(),
                            punct.as_char().encode_utf8(&mut [0; 4]),
                        );
                    }
                    TokenTree::Literal(literal) => print_at(out, line, literal.span(), "0"),
                }
            }
        }

        fn print_at(out: &mut String, line: &mut usize, span: Span, text: &str) {
            // Tokens `syn` made up (no source line) stay on the current one.
            while *line < span.start().line {
                out.push('\n');
                *line += 1;
            }
            let word = |c: char| c.is_alphanumeric() || c == '_';
            if out.ends_with(word) && text.starts_with(word) {
                out.push(' ');
            }
            out.push_str(text);
        }
    }

    /// The motion in the source `name` (see `motion_scan::motion`); panics,
    /// naming `name`, if it doesn't parse.
    fn motion_in(name: &str, source: &str, theme: bool) -> Vec<(usize, &'static str)> {
        motion_scan::motion(source, theme).unwrap_or_else(|error| {
            let at = error.span().start();
            panic!("{name}:{}:{}: {error}", at.line, at.column + 1)
        })
    }

    /// The motion found in the fixture `file` once its test-only code is
    /// left out, as in `ui/theme.rs` if `allow_theme_init`.
    fn scan_as(file: &str, allow_theme_init: bool) -> Vec<(usize, &'static str)> {
        motion_in(file, file, allow_theme_init)
    }

    fn scan(file: &str) -> Vec<(usize, &'static str)> {
        scan_as(file, true)
    }

    /// `body` (statements or an expression) inside a function, each line
    /// where it would be alone.
    fn in_fn(body: &str) -> String {
        format!("fn fixture() {{ {body}\n}}")
    }

    /// Redraw budget (§4.2): a wait is a status line with words, never a
    /// loader or an animation, each of which requests frames for as long as
    /// it shows, and every entry point goes through `ui::theme::init`.
    #[test]
    fn no_view_shows_an_animated_loader_or_animates() {
        let sources = interface_sources();
        for scanned in [
            "ui.rs",
            "ui/kit.rs",
            "ui/theme.rs",
            "views/library.rs",
            "daemon.rs",
            "main.rs",
        ] {
            assert!(
                sources.iter().any(|(name, _)| name == scanned),
                "{scanned} was not read"
            );
        }
        let found: Vec<String> = sources
            .iter()
            .flat_map(|(name, source)| {
                motion_in(name, source, name == "ui/theme.rs")
                    .into_iter()
                    .map(move |(line, what)| format!("{name}:{line} uses {what}"))
            })
            .collect();
        assert!(
            found.is_empty(),
            "show a wait as a status line with words and initialise GPUI via ui::theme::init:\n{}",
            found.join("\n")
        );
        // The scan sees what ships: read as any other file, ui/theme.rs
        // sets GPUI and its motion up.
        let (_, theme) = sources
            .iter()
            .find(|(name, _)| name == "ui/theme.rs")
            .unwrap();
        let found = motion_in("ui/theme.rs", theme, false);
        for needle in ["gpui::init", "set_reduce_motion"] {
            assert!(
                found.iter().any(|&(_, what)| what == needle),
                "{needle}: {found:?}"
            );
        }
    }

    /// Paths, method calls and imports are read on the syntax tree, however
    /// they are spelled: generic arguments and turbofish are left out, a
    /// qualified self stands for its type, and a name the file imports
    /// (grouped, glob, `self`, renamed, or a type alias) for its path.
    #[test]
    fn the_motion_scan_reads_paths_calls_and_imports_however_spelled() {
        for (code, what) in [
            ("<Spinner>::new()", "Spinner::new"),
            ("<Spinner as Wait>::new()", "Spinner::new"),
            ("Spinner::<u8>::new()", "Spinner::new"),
            ("<Skeleton<u8>>::new()", "Skeleton::new"),
            (
                "div().with_spring::<f32>(\"opacity\", spring, |this, _| this)",
                "with_spring(",
            ),
            (
                "div().with_animation::<Div>(\"a\", anim, |x, _| x)",
                "with_animation(",
            ),
            ("button.loading::<u8>(true)", ".loading("),
            ("<Progress>::loading(progress, true)", "::loading("),
            (
                "AnimationExt::with_animation(div(), \"a\", anim, |x, _| x)",
                "with_animation(",
            ),
            (
                "let start = Window::request_animation_frame;",
                "request_animation_frame(",
            ),
            ("let wait: <Shimmer as Text>::Out = make();", "Shimmer"),
            // A macro body that parses is read as code too.
            ("let items = vec![<Spinner>::new()];", "Spinner::new"),
        ] {
            assert_eq!(scan(&in_fn(code)), [(1, what)], "{code}");
        }
        // The accessor is spared whatever its turbofish.
        assert!(scan(&in_fn("let busy = search.loading::<u8>();")).is_empty());

        for (file, found) in [
            // The gpui::init that only ui/theme.rs may call, imported in a
            // group, a glob, renamed, or through `self`.
            (
                "use gpui::{App, init};\nfn start(cx: &mut App) {\n    init(cx);\n}\n",
                &[(1, "gpui::init"), (3, "gpui::init")][..],
            ),
            (
                "use gpui::*;\nfn start(cx: &mut App) {\n    init(cx);\n}\n",
                &[(3, "gpui::init")],
            ),
            (
                "use gpui::{App, init as setup};\nfn start(cx: &mut App) {\n    setup(cx);\n}\n",
                &[(1, "gpui::init"), (3, "gpui::init")],
            ),
            (
                "use gpui::{self as ui, App};\nfn start(cx: &mut App) {\n    cx.update(ui::init);\n}\n",
                &[(3, "gpui::init")],
            ),
            (
                "use gpui::base::{self, set_reduce_motion_preference as reduce};\nfn start(cx: &mut App) {\n    reduce(base::ReduceMotionPreference::Allow, cx);\n}\n",
                &[(1, "set_reduce_motion"), (3, "set_reduce_motion")],
            ),
        ] {
            assert_eq!(scan_as(file, false), found, "{file}");
            assert!(scan(file).is_empty(), "{file}");
        }

        for (file, found) in [
            // ReducedMotion::Continue, imported in a group with `self`, in
            // a nested glob, or renamed.
            (
                "use gpui::ReducedMotion::{self, Continue};\nfn view() -> Div {\n    div().reduced_motion(Continue)\n}\n",
                &[
                    (1, "ReducedMotion::Continue"),
                    (3, "ReducedMotion::Continue"),
                ][..],
            ),
            (
                "use gpui::{ReducedMotion::*, div};\nfn view() -> Div {\n    div().reduced_motion(Continue)\n}\n",
                &[(3, "ReducedMotion::Continue")],
            ),
            (
                "use gpui::ReducedMotion as Motion;\nfn view() -> Div {\n    div().reduced_motion(Motion::Continue)\n}\n",
                &[(3, "ReducedMotion::Continue")],
            ),
            // A loader under another name: a renamed import, a type alias.
            (
                "use gpui_component::spinner::Spinner as Wait;\nfn view() {\n    Wait::new();\n}\n",
                &[(3, "Spinner::new")],
            ),
            (
                "type Wait = gpui_component::Skeleton;\nfn view() {\n    let _ = <Wait>::new();\n}\n",
                &[(3, "Skeleton::new")],
            ),
        ] {
            assert_eq!(scan(file), found, "{file}");
        }
    }

    #[test]
    fn the_motion_scan_finds_each_loader_and_spares_the_loading_accessor() {
        for (code, what) in [
            ("Spinner::new().small()", "Spinner::new"),
            ("Skeleton::new().w_full()", "Skeleton::new"),
            ("ShimmerText::new(\"Loading\")", "Shimmer"),
            ("Button::new(\"save\").loading(true)", ".loading("),
            ("Progress::new(\"download\").loading(busy)", ".loading("),
            (
                "Progress::loading(Progress::new(\"job\"), true)",
                "::loading(",
            ),
            (
                "div().with_animation(\"pulse\", animation, |this, _| this)",
                "with_animation(",
            ),
            (
                "div().with_animations([anim], |this, _| this)",
                "with_animations(",
            ),
            (
                "div().with_spring(\"id\", spring, |this, _| this)",
                "with_spring(",
            ),
            ("Animation::new(Duration::from_secs(1))", "Animation::new"),
            (
                "window.request_animation_frame()",
                "request_animation_frame(",
            ),
            (
                "div().reduced_motion(ReducedMotion::Continue)",
                "ReducedMotion::Continue",
            ),
            // The tokens are matched, not their spacing.
            ("Spinner :: new()", "Spinner::new"),
            ("button . loading (true)", ".loading("),
        ] {
            assert_eq!(scan(&in_fn(code)), [(1, what)], "{code}");
        }
        for (code, what) in [
            ("gpui::init(cx);", "gpui::init"),
            ("cx.update(gpui::init);", "gpui::init"),
            (
                "set_reduce_motion_preference(Allow, cx);",
                "set_reduce_motion",
            ),
        ] {
            assert_eq!(scan_as(&in_fn(code), false), [(1, what)], "{code}");
            assert!(scan(&in_fn(code)).is_empty(), "{code}");
        }
        assert_eq!(scan_as("use gpui::init;\n", false), [(1, "gpui::init")]);
        // rustfmt may put the flag on its own line; the line is the call's.
        assert_eq!(
            scan(&in_fn(
                "Progress::new(\"download\")\n    .loading(\n        busy,\n    )"
            )),
            [(2, ".loading(")]
        );
        // The library reads its search's state through an accessor of the
        // same name.
        for code in [
            "summary(count, search.loading(), search.has_more())",
            "count == 0 && search.loading().is_none()",
            "let loading = search.loading(\n);",
        ] {
            assert!(scan(&in_fn(code)).is_empty(), "{code}");
        }
        // Comments, doc comments, string literals and #[cfg(test)] items are
        // skipped while preserving exact line numbers and scanning
        // #[cfg(not(test))].
        let source = "\
fn wait() {}\n\
fn done() {} // Never Shimmer here\n\
const LABEL: &str = \"Never Spinner::new or Shimmer here\";\n\
#[cfg(test)]\n\
mod fixtures {\n\
    fn f() { Spinner::new(); }\n\
}\n\
#[cfg(all(test, unix))]\n\
fn helper() { Skeleton::new(); }\n\
#[cfg(not(test))]\n\
fn real_after_tests() {\n\
    ShimmerText::new();\n\
}\n\
#[cfg(test)]\n\
pub(crate) mod tests {\n\
    fn t() { with_animation(\"a\", anim, |x, _| x); }\n\
}\n\
/// Never Spinner::new here.\n\
/* Nor Skeleton::new() */\n\
fn documented() { let _ = r#\"Spinner::new()\"#; }\n";
        assert_eq!(scan(source), [(12, "Shimmer")]);
    }

    /// A `#[cfg(test)]` field, struct-literal field, argument, match arm or
    /// statement is left out on its own: the production code after it is
    /// still scanned.
    #[test]
    fn the_motion_scan_keeps_the_code_after_a_test_only_field() {
        // As `TranscriptView` in views/panel/view.rs: a test-only counter as
        // the last field, then the production constructor.
        let view = "\
pub(super) struct TranscriptView {\n\
    panel: WeakEntity<RecordingPanel>,\n\
    #[cfg(test)]\n\
    renders: usize,\n\
}\n\
\n\
impl TranscriptView {\n\
    fn new(panel: WeakEntity<RecordingPanel>) -> Self {\n\
        let _wait = Spinner::new();\n\
        Self {\n\
            panel,\n\
            #[cfg(test)]\n\
            renders: 0,\n\
        }\n\
    }\n\
}\n";
        assert_eq!(scan(view), [(9, "Spinner::new")]);

        for (file, found) in [
            // The last field, without a comma.
            (
                "struct View {\n#[cfg(test)]\nrenders: usize\n}\nfn wait() { Spinner::new(); }\n",
                (5, "Spinner::new"),
            ),
            // A test-only argument, then the body.
            (
                "fn f(#[cfg(test)] probe: u8) {\nSpinner::new();\n}\n",
                (2, "Spinner::new"),
            ),
            // A test-only block and binding, then the tail expression.
            (
                "fn render() {\n#[cfg(test)]\n{\nSpinner::new();\n}\n#[cfg(test)]\nlet _probe = Probe { s: Skeleton::new() };\ndiv().with_animation(\"a\", anim, |x, _| x)\n}\n",
                (8, "with_animation("),
            ),
            // A test-only fn returning `impl Iterator<Item = u8>`.
            (
                "#[cfg(test)]\nfn probe() -> impl Iterator<Item = u8> { Skeleton::new() }\nfn wait() { Spinner::new(); }\n",
                (3, "Spinner::new"),
            ),
            // A test-only generic parameter, nested generics included.
            (
                "fn view<#[cfg(test)] T>() { Spinner::new(); }",
                (1, "Spinner::new"),
            ),
            (
                "fn f<#[cfg(test)] const N: usize>() {\nSpinner::new();\n}\n",
                (2, "Spinner::new"),
            ),
            (
                "struct S<A, #[cfg(test)] B> {\nwait: [A; { Spinner::new(); 1 }],\n}\n",
                (2, "Spinner::new"),
            ),
            (
                "fn f<#[cfg(test)] T: Into<Vec<u8>>, U>() {\nSpinner::new();\n}\n",
                (2, "Spinner::new"),
            ),
            // An inline const block.
            (
                "fn view() { #[cfg(test)] const { () } Spinner::new(); }",
                (1, "Spinner::new"),
            ),
            // Arrows, comparisons and shifts in a test-only signature or
            // body.
            (
                "#[cfg(test)]\nfn probe<T: Into<u8>>(n: u8) -> bool { n >= 2 && n << 1 <= 4 && n >> 1 > 0 && { Skeleton::new(); true } }\nfn wait() { Spinner::new(); }\n",
                (3, "Spinner::new"),
            ),
            // A test-only field whose type has generics, with commas and
            // blocks in them, on one line or several.
            (
                "struct View {\n#[cfg(test)]\nprobe: Probe<u8, { Skeleton::new(); 1 }>,\nwait: [u8; { Spinner::new(); 1 }],\n}\n",
                (4, "Spinner::new"),
            ),
            (
                "struct View {\n#[cfg(test)]\nprobe: HashMap<\n    u8,\n    Probe<{ Skeleton::new(); 1 }>,\n>,\nwait: [u8; { Spinner::new(); 1 }],\n}\n",
                (7, "Spinner::new"),
            ),
        ] {
            assert_eq!(scan(file), [found], "{file}");
        }

        for (body, found) in [
            // A struct-literal field, then a production one.
            (
                "View {\n#[cfg(test)]\nrenders: 0,\nlabel: ShimmerText::new(),\n}\n",
                (4, "Shimmer"),
            ),
            // The literal's last field, then production code.
            (
                "let v = View { #[cfg(test)] renders: 0 };\nSkeleton::new();\n",
                (2, "Skeleton::new"),
            ),
            // A test-only match arm, then a production one.
            (
                "match kind {\n#[cfg(test)]\nKind::Probe => Skeleton::new(),\nKind::Wait => Spinner::new(),\n}\n",
                (4, "Spinner::new"),
            ),
            // A comparison in a test-only arm.
            (
                "match n {\n#[cfg(test)]\n0 => a < b,\n_ => Spinner::new(),\n}\n",
                (4, "Spinner::new"),
            ),
        ] {
            assert_eq!(scan(&in_fn(body)), [found], "{body}");
        }
    }

    /// Each kind of node a `#[cfg(test)]` can sit on is left out whole, and
    /// only that node: the test-only `Skeleton::new` is never found, the
    /// production `Spinner::new` after it always is.
    #[test]
    fn the_motion_scan_leaves_out_each_kind_of_test_only_node() {
        for (file, found) in [
            // A where clause on a test-only fn.
            (
                "#[cfg(test)]\nfn probe<T>(value: T) -> u8\nwhere\n    T: Into<u8>,\n{\n    Skeleton::new();\n    value.into()\n}\nfn wait() { Spinner::new(); }\n",
                (9, "Spinner::new"),
            ),
            // Impl and trait items.
            (
                "impl View {\n#[cfg(test)]\nfn probe() { Skeleton::new(); }\nfn wait() { Spinner::new(); }\n}\n",
                (4, "Spinner::new"),
            ),
            (
                "trait Wait {\n#[cfg(test)]\nconst PROBE: u8 = { Skeleton::new(); 1 };\nfn wait() { Spinner::new(); }\n}\n",
                (4, "Spinner::new"),
            ),
            // Enum variants and their fields, tuple fields.
            (
                "enum Kind {\n#[cfg(test)]\nProbe = { Skeleton::new(); 1 },\nWait = { Spinner::new(); 2 },\n}\n",
                (4, "Spinner::new"),
            ),
            (
                "enum Kind {\nWait {\n#[cfg(test)]\nprobe: [u8; { Skeleton::new(); 1 }],\nwait: [u8; { Spinner::new(); 1 }],\n},\n}\n",
                (5, "Spinner::new"),
            ),
            (
                "struct Row(\n#[cfg(test)] [u8; { Skeleton::new(); 1 }],\n[u8; { Spinner::new(); 1 }],\n);\n",
                (3, "Spinner::new"),
            ),
            // A generic parameter with a default, a fn parameter.
            (
                "struct S<\n#[cfg(test)] T = [u8; { Skeleton::new(); 1 }],\nU = [u8; { Spinner::new(); 1 }],\n>(U);\n",
                (3, "Spinner::new"),
            ),
            (
                "fn f(\n#[cfg(test)] probe: [u8; { Skeleton::new(); 1 }],\nwait: [u8; { Spinner::new(); 1 }],\n) {}\n",
                (3, "Spinner::new"),
            ),
            // A module that says it is test-only from inside.
            (
                "mod probe {\n#![cfg(test)]\nfn f() { Skeleton::new(); }\n}\nfn wait() { Spinner::new(); }\n",
                (5, "Spinner::new"),
            ),
        ] {
            assert_eq!(scan(file), [found], "{file}");
        }
        // A file that says it is test-only.
        assert!(scan("#![cfg(test)]\nfn f() { Skeleton::new(); }\n").is_empty());

        for (body, found) in [
            // A test-only closure parameter, whatever its type.
            (
                "let make = |#[cfg(test)] _test: bool| Spinner::new();",
                (1, "Spinner::new"),
            ),
            (
                "let make = |#[cfg(test)] _test: Option<u8>| Spinner::new();",
                (1, "Spinner::new"),
            ),
            (
                "let make = |#[cfg(test)] _probe, wait: u8| Spinner::new();",
                (1, "Spinner::new"),
            ),
            // A test-only match arm with a guard.
            (
                "match n {\n#[cfg(test)]\nx if x < LIMIT => Skeleton::new(),\n_ => Spinner::new(),\n}\n",
                (4, "Spinner::new"),
            ),
            // Statements: an expression whose attribute `syn` puts on its
            // left operand, a macro call, an item.
            (
                "#[cfg(test)]\nprobe = Skeleton::new() + 1;\nSpinner::new();\n",
                (3, "Spinner::new"),
            ),
            (
                "#[cfg(test)]\nprintln!(\"{:?}\", Skeleton::new());\nSpinner::new();\n",
                (3, "Spinner::new"),
            ),
            (
                "#[cfg(test)]\nfn probe() { Skeleton::new(); }\nSpinner::new();\n",
                (3, "Spinner::new"),
            ),
            // Arguments and array and tuple elements.
            (
                "call(\n#[cfg(test)] Skeleton::new(),\nSpinner::new(),\n);\n",
                (3, "Spinner::new"),
            ),
            (
                "view.call(#[cfg(test)] Skeleton::new(), Spinner::new());",
                (1, "Spinner::new"),
            ),
            (
                "let all = [#[cfg(test)] Skeleton::new(), Spinner::new()];",
                (1, "Spinner::new"),
            ),
            (
                "let pair = (#[cfg(test)] Skeleton::new(), Spinner::new());",
                (1, "Spinner::new"),
            ),
        ] {
            assert_eq!(scan(&in_fn(body)), [found], "{body}");
        }
    }

    /// A cfg is test-only when it can't hold with `test` off; anything else
    /// may ship and is scanned.
    #[test]
    fn the_motion_scan_leaves_out_only_cfgs_that_need_test() {
        for (predicate, test_only) in [
            ("test", true),
            ("all(test, unix)", true),
            ("all(unix, any(test, all(test, feature = \"probe\")))", true),
            ("not(not(test))", true),
            ("not(any(not(test), windows))", true),
            ("any()", true),
            ("false", true),
            ("not(test)", false),
            ("any(test, unix)", false),
            ("all(not(test), unix)", false),
            ("feature = \"test\"", false),
            ("all()", false),
            ("unix", false),
        ] {
            let file = format!("#[cfg({predicate})]\nfn f() {{ Spinner::new(); }}\n");
            let expected: &[_] = if test_only {
                &[]
            } else {
                &[(2, "Spinner::new")]
            };
            assert_eq!(scan(&file), expected, "{predicate}");
        }
        // `cfg_attr` is not expanded.
        assert_eq!(
            scan("#[cfg_attr(test, allow(dead_code))]\nfn f() { Spinner::new(); }\n"),
            [(2, "Spinner::new")]
        );
    }

    /// A macro body is a token stream `syn` doesn't parse: it is scanned
    /// whole, test-only attributes included (a false alarm at worst).
    #[test]
    fn the_motion_scan_reads_macro_bodies_whole() {
        for (body, found) in [
            ("let items = vec![Spinner::new()];", (1, "Spinner::new")),
            (
                "let items = vec![\n    label(),\n    Spinner::new(),\n];",
                (3, "Spinner::new"),
            ),
            ("div! { ShimmerText::new(\"Loading\") }", (1, "Shimmer")),
            // The documented limit: a test-only element is flagged.
            (
                "let items = vec![#[cfg(test)] Skeleton::new(), label()];",
                (1, "Skeleton::new"),
            ),
            // Shapes rustfmt leaves alone in a macro body: a comparison glued
            // to its operands before production code, however the field,
            // list or statement ends.
            (
                "fields!(\n#[cfg(test)] flag: a<b, widget: Spinner::new(),\n);\n",
                (2, "Spinner::new"),
            ),
            (
                "fields!(#[cfg(test)] flag: a<b, widget: Spinner::new())",
                (1, "Spinner::new"),
            ),
            (
                "fields! {\n#[cfg(test)] flag: a<b, widget: Spinner::new();\n}\n",
                (2, "Spinner::new"),
            ),
            (
                "fields![#[cfg(test)] flag: a<b, widget: Spinner::new(),]",
                (1, "Spinner::new"),
            ),
            (
                "fields!(#[cfg(test)] flag: a<b, widget: W { s: Spinner::new() })",
                (1, "Spinner::new"),
            ),
            (
                "m!(#[cfg(test)] if a<b { probe(); } Spinner::new();)",
                (1, "Spinner::new"),
            ),
            (
                "fields!(#[cfg(test)] flag: a<b, widget: Spinner::new() > c)",
                (1, "Spinner::new"),
            ),
        ] {
            assert_eq!(scan(&in_fn(body)), [found], "{body}");
        }
        // An item macro, and a test-only one left out whole.
        assert_eq!(
            scan("#[cfg(test)]\nprobes! { Skeleton::new() }\nwaits! { Spinner::new() }\n"),
            [(3, "Spinner::new")]
        );
    }

    /// Byte ranges of `source` taken by items gated under `#[cfg(test)]`
    /// (or `#[cfg(all(test, ...))]`).
    fn test_item_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
        let clean = strip_comments_and_strings(source);
        let bytes = clean.as_bytes();
        let mut ranges = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'#'
                && let Some((after_attr, inner)) = parse_outer_attr(bytes, i)
                && is_cfg_test_attr(inner)
            {
                let end = find_attributed_item_end(bytes, after_attr);
                ranges.push(i..end);
                i = end;
                continue;
            }
            i += 1;
        }
        ranges
    }

    fn unnamed_text_children(source: &str, kit: bool) -> Vec<usize> {
        let text = shipping_text(source);
        let code = strip_comments_and_strings(&text);
        code.match_indices(".child")
            .filter_map(|(offset, prefix)| {
                let args = text[offset + prefix.len()..]
                    .trim_start()
                    .strip_prefix('(')?
                    .trim_start();
                let literal = if let Some(format_args) = args.strip_prefix("format!") {
                    format_args.trim_start().strip_prefix('(')?.trim_start()
                } else {
                    args
                }
                .strip_prefix('"')?;
                let starts_with_capital = literal.starts_with(char::is_uppercase);
                let mascot = kit && literal.starts_with("Bro_\"");
                (starts_with_capital && !mascot)
                    .then(|| text[..offset].bytes().filter(|&byte| byte == b'\n').count() + 1)
            })
            .collect()
    }

    #[test]
    fn visible_sentence_children_use_named_text_nodes() {
        let errors: Vec<_> = interface_sources()
            .into_iter()
            .filter(|(name, _)| {
                name == "ui.rs" || name == "ui/kit.rs" || name.starts_with("views/")
            })
            .flat_map(|(name, source)| {
                unnamed_text_children(&source, name == "ui/kit.rs")
                    .into_iter()
                    .map(move |line| format!("{name}:{line}"))
            })
            .collect();
        assert!(errors.is_empty(), "use named text nodes at: {errors:?}");
    }

    #[test]
    fn the_named_text_check_reads_multiline_children_and_format_calls() {
        let source = r#"div().child("A label")
            .child(
                format!("From {}", supplier),
            )
            .child(" · ")
            .child("Bro_");"#;
        assert_eq!(unnamed_text_children(source, false), [1, 2, 6]);
        assert_eq!(unnamed_text_children(source, true), [1, 2]);
    }

    /// Byte ranges of `source` taken by `tracing::…!(…)` calls: the log may
    /// carry an error's code (WRT-04), visible text may not.
    fn log_call_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
        let clean = strip_comments_and_strings(source);
        let bytes = clean.as_bytes();
        let mut ranges = Vec::new();
        for (at, _) in clean.match_indices("tracing::") {
            let Some(open) = bytes[at..].iter().position(|&b| b == b'(').map(|o| at + o) else {
                continue;
            };
            let mut depth = 0usize;
            let mut end = bytes.len();
            for (j, &b) in bytes.iter().enumerate().skip(open) {
                match b {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = j + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            ranges.push(at..end);
        }
        ranges
    }

    /// `source` as it ships, with its words: comments, `#[cfg(test)]` items
    /// and log calls blanked, string literals kept, offsets and lines as in
    /// the original.
    fn shipping_text(source: &str) -> String {
        let mut text = strip_comments(source);
        for range in test_item_ranges(source)
            .into_iter()
            .chain(log_call_ranges(source))
        {
            let mut blank = String::with_capacity(range.len());
            push_blanked(&mut blank, &text[range.clone()]);
            text.replace_range(range, &blank);
        }
        text
    }

    #[test]
    fn views_use_the_shared_type_scale() {
        let mut findings = Vec::new();
        for (path, source) in interface_sources() {
            if path == "ui/type_scale.rs" {
                continue;
            }
            let shipping = shipping_text(&source);
            let mut code: String = shipping.chars().filter(|ch| !ch.is_whitespace()).collect();
            if path == "ui/kit.rs" {
                // The fixed-size mouth glyphs are part of the mascot artwork.
                code = code.replacen(".text_size(px(9.)).line_height(px(10.))", "", 2);
            }
            for method in [
                ".text_xs(",
                ".text_sm(",
                ".text_base(",
                ".text_lg(",
                ".text_xl(",
                ".text_2xl(",
                ".text_3xl(",
                ".text_4xl(",
                ".text_size(",
                ".line_height(px(",
            ] {
                if code.contains(method) {
                    findings.push(format!("{path}: {method}"));
                }
            }
        }
        assert!(
            findings.is_empty(),
            "Use a shared TextRole for visible text:\n{}",
            findings.join("\n")
        );
    }

    fn shipping_literals(source: &str) -> Vec<(usize, String)> {
        let shipping = shipping_text(source);
        let mut literals = Vec::new();
        // Preserve the original line number before normalising multiline copy.
        strip_source(&shipping, true, |range| {
            let line = shipping[..range.start]
                .bytes()
                .filter(|&b| b == b'\n')
                .count()
                + 1;
            let words = shipping[range].replace("\\\n", " ");
            literals.push((line, words.split_whitespace().collect::<Vec<_>>().join(" ")));
        });
        literals
    }

    fn copy_sources() -> Vec<(String, String)> {
        let mut sources = interface_sources();
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for entry in std::fs::read_dir(crates).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "broscribed" || name.starts_with("broscribe-platform-") {
                sources.extend(
                    rust_sources(&entry.path().join("src"))
                        .into_iter()
                        .filter(|(path, _)| !path.ends_with("/tests.rs"))
                        .map(|(path, source)| (format!("{name}/{path}"), source)),
                );
            }
        }
        sources
    }

    fn retired_words(words: &str) -> Option<&'static str> {
        let lower = words.to_ascii_lowercase();
        let bytes = lower.as_bytes();
        [
            "recording agent",
            "recording helper",
            "the helper",
            "the agent",
            "background recorder",
            "recording panel",
            "side panel",
            "untitled recording",
            "e-mail",
            "e-mails",
            "ai agents",
            "agent cli",
            "vendor",
            "saving: final transcript",
            "processing a recording",
            "provisional transcript",
            "preview transcript",
            "provisional result",
            "provisional results",
            "helper returned",
            "background processing",
            "voiceprint",
            "voiceprints",
            "enrollment",
            "enrolment",
            "embedding",
            "embeddings",
            "cluster",
            "clusters",
            "passage recording",
            "recognised automatically",
            "recognized automatically",
            "status icon",
            "add terms",
            "one term per line",
            "broscribe agent",
        ]
        .into_iter()
        .find(|phrase| {
            lower.match_indices(phrase).any(|(start, _)| {
                let end = start + phrase.len();
                (start == 0 || !is_ident_byte(bytes[start - 1]))
                    && bytes.get(end).is_none_or(|&b| !is_ident_byte(b))
                    && &words[start..end] != super::terms::AGENT_NAME
            })
        })
    }

    fn glossary_protocol_literal(path: &str, words: &str) -> bool {
        // Existing disk names, session purposes, setting IDs and CLI flags.
        (matches!(
            path,
            "views/settings.rs" | "views/setup.rs" | "views/people.rs" | "broscribed/sample.rs"
        ) && matches!(
            words,
            "enrollment" | "--delete-enrollment" | "--keep-as-enrollment"
        )) || (path == "views/settings.rs"
            && (words.starts_with("enrollment-") || words.starts_with("settings-enrollment-"))
            && words
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'-'))
    }

    #[test]
    fn copy_uses_glossary() {
        let findings: Vec<_> = copy_sources()
            .into_iter()
            .flat_map(|(path, source)| {
                shipping_literals(&source)
                    .into_iter()
                    .filter_map(move |(line, words)| {
                        if glossary_protocol_literal(&path, &words) {
                            return None;
                        }
                        retired_words(&words)
                            .map(|retired| format!("{path}:{line}: {retired}: {words}"))
                    })
            })
            .collect();
        assert!(
            findings.is_empty(),
            "Use the words in docs/design.md §4.4:\n{}",
            findings.join("\n")
        );
    }

    #[test]
    fn glossary_reads_literals_and_spares_identifiers_tests_and_tracing() {
        let source = r#"
            let recording_panel = r"An old Recording panel";
            tracing::warn!("the agent");
            #[cfg(test)] mod tests { const LABEL: &str = "AI agents"; }
            let current = "BroScribe Agent";
            let code = "recording_panel";
            let email = "email";
            let wrong_case = "BroScribe agent";
        "#;
        let findings: Vec<_> = shipping_literals(source)
            .into_iter()
            .filter_map(|(line, words)| retired_words(&words).map(|word| (line, word)))
            .collect();
        assert_eq!(findings, [(2, "recording panel"), (8, "broscribe agent")]);
    }

    #[test]
    fn visible_units_use_the_shared_formatters() {
        let mut findings = Vec::new();
        for (path, source) in copy_sources() {
            if path == "ui/format.rs" {
                continue;
            }
            for (line, words) in shipping_literals(&source) {
                if words.contains("}%")
                    || words.contains("} %")
                    || words.contains("(s)")
                    || words.contains(" MiB")
                    || words.contains(" GiB")
                    || words.contains("%H:%M")
                        && !matches!(
                            path.as_str(),
                            "views/import_sheet.rs" | "broscribe-platform-macos/decoder.rs"
                        )
                {
                    findings.push(format!("{path}:{line}: {words}"));
                }
            }
        }
        assert!(
            findings.is_empty(),
            "Use ui::format for visible units:\n{}",
            findings.join("\n")
        );
    }

    #[test]
    fn no_debug_format_in_views() {
        let mut findings = Vec::new();
        for (path, source) in interface_sources() {
            for (line, words) in shipping_literals(&source) {
                // These are element IDs. Their names are stable for snapshots.
                if matches!(
                    (path.as_str(), words.as_str()),
                    ("ui.rs", "a live {role:?} node needs a non-empty element id")
                        | ("views/import_sheet.rs", "import-error-{field:?}")
                        | ("views/panel/view.rs", "status-{:?}-{}-{}")
                ) {
                    continue;
                }
                let debug = words
                    .split('{')
                    .skip(1)
                    .filter_map(|field| field.split_once('}'))
                    .any(|(field, _)| {
                        field
                            .split_once(':')
                            .is_some_and(|(_, spec)| spec.ends_with('?'))
                    });
                let internal = [
                    "{generation}",
                    "{selection_generation}",
                    "{account_id}",
                    "{tenant_id}",
                    "{object_id}",
                    "{route_id}",
                ]
                .into_iter()
                .any(|field| words.contains(field) && words.contains(' '));
                if debug || internal {
                    findings.push(format!("{path}:{line}: {words}"));
                }
            }
        }
        assert!(
            findings.is_empty(),
            "Give visible states and identifiers plain names:\n{}",
            findings.join("\n")
        );
    }

    #[test]
    fn agent_bundle_declares_the_os_facing_name() {
        let script = include_str!("../../../scripts/bundle-macos.sh");
        let helper = script
            .split_once("<string>org.broscribe.Agent</string>")
            .expect("macOS agent bundle metadata")
            .1;
        let plist = helper.split_once("</plist>").unwrap().0;
        assert!(plist.contains("<key>CFBundleDisplayName</key><string>BroScribe Agent</string>"));
    }

    #[test]
    fn reading_captions_and_tabular_figures_are_wired_into_their_views() {
        let compact = |source: &str| {
            shipping_text(source)
                .chars()
                .filter(|ch| !ch.is_whitespace())
                .collect::<String>()
        };
        // These are the production element builders, including the inline role
        // on table cells and the tabular wrappers around changing positions.
        for (source, patterns) in [
            (
                include_str!("views/meeting/overview.rs"),
                &[
                    ".text_role(TextRole::Reading).child(text)",
                    ".id((\"action-text\",ix)).spoken(Role::Label,spoken).text_role(TextRole::Reading)",
                    ".link().small().map(tabular)",
                ][..],
            ),
            (
                include_str!("views/generate.rs"),
                &[
                    ".id(\"enrichment-credit\").aria_label(spoken).role(Role::Label).text_role(TextRole::Caption)",
                ][..],
            ),
            (
                include_str!("views/meeting/saved_transcript.rs"),
                &[
                    "tabular(div()).id((\"line-time\",ix)).font_family(mono).text_role(TextRole::Mono)",
                    ".text_role(TextRole::Reading).when(current_hit||library_match.is_some()",
                ][..],
            ),
            (
                include_str!("views/setup.rs"),
                &[
                    "kit::inset_box(\"passage\",cx).role(Role::Article).aria_label(aria).max_w(gpui::rems(36.)).text_role(TextRole::Reading)",
                ][..],
            ),
            (
                include_str!("views/templates.rs"),
                &[
                    ".id((rcx.id(\"p\"),j)).spoken(Role::Label,text.spoken()).text_role(TextRole::Reading)",
                ][..],
            ),
            (
                include_str!("views/panel/view.rs"),
                &[
                    ".pb_2().text_role(TextRole::Reading)",
                    "tabular(div()).text_role(TextRole::Caption)",
                    "text_line(\"visor-context\",Role::Label,context.clone()).text_role(TextRole::Caption)",
                    "timer_line(\"visor-elapsed\",\"Elapsedtime\",spoken_position(elapsed)",
                ][..],
            ),
            (
                include_str!("views/library.rs"),
                &[".text_role(TextRole::Caption)", "tabular(div())"][..],
            ),
            (
                include_str!("views/people.rs"),
                &["row_body(", ".text_role(TextRole::Caption)"][..],
            ),
            (
                include_str!("views/settings.rs"),
                &["text_line(id,Role::Label,text).text_role(TextRole::Caption)"][..],
            ),
            (
                include_str!("ui/kit.rs"),
                &[
                    ".text_role(TextRole::Caption).text_color(words)",
                    "ifmono{TextRole::Mono}else{TextRole::Body}",
                ][..],
            ),
            (
                include_str!("ui.rs"),
                &[
                    "el.child(tabular(text_line(SharedString::from(format!(\"{}-counts\"",
                    "el.child(tabular(text_line(format!(\"{}-detail\"",
                ][..],
            ),
        ] {
            let code = compact(source);
            for pattern in patterns {
                assert!(
                    code.contains(pattern),
                    "typography wiring missing: {pattern}"
                );
            }
        }
    }

    #[test]
    fn text_containers_follow_rem_height_and_use_bundled_inter_weights() {
        let mut findings = Vec::new();
        for (path, source) in interface_sources() {
            let shipping = strip_comments_and_strings(&shipping_text(&source));
            let mut code: String = shipping.chars().filter(|ch| !ch.is_whitespace()).collect();
            if path == "ui/kit.rs" {
                // The decorative visor face and meter segments pack art, not text.
                code = code
                    .replacen(".h(px(10.))", "", 2)
                    .replacen(".h(px(13.))", "", 1)
                    .replacen(".h(px(3.))", "", 1)
                    .replacen(".h(px(8.))", "", 1)
                    .replacen(".h(px(6.))", "", 1)
                    .replacen(".h(px(36.))", "", 1);
            }
            for pattern in [
                ".h(px(",
                ".min_h(px(",
                ".max_h(px(",
                "FontWeight::BOLD",
                "FontWeight::EXTRA_BOLD",
                "FontWeight::BLACK",
            ] {
                if code.contains(pattern) {
                    findings.push(format!("{path}: {pattern}"));
                }
            }
        }
        assert!(
            findings.is_empty(),
            "Scale text containers in rems and use bundled weights:\n{}",
            findings.join("\n")
        );
    }

    /// The names an error goes by in the views.
    const ERROR_NAMES: [&str; 6] = ["e", "err", "error", "why", "reason", "cause"];

    /// An error turned into text whole: `<name>.to_string()`, in a format
    /// string or passed to a helper (`failure_text(…, &e.to_string(), …)`).
    const ERROR_TO_STRING: [&str; 6] = [
        "e.to_string()",
        "err.to_string()",
        "error.to_string()",
        "why.to_string()",
        "reason.to_string()",
        "cause.to_string()",
    ];

    /// The format macros whose arguments the scan reads.
    const FORMAT_MACROS: [&str; 4] = ["format!(", "format_args!(", "write!(", "writeln!("];

    /// A line ending in this comment (or the line after one holding only
    /// it) is a reviewed use that is not a raw error: an element id, or
    /// Debug text another unit words ([SYS-63](U20)).
    const RAW_TEXT_ALLOWED: &str = "raw-text-allowed:";

    /// Raw errors put into text:
    /// - an error interpolated whole (`{e}`, `{error}`, `{e:?}` …) or its
    ///   message read directly (`e.message`);
    /// - an error turned into a string (`e.to_string()`), wherever it goes;
    /// - an error as the argument of a format macro (`format!(\"{}\", e)`);
    /// - Debug text of anything (`{:?}`, `{:#?}`, `{x:?}`), an error's named
    ///   capture included (`{cause:?}`, `{why:#?}`, `{error:?}`).
    fn raw_errors_in(text: &str) -> Vec<(usize, &'static str)> {
        let bytes = text.as_bytes();
        let line = |at: usize| text[..at].matches('\n').count() + 1;
        let whole = |at: usize, len: usize| {
            (at == 0 || !is_ident_byte(bytes[at - 1]))
                && bytes.get(at + len).is_none_or(|&b| !is_ident_byte(b))
        };
        let mut found = Vec::new();
        let interpolations = ["{e}", "{e:", "{err}", "{err:", "{error}", "{error:"];
        for needle in interpolations {
            for (at, _) in text.match_indices(needle) {
                found.push((line(at), needle));
            }
        }
        for needle in ["e.message", "err.message", "error.message"] {
            for (at, _) in text.match_indices(needle) {
                if whole(at, needle.len()) {
                    found.push((line(at), needle));
                }
            }
        }
        for needle in ERROR_TO_STRING {
            for (at, _) in text.match_indices(needle) {
                if at == 0 || !is_ident_byte(bytes[at - 1]) {
                    found.push((line(at), needle));
                }
            }
        }
        for (at, _) in text.match_indices("?}") {
            if let Some(open) = text[..at].rfind('{') {
                let spec = &text[open + 1..at];
                let (name, format) = spec.split_once(':').unwrap_or((spec, ""));
                let debug = matches!(format, "" | "#")
                    && spec.contains(':')
                    && name.bytes().all(is_ident_byte);
                // `{e:?}`, `{err:?}` and `{error:?}` are found above already;
                // any other named capture (`{cause:?}`, `{why:?}`) here.
                let found_above = interpolations.contains(&format!("{{{name}:").as_str());
                if debug && !found_above {
                    found.push((line(open), "{:?}"));
                }
            }
        }
        for macro_name in FORMAT_MACROS {
            for (at, _) in text.match_indices(macro_name) {
                if at > 0 && is_ident_byte(bytes[at - 1]) {
                    continue;
                }
                let open = at + macro_name.len() - 1;
                for argument in macro_arguments(text, open).into_iter().skip(1) {
                    let argument = argument
                        .rsplit_once('=')
                        .map_or(argument, |(_, value)| value);
                    let argument = argument.trim().trim_start_matches(['&', '*']).trim();
                    if ERROR_NAMES.contains(&argument) {
                        found.push((line(at), "an error as a format argument"));
                    }
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        found
    }

    /// The top-level arguments of the macro call whose `(` is at `open`:
    /// commas inside brackets and string literals don't split.
    fn macro_arguments(text: &str, open: usize) -> Vec<&str> {
        let bytes = text.as_bytes();
        let mut arguments = Vec::new();
        let (mut depth, mut start, mut i) = (0usize, open + 1, open);
        let mut in_string = false;
        while i < bytes.len() {
            let b = bytes[i];
            if in_string {
                match b {
                    b'\\' => i += 1,
                    b'"' => in_string = false,
                    _ => {}
                }
            } else {
                match b {
                    b'"' => in_string = true,
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            arguments.push(&text[start..i]);
                            return arguments;
                        }
                    }
                    b',' if depth == 1 => {
                        arguments.push(&text[start..i]);
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        arguments
    }

    /// [`raw_errors_in`] of `source` as it ships, but the lines marked
    /// [`RAW_TEXT_ALLOWED`] (on the line, or alone on the line above).
    fn raw_errors(source: &str) -> Vec<(usize, &'static str)> {
        let lines: Vec<&str> = source.lines().collect();
        let allowed = |line: usize| {
            let marked = |n: usize| {
                n >= 1
                    && lines
                        .get(n - 1)
                        .is_some_and(|text| text.contains(RAW_TEXT_ALLOWED))
            };
            marked(line) || (marked(line - 1) && lines[line - 2].trim_start().starts_with("//"))
        };
        raw_errors_in(&shipping_text(source))
            .into_iter()
            .filter(|&(line, _)| !allowed(line))
            .collect()
    }

    /// WRT-04, WRT-08: views put no raw error into visible text. A failure
    /// reads "Couldn't <do X>. <Cause>. <Next step>." with its cause from
    /// `CliError::user_text` (or `jobs::core_user_text`); the code stays in
    /// the log, Copy details and Diagnostics.
    #[test]
    fn views_show_no_raw_error_or_code() {
        let sources = interface_sources();
        let views: Vec<_> = sources
            .iter()
            .filter(|(name, _)| name.starts_with("views/") || name == "state.rs")
            .collect();
        for scanned in [
            "views/setup.rs",
            "views/meeting.rs",
            "views/panel.rs",
            "state.rs",
        ] {
            assert!(
                views.iter().any(|(name, _)| name == scanned),
                "{scanned} was not read"
            );
        }
        let found: Vec<String> = views
            .iter()
            .flat_map(|(name, source)| {
                raw_errors(source)
                    .into_iter()
                    .map(move |(line, what)| format!("{name}:{line} shows {what}"))
            })
            .collect();
        assert!(
            found.is_empty(),
            "show failures as \"Couldn't <do X>. <Cause>. <Next step>.\" with the cause from \
             CliError::user_text (jobs::failure_text):\n{}",
            found.join("\n")
        );
    }

    #[test]
    fn the_raw_error_scan_finds_interpolations_and_spares_logs_and_tests() {
        for (code, what) in [
            ("format!(\"Cannot read people: {e}\")", "{e}"),
            ("format!(\"Search failed: {error}\")", "{error}"),
            ("format!(\"x {e:?}\")", "{e:"),
            ("Some(e.message.clone())", "e.message"),
            ("Err(error) => Failed(error.message)", "error.message"),
            // Positional and named arguments.
            (
                "format!(\"Failed: {}\", e)",
                "an error as a format argument",
            ),
            ("format!(\"{}\", e)", "an error as a format argument"),
            (
                "row.child(format!(\"Couldn't {}: {}\", what, &why))",
                "an error as a format argument",
            ),
            (
                "format!(\"x {cause}\", cause = reason)",
                "an error as a format argument",
            ),
            ("write!(f, \"{}\", err)", "an error as a format argument"),
            // An error turned into a string, wherever it goes.
            ("tell(card, error.to_string(), true)", "error.to_string()"),
            (
                "failure_text(\"Couldn't save.\", &e.to_string(), \"\")",
                "e.to_string()",
            ),
            (".map_err(|e| e.to_string())", "e.to_string()"),
            ("sentence(&why.to_string())", "why.to_string()"),
            // Debug text of anything.
            ("format!(\"Status: {:?}\", status)", "{:?}"),
            ("format!(\"{:#?}\", value)", "{:?}"),
            ("format!(\"Stop reading {selected:?}\")", "{:?}"),
            // An error's named Debug capture, whatever its name.
            ("format!(\"Couldn't read it: {cause:?}\")", "{:?}"),
            ("format!(\"Couldn't read it: {why:#?}\")", "{:?}"),
            ("format!(\"Couldn't read it: {reason:?}\")", "{:?}"),
            ("format!(\"Couldn't read it: {error:?}\")", "{error:"),
            ("format!(\"Couldn't read it: {err:#?}\")", "{err:"),
        ] {
            assert_eq!(raw_errors_in(&shipping_text(code)), [(1, what)], "{code}");
        }
        // Debug of an error is found once, as before.
        assert_eq!(
            raw_errors_in(&shipping_text("format!(\"Group catalog: {:?}\", error)")),
            [(1, "an error as a format argument"), (1, "{:?}")]
        );
        // A reviewed use is marked on its line or alone on the line above.
        for code in [
            "SharedString::from(format!(\"x-{kind:?}\")) // raw-text-allowed: an element id",
            "// raw-text-allowed: SYS-63 (U20) words it.\nformat!(\"Google \u{b7} {:?}\", s)",
        ] {
            assert!(raw_errors(code).is_empty(), "{code}");
        }
        // A marker covers its own line and the next one only.
        assert_eq!(
            raw_errors("// raw-text-allowed: x\nfine()\nformat!(\"{:?}\", s)"),
            [(3, "{:?}")]
        );
        assert_eq!(
            raw_errors("let a = 1; // raw-text-allowed: x\nformat!(\"{:?}\", s)"),
            [(2, "{:?}")]
        );
        for code in [
            "failure_text(\"Couldn't load people.\", &e.user_text(), \"\")",
            "problem.message.clone()",
            "self.message = None",
            "notice.message",
            "e.messages()",
            "tracing::warn!(\"reading: {e}\")",
            "tracing::warn!(\n    error = %e,\n    \"reading {} ({})\",\n    a,\n    b\n)",
            "tracing::warn!(\"x {:?}\", error.to_string())",
            "// format!(\"{e}\")",
            "format!(\"{} of {}\", count, total)",
            "format!(\"{name} isn't {}\", reasons.len())",
            "code.to_string()",
            "self.error_text.to_string()",
            "format!(\"{{literal}}\")",
            "if let Some(x) = y.ok()? {}",
        ] {
            assert!(raw_errors_in(&shipping_text(code)).is_empty(), "{code}");
        }
        let source = "\
fn shown() -> String { failure_text(\"Couldn\u{2019}t\", &cause, \"\") }\n\
#[cfg(test)]\n\
mod tests {\n\
    fn t() { assert_eq!(format!(\"{e}\"), x); }\n\
}\n\
fn late() -> String { format!(\"{error}\") }\n";
        assert_eq!(raw_errors_in(&shipping_text(source)), [(6, "{error}")]);
    }
}
