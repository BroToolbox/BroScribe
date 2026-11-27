//! `broscribe-app`: wires the native platform into the GPUI app.
//!
//! GPUI owns the main loop. The status item is created inside the launch
//! callback, on that loop; its clicks arrive through a channel drained on the
//! foreground executor. The native menu bar and shortcuts are GPUI actions on
//! the same loop. `cx.quit()` never returns from `run` (AppKit exits inside
//! `terminate:`), so everything that must happen at exit is in
//! `cx.on_app_quit`. The recording agent owns capture. The guarded Quit
//! command can ask it to stop and save the session the person confirmed.
//!
//! A launch that cannot start (unreadable settings, unusable library or data
//! folder, no CLI) shows a startup error window and then exits with a
//! failure status; stderr alone is invisible to an app opened from the
//! Finder.
//!
//! A second launch on the same data folder hands over to the running app
//! through the app's own single-instance channel, which brings its main
//! window forward (`Shell::show(None)`); when that fails it shows where the
//! running app is ("BroScribe is already running") and exits.

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;

use broscribe_app::daemon::{DaemonClient, DaemonEvent};
use broscribe_app::launch::{self, AgentStarter, Args, Prepared, StartupFailure};
use broscribe_app::menu::{self, AppCommand, StatusSources};
use broscribe_app::notify::{NotificationEvent, NotificationHost};
use broscribe_app::state::{AppState, AppStateInit, Route};
use broscribe_app::ui;
use broscribe_app::ui::windows;
use broscribe_app::ui_state;
use broscribe_app::views::about;
use broscribe_app::views::panel::{self, PanelReason, PanelWindow};
use broscribe_app::views::setup::{self, SetupDeps, SetupRuns};
use broscribe_platform::overlay::RecordingOverlay;
use broscribe_platform::shell::DockCommand;
use broscribe_platform::{HandoffReceiver, HandoffRequest, StatusIconPlace};
use broscribe_platform_native::NativePlatform;
use clap::Parser as _;
use futures::channel::mpsc;
use gpui::component::Root;
use gpui::{App, AppContext as _, Entity, WindowHandle};

/// What the menu-bar item, the menu bar and the Dock's reopen act on.
struct Shell {
    state: Entity<AppState>,
    deps: SetupDeps,
    window: Option<WindowHandle<Root>>,
    panel: Option<PanelWindow>,
    about: Option<WindowHandle<Root>>,
    /// Presents the recording panel without activating the app (design §4).
    overlay: Rc<dyn RecordingOverlay>,
    daemon: Entity<DaemonClient>,
    notifications: Entity<NotificationHost>,
    agent: Entity<AgentStarter>,
    quit_prompt: Rc<std::cell::Cell<bool>>,
    pending_dock_recording: Option<DockCommand>,
    _update_checks: gpui::Task<()>,
}

impl Shell {
    fn app_command(&mut self, command: AppCommand, cx: &mut App) {
        match command {
            AppCommand::Quit => self.request_quit(cx),
            AppCommand::TextSize(change) => menu::apply_text_size(&self.state, change, cx),
            AppCommand::About => self.show_about(cx),
            AppCommand::Diagnostics | AppCommand::Licences => {
                self.show_about(cx);
                if let Some(window) = self.about {
                    about::help_destination(window, command == AppCommand::Diagnostics, cx);
                }
            }
            AppCommand::OpenMain => self.show(None, cx),
            AppCommand::Search
            | AppCommand::Import { .. }
            | AppCommand::Library
            | AppCommand::ReviewQueue
            | AppCommand::People => {
                self.show(None, cx);
                if let Some(window) = self.window {
                    ui::main_window_command(window, command, cx);
                }
            }
            AppCommand::Shortcuts => {
                self.show(None, cx);
                if let Some(window) = self.window {
                    menu::show_shortcuts(window.into(), cx);
                }
            }
            AppCommand::Settings => self.show_settings(None, cx),
            AppCommand::RecordingControls => self.show_panel(PanelReason::Command, cx),
            AppCommand::RecordingControlsFromStatus => self.show_panel(PanelReason::Explicit, cx),
            AppCommand::Setup => {
                if let Err(error) = ui::show_setup_window(&self.state, &self.deps, cx) {
                    tracing::error!(%error, "cannot open the setup assistant");
                }
            }
            AppCommand::UpdatesFromStatus if menu::update_waiting(cx) => self.show(None, cx),
            AppCommand::Updates | AppCommand::UpdatesFromStatus => {
                if let Err(error) = ui::settings_window::show_updates(&self.state, &self.deps, cx) {
                    tracing::error!(%error, "cannot open Updates");
                }
            }
            AppCommand::Record(action) => self.recording_command(action, cx),
            AppCommand::StartAgent => self.start_agent(cx),
            AppCommand::CopyAgentRecovery => self
                .agent
                .update(cx, |agent, cx| agent.copy_recovery_details(cx)),
        }
    }

    fn dock_command(&mut self, command: DockCommand, cx: &mut App) {
        let action = match command {
            DockCommand::ReviewQueue => AppCommand::ReviewQueue,
            DockCommand::ShowRecordingControls => AppCommand::RecordingControlsFromStatus,
            recording => {
                let daemon = self.daemon.read(cx);
                if daemon.is_connecting() {
                    self.pending_dock_recording = Some(recording);
                    return;
                }
                if !daemon.connected {
                    self.show_panel(PanelReason::Explicit, cx);
                    return;
                }
                AppCommand::Record(match recording {
                    DockCommand::StartRecording => menu::RecordingMenuAction::Start,
                    _ => menu::RecordingMenuAction::Stop,
                })
            }
        };
        self.app_command(action, cx);
    }

    fn request_quit(&mut self, cx: &mut App) {
        if self.quit_prompt.get() {
            return;
        }
        let Some(recording) = self.daemon.read(cx).panel.recording.clone() else {
            cx.quit();
            return;
        };
        self.show(None, cx);
        if let Some(window) = self.window {
            windows::confirm_recording_quit(
                window,
                recording,
                self.daemon.clone(),
                self.quit_prompt.clone(),
                |cx| cx.quit(),
                cx,
            );
        }
    }

    fn show(&mut self, route: Option<Route>, cx: &mut App) {
        ui::show_main_window(&mut self.window, &self.state, &self.deps, cx);
        self.sync_taskbar(cx);
        if let Some(route) = route {
            self.state.update(cx, |state, cx| state.navigate(route, cx));
        }
    }

    fn show_settings(&self, page: Option<broscribe_app::state::SettingsPage>, cx: &mut App) {
        if let Err(error) = ui::settings_window::show(&self.state, &self.deps, page, cx) {
            tracing::error!(%error, "cannot open Settings");
        }
    }

    fn recording_command(&mut self, action: menu::RecordingMenuAction, cx: &mut App) {
        let daemon = self.daemon.read(cx);
        if !daemon.connected
            || (action == menu::RecordingMenuAction::Start
                && (daemon.record_start_pending() || menu::update_waiting(cx)))
        {
            return;
        }
        if action == menu::RecordingMenuAction::Bookmark
            && let Some(panel) = &self.panel
            && panel
                .handle
                .update(cx, |_, window, cx| {
                    panel
                        .panel
                        .update(cx, |panel, cx| panel.bookmark(window, cx))
                        .is_ok()
                })
                .unwrap_or(false)
        {
            return;
        }
        let call = action.call(self.daemon.read(cx).panel.recording.as_ref());
        if let Some(call) = call {
            self.daemon.update(cx, |daemon, cx| daemon.send(call, cx));
            if matches!(
                action,
                menu::RecordingMenuAction::Start | menu::RecordingMenuAction::Stop
            ) {
                self.show_panel(PanelReason::Automatic, cx);
            }
        }
    }

    fn start_agent(&mut self, cx: &mut App) {
        self.agent.update(cx, |agent, cx| agent.recover(cx));
    }

    fn show_panel(&mut self, reason: PanelReason, cx: &mut App) {
        let main = self
            .window
            .and_then(|main| main.update(cx, |_, window, _| window.bounds()).ok());
        panel::show(
            &mut self.panel,
            &self.overlay,
            reason,
            main,
            &self.daemon,
            &self.notifications,
            &self.state,
            &self.agent,
            cx,
        );
        self.sync_taskbar(cx);
    }

    fn sync_taskbar(&self, cx: &mut App) {
        let daemon = self.daemon.read(cx);
        let recording = daemon
            .connected
            .then(|| {
                daemon
                    .panel
                    .recording
                    .as_ref()
                    .map(|recording| recording.status)
            })
            .flatten();
        for window in self
            .window
            .into_iter()
            .chain(self.panel.as_ref().map(|panel| panel.handle))
        {
            menu::sync_taskbar(window, recording, cx);
        }
    }

    fn show_about(&mut self, cx: &mut App) {
        show_about(
            &mut self.about,
            Some(self.state.read(cx).cli().clone()),
            Some(self.daemon.clone()),
            cx,
        );
    }
}

/// Brings the About window forward, opening it if needed. A deliberate
/// open: an accessory launch gets its Dock icon first (SHL-15).
fn show_about(
    about: &mut Option<WindowHandle<Root>>,
    diagnostics: Option<broscribe_app::jobs::CliRunner>,
    agent: Option<Entity<DaemonClient>>,
    cx: &mut App,
) {
    windows::activate_for_regular_window(cx);
    let reused = about.as_ref().is_some_and(|w| {
        w.update(cx, |_, window, _| window.activate_window())
            .is_ok()
    });
    if !reused {
        match about::open_about_window(
            NativePlatform::about_reader(about::APP_VERSION),
            diagnostics,
            agent,
            cx,
        ) {
            Ok(handle) => *about = Some(handle),
            Err(e) => {
                tracing::error!("cannot open the About window: {e}");
                windows::settle_presence(cx);
            }
        }
    }
}

fn show_help(about: &mut Option<WindowHandle<Root>>, command: AppCommand, cx: &mut App) {
    show_about(about, None, None, cx);
    if command != AppCommand::About
        && let Some(window) = *about
    {
        about::help_destination(window, command == AppCommand::Diagnostics, cx);
    }
}

fn main() -> ExitCode {
    launch::init_logging();
    let args = Args::parse();
    let platform_paths = NativePlatform::paths();
    let (init, mut instance) = match launch::prepare(&args, platform_paths.as_ref()) {
        Ok(Prepared::Normal(normal)) => match normal.initialize(&args) {
            Ok(initialized) => initialized,
            Err(failure) => return startup_failed(failure, args.accessory),
        },
        Ok(Prepared::Maintenance { paths, admission }) => {
            return maintenance_started(paths.data_dir, admission, args.accessory);
        }
        Ok(Prepared::Exit(code)) => return code,
        Ok(Prepared::AlreadyRunning(place)) => return already_running(place),
        Err(failure) => return startup_failed(failure, args.accessory),
    };
    let handoffs = instance.take_handoffs();
    // AppKit may exit from `terminate:` without returning from `run`.
    // This root is never tied to a window; asynchronous effects own clones too.
    let _admission = init.admission.clone();

    let shell: Rc<RefCell<Option<Shell>>> = Rc::new(RefCell::new(None));
    let app = gpui::application().with_assets(ui::assets::Assets);
    app.on_reopen({
        let shell = shell.clone();
        move |cx| {
            if let Some(shell) = shell.borrow_mut().as_mut() {
                shell.show(None, cx);
            }
        }
    });
    app.run(move |cx| launched(init, args, handoffs, &shell, cx));
    drop(instance);
    ExitCode::SUCCESS
}

/// A second launch whose handoff failed: says the app is already running
/// and where its icon is (SHL-14), then exits with a failure status when
/// closed. Nothing else starts.
fn already_running(place: StatusIconPlace) -> ExitCode {
    let app = gpui::application().with_assets(ui::assets::Assets);
    let window: Rc<RefCell<Option<WindowHandle<Root>>>> = Rc::new(RefCell::new(None));
    app.on_reopen({
        let window = window.clone();
        move |cx| {
            if let Some(window) = window.borrow().as_ref() {
                window
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
            }
        }
    });
    app.run(move |cx| {
        ui::theme::init(cx);
        ui::install_announcer(NativePlatform::announcer(), cx);
        let about = Rc::new(RefCell::new(None));
        menu::install_app_menu(
            None,
            move |command, cx| match command {
                AppCommand::Quit => cx.quit(),
                AppCommand::About | AppCommand::Diagnostics | AppCommand::Licences => {
                    show_help(&mut about.borrow_mut(), command, cx);
                }
                _ => {}
            },
            cx,
        );
        // `terminate:` would otherwise exit with success.
        cx.on_app_quit(|_| -> futures::future::Ready<()> { std::process::exit(1) })
            .detach();
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        match ui::open_already_running_window(place, |cx| cx.quit(), cx) {
            Ok(handle) => *window.borrow_mut() = Some(handle),
            Err(e) => {
                tracing::error!("cannot open the already-running notice: {e}");
                std::process::exit(1);
            }
        }
        // The user launched BroScribe: the notice comes forward.
        cx.activate(true);
    });
    ExitCode::FAILURE
}

/// Intentional control-only startup, using the startup-notice window path.
fn maintenance_started(
    data_dir: std::path::PathBuf,
    admission: Arc<broscribe_core::maintenance::ModeLease>,
    accessory: bool,
) -> ExitCode {
    let _admission = admission;
    let app = gpui::application().with_assets(ui::assets::Assets);
    let window: Rc<RefCell<Option<WindowHandle<Root>>>> = Rc::new(RefCell::new(None));
    app.on_reopen({
        let window = window.clone();
        move |cx| {
            if let Some(window) = window.borrow().as_ref() {
                window
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
            }
        }
    });
    app.run(move |cx| {
        ui::theme::init(cx);
        ui::install_announcer(NativePlatform::announcer(), cx);
        if accessory {
            cx.set_activation_policy(gpui::ActivationPolicy::Accessory);
        }
        let about = Rc::new(RefCell::new(None));
        menu::install_app_menu(
            None,
            move |command, cx| match command {
                AppCommand::Quit => cx.quit(),
                AppCommand::About | AppCommand::Diagnostics | AppCommand::Licences => {
                    show_help(&mut about.borrow_mut(), command, cx);
                }
                _ => {}
            },
            cx,
        );
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        match ui::open_maintenance_window(data_dir, cx) {
            Ok(handle) => *window.borrow_mut() = Some(handle),
            Err(error) => {
                tracing::error!("cannot open maintenance window: {error}");
                std::process::exit(1);
            }
        }
        if !accessory {
            cx.activate(true);
        }
    });
    ExitCode::SUCCESS
}

/// Shows why the launch failed until the user quits or closes the window,
/// then exits with a failure status. Nothing else starts: no agent link,
/// library or status item.
fn startup_failed(failure: StartupFailure, accessory: bool) -> ExitCode {
    tracing::error!("cannot start: {failure}");
    let _admission = failure.admission();
    let window: Rc<RefCell<Option<WindowHandle<Root>>>> = Rc::new(RefCell::new(None));
    let app = gpui::application()
        .with_assets(ui::assets::Assets)
        .with_restart_arguments(std::env::args_os().skip(1).collect());
    app.on_reopen({
        let window = window.clone();
        move |cx| {
            if let Some(window) = window.borrow().as_ref() {
                window
                    .update(cx, |_, window, _| window.activate_window())
                    .ok();
            }
        }
    });
    app.run(move |cx| {
        ui::theme::init(cx);
        ui::install_announcer(NativePlatform::announcer(), cx);
        if accessory {
            cx.set_activation_policy(gpui::ActivationPolicy::Accessory);
        }
        let about = Rc::new(RefCell::new(None));
        menu::install_app_menu(
            None,
            move |command, cx| match command {
                AppCommand::Quit => cx.quit(),
                AppCommand::About | AppCommand::Diagnostics | AppCommand::Licences => {
                    show_help(&mut about.borrow_mut(), command, cx);
                }
                // Nothing else exists while the launch has failed.
                _ => {}
            },
            cx,
        );
        // `terminate:` would otherwise exit with success.
        cx.on_app_quit(|_| -> futures::future::Ready<()> { std::process::exit(1) })
            .detach();
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        match ui::open_startup_error_window(failure, |cx| cx.quit(), cx) {
            Ok(handle) => *window.borrow_mut() = Some(handle),
            Err(e) => {
                tracing::error!("cannot open the startup error window: {e}");
                std::process::exit(1);
            }
        }
        // An accessory launch (the agent's) shows the window without
        // taking focus; a launch from the Finder brings it forward.
        if !accessory {
            cx.activate(true);
        }
    });
    ExitCode::FAILURE
}

fn launched(
    init: AppStateInit,
    args: Args,
    handoffs: Option<HandoffReceiver>,
    shell: &Rc<RefCell<Option<Shell>>>,
    cx: &mut App,
) {
    let accessory = args.accessory || args.dock_action.is_some();
    ui::theme::init(cx);
    // Increase Contrast, Reduce Transparency and contrast themes (ACC-32).
    ui::theme::follow_display_accessibility(NativePlatform::display_accessibility(), cx);
    ui::install_announcer(NativePlatform::announcer(), cx);
    let mac = NativePlatform::host_platform() == broscribe_platform::HostPlatform::MacOs;
    let state = cx.new(|cx| AppState::new(init, cx));
    windows::install_presence(
        accessory,
        mac && !state.read(cx).config().show_status_item(),
        Rc::new(windows::GpuiActivation),
        cx,
    );
    ui::follow_reading_size(&state, cx);
    // Where the windows were left, and the section the main window showed
    // (SHL-39).
    ui_state::install(&state.read(cx).paths().data_dir.clone(), cx);
    if let Some(section) = ui_state::main_window_section(cx) {
        state.update(cx, |state, cx| state.navigate(section.route(), cx));
    }
    let daemon = DaemonClient::attach(
        state.read(cx).paths().data_dir.clone(),
        state.read(cx).cli().admission(),
        cx,
    );
    state.update(cx, |state, cx| state.attach_daemon(daemon.clone(), cx));
    let autostart = std::env::current_exe()
        .ok()
        .map(|exe| Arc::from(NativePlatform::autostart(exe)));
    let agent =
        cx.new(|cx| AgentStarter::with_autostart(state.clone(), daemon.clone(), autostart, cx));
    state.update(cx, |state, _| state.attach_agent_starter(&agent));
    let paths = state.read(cx).paths();
    let profile = paths.data_dir_overridden.then(|| paths.data_dir.clone());
    let notifications = NotificationHost::attach(
        daemon.clone(),
        NativePlatform::notifier(profile.as_deref()),
        paths.data_dir.clone(),
        cx,
    );
    notifications.update(cx, |host, _| host.follow_meeting_focus(&state));
    // Finished generations the user started are announced locally.
    state.update(cx, |state, _| {
        let notifications = notifications.clone();
        state.set_generation_sink(Rc::new(move |request, cx| {
            notifications.update(cx, |host, cx| host.post_local(request, cx))
        }))
    });
    // Installation checks and socket IO never block GPUI.
    let data_dir = state.read(cx).paths().data_dir.clone();
    let observation_admission = state.read(cx).cli().admission();
    let installed = cx.background_executor().spawn(async move {
        let _admission = observation_admission;
        let installed = std::env::current_exe()
            .map_err(|e| broscribe_core::Error::io("locating app", e))
            .and_then(|exe| NativePlatform::autostart(exe).is_enabled());
        let running = broscribe_platform_native::ipc::Client::connect_daemon(
            &broscribe_platform_native::ipc::Endpoint::new(&data_dir),
            std::time::Duration::from_secs(2),
        )
        .map(|client| client.is_some())
        .unwrap_or(true);
        (installed, running)
    });
    let fallback_state = state.clone();
    let fallback_daemon = daemon.clone();
    cx.spawn(async move |cx| {
        let (installed, running) = installed.await;
        cx.update(|cx| {
            let allowed = broscribe_app::daemon::local_fallback_allowed(
                installed,
                running || fallback_daemon.read(cx).connected,
            );
            fallback_state.update(cx, |state, cx| state.set_local_jobs_allowed(allowed, cx));
        });
    })
    .detach();
    // Capture folders of earlier processes only; an assistant of this one
    // may already be writing its own, so the sweep can run off the UI loop.
    let cache_dir = state.read(cx).paths().cache_dir.clone();
    let cleanup_admission = state.read(cx).cli().admission();
    cx.background_executor()
        .spawn(async move {
            let _admission = cleanup_admission;
            setup::remove_orphan_capture_dirs(&cache_dir)
        })
        .detach();
    state.read(cx).discard_abandoned_setup_imports(cx);
    state.update(cx, |state, _| {
        state.set_player(NativePlatform::audio_player());
        state.set_browser(Arc::from(NativePlatform::browser()));
    });
    let deps = SetupDeps {
        mic: Arc::from(NativePlatform::mic_capture()),
        system: Arc::from(NativePlatform::system_info()),
    };

    let watcher = NativePlatform::library_watcher();
    state.update(cx, |state, cx| {
        if let Err(e) = state.attach_watcher(watcher.as_ref(), cx) {
            tracing::warn!("library watcher unavailable; refreshing on activation only: {e}");
        }
    });

    let paths = state.read(cx).paths();
    let library_override = args
        .library
        .as_ref()
        .or(paths.library_from_env.as_ref())
        .and_then(|path| std::path::absolute(path).ok());
    let jump_list = NativePlatform::jump_list(profile.as_deref(), library_override.as_deref())
        .inspect_err(|error| {
            tracing::warn!(code = error.code(), "initializing the taskbar jump list")
        })
        .ok()
        .flatten();
    let (tx, clicks) = mpsc::unbounded::<String>();
    let item = (!mac || state.read(cx).config().show_status_item())
        .then(|| NativePlatform::status_item(tx.clone()))
        .and_then(|result| {
            result
                .inspect_err(|error| tracing::warn!(%error, "status item unavailable"))
                .ok()
        });
    let window = open_initial_window(&args, item.is_some(), &state, &deps, cx)
        .inspect_err(|error| tracing::error!(%error, "cannot open the main window"))
        .ok()
        .flatten();
    if !accessory {
        cx.activate(true);
    }
    *shell.borrow_mut() = Some(Shell {
        state: state.clone(),
        deps,
        window,
        panel: None,
        about: None,
        overlay: NativePlatform::recording_overlay(),
        daemon: daemon.clone(),
        notifications: notifications.clone(),
        agent: agent.clone(),
        _update_checks: broscribe_app::views::update::start_automatic_checks(&state, cx),
        quit_prompt: Rc::new(std::cell::Cell::new(false)),
        pending_dock_recording: None,
    });
    observe_shell_window_close(shell, NativePlatform::host_platform(), cx);

    // A later launch brings the main window forward through the app's own
    // channel (SHL-14), whether or not the agent runs.
    if let Some(handoffs) = handoffs {
        let shell = shell.clone();
        launch::serve_app_control(
            handoffs,
            move |handoff, cx| {
                use broscribe_platform::system::NotificationAuthorization as Auth;
                match handoff.request {
                    HandoffRequest::NotificationStatus => {
                        let notifier = NativePlatform::notifier(profile.as_deref());
                        cx.background_executor()
                            .spawn(async move {
                                let status = match notifier.authorization() {
                                    Auth::Unsupported => "unsupported",
                                    Auth::Unknown => "unknown",
                                    Auth::NotDetermined => "not_determined",
                                    Auth::Granted => "granted",
                                    Auth::Denied => "denied",
                                };
                                handoff.reply(status);
                            })
                            .detach();
                    }
                    HandoffRequest::RequestNotifications => {
                        let notifier = NativePlatform::notifier(profile.as_deref());
                        cx.background_executor()
                            .spawn(async move {
                                notifier.request_authorization();
                            })
                            .detach();
                        handoff.done();
                    }
                    HandoffRequest::AccessibilityStatus | HandoffRequest::RequestAccessibility => {
                        accessibility_handoff(handoff, cx, NativePlatform::app_ui_reader);
                    }
                    HandoffRequest::Show | HandoffRequest::Dock(_) => {
                        if let Some(shell) = shell.borrow_mut().as_mut() {
                            if let HandoffRequest::Dock(command) = handoff.request {
                                shell.dock_command(command, cx);
                            } else {
                                shell.show(None, cx);
                            }
                        }
                        // As before: a launch handed over is answered either way.
                        handoff.done();
                    }
                    HandoffRequest::LibrarySwitchStatus
                    | HandoffRequest::LibraryApplied
                    | HandoffRequest::ReloadLibrary => {
                        let Some(state) = shell.borrow().as_ref().map(|s| s.state.clone()) else {
                            return;
                        };
                        let imports = broscribe_app::views::import_sheet::ImportRuns::global(cx);
                        let current = state.read(cx);
                        let busy = broscribe_app::views::settings::library_switch_busy(
                            current,
                            imports.read(cx),
                            cx,
                        );
                        let data_dir = current.paths().data_dir.clone();
                        let default_library = current.paths().default_library_dir.clone();
                        let open_library = current.library().root().to_owned();
                        if handoff.request == HandoffRequest::LibrarySwitchStatus {
                            handoff.reply(if busy { "busy" } else { "ready" });
                        } else if handoff.request == HandoffRequest::ReloadLibrary && busy {
                            handoff.reply("busy");
                        } else {
                            let checking = cx.background_executor().spawn(async move {
                                let config =
                                    broscribe_core::config::Config::load(&data_dir).ok()?;
                                let desired = config
                                    .library
                                    .unwrap_or(default_library)
                                    .canonicalize()
                                    .ok()?;
                                Some(desired == open_library.canonicalize().ok()?)
                            });
                            cx.spawn(async move |cx| {
                                let same = checking.await;
                                cx.update(|cx| {
                                    let Some(same) = same else {
                                        return;
                                    };
                                    if handoff.request == HandoffRequest::LibraryApplied {
                                        handoff.reply(if same { "applied" } else { "pending" });
                                    } else if broscribe_app::views::settings::library_switch_busy(
                                        state.read(cx),
                                        imports.read(cx),
                                        cx,
                                    ) {
                                        handoff.reply("busy");
                                    } else {
                                        handoff.done();
                                        if !same {
                                            cx.restart();
                                        }
                                    }
                                });
                            })
                            .detach();
                        }
                    }
                }
            },
            cx,
        )
        .detach();
    }

    menu::install_app_menu(
        Some(&state),
        {
            let shell = shell.clone();
            move |command, cx| {
                let mut shell = shell.borrow_mut();
                let Some(shell) = shell.as_mut() else {
                    return;
                };
                shell.app_command(command, cx);
            }
        },
        cx,
    );

    menu::install_dock_menu(mac, jump_list, &daemon, cx);
    if let Some(command) = args.dock_action
        && let Some(shell) = shell.borrow_mut().as_mut()
    {
        shell.dock_command(command, cx);
    }

    cx.subscribe(&state, {
        let shell = shell.clone();
        move |_, event: &broscribe_app::state::AppEvent, cx| {
            if let Some(shell) = shell.borrow_mut().as_mut() {
                match event {
                    broscribe_app::state::AppEvent::OpenMainWindow => shell.show(None, cx),
                    broscribe_app::state::AppEvent::SetupFinished => {
                        shell.show(None, cx);
                        if let Some(main) = shell.window {
                            ui::focus_main_content(main, cx);
                        }
                    }
                    broscribe_app::state::AppEvent::OpenSettings(page) => {
                        shell.show_settings(*page, cx)
                    }
                    broscribe_app::state::AppEvent::OpenWordList => {
                        if let Err(error) =
                            ui::settings_window::show_word_list(&shell.state, &shell.deps, cx)
                        {
                            tracing::error!(%error, "cannot open the word list");
                        }
                    }
                    broscribe_app::state::AppEvent::OpenRecordingControls => {
                        shell.show_panel(PanelReason::Automatic, cx);
                    }
                    broscribe_app::state::AppEvent::OpenRecordingSetup => {
                        if let Err(error) =
                            ui::show_agent_setup_window(&shell.state, &shell.deps, cx)
                        {
                            tracing::error!(%error, "cannot open the setup assistant");
                        }
                    }
                    broscribe_app::state::AppEvent::ShowSetup => {
                        shell.app_command(AppCommand::Setup, cx);
                    }
                    broscribe_app::state::AppEvent::OpenLoginItems
                    | broscribe_app::state::AppEvent::OpenMicrophonePrivacy
                    | broscribe_app::state::AppEvent::OpenSystemAudioPrivacy => {
                        use broscribe_platform::SystemSettingsOpener as _;
                        let pane = match event {
                            broscribe_app::state::AppEvent::OpenLoginItems => {
                                broscribe_platform::SettingsPane::LoginItems
                            }
                            broscribe_app::state::AppEvent::OpenSystemAudioPrivacy => {
                                broscribe_platform::SettingsPane::SystemAudio
                            }
                            _ => broscribe_platform::SettingsPane::Microphone,
                        };
                        cx.background_executor()
                            .spawn(async move {
                                if let Err(error) = NativePlatform::system_settings().open(pane) {
                                    tracing::warn!(
                                        code = error.code(),
                                        "cannot open system settings"
                                    );
                                }
                            })
                            .detach();
                    }
                    _ => {}
                }
            }
        }
    })
    .detach();

    // State events drive opening once per session. Closing the panel leaves
    // capture and its daemon subscription alive; later meter ticks don't reopen it.
    let mut session: Option<String> = None;
    // The first snapshot after (re)connecting: the speaker recheck may have
    // finished or progressed while the app was not listening.
    let mut recheck_synced = false;
    cx.subscribe(&daemon, {
        let shell = shell.clone();
        let state = state.clone();
        move |_, event: &DaemonEvent, cx| {
            if matches!(event, DaemonEvent::Disconnected) {
                recheck_synced = false;
                if let Some(shell) = shell.borrow_mut().as_mut()
                    && shell.pending_dock_recording.take().is_some()
                {
                    shell.show_panel(PanelReason::Explicit, cx);
                }
            }
            if matches!(
                event,
                DaemonEvent::Disconnected | DaemonEvent::Received(broscribe_ipc::Event::State(_))
            ) && let Some(shell) = shell.borrow().as_ref()
            {
                shell.sync_taskbar(cx);
            }
            if let DaemonEvent::Received(broscribe_ipc::Event::State(snapshot)) = event {
                if let Some(shell) = shell.borrow_mut().as_mut()
                    && let Some(command) = shell.pending_dock_recording.take()
                {
                    shell.dock_command(command, cx);
                }
                if !recheck_synced {
                    recheck_synced = true;
                    state.update(cx, |state, cx| state.refresh_recheck(cx));
                }
                let next = snapshot
                    .recording
                    .as_ref()
                    .map(|recording| recording.session_id.clone());
                if next.is_some()
                    && next != session
                    && let Some(shell) = shell.borrow_mut().as_mut()
                {
                    shell.show_panel(PanelReason::Automatic, cx);
                }
                session = next;
                if let Some(progress) = &snapshot.jobs.running {
                    state.update(cx, |state, cx| state.daemon_progress(progress, cx));
                }
            } else if let DaemonEvent::Received(broscribe_ipc::Event::JobProgress(progress)) = event
            {
                state.update(cx, |state, cx| state.daemon_progress(progress, cx));
            } else if let DaemonEvent::Received(broscribe_ipc::Event::ShowMainWindow) = event {
                // `broscribe app show` through the agent (`app.show`); a
                // second launch uses the app's own channel instead.
                state.update(cx, |_, cx| {
                    cx.emit(broscribe_app::state::AppEvent::OpenMainWindow)
                });
            }
        }
    })
    .detach();
    cx.subscribe(&notifications, {
        let shell = shell.clone();
        let notifications = notifications.clone();
        move |_, event: &NotificationEvent, cx| {
            if let Some(shell) = shell.borrow_mut().as_mut() {
                match event {
                    NotificationEvent::FallbackChanged
                        if notifications.read(cx).has_fallback_ask() =>
                    {
                        shell.show_panel(PanelReason::Automatic, cx)
                    }
                    NotificationEvent::OpenPanel => shell.show_panel(PanelReason::Explicit, cx),
                    NotificationEvent::OpenLibrary => shell.show(Some(Route::library()), cx),
                    NotificationEvent::OpenRecording(session) => {
                        if shell
                            .daemon
                            .read(cx)
                            .panel
                            .stopped_session(session)
                            .is_some_and(|entry| {
                                matches!(
                                    entry.state,
                                    broscribe_ipc::StoppedRecordingState::Failed { .. }
                                )
                            })
                        {
                            shell.show_panel(PanelReason::Explicit, cx);
                            if shell
                                .panel
                                .as_ref()
                                .is_some_and(|panel| panel.show_failed_recording(session, cx))
                            {
                                return;
                            }
                        }
                        shell.show(Some(Route::library()), cx);
                    }
                    NotificationEvent::OpenMainWindow => shell.show(None, cx),
                    NotificationEvent::OpenIntegrations(provider) => {
                        if let Err(error) = ui::settings_window::show_calendar(
                            &shell.state,
                            &shell.deps,
                            provider,
                            cx,
                        ) {
                            tracing::error!(%error, "cannot open calendar settings");
                        }
                    }
                    NotificationEvent::OpenMeeting(id) => {
                        match broscribe_core::MeetingId::parse(id) {
                            Ok(id) => shell.show(Some(Route::meeting(id)), cx),
                            Err(error) => tracing::warn!(%error, "invalid notification meeting"),
                        }
                    }
                    _ => {}
                }
            }
        }
    })
    .detach();

    menu::install(
        item,
        clicks,
        StatusSources {
            state: state.clone(),
            daemon: daemon.clone(),
            notifications: notifications.clone(),
            agent: agent.clone(),
        },
        mac.then(|| Box::new(move || NativePlatform::status_item(tx.clone())) as _),
        cx,
    );
    if let Some(shell) = shell.borrow().as_ref() {
        shell.sync_taskbar(cx);
    }

    // Quitting stops the app's own playback and setup work and waits,
    // bounded, for setup helper-capture cancellations to finish; meeting
    // recordings belong to the agent and continue.
    cx.on_app_quit({
        let state = state.clone();
        move |cx| {
            let stopping = state.update(cx, |state, _| state.on_quit());
            let captures = SetupRuns::quit(cx);
            let timeout = cx.background_executor().timer(setup::QUIT_CANCEL_TIMEOUT);
            async move {
                let cleanup = async move {
                    captures.await;
                    if let Some(stopping) = stopping {
                        stopping.finished().await;
                    }
                };
                futures::future::select(Box::pin(cleanup), Box::pin(timeout)).await;
            }
        }
    })
    .detach();

    // Where the windows are, for the next launch.
    cx.on_app_quit(ui_state::save).detach();

    state.update(cx, |state, cx| state.refresh_all(cx));
}

fn accessibility_handoff(
    handoff: broscribe_platform::Handoff,
    cx: &App,
    reader: impl FnOnce() -> Box<dyn broscribe_platform::ui::AppUiReader> + Send + 'static,
) {
    if handoff.request == HandoffRequest::AccessibilityStatus {
        cx.background_executor()
            .spawn(async move {
                handoff.reply(if reader().is_trusted() {
                    "granted"
                } else {
                    "denied"
                });
            })
            .detach();
    } else {
        cx.background_executor()
            .spawn(async move {
                reader().request_trust();
            })
            .detach();
        handoff.done();
    }
}

fn open_initial_window(
    args: &Args,
    has_item: bool,
    state: &Entity<AppState>,
    deps: &SetupDeps,
    cx: &mut App,
) -> gpui::Result<Option<WindowHandle<Root>>> {
    let mut place = NativePlatform::status_icon_place();
    // A watcher that disappears after successful registration can return.
    if has_item && place == StatusIconPlace::NoTrayHost {
        place = StatusIconPlace::SystemTray;
    }
    ui::remember_status_icon_place(place, cx);
    if !args.accessory && args.dock_action.is_none() {
        ui::open_main_window(state, deps, cx).map(Some)
    } else if args.at_login && place == StatusIconPlace::NoTrayHost {
        ui::open_main_window_at_login(state, deps, cx).map(Some)
    } else {
        Ok(None)
    }
}

fn observe_shell_window_close(
    shell: &Rc<RefCell<Option<Shell>>>,
    platform: broscribe_platform::HostPlatform,
    cx: &mut App,
) {
    let quitting = Rc::new(std::cell::Cell::new(false));
    cx.on_app_quit({
        let quitting = quitting.clone();
        move |_| {
            quitting.set(true);
            async {}
        }
    })
    .detach();
    let shell = shell.clone();
    cx.on_window_closed(move |cx, id| {
        let mut slot = shell.borrow_mut();
        let Some(shell) = slot.as_mut() else {
            return;
        };
        if shell.window.is_some_and(|window| window.window_id() == id) {
            shell.window = None;
            if !quitting.get() {
                shell
                    .notifications
                    .update(cx, |host, cx| host.main_window_closed(platform, cx));
            }
        }
        if shell
            .panel
            .as_ref()
            .is_some_and(|panel| panel.handle.window_id() == id)
        {
            shell.panel = None;
            let daemon = shell.daemon.read(cx);
            if !quitting.get() && daemon.connected && daemon.panel.recording.is_some() {
                shell
                    .notifications
                    .update(cx, |host, cx| host.recording_controls_closed(platform, cx));
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_app::daemon::{EventStream, Transport};
    use broscribe_core::{Library, config::Config, maintenance::ModeLease};
    use broscribe_ipc::{Call, IpcError};
    use broscribe_platform::{
        HostPlatform,
        fakes::{FakeMicCapture, FakeNotifier, FakePaths, FakeStatusIconPlace, FakeSystemInfo},
        overlay::StandardOverlay,
    };
    use gpui::TestAppContext;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::time::Duration;

    #[gpui::test]
    fn accessibility_handoffs_defer_the_platform_calls(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeAppUiReader;
        use std::sync::mpsc::{TryRecvError, channel};

        for request in [
            HandoffRequest::AccessibilityStatus,
            HandoffRequest::RequestAccessibility,
        ] {
            let (handoff, reply) = broscribe_platform::Handoff::new(request);
            let (started, ran) = channel();
            cx.update(|cx| {
                accessibility_handoff(handoff, cx, move || {
                    started.send(()).unwrap();
                    Box::new(FakeAppUiReader::default())
                });
                assert_eq!(
                    ran.try_recv(),
                    Err(TryRecvError::Empty),
                    "work ran on the UI loop"
                );
                if request == HandoffRequest::AccessibilityStatus {
                    assert_eq!(reply.try_recv(), Err(TryRecvError::Empty));
                } else {
                    assert_eq!(
                        reply.try_recv(),
                        Ok(broscribe_platform::shell::HANDOFF_DONE)
                    );
                }
            });
            cx.run_until_parked();
            assert_eq!(ran.try_recv(), Ok(()));
            if request == HandoffRequest::AccessibilityStatus {
                assert_eq!(reply.try_recv(), Ok("granted"));
            }
        }
    }

    #[derive(Default)]
    struct Agent(Mutex<Vec<Call>>);

    impl Transport for Agent {
        fn subscribe(&self) -> Result<Box<dyn EventStream>, IpcError> {
            unreachable!("detached client")
        }

        fn call(&self, call: Call) -> Result<Value, IpcError> {
            self.0.lock().unwrap().push(call);
            Ok(json!({ "accepted": true }))
        }
    }

    fn shell_fixture(
        cx: &mut TestAppContext,
        dir: &std::path::Path,
        transport: Arc<Agent>,
        notifier: FakeNotifier,
    ) -> Rc<RefCell<Option<Shell>>> {
        cx.update(|cx| {
            ui::theme::init(cx);
            let mut paths = broscribe_platform::resolve_paths(&FakePaths::new(dir)).unwrap();
            paths.data_dir_overridden = true;
            let mut config = Config::default();
            config.setup.defer_assistant = true;
            let admission = Arc::new(ModeLease::acquire(&paths.data_dir, Duration::ZERO).unwrap());
            let init = AppStateInit::new(
                paths,
                config,
                Library::open(dir.join("lib")).unwrap(),
                dir.join("no-cli"),
                admission,
            )
            .unwrap();
            let state = cx.new(|cx| AppState::new(init, cx));
            let daemon = DaemonClient::detached(transport.clone(), cx);
            daemon.update(cx, |daemon, _| daemon.connected = true);
            let agent = cx.new(|cx| AgentStarter::new(state.clone(), daemon.clone(), cx));
            let notifications =
                NotificationHost::attach(daemon.clone(), Box::new(notifier), dir.into(), cx);
            Rc::new(RefCell::new(Some(Shell {
                state: state.clone(),
                deps: SetupDeps {
                    mic: Arc::new(FakeMicCapture::new(16_000)),
                    system: Arc::new(FakeSystemInfo::default()),
                },
                window: None,
                panel: None,
                about: None,
                overlay: Rc::new(StandardOverlay),
                daemon,
                notifications,
                agent,
                quit_prompt: Rc::default(),
                pending_dock_recording: None,
                _update_checks: gpui::Task::ready(()),
            })))
        })
    }

    #[gpui::test]
    fn only_a_login_launch_without_a_tray_shows_the_main_window_unfocused(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let shell = shell_fixture(
            cx,
            dir.path(),
            Arc::new(Agent::default()),
            FakeNotifier::default(),
        );
        let slot = shell.borrow();
        let Shell { state, deps, .. } = slot.as_ref().unwrap();
        state.update(cx, |state, cx| state.navigate(Route::people(), cx));
        let sentinel = cx.add_window(|_, _| gpui::Empty);
        sentinel
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        let mut args = Args::try_parse_from(["broscribe-app", "--accessory"]).unwrap();
        let _host = FakeStatusIconPlace::new(StatusIconPlace::NoTrayHost);
        assert!(
            cx.update(|cx| open_initial_window(&args, false, state, deps, cx))
                .unwrap()
                .is_none()
        );
        args.at_login = true;
        let main = cx
            .update(|cx| open_initial_window(&args, false, state, deps, cx))
            .unwrap()
            .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.active_window()), Some(sentinel.into()));
        main.update(cx, |_, window, cx| {
            assert!(window.visibility().is_visible());
            assert!(window.focused(cx).is_none());
            assert_eq!(state.read(cx).route(), &Route::people());
            assert_eq!(cx.windows().len(), 2, "a deferred assistant stays closed");
        })
        .unwrap();
        for place in [
            StatusIconPlace::SystemTray,
            StatusIconPlace::MenuBar,
            StatusIconPlace::NotificationArea,
        ] {
            let _host = FakeStatusIconPlace::new(place);
            assert!(
                cx.update(|cx| open_initial_window(&args, true, state, deps, cx))
                    .unwrap()
                    .is_none()
            );
        }
        state.update(cx, |state, cx| state.set_config(Config::default(), cx));
        cx.update(|cx| open_initial_window(&args, false, state, deps, cx))
            .unwrap()
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| cx.windows().len()),
            4,
            "fresh setup opens alongside the main window"
        );
        assert_eq!(cx.update(|cx| cx.active_window()), Some(sentinel.into()));
    }

    #[gpui::test]
    fn main_window_close_posts_platform_notice_once_across_launches(cx: &mut TestAppContext) {
        use broscribe_platform::system::NotificationDelivery;

        for (host, destination) in [
            (
                HostPlatform::Windows,
                Some("the Start menu or from Show hidden icons (^) on the taskbar."),
            ),
            (HostPlatform::Linux, Some("your app launcher.")),
            (HostPlatform::MacOs, None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let notifier = FakeNotifier::default();
            *notifier.delivery.lock().unwrap() = Some(NotificationDelivery::Delivered);
            for _ in 0..2 {
                let agent = Arc::new(Agent::default());
                let shell = shell_fixture(cx, dir.path(), agent.clone(), notifier.clone());
                cx.update(|cx| {
                    observe_shell_window_close(&shell, host, cx);
                    shell.borrow_mut().as_mut().unwrap().show(None, cx);
                });
                let other = cx.add_window(|_, _| gpui::Empty);
                let previous = notifier.actions.lock().unwrap().len();
                other
                    .update(cx, |_, window, _| window.remove_window())
                    .unwrap();
                cx.run_until_parked();
                assert_eq!(notifier.actions.lock().unwrap().len(), previous);
                let home = shell.borrow().as_ref().unwrap().window.unwrap();
                home.update(cx, |_, window, _| window.remove_window())
                    .unwrap();
                cx.run_until_parked();
                assert!(shell.borrow().as_ref().unwrap().window.is_none());
                assert!(agent.0.lock().unwrap().is_empty());
                // The close observer owns a strong reference for the life of the app.
                shell.borrow_mut().take();
            }
            let notices = notifier.actions.lock().unwrap();
            if let Some(destination) = destination {
                let [notice] = notices.as_slice() else {
                    panic!("expected one notice, got {notices:?}");
                };
                assert_eq!(notice.id, "window-closed");
                assert_eq!(notice.title, "BroScribe is still running");
                assert_eq!(
                    notice.body,
                    format!("Recording and processing continue. Open BroScribe from {destination}")
                );
                assert!(notice.actions.is_empty());
                assert!(notice.opens_on_click);
                assert!(dir.path().join("state/window-closed-shown").is_file());
            } else {
                assert!(notices.is_empty());
            }
        }
    }

    #[gpui::test]
    fn closing_controls_during_capture_posts_one_notice_and_leaves_capture_running(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::system::NotificationDelivery;
        for (host, place, title, reopen) in [
            (
                HostPlatform::MacOs,
                StatusIconPlace::MenuBar,
                "Still Recording",
                "menu bar",
            ),
            (
                HostPlatform::MacOs,
                StatusIconPlace::NoTrayHost,
                "Still Recording",
                "Window › Show Recording Controls",
            ),
            (
                HostPlatform::Windows,
                StatusIconPlace::NotificationArea,
                "Still recording",
                "Show hidden icons (^)",
            ),
            (
                HostPlatform::Linux,
                StatusIconPlace::SystemTray,
                "Still recording",
                "system tray",
            ),
            (
                HostPlatform::Linux,
                StatusIconPlace::NoTrayHost,
                "Still recording",
                "Recording button",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let notifier = FakeNotifier::default();
            *notifier.delivery.lock().unwrap() = Some(NotificationDelivery::Delivered);
            for _ in 0..2 {
                let agent = Arc::new(Agent::default());
                let shell = shell_fixture(cx, dir.path(), agent.clone(), notifier.clone());
                cx.update(|cx| {
                    ui::remember_status_icon_place(place, cx);
                    observe_shell_window_close(&shell, host, cx);
                    let mut slot = shell.borrow_mut();
                    let shell = slot.as_mut().unwrap();
                    shell.daemon.update(cx, |daemon, _| daemon.panel.recording = Some(serde_json::from_value(json!({
                        "session_id": "synthetic", "status": "recording", "started_at": "2026-11-23T09:00:00Z", "elapsed_s": 4., "health": { "tracks": [] }
                    })).unwrap()));
                    shell.show_panel(PanelReason::Automatic, cx);
                });
                let panel = shell
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .panel
                    .as_ref()
                    .unwrap()
                    .handle;
                panel
                    .update(cx, |_, window, _| window.remove_window())
                    .unwrap();
                cx.run_until_parked();
                let mut slot = shell.borrow_mut();
                let shell = slot.take().unwrap();
                assert!(shell.panel.is_none());
                assert!(
                    shell
                        .daemon
                        .read_with(cx, |daemon, _| daemon.panel.recording.is_some())
                );
                assert!(agent.0.lock().unwrap().iter().all(|call| !matches!(
                    call,
                    Call::RecordStop { .. } | Call::RecordDiscard { .. }
                )));
            }
            let notices = notifier.actions.lock().unwrap();
            let [notice] = notices.as_slice() else {
                panic!("expected one notice, got {notices:?}");
            };
            assert_eq!(
                (notice.id.as_str(), notice.title.as_str()),
                ("still-recording", title)
            );
            assert!(notice.body.contains(reopen));
            assert!(notice.opens_on_click);
            assert!(dir.path().join("state/still-recording-shown").is_file());
        }
    }

    #[gpui::test]
    fn shell_open_preserves_route_and_menu_recording_keeps_focus(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let transport = Arc::new(Agent::default());
        let shell = shell_fixture(cx, dir.path(), transport.clone(), FakeNotifier::default());
        let shell = cx.update(|cx| {
            let state = shell.borrow().as_ref().unwrap().state.clone();
            let commands = shell.clone();
            menu::install_app_menu(
                Some(&state),
                move |command, cx| {
                    commands
                        .borrow_mut()
                        .as_mut()
                        .unwrap()
                        .app_command(command, cx)
                },
                cx,
            );
            state.update(cx, |state, cx| state.navigate(Route::people(), cx));
            cx.dispatch_action(&menu::OpenBroScribe);
            shell
        });
        cx.run_until_parked();
        let home = shell.borrow().as_ref().unwrap().window.unwrap();
        let state = shell.borrow().as_ref().unwrap().state.clone();
        assert_eq!(
            state.read_with(cx, |state, _| state.route().clone()),
            Route::people()
        );
        home.update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| cx.dispatch_action(&menu::OpenBroScribe));
        cx.run_until_parked();
        let reopened = shell.borrow().as_ref().unwrap().window.unwrap();
        assert_ne!(reopened, home);
        assert_eq!(
            state.read_with(cx, |state, _| state.route().clone()),
            Route::people()
        );
        cx.run_until_parked();
        cx.update(|cx| cx.dispatch_action(&menu::OpenBroScribe));
        assert_eq!(shell.borrow().as_ref().unwrap().window, Some(reopened));
        cx.update(|cx| {
            shell.borrow().as_ref().unwrap().show_settings(None, cx);
            cx.dispatch_action(&menu::KeyboardShortcuts);
        });
        cx.run_until_parked();
        cx.update_window(reopened.into(), |_, window, cx| {
            use gpui::component::WindowExt as _;
            assert!(
                window.has_active_dialog(cx),
                "Keyboard shortcuts opens in the main window"
            );
            window.close_dialog(cx);
        })
        .unwrap();
        let active = cx.update(|cx| cx.active_window());
        cx.update(|cx| cx.dispatch_action(&menu::BeginRecording));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.active_window()), active);
        let recording: broscribe_ipc::RecordingState = serde_json::from_value(json!({
            "session_id": "s-synthetic", "status": "paused", "started_at": "2026-11-23T09:00:00Z",
            "elapsed_s": 4., "health": { "tracks": [] }
        }))
        .unwrap();
        shell
            .borrow()
            .as_ref()
            .unwrap()
            .daemon
            .update(cx, |daemon, _| daemon.panel.recording = Some(recording));
        cx.update(|cx| cx.dispatch_action(&menu::StopAndSaveRecording));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.active_window()), active);
        let calls = transport.0.lock().unwrap();
        assert!(matches!(calls.first(), Some(Call::RecordStart(_))));
        assert_eq!(
            calls.last(),
            Some(&Call::RecordStop {
                session_id: "s-synthetic".into()
            })
        );
    }
}
