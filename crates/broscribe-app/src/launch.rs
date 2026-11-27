//! Startup: logging, folders, config and library, resolved like the CLI
//! (`--library` flag > `BROSCRIBE_LIBRARY` > config > platform default),
//! the single-instance handoff (SHL-14) and the shared "Start recording
//! agent" control.
//!
//! A launch that cannot start reports why in a native window
//! ([`StartupFailure`], shown by [`crate::ui::open_startup_error_window`]):
//! an app opened from the Finder has no visible stderr.

#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use broscribe_core::Library;
use broscribe_core::config::Config;
use broscribe_core::lock::{AppInstanceLock, LockGuard};
use broscribe_core::maintenance::{Mode, ModeLease};
use broscribe_platform::paths::{ENV_DATA_DIR, resolve_paths_from};
#[cfg(test)]
use broscribe_platform::resolve_paths;
use broscribe_platform::shell::{DockCommand, HANDOFF_TIMEOUT};
use broscribe_platform::{
    AgentServiceStatus, AppPaths, Autostart, HandoffListener, HandoffReceiver, HandoffRequest,
    InstanceChannel, ResolvedPaths, StatusIconPlace, handoff_channel,
};
use broscribe_platform_native::NativePlatform;
use clap::Parser;
use futures::StreamExt as _;
use gpui::{App, Context, Entity, Subscription, Task};
use serde_json::Value;

use crate::daemon::{DaemonClient, DaemonEvent};
use crate::jobs::{self, ChildKind, CliError};
use crate::state::{AppState, AppStateInit};

#[derive(Debug, Parser)]
#[command(name = "broscribe-app", version, about = "BroScribe desktop app")]
pub struct Args {
    /// Library folder (overrides BROSCRIBE_LIBRARY and the config).
    #[arg(long, value_name = "PATH")]
    pub library: Option<PathBuf>,
    /// Run as a status-only notification host, without opening a window.
    #[arg(long)]
    pub accessory: bool,
    /// Distinguish a login launch from a launch to deliver a notification.
    #[arg(long, requires = "accessory")]
    pub at_login: bool,
    /// A Windows taskbar link requests a stable command.
    #[arg(long, hide = true, conflicts_with = "accessory", value_parser = parse_dock_command)]
    pub dock_action: Option<DockCommand>,
    /// A Windows shell shortcut selects the data directory.
    #[arg(long, hide = true)]
    pub dock_data_dir: Option<PathBuf>,
}

fn parse_dock_command(word: &str) -> Result<DockCommand, &'static str> {
    DockCommand::parse(word).ok_or("Unknown BroScribe taskbar action")
}

impl Args {
    pub fn handoff(&self) -> Option<HandoffRequest> {
        (!self.accessory).then(|| {
            self.dock_action
                .map_or(HandoffRequest::Show, HandoffRequest::Dock)
        })
    }
}

/// Logs to stderr (`RUST_LOG` filter, `info` by default). Also forwards the
/// `log` crate, which GPUI uses for its warnings (a missing text system is
/// only reported there).
pub fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // With tracing-subscriber's default `tracing-log` feature, `try_init`
    // also installs the `log` → `tracing` bridge.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// What a launch does after the single-instance check.
#[derive(Debug)]
pub enum Startup {
    /// The only app on this data folder: keep it for the process's life.
    Run(AppInstance),
    /// Another app holds the data folder; exit with this code.
    Exit(ExitCode),
    /// Another app holds the data folder and couldn't be brought forward:
    /// say so, and where its icon is (SHL-14).
    AlreadyRunning(StatusIconPlace),
    /// The data folder's instance lock is unusable: report it, then exit
    /// with a failure.
    Failed(StartupFailure),
}

/// The running app's claim on its data folder: the app instance lock, and
/// the channel later launches reach it through (SHL-14). Kept for the
/// process's life.
pub struct AppInstance {
    /// `None` when the channel couldn't listen: later launches then say
    /// where the app is instead of bringing it forward. Declared before the
    /// lock, so it stops listening before the lock is released.
    _listener: Option<Box<dyn HandoffListener>>,
    _lock: LockGuard,
    handoffs: Option<HandoffReceiver>,
}

impl AppInstance {
    /// Later launches' requests, to drain on the UI loop
    /// ([`serve_handoffs`]); `None` once taken.
    pub fn take_handoffs(&mut self) -> Option<HandoffReceiver> {
        self.handoffs.take()
    }
}

impl std::fmt::Debug for AppInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppInstance")
            .field("listening", &self._listener.is_some())
            .finish_non_exhaustive()
    }
}

/// The admitted mode decision, before any normal-only resources are constructed.
pub enum Prepared {
    Normal(NormalStartup),
    Maintenance {
        paths: ResolvedPaths,
        admission: Arc<ModeLease>,
    },
    Exit(ExitCode),
    /// Another app runs on this data folder and didn't come forward.
    AlreadyRunning(StatusIconPlace),
}

/// Normal-only startup ownership. Keeping the decision separate avoids a heap
/// allocation merely to carry a large app-state payload in the mode enum.
pub struct NormalStartup {
    paths: ResolvedPaths,
    admission: Arc<ModeLease>,
    instance: AppInstance,
    located: std::io::Result<PathBuf>,
}

impl NormalStartup {
    pub fn initialize(
        self,
        args: &Args,
    ) -> std::result::Result<(AppStateInit, AppInstance), StartupFailure> {
        let Self {
            paths,
            admission,
            instance,
            located,
        } = self;
        let normal = (|| {
            let config = Config::load(&paths.data_dir).map_err(|_| {
                StartupFailure::new(StartupStage::Config, Some(Config::path(&paths.data_dir)))
            })?;
            let library_dir = std::path::absolute(
                paths.library(args.library.as_deref(), config.library.as_deref()),
            )
            .map_err(|_| StartupFailure::new(StartupStage::Paths, None))?;
            let library = Library::open(library_dir.clone())
                .map_err(|_| StartupFailure::new(StartupStage::Library, Some(library_dir)))?;
            let cli = located.map_err(|_| StartupFailure::new(StartupStage::Cli, None))?;
            if jobs::check_cli(&cli).is_err() {
                return Err(StartupFailure::new(StartupStage::Cli, Some(cli)));
            }
            tracing::info!("starting BroScribe");
            let init = AppStateInit::new(paths, config, library, cli, admission.clone())
                .map_err(|_| StartupFailure::new(StartupStage::Paths, None))?;
            Ok((init, instance))
        })();
        normal.map_err(|mut failure| {
            failure.admission = Some(admission);
            failure
        })
    }
}

/// One app per data folder. The first one listens on the data folder's
/// single-instance channel. A second accessory launch (the agent's) exits
/// silently; a second normal launch asks the running app, through that
/// channel, to bring its main window forward (never through the recording
/// agent), and when it can't, shows where the running app is
/// ([`Startup::AlreadyRunning`], with the place `place` reports). An
/// unusable lock is a [`Startup::Failed`] the caller reports.
pub fn single_instance(
    lock: broscribe_core::Result<Option<LockGuard>>,
    request: Option<HandoffRequest>,
    channel: &dyn InstanceChannel,
    place: impl FnOnce() -> StatusIconPlace,
) -> Startup {
    match lock {
        Ok(Some(guard)) => {
            let (sender, handoffs) = handoff_channel();
            let listener = channel
                .listen(sender)
                .inspect_err(|error| {
                    tracing::warn!(
                        %error,
                        "a later launch can't bring this BroScribe forward"
                    );
                })
                .ok();
            Startup::Run(AppInstance {
                _listener: listener,
                _lock: guard,
                handoffs: Some(handoffs),
            })
        }
        Ok(None) => match request.map(|request| channel.send(request, HANDOFF_TIMEOUT)) {
            None | Some(Ok(())) => Startup::Exit(ExitCode::SUCCESS),
            Some(Err(error)) => {
                tracing::warn!(
                    %error,
                    "BroScribe is already running and didn't bring its window forward"
                );
                Startup::AlreadyRunning(place())
            }
        },
        Err(_) => Startup::Failed(StartupFailure::new(StartupStage::Instance, None)),
    }
}

/// [`single_instance`] on `data_dir`, through the app's own channel.
/// Claims the normal app instance only under startup admission.
pub fn claim_instance(admission: &ModeLease, request: Option<HandoffRequest>) -> Startup {
    let data_dir = admission.data_dir();
    if admission.require_normal().is_err() {
        return Startup::Failed(StartupFailure::new(
            StartupStage::Admission,
            Some(data_dir.into()),
        ));
    }
    let channel = NativePlatform::instance_channel(data_dir);
    match single_instance(
        AppInstanceLock::try_acquire(data_dir),
        request,
        channel.as_ref(),
        NativePlatform::status_icon_place,
    ) {
        Startup::Failed(failure) => Startup::Failed(StartupFailure {
            path: Some(data_dir.to_owned()),
            ..failure
        }),
        startup => startup,
    }
}

/// Answers later launches on the UI loop after handing the command to the shell.
pub fn serve_handoffs(
    requests: HandoffReceiver,
    handle: impl Fn(HandoffRequest, &mut App) + 'static,
    cx: &mut App,
) -> Task<()> {
    serve_app_control(
        requests,
        move |handoff, cx| {
            handle(handoff.request, cx);
            handoff.done();
        },
        cx,
    )
}

/// Runs authenticated requests on the desktop's UI loop.
pub fn serve_app_control(
    mut requests: HandoffReceiver,
    handle: impl Fn(broscribe_platform::shell::Handoff, &mut App) + 'static,
    cx: &mut App,
) -> Task<()> {
    cx.spawn(async move |cx| {
        while let Some(handoff) = requests.next().await {
            cx.update(|cx| handle(handoff, cx));
        }
    })
}

/// The step a launch failed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupStage {
    /// Resolving a stable profile or library path failed.
    Paths,
    /// The mode lock or persisted maintenance request could not be read.
    Admission,
    /// The data folder's single-instance lock.
    Instance,
    /// `config.toml` exists but cannot be read or is invalid.
    Config,
    /// The library folder cannot be opened or created.
    Library,
    /// The `broscribe` CLI next to the app (or named by its override)
    /// cannot be located, is missing or cannot be run.
    Cli,
}

/// Why the app cannot start, shown in the startup error window (and logged).
#[derive(Clone, Debug)]
pub struct StartupFailure {
    pub stage: StartupStage,
    /// The file or folder involved, when there is one.
    pub path: Option<PathBuf>,
    admission: Option<Arc<ModeLease>>,
}

impl PartialEq for StartupFailure {
    fn eq(&self, other: &Self) -> bool {
        self.stage == other.stage && self.path == other.path
    }
}
impl Eq for StartupFailure {}
impl StartupFailure {
    // Source errors can contain file contents, process output or private paths.
    pub fn new(stage: StartupStage, path: Option<PathBuf>) -> Self {
        Self {
            stage,
            path,
            admission: None,
        }
    }

    /// A failure after admission still owns that capability while displayed.
    pub fn admission(&self) -> Option<Arc<ModeLease>> {
        self.admission.clone()
    }
    /// The window's heading.
    pub fn title(&self) -> &'static str {
        match self.stage {
            StartupStage::Paths => "Couldn't find BroScribe's folders",
            StartupStage::Admission => "Couldn't use BroScribe's data folder",
            StartupStage::Instance => "Couldn't lock BroScribe's data folder",
            StartupStage::Config => "Couldn't read BroScribe's settings",
            StartupStage::Library => "Couldn't open BroScribe's library",
            StartupStage::Cli => "Couldn't start BroScribe's command-line tool",
        }
    }

    /// A support detail drawn from the launch stage, with no input values.
    pub fn detail(&self) -> &'static str {
        match self.stage {
            StartupStage::Paths => "A data or library folder couldn't be found.",
            StartupStage::Admission => {
                "Another BroScribe window or command may be using the data folder."
            }
            StartupStage::Instance => "BroScribe couldn't get exclusive access to the data folder.",
            StartupStage::Config => "The settings file may have a mistake or be unreadable.",
            StartupStage::Library => "The library folder may be unavailable or unwritable.",
            StartupStage::Cli => "The command-line tool is missing or couldn't run.",
        }
    }

    /// The location's purpose is safe to share even when its path is private.
    pub fn location_kind(&self) -> &'static str {
        match self.stage {
            StartupStage::Paths => "Data or library folder",
            StartupStage::Admission | StartupStage::Instance => "Data folder",
            StartupStage::Config => "Settings file",
            StartupStage::Library => "Library folder",
            StartupStage::Cli => "Command-line tool",
        }
    }

    /// A shareable report. The file path stays in the local window.
    pub(crate) fn support_report(&self) -> String {
        let mut text = format!(
            "{}.\n\nDetails: {}\n\n{}",
            self.title(),
            self.detail(),
            self.guidance()
        );
        if self.path.is_some() {
            text.push_str(&format!("\n\nLocation: {}", self.location_kind()));
        }
        if self.stage == StartupStage::Cli {
            text.push_str("\n\nCheck that the custom command-line tool is installed correctly.");
        }
        text
    }

    /// What the user can do about it. Nothing here was changed or replaced.
    pub fn guidance(&self) -> &'static str {
        match self.stage {
            StartupStage::Paths => {
                "Check any custom data or library folder you set, then choose Try again. Nothing was opened or changed."
            }
            StartupStage::Admission => {
                "Close other BroScribe windows or commands, then choose Try again. Nothing was changed."
            }
            StartupStage::Instance => {
                "Check that the data folder exists and that you can write to it, then choose Try again."
            }
            StartupStage::Config => {
                "Fix the settings file below or move it elsewhere, then choose Try again. BroScribe hasn't changed it."
            }
            StartupStage::Library => {
                "Check that the library folder below is available, for example that its drive is connected, and that you can write to it. Then choose Try again. Nothing in it was changed."
            }
            StartupStage::Cli => "Reinstall BroScribe, then open it again.",
        }
    }
}

impl std::fmt::Display for StartupFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}. {}", self.title(), self.detail())
    }
}

/// Admits startup before instance handoff, settings, library or CLI checks.
/// A maintenance result owns only control metadata and its process lease.
pub fn prepare(args: &Args, paths: &dyn AppPaths) -> std::result::Result<Prepared, StartupFailure> {
    prepare_with_cli(args, paths, jobs::locate_cli())
}

/// [`prepare`] with the CLI location already resolved (`located`).
fn prepare_with_cli(
    args: &Args,
    paths: &dyn AppPaths,
    located: std::io::Result<PathBuf>,
) -> std::result::Result<Prepared, StartupFailure> {
    let paths = resolve_paths_from(paths, |key| {
        if key == ENV_DATA_DIR
            && let Some(profile) = &args.dock_data_dir
        {
            return Some(profile.clone().into_os_string());
        }
        std::env::var_os(key)
    })
    .map_err(|_| StartupFailure::new(StartupStage::Paths, None))?;
    let admission = Arc::new(
        ModeLease::acquire(&paths.data_dir, ModeLease::DEFAULT_WAIT).map_err(|_| {
            StartupFailure::new(StartupStage::Admission, Some(paths.data_dir.clone()))
        })?,
    );
    if admission.mode() == Mode::Maintenance {
        return Ok(Prepared::Maintenance { paths, admission });
    }
    match claim_instance(&admission, args.handoff()) {
        Startup::Run(instance) => Ok(Prepared::Normal(NormalStartup {
            paths,
            admission,
            instance,
            located,
        })),
        Startup::Exit(code) => Ok(Prepared::Exit(code)),
        Startup::AlreadyRunning(place) => Ok(Prepared::AlreadyRunning(place)),
        Startup::Failed(mut failure) => {
            failure.admission = Some(admission);
            Err(failure)
        }
    }
}

/// How long a started agent may take to connect before the control says it
/// did not: longer than the client's longest reconnection delay (8 s).
pub const AGENT_CONNECT_WAIT: Duration = Duration::from_secs(20);

/// The "Start recording agent" control's state (UI11). Kept until the next
/// start or until the agent connects; meters and other events never clear it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStartStatus {
    Idle,
    Connecting,
    Stopped,
    /// `daemon start` runs; a second request is ignored.
    Starting,
    /// The agent was started and the app waits for its stream.
    WaitingForAgent,
    /// `daemon start` answered that no agent is installed.
    NotInstalled,
    NeedsApproval,
    Outdated {
        agent_protocol: Option<u32>,
    },
    DidNotConnect,
    /// Why it failed, as a sentence ([`CliError::user_text`]).
    Failed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentRecovery {
    Start,
    Setup,
    LoginItems,
    Reinstall,
    Retry,
}

impl AgentRecovery {
    pub(crate) fn label(self, mac_menu: bool) -> &'static str {
        match self {
            Self::Start => "Start BroScribe Agent",
            Self::Setup if mac_menu => "Set Up BroScribe Agent",
            Self::Setup => "Set up BroScribe Agent",
            Self::LoginItems => "Open Login Items",
            Self::Reinstall => "Reinstall",
            Self::Retry if mac_menu => "Try Again",
            Self::Retry => "Try again",
        }
    }
}

impl AgentStartStatus {
    pub(crate) fn unavailable(daemon: &DaemonClient, status: &Self) -> Option<Self> {
        if daemon.connected {
            return None;
        }
        Some(match status {
            Self::Starting | Self::WaitingForAgent | Self::Failed(_) => status.clone(),
            _ if daemon.incompatible_agent.is_some() => Self::Outdated {
                agent_protocol: daemon.incompatible_agent.flatten(),
            },
            Self::Idle if daemon.connecting => Self::Connecting,
            _ => status.clone(),
        })
    }

    pub(crate) fn recovery(&self) -> Option<AgentRecovery> {
        Some(match self {
            Self::Connecting | Self::Starting | Self::WaitingForAgent => return None,
            Self::Idle | Self::Stopped => AgentRecovery::Start,
            Self::NotInstalled => AgentRecovery::Setup,
            Self::NeedsApproval => AgentRecovery::LoginItems,
            Self::Outdated { .. } => AgentRecovery::Reinstall,
            Self::DidNotConnect | Self::Failed(_) => AgentRecovery::Retry,
        })
    }

    pub(crate) fn copy_details(&self) -> Option<String> {
        let app = env!("CARGO_PKG_VERSION");
        let protocol = broscribe_ipc::PROTOCOL_VERSION;
        let detail = match self {
            Self::Outdated { agent_protocol } => format!(
                "Agent protocol: {}",
                agent_protocol.map_or_else(|| "Unknown".into(), |version| version.to_string())
            ),
            Self::DidNotConnect => "Agent connection timed out after 20 seconds.".to_owned(),
            Self::Failed(_) => "Agent startup failed.".to_owned(),
            _ => return None,
        };
        Some(format!("BroScribe {app} (protocol {protocol})\n{detail}"))
    }

    /// The line shown beside the control, if any.
    pub fn message(&self) -> Option<String> {
        match self {
            Self::Idle => Some(format!("Can't reach {}.", crate::ui::terms::AGENT_NAME)),
            Self::Connecting => Some("Connecting to BroScribe Agent…".into()),
            Self::Stopped => Some("BroScribe Agent isn't running.".into()),
            Self::Starting => Some("Starting BroScribe Agent…".into()),
            Self::WaitingForAgent => {
                Some("BroScribe Agent started. Waiting for it to connect…".into())
            }
            Self::NotInstalled => Some("BroScribe Agent isn't installed.".into()),
            Self::NeedsApproval => Some("Waiting for your approval in Login Items".into()),
            Self::Outdated { .. } => {
                Some("BroScribe Agent is older than this app. Reinstall it to continue.".into())
            }
            Self::DidNotConnect => {
                Some("Couldn't start BroScribe Agent. It didn't respond within 20 seconds.".into())
            }
            Self::Failed(cause) => Some(crate::jobs::failure_text(
                "Couldn't start BroScribe Agent.",
                cause,
                "",
            )),
        }
    }

    /// A failure (`Alert`) rather than progress (`Status`).
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::DidNotConnect | Self::Failed(_))
    }

    /// Whether the control must ignore another click.
    pub fn is_pending(&self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Starting | Self::WaitingForAgent
        )
    }
}

/// The one "Start recording agent" request of the app, shared by every
/// surface that offers it (recording panel, menu-bar item): single-flight,
/// with its own pending / waiting / failed state.
pub struct AgentStarter {
    state: Entity<AppState>,
    daemon: Entity<DaemonClient>,
    status: AgentStartStatus,
    /// The `daemon start` child, then the wait for the agent to connect.
    run: Option<Task<()>>,
    autostart: Option<Arc<dyn Autostart>>,
    probe: Option<Task<()>>,
    _daemon: Subscription,
}

impl AgentStarter {
    pub fn new(
        state: Entity<AppState>,
        daemon: Entity<DaemonClient>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_autostart(state, daemon, None, cx)
    }

    pub fn with_autostart(
        state: Entity<AppState>,
        daemon: Entity<DaemonClient>,
        autostart: Option<Arc<dyn Autostart>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&daemon, |this, daemon, event: &DaemonEvent, cx| {
            if matches!(event, DaemonEvent::Received(_))
                && this.status != AgentStartStatus::Starting
                && this.status != AgentStartStatus::Idle
                && daemon.read(cx).connected
            {
                this.run = None;
                this.set_status(AgentStartStatus::Idle, cx);
            } else if matches!(event, DaemonEvent::Disconnected) {
                this.refresh_service(cx);
            }
        });
        Self {
            state,
            daemon,
            status: AgentStartStatus::Idle,
            run: None,
            autostart,
            probe: None,
            _daemon: subscription,
        }
    }

    pub fn status(&self) -> &AgentStartStatus {
        &self.status
    }

    /// Read the service after the stream resolves or the user returns from
    /// System Settings. Results of explicit operations keep their own state.
    pub fn refresh_service(&mut self, cx: &mut Context<Self>) {
        let Some(service) = self.autostart.clone() else {
            return;
        };
        if self.daemon.read(cx).connected
            || self.probe.is_some()
            || !matches!(
                self.status,
                AgentStartStatus::Idle
                    | AgentStartStatus::Stopped
                    | AgentStartStatus::NotInstalled
                    | AgentStartStatus::NeedsApproval
            )
        {
            return;
        }
        let read = cx
            .background_executor()
            .spawn(async move { service.agent_status().ok() });
        self.probe = Some(cx.spawn(async move |this, cx| {
            let read = read.await;
            this.update(cx, |this, cx| {
                this.probe = None;
                if this.daemon.read(cx).connected {
                    return;
                }
                let status = match read {
                    Some(AgentServiceStatus::NotInstalled) => AgentStartStatus::NotInstalled,
                    Some(AgentServiceStatus::Stopped) => AgentStartStatus::Stopped,
                    Some(AgentServiceStatus::NeedsApproval) => AgentStartStatus::NeedsApproval,
                    _ => AgentStartStatus::Idle,
                };
                this.set_status(status, cx);
            })
            .ok();
        }));
    }

    pub fn recover(&mut self, cx: &mut Context<Self>) {
        let Some(status) = AgentStartStatus::unavailable(self.daemon.read(cx), &self.status) else {
            return;
        };
        match status.recovery() {
            Some(AgentRecovery::Setup) => self.state.update(cx, |_, cx| {
                cx.emit(crate::state::AppEvent::OpenRecordingSetup)
            }),
            Some(AgentRecovery::LoginItems) => self
                .state
                .update(cx, |_, cx| cx.emit(crate::state::AppEvent::OpenLoginItems)),
            Some(AgentRecovery::Reinstall) => {
                self.start_work(true, cx);
            }
            Some(AgentRecovery::Start | AgentRecovery::Retry) => {
                self.start(cx);
            }
            None => {}
        }
    }

    pub fn copy_recovery_details(&self, cx: &mut App) {
        if let Some(details) = AgentStartStatus::unavailable(self.daemon.read(cx), &self.status)
            .and_then(|status| status.copy_details())
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(details));
        }
    }

    /// Runs `daemon start` unless a start is already running; `false` when
    /// this request was ignored.
    pub fn start(&mut self, cx: &mut Context<Self>) -> bool {
        self.start_work(false, cx)
    }

    fn start_work(&mut self, reinstall: bool, cx: &mut Context<Self>) -> bool {
        if self.status.is_pending() {
            return false;
        }
        self.probe = None;
        self.set_status(AgentStartStatus::Starting, cx);
        let state = self.state.clone();
        let service = self.autostart.clone();
        self.run = Some(cx.spawn(async move |this, cx| {
            let check = |cx: &gpui::AsyncApp| {
                let service = service.clone();
                cx.background_executor()
                    .spawn(async move { service.and_then(|service| service.agent_status().ok()) })
            };
            let before = check(cx).await;
            let status = if before == Some(AgentServiceStatus::NeedsApproval) {
                AgentStartStatus::NeedsApproval
            } else if before == Some(AgentServiceStatus::NotInstalled) && !reinstall {
                AgentStartStatus::NotInstalled
            } else {
                let result = async {
                    if reinstall {
                        // The install and start commands never stop the peer.
                        let child = cx.update(|cx| {
                            state.read(cx).run_cli(
                                vec!["daemon".into(), "install".into()],
                                ChildKind::Command,
                                cx,
                            )
                        });
                        if let Err(error) = child.await {
                            return AgentStartStatus::Failed(error.user_text());
                        }
                    }
                    let child = cx.update(|cx| {
                        state.read(cx).run_cli(
                            vec!["daemon".into(), "start".into()],
                            ChildKind::Command,
                            cx,
                        )
                    });
                    start_outcome(child.await)
                }
                .await;
                if check(cx).await == Some(AgentServiceStatus::NeedsApproval) {
                    AgentStartStatus::NeedsApproval
                } else {
                    result
                }
            };
            let waiting = this
                .update(cx, |this, cx| {
                    let status = if this.daemon.read(cx).connected {
                        AgentStartStatus::Idle
                    } else {
                        status
                    };
                    let waiting = status == AgentStartStatus::WaitingForAgent;
                    this.set_status(status, cx);
                    waiting
                })
                .unwrap_or(false);
            if !waiting {
                return;
            }
            cx.background_executor().timer(AGENT_CONNECT_WAIT).await;
            let after = check(cx).await;
            this.update(cx, |this, cx| {
                if this.status == AgentStartStatus::WaitingForAgent {
                    this.set_status(
                        if after == Some(AgentServiceStatus::NeedsApproval) {
                            AgentStartStatus::NeedsApproval
                        } else {
                            AgentStartStatus::DidNotConnect
                        },
                        cx,
                    );
                }
            })
            .ok();
        }));
        true
    }

    fn set_status(&mut self, status: AgentStartStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }
}

/// What a finished `daemon start` (`{"installed": bool, "running": bool}`)
/// means for the control.
fn start_outcome(result: std::result::Result<Value, CliError>) -> AgentStartStatus {
    match result {
        // `CliRunner` logged it with its code.
        Err(error) => AgentStartStatus::Failed(error.user_text()),
        Ok(reply) if reply["installed"].as_bool() == Some(false) => AgentStartStatus::NotInstalled,
        Ok(_) => AgentStartStatus::WaitingForAgent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_recovery_states_share_one_message_and_action_table() {
        use AgentRecovery::*;
        let cases = [
            (
                AgentStartStatus::Connecting,
                "Connecting to BroScribe Agent…",
                None,
                false,
            ),
            (
                AgentStartStatus::Idle,
                "Can't reach BroScribe Agent.",
                Some(Start),
                false,
            ),
            (
                AgentStartStatus::Stopped,
                "BroScribe Agent isn't running.",
                Some(Start),
                false,
            ),
            (
                AgentStartStatus::NotInstalled,
                "BroScribe Agent isn't installed.",
                Some(Setup),
                false,
            ),
            (
                AgentStartStatus::Starting,
                "Starting BroScribe Agent…",
                None,
                false,
            ),
            (
                AgentStartStatus::WaitingForAgent,
                "BroScribe Agent started. Waiting for it to connect…",
                None,
                false,
            ),
            (
                AgentStartStatus::NeedsApproval,
                "Waiting for your approval in Login Items",
                Some(LoginItems),
                false,
            ),
            (
                AgentStartStatus::Outdated {
                    agent_protocol: Some(6),
                },
                "BroScribe Agent is older than this app. Reinstall it to continue.",
                Some(Reinstall),
                true,
            ),
            (
                AgentStartStatus::DidNotConnect,
                "Couldn't start BroScribe Agent. It didn't respond within 20 seconds.",
                Some(Retry),
                true,
            ),
        ];
        for (state, message, recovery, details) in cases {
            assert_eq!(state.message().as_deref(), Some(message));
            assert_eq!(state.recovery(), recovery);
            assert_eq!(state.copy_details().is_some(), details);
        }
    }

    #[test]
    fn accessory_is_explicit_and_accepts_library() {
        assert!(!Args::try_parse_from(["broscribe-app"]).unwrap().accessory);
        let args = Args::try_parse_from([
            "broscribe-app",
            "--accessory",
            "--library",
            "/tmp/synthetic",
        ])
        .unwrap();
        assert!(args.accessory);
        assert!(!args.at_login);
        assert_eq!(args.library, Some(PathBuf::from("/tmp/synthetic")));
        assert!(
            Args::try_parse_from(["broscribe-app", "--accessory", "--at-login"])
                .unwrap()
                .at_login
        );
        assert!(Args::try_parse_from(["broscribe-app", "--at-login"]).is_err());
    }

    #[test]
    fn startup_copy_names_each_failure_its_cause_and_its_recovery() {
        for (stage, heading, cause, recovery) in [
            (
                StartupStage::Paths,
                "Couldn't find BroScribe's folders",
                "A data or library folder couldn't be found.",
                "Check any custom data or library folder you set, then choose Try again. Nothing was opened or changed.",
            ),
            (
                StartupStage::Admission,
                "Couldn't use BroScribe's data folder",
                "Another BroScribe window or command may be using the data folder.",
                "Close other BroScribe windows or commands, then choose Try again. Nothing was changed.",
            ),
            (
                StartupStage::Instance,
                "Couldn't lock BroScribe's data folder",
                "BroScribe couldn't get exclusive access to the data folder.",
                "Check that the data folder exists and that you can write to it, then choose Try again.",
            ),
            (
                StartupStage::Config,
                "Couldn't read BroScribe's settings",
                "The settings file may have a mistake or be unreadable.",
                "Fix the settings file below or move it elsewhere, then choose Try again. BroScribe hasn't changed it.",
            ),
            (
                StartupStage::Library,
                "Couldn't open BroScribe's library",
                "The library folder may be unavailable or unwritable.",
                "Check that the library folder below is available, for example that its drive is connected, and that you can write to it. Then choose Try again. Nothing in it was changed.",
            ),
            (
                StartupStage::Cli,
                "Couldn't start BroScribe's command-line tool",
                "The command-line tool is missing or couldn't run.",
                "Reinstall BroScribe, then open it again.",
            ),
        ] {
            let failure = StartupFailure::new(stage, Some("/synthetic/private".into()));
            assert_eq!(failure.title(), heading, "{stage:?}");
            assert_eq!(failure.detail(), cause, "{stage:?}");
            assert_eq!(failure.guidance(), recovery, "{stage:?}");
            assert_eq!(failure.to_string(), format!("{heading}. {cause}"));
            let report = failure.support_report();
            assert!(report.contains(recovery));
            assert_eq!(
                report.contains("custom command-line tool"),
                stage == StartupStage::Cli
            );
            assert!(!report.contains("/synthetic/private"));
        }
    }

    use broscribe_platform::fakes::FakeInstanceChannel;
    use broscribe_platform::shell::next_handoff_blocking;

    fn menu_bar() -> StatusIconPlace {
        StatusIconPlace::MenuBar
    }

    /// The running app's side: answers every later launch on its own
    /// thread, counting them, until its instance is dropped.
    fn answering(instance: &mut AppInstance) -> std::thread::JoinHandle<usize> {
        let mut requests = instance.take_handoffs().unwrap();
        std::thread::spawn(move || {
            let mut shown = 0;
            while let Some(handoff) = next_handoff_blocking(&mut requests) {
                assert_eq!(handoff.request, HandoffRequest::Show);
                shown += 1;
                handoff.done();
            }
            shown
        })
    }

    #[test]
    fn jump_list_launch_forwards_the_exact_command_to_the_running_app() {
        let dir = tempfile::tempdir().unwrap();
        let channel = FakeInstanceChannel::new();
        let Startup::Run(mut running) = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            None,
            &channel,
            menu_bar,
        ) else {
            panic!("the first launch runs");
        };
        let args = Args::try_parse_from(["broscribe-app", "--dock-action", "record-stop"]).unwrap();
        let mut requests = running.take_handoffs().unwrap();
        let app = std::thread::spawn(move || {
            let handoff = next_handoff_blocking(&mut requests).unwrap();
            let request = handoff.request;
            handoff.done();
            request
        });
        let second = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            args.handoff(),
            &channel,
            menu_bar,
        );
        assert!(matches!(second, Startup::Exit(code) if code == ExitCode::SUCCESS));
        assert_eq!(
            app.join().unwrap(),
            HandoffRequest::Dock(DockCommand::StopRecording)
        );
        assert!(Args::try_parse_from(["broscribe-app", "--dock-action", "0"]).is_err());
    }

    #[test]
    fn a_notification_shortcut_can_select_its_profile_without_a_dock_command() {
        let args = Args::try_parse_from(["broscribe-app", "--dock-data-dir", "/synthetic profile"])
            .unwrap();
        assert_eq!(
            args.dock_data_dir.as_deref(),
            Some(Path::new("/synthetic profile"))
        );
        assert_eq!(args.handoff(), Some(HandoffRequest::Show));
    }

    #[test]
    fn second_launch_hands_off_to_the_running_app_without_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let channel = FakeInstanceChannel::new();
        let Startup::Run(mut running) = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            Some(HandoffRequest::Show),
            &channel,
            menu_bar,
        ) else {
            panic!("the first launch runs");
        };
        assert!(channel.is_listening(), "the first launch listens");
        let app = answering(&mut running);

        // A second normal launch: the running app shows its window.
        let second = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            Some(HandoffRequest::Show),
            &channel,
            || panic!("no notice when the handoff works"),
        );
        assert!(matches!(second, Startup::Exit(code) if code == ExitCode::SUCCESS));
        assert_eq!(channel.sent(), [HandoffRequest::Show]);

        // An accessory launch (the agent's) exits silently, sending nothing.
        let accessory = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            None,
            &channel,
            menu_bar,
        );
        assert!(matches!(accessory, Startup::Exit(code) if code == ExitCode::SUCCESS));
        assert_eq!(channel.sent().len(), 1);

        // A handoff that fails says where the running app is.
        channel.fail_sends(true);
        let failed = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            Some(HandoffRequest::Show),
            &channel,
            || StatusIconPlace::NotificationArea,
        );
        assert!(matches!(
            failed,
            Startup::AlreadyRunning(StatusIconPlace::NotificationArea)
        ));
        channel.fail_sends(false);

        // Once the running app is gone, the next launch runs and listens.
        drop(running);
        assert_eq!(app.join().unwrap(), 1);
        assert!(!channel.is_listening());
        let next = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            Some(HandoffRequest::Show),
            &channel,
            menu_bar,
        );
        assert!(matches!(next, Startup::Run(_)));
        assert!(channel.is_listening());
    }

    #[test]
    fn a_running_app_that_cant_listen_still_runs_and_later_launches_get_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        // Another listener already holds the channel: this app runs anyway.
        let channel = FakeInstanceChannel::new();
        let (squatter, _requests) = handoff_channel();
        let _squatter = channel.listen(squatter).unwrap();
        let Startup::Run(running) = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            Some(HandoffRequest::Show),
            &channel,
            menu_bar,
        ) else {
            panic!("the first launch runs");
        };
        assert_eq!(
            format!("{running:?}"),
            "AppInstance { listening: false, .. }"
        );

        // Nobody answers on the channel (the running app couldn't listen):
        // the second launch shows the notice. A running app whose loop never
        // handles the request fails the same way (`NativeInstanceChannel`'s
        // `an_unhandled_request_fails_the_handoff`).
        let second = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            Some(HandoffRequest::Show),
            &FakeInstanceChannel::new(),
            || StatusIconPlace::NoTrayHost,
        );
        assert!(matches!(
            second,
            Startup::AlreadyRunning(StatusIconPlace::NoTrayHost)
        ));
        drop(running);
    }

    /// SHL-14: a second launch's `show` reaches the running app's UI loop,
    /// which brings the main window forward (`Shell::show(None)` there; the
    /// main window here, an accessory launch's first one, so it gets its
    /// Dock icon), and only then is the second launch told it worked. The
    /// recording agent plays no part.
    #[gpui::test]
    fn a_second_launch_brings_the_running_apps_window_forward(cx: &mut gpui::TestAppContext) {
        use std::sync::Arc;

        use broscribe_platform::ActivationPolicy::Regular;
        use broscribe_platform::fakes::{
            ActivationCall, FakeActivation, FakeMicCapture, FakePaths, FakeSystemInfo,
        };
        use gpui::AppContext as _;

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config
            .setup
            .completed_steps
            .push(crate::views::setup::ASSISTANT_DONE.into());
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
        cx.update(gpui::init);
        let deps = crate::views::setup::SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let activation = std::rc::Rc::new(FakeActivation::new());
        cx.update(|cx| crate::ui::windows::install_presence(true, false, activation.clone(), cx));
        activation.clear();

        // The running app: an accessory launch, without a window yet.
        let channel = FakeInstanceChannel::new();
        let Startup::Run(mut running) = single_instance(
            AppInstanceLock::try_acquire(dir.path()),
            None,
            &channel,
            menu_bar,
        ) else {
            panic!("the first launch runs");
        };
        let main = std::rc::Rc::new(std::cell::RefCell::new(None));
        cx.update(|cx| {
            let main = main.clone();
            serve_handoffs(
                running.take_handoffs().unwrap(),
                move |request, cx| {
                    assert_eq!(request, HandoffRequest::Show);
                    crate::ui::show_main_window(&mut main.borrow_mut(), &state, &deps, cx);
                },
                cx,
            )
            .detach();
        });
        assert!(cx.windows().is_empty());

        // A second launch's request arrives (on the real channel it waits
        // for the answer on its own process's thread).
        let answered = channel.deliver(HandoffRequest::Show).unwrap();
        assert!(answered.try_recv().is_err(), "not answered before it ran");
        cx.run_until_parked();
        answered
            .try_recv()
            .expect("answered once the window showed");
        assert_eq!(cx.windows().len(), 1, "the main window opened");
        assert!(main.borrow().is_some());
        assert_eq!(
            activation.calls(),
            [ActivationCall::Policy(Regular), ActivationCall::Activate]
        );
        drop(running);
    }

    #[test]
    fn unusable_lock_is_reported_with_its_data_folder() {
        let channel = FakeInstanceChannel::new();
        let failed = single_instance(
            Err(broscribe_core::Error::Internal("synthetic".into())),
            Some(HandoffRequest::Show),
            &channel,
            menu_bar,
        );
        assert!(
            matches!(&failed, Startup::Failed(f) if f.stage == StartupStage::Instance),
            "{failed:?}"
        );
        assert!(channel.sent().is_empty());
        assert!(!channel.is_listening());

        // Admission succeeds, but the subordinate instance folder is unusable.
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let admission = ModeLease::acquire(&data_dir, Duration::ZERO).unwrap();
        std::fs::write(data_dir.join("run"), b"synthetic").unwrap();
        match claim_instance(&admission, Some(HandoffRequest::Show)) {
            Startup::Failed(failure) => {
                assert_eq!(failure.stage, StartupStage::Instance);
                assert_eq!(failure.path.as_deref(), Some(data_dir.as_path()));
                assert!(
                    !failure
                        .to_string()
                        .contains(&data_dir.display().to_string())
                );
            }
            other => panic!("expected a reported failure, got {other:?}"),
        }
    }

    fn args_with_library(library: Option<PathBuf>) -> Args {
        Args {
            library,
            accessory: false,
            at_login: false,
            dock_action: None,
            dock_data_dir: None,
        }
    }

    fn normal_state(
        prepared: std::result::Result<Prepared, StartupFailure>,
        args: &Args,
    ) -> std::result::Result<(AppStateInit, AppInstance), StartupFailure> {
        match prepared? {
            Prepared::Normal(normal) => normal.initialize(args),
            _ => panic!("expected normal startup"),
        }
    }

    #[test]
    fn maintenance_startup_never_opens_settings_library_or_checks_cli() {
        use broscribe_core::maintenance::{Mode, ModeChange};
        let dir = tempfile::tempdir().unwrap();
        let paths = broscribe_platform::fakes::FakePaths::new(dir.path());
        let resolved = resolve_paths(&paths).unwrap();
        std::fs::create_dir_all(&resolved.data_dir).unwrap();
        let config = Config::path(&resolved.data_dir);
        let corrupt = b"library = [unterminated\n";
        std::fs::write(&config, corrupt).unwrap();
        let library = dir.path().join("never-created");
        ModeChange::acquire(&resolved.data_dir)
            .unwrap()
            .set(Mode::Maintenance)
            .unwrap();

        let prepared = prepare_with_cli(
            &args_with_library(Some(library.clone())),
            &paths,
            Err(std::io::ErrorKind::NotFound.into()),
        )
        .expect("maintenance does not require valid settings or a CLI");
        assert!(matches!(&prepared, Prepared::Maintenance { .. }));
        assert_eq!(std::fs::read(&config).unwrap(), corrupt);
        assert!(!library.exists());
        assert!(
            !resolved.data_dir.join("run").exists(),
            "no instance handoff"
        );
        assert!(ModeChange::acquire(&resolved.data_dir).is_err());
        drop(prepared);
        assert!(
            ModeChange::acquire(&resolved.data_dir).is_ok(),
            "dropping the maintenance launch releases its admission"
        );
    }

    #[test]
    fn invalid_settings_stop_the_launch_without_touching_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = broscribe_platform::fakes::FakePaths::new(dir.path());
        let data_dir = resolve_paths(&paths).unwrap().data_dir;
        std::fs::create_dir_all(&data_dir).unwrap();
        let config = Config::path(&data_dir);
        std::fs::write(&config, "library = [unterminated\n").unwrap();
        let args = args_with_library(None);
        let failure = match normal_state(prepare(&args, &paths), &args) {
            Err(failure) => failure,
            Ok(_) => panic!("an unreadable config must stop the launch"),
        };
        assert_eq!(failure.stage, StartupStage::Config);
        assert_eq!(failure.path.as_deref(), Some(config.as_path()));
        assert!(!failure.detail().is_empty());
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "library = [unterminated\n",
            "the settings file is left as it was"
        );
        assert!(
            broscribe_core::maintenance::ModeChange::acquire(&data_dir).is_err(),
            "the startup failure screen is still an admitted client"
        );
        drop(failure);
        assert!(
            broscribe_core::maintenance::ModeChange::acquire(&data_dir).is_ok(),
            "dropping the startup failure releases its admission"
        );
    }

    #[test]
    fn unusable_library_names_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let paths = broscribe_platform::fakes::FakePaths::new(dir.path());
        let library = dir.path().join("library-is-a-file");
        std::fs::write(&library, b"synthetic").unwrap();
        let args = args_with_library(Some(library.clone()));
        let failure = match normal_state(prepare(&args, &paths), &args) {
            Err(failure) => failure,
            Ok(_) => panic!("a library that cannot be opened must stop the launch"),
        };
        assert_eq!(failure.stage, StartupStage::Library);
        assert_eq!(failure.path.as_deref(), Some(library.as_path()));
    }

    #[test]
    fn missing_or_unusable_cli_stops_the_launch_with_its_path() {
        use crate::test_support::write_cli;
        let dir = tempfile::tempdir().unwrap();
        let paths = broscribe_platform::fakes::FakePaths::new(dir.path());
        let args = args_with_library(Some(dir.path().join("lib")));
        // An installation folder with the app but no CLI beside it.
        let install = dir.path().join("install");
        std::fs::create_dir_all(&install).unwrap();
        let app = install.join("broscribe-app");
        std::fs::write(&app, b"synthetic").unwrap();
        let sibling = jobs::cli_path_from(&app, None);
        // An override naming a folder.
        let folder = jobs::cli_path_from(&app, Some(install.clone().into_os_string()));
        let unusable = vec![sibling, folder];
        // A file nobody may run: only Unix has an execute bit to lack.
        #[cfg(unix)]
        let unusable = {
            let mut unusable = unusable;
            use std::os::unix::fs::PermissionsExt as _;
            let plain = install.join("plain-file");
            std::fs::write(&plain, b"synthetic").unwrap();
            std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o600)).unwrap();
            unusable.push(plain);
            unusable
        };
        for cli in unusable {
            let failure =
                match normal_state(prepare_with_cli(&args, &paths, Ok(cli.clone())), &args) {
                    Err(failure) => failure,
                    Ok(_) => panic!("an unusable CLI must stop the launch: {}", cli.display()),
                };
            assert_eq!(failure.stage, StartupStage::Cli);
            assert_eq!(failure.path.as_deref(), Some(cli.as_path()));
        }
        let unlocated = normal_state(
            prepare_with_cli(&args, &paths, Err(std::io::ErrorKind::NotFound.into())),
            &args,
        );
        assert!(matches!(unlocated, Err(f) if f.stage == StartupStage::Cli && f.path.is_none()));

        let runnable = write_cli(&install.join("broscribe"), "", "");
        let Prepared::Normal(normal) = prepare_with_cli(&args, &paths, Ok(runnable.clone()))
            .expect("a runnable CLI admits normal startup")
        else {
            panic!("expected normal startup")
        };
        let (init, _instance) = normal
            .initialize(&args)
            .expect("a runnable CLI starts the app");
        assert_eq!(init.cli.cli(), runnable);
    }

    mod agent_start {
        use crate::test_support::{ps_path, ps_str, write_cli};
        use std::sync::Arc;

        use broscribe_ipc::Event;
        use broscribe_platform::fakes::FakePaths;
        use gpui::{AppContext as _, TestAppContext};

        use super::*;
        use crate::daemon::{EventStream, Transport};
        use broscribe_ipc::Call;

        struct NoAgent;
        impl Transport for NoAgent {
            fn subscribe(
                &self,
            ) -> std::result::Result<Box<dyn EventStream>, broscribe_ipc::IpcError> {
                Err(broscribe_ipc::IpcError::new(
                    crate::daemon::AGENT_UNREACHABLE,
                    "BroScribe Agent isn't running",
                ))
            }
            fn call(&self, _: Call) -> std::result::Result<Value, broscribe_ipc::IpcError> {
                Err(broscribe_ipc::IpcError::new(
                    crate::daemon::AGENT_UNREACHABLE,
                    "BroScribe Agent isn't running",
                ))
            }
        }

        /// A CLI whose `daemon start` logs its arguments and answers with
        /// `reply` (or fails when `reply` is empty).
        fn make_starter(
            cx: &mut TestAppContext,
            dir: &Path,
            reply: &str,
        ) -> (Entity<AgentStarter>, Entity<DaemonClient>) {
            let log = dir.join("args.log");
            let (answer, answer_ps) = if reply.is_empty() {
                (
                    "echo 'synthetic start failure' >&2; exit 3".to_owned(),
                    "fx_eecho 'synthetic start failure';exit 3".to_owned(),
                )
            } else {
                (
                    format!("echo '{reply}'"),
                    format!("fx_echo {}", ps_str(reply)),
                )
            };
            let cli = write_cli(
                &dir.join("synthetic-cli"),
                &format!("printf '%s\\n' \"$*\" >> '{}'\n{answer}\n", log.display()),
                &format!("fx_log_args {};{answer_ps}", ps_path(&log)),
            );
            let state = cx.new(|cx| {
                AppState::new(
                    crate::test_support::app_init(
                        resolve_paths(&FakePaths::new(dir)).unwrap(),
                        Config::default(),
                        Library::open(dir.join("lib")).unwrap(),
                        cli,
                    ),
                    cx,
                )
            });
            let daemon = cx.update(|cx| DaemonClient::detached(Arc::new(NoAgent), cx));
            let starter = cx.new(|cx| AgentStarter::new(state, daemon.clone(), cx));
            (starter, daemon)
        }

        fn starts(dir: &Path) -> usize {
            std::fs::read_to_string(dir.join("args.log"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.ends_with("daemon start"))
                .count()
        }

        #[gpui::test]
        fn a_second_click_while_starting_runs_nothing(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let (starter, daemon) =
                make_starter(cx, dir.path(), r#"{"installed":true,"running":false}"#);
            starter.update(cx, |starter, cx| {
                assert!(starter.start(cx));
                assert_eq!(starter.status(), &AgentStartStatus::Starting);
                assert!(!starter.start(cx), "single-flight");
            });
            cx.run_until_parked();
            assert_eq!(starts(dir.path()), 1);
            starter.read_with(cx, |starter, _| {
                assert_eq!(starter.status(), &AgentStartStatus::WaitingForAgent);
                assert!(!starter.status().is_failure());
            });
            // The agent's stream arriving means it is connected.
            daemon.update(cx, |daemon, cx| daemon.received(Event::ShowMainWindow, cx));
            cx.run_until_parked();
            starter.read_with(cx, |starter, _| {
                assert_eq!(starter.status(), &AgentStartStatus::Idle)
            });
            // A waiting start that was answered never fails later.
            cx.executor().advance_clock(AGENT_CONNECT_WAIT);
            cx.run_until_parked();
            starter.read_with(cx, |starter, _| {
                assert_eq!(starter.status(), &AgentStartStatus::Idle)
            });
        }

        #[gpui::test]
        fn an_agent_that_never_connects_is_reported(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let (starter, _daemon) =
                make_starter(cx, dir.path(), r#"{"installed":true,"running":false}"#);
            starter.update(cx, |starter, cx| starter.start(cx));
            cx.run_until_parked();
            cx.executor().advance_clock(AGENT_CONNECT_WAIT);
            cx.run_until_parked();
            starter.read_with(cx, |starter, _| {
                assert!(
                    matches!(starter.status(), AgentStartStatus::DidNotConnect),
                    "{:?}",
                    starter.status()
                );
                assert!(starter.status().is_failure());
            });
            // The failure stays until the user tries again.
            starter.update(cx, |starter, cx| assert!(starter.start(cx)));
            cx.run_until_parked();
            assert_eq!(starts(dir.path()), 2);
        }

        #[gpui::test]
        fn failures_and_a_missing_agent_have_distinct_messages(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let (starter, _daemon) = make_starter(cx, dir.path(), "");
            starter.update(cx, |starter, cx| starter.start(cx));
            cx.run_until_parked();
            starter.read_with(cx, |starter, _| {
                assert!(matches!(starter.status(), AgentStartStatus::Failed(_)));
                assert!(
                    starter
                        .status()
                        .message()
                        .unwrap()
                        .starts_with("Couldn't start BroScribe Agent")
                );
            });

            let dir = tempfile::tempdir().unwrap();
            let (starter, _daemon) =
                make_starter(cx, dir.path(), r#"{"installed":false,"running":false}"#);
            starter.update(cx, |starter, cx| starter.start(cx));
            cx.run_until_parked();
            starter.read_with(cx, |starter, _| {
                assert_eq!(starter.status(), &AgentStartStatus::NotInstalled);
                assert!(!starter.status().is_failure());
            });
        }
    }
}
