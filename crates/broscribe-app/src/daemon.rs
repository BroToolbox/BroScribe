//! Background IPC transport. GPUI remains the sole UI-loop owner.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use broscribe_core::maintenance::{Mode, ModeLease};
use broscribe_ipc::live::LiveSnapshot;
use broscribe_ipc::{AgentPing, AgentReadiness};
use broscribe_ipc::{Call, Event, IpcError, RecordingStatus};
use broscribe_platform_native::ipc::{CallError, Client, DEFAULT_TIMEOUT, Endpoint, Subscription};
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Task, WeakEntity,
    Window, div,
};
use serde_json::Value;

use crate::readers::AppReaders;
use crate::views::panel::PanelModel;

pub trait EventStream: Send {
    /// The next event; `Ok(None)` once the stream ended. A broken stream is
    /// [`AGENT_UNREACHABLE`].
    fn next_event(&mut self) -> Result<Option<Event>, IpcError>;
}
impl EventStream for Subscription {
    fn next_event(&mut self) -> Result<Option<Event>, IpcError> {
        Subscription::next_event(self).map_err(|e| IpcError::new(AGENT_UNREACHABLE, e.to_string()))
    }
}

pub trait Transport: Send + Sync {
    /// Opens the agent's event stream; why not as `{code, message}`
    /// ([`AGENT_UNREACHABLE`] when no agent answers).
    fn subscribe(&self) -> Result<Box<dyn EventStream>, IpcError>;
    /// The agent's answer, or its `{code, message}`: a refusal of the agent,
    /// or [`AGENT_UNREACHABLE`] / [`AGENT_UNCERTAIN`] when the request did
    /// not get an answer. Callers test the code, never the text.
    fn call(&self, call: Call) -> Result<Value, IpcError>;
}

/// The code of a request that never reached the agent (none runs, the
/// connection was refused or closed, or it is not in normal operation).
pub const AGENT_UNREACHABLE: &str = "daemon_unavailable";
/// The code of a request that may or may not have run: the connection broke
/// after it was sent and the agent could not tell its outcome.
pub const AGENT_UNCERTAIN: &str = "uncertain";
/// The code of a start whose capture stopped before it was committed; its
/// message ends in the capture's stop code.
pub const CAPTURE_FAILED: &str = "capture_failed";
/// The code of an agent that answered but is not in normal operation
/// (maintenance mode, or not ready yet).
pub const AGENT_NOT_OPERATIONAL: &str = "agent_not_operational";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StartRefusal {
    pub words: String,
    pub recovery: Option<broscribe_ipc::recording_words::StartRecovery>,
    pub code: String,
}

pub(crate) fn start_refusal(error: &IpcError, setup_capturing: bool) -> StartRefusal {
    let (cause, recovery) = broscribe_ipc::recording_words::start_refusal(error, setup_capturing)
        .unwrap_or_else(|| (agent_error_text(error), None));
    StartRefusal {
        words: crate::jobs::failure_text("Couldn't start recording.", &cause, ""),
        recovery,
        code: error.code.clone(),
    }
}

/// A failed agent call as `{code, message}`; its text stays the message the
/// log shows.
pub(crate) fn agent_call_error(error: CallError) -> IpcError {
    match error {
        CallError::Remote(error) => error,
        CallError::NotSent(ref io) => {
            if let Some(mismatch) = io
                .get_ref()
                .and_then(|cause| cause.downcast_ref::<IpcError>())
                .filter(|error| error.code == IpcError::UNSUPPORTED_VERSION)
            {
                mismatch.clone()
            } else {
                IpcError::new(AGENT_UNREACHABLE, error.to_string())
            }
        }
        CallError::Uncertain { .. } => IpcError::new(AGENT_UNCERTAIN, error.to_string()),
    }
}

/// What a failed agent request tells the user, never its code (design §4.2
/// Plain failures; the log gets the code). A request that never reached the
/// agent, one whose outcome is unknown and the agent's protocol refusals
/// (shutting down, the same request still running, another version, a
/// request it couldn't read) are said in words: their messages are the
/// system's or the protocol's ("Daemon not reached: Connection refused (os
/// error 61)"). The agent builds its other refusals from library errors
/// (`IpcError::from_core`), so `io`, `schema`, `decode`, `internal`,
/// `lease_lost` and `inference` get the CLI's fixed sentences
/// ([`crate::jobs::answered_cause`]: their messages name files and hold the
/// system's words); any other refusal is the agent's words without a path
/// ([`crate::jobs::refusal_text`]).
pub fn agent_error_text(error: &IpcError) -> String {
    match error.code.as_str() {
        AGENT_UNREACHABLE => "BroScribe Agent isn't running.".to_owned(),
        AGENT_UNCERTAIN => {
            "BroScribe Agent didn't confirm whether it happened. Check before trying again."
                .to_owned()
        }
        AGENT_NOT_OPERATIONAL => "BroScribe Agent isn't ready to record yet.".to_owned(),
        IpcError::SHUTTING_DOWN => "BroScribe Agent is shutting down.".to_owned(),
        IpcError::IN_PROGRESS => "BroScribe Agent is still doing this.".to_owned(),
        IpcError::UNSUPPORTED_VERSION => {
            "BroScribe Agent is a different version. Restart BroScribe.".to_owned()
        }
        IpcError::PROTOCOL | IpcError::INVALID_REQUEST => {
            "BroScribe Agent didn't understand the request.".to_owned()
        }
        // "recording did not start: <the capture's stop code>": the code
        // stays in the log; why in words is WRT-101's start refusal (U31).
        CAPTURE_FAILED => "The recording didn't start. Try again.".to_owned(),
        code => crate::jobs::answered_cause(code)
            .map_or_else(|| crate::jobs::refusal_text(&error.message), str::to_owned),
    }
}

/// [`agent_error_text`] of a connection to the agent, for the request
/// `method`, that failed before the request was sent (the socket couldn't
/// be opened): logged once, by method and the error's kind only.
pub(crate) fn connect_error_text(method: &str, error: &std::io::Error) -> String {
    tracing::warn!(method, kind = ?error.kind(), "connecting to the agent failed");
    agent_error_text(&IpcError::new(AGENT_UNREACHABLE, error.to_string()))
}

/// [`agent_error_text`] of a failed call made without [`Transport`] (the
/// setup assistant's helper calls): logged once, by method and code, as
/// [`DaemonClient`] logs its requests.
pub(crate) fn call_error_text(method: &str, error: CallError) -> String {
    let error = agent_call_error(error);
    agent_failure_text(method, &error)
}

/// [`agent_error_text`] of the request `method` that failed with `error`,
/// logged once by method and code: never the agent's words, which may echo
/// what the user typed or name a file.
pub(crate) fn agent_failure_text(method: &str, error: &IpcError) -> String {
    tracing::warn!(method, code = %error.code, "agent request failed");
    agent_error_text(error)
}

pub struct SocketTransport {
    socket: Endpoint,
}
impl SocketTransport {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            socket: Endpoint::new(&data_dir),
        }
    }
}
impl Transport for SocketTransport {
    fn subscribe(&self) -> Result<Box<dyn EventStream>, IpcError> {
        let mut client = Client::connect_daemon(&self.socket, DEFAULT_TIMEOUT)
            .map_err(|e| IpcError::new(AGENT_UNREACHABLE, e.to_string()))?
            .ok_or_else(|| IpcError::new(AGENT_UNREACHABLE, "BroScribe Agent isn't running"))?;
        require_operational_agent(&mut client)?;
        Ok(Box::new(client.subscribe().map_err(agent_call_error)?))
    }
    fn call(&self, call: Call) -> Result<Value, IpcError> {
        // The shared client reconciles uncertain mutations by request ID. Never
        // execute locally after a transport failure, including NotSent.
        if call.is_mutating() {
            Client::call_mutating(&self.socket, call, DEFAULT_TIMEOUT).map_err(agent_call_error)
        } else {
            Client::connect_daemon(&self.socket, DEFAULT_TIMEOUT)
                .map_err(|e| IpcError::new(AGENT_UNREACHABLE, e.to_string()))?
                .ok_or_else(|| IpcError::new(AGENT_UNREACHABLE, "BroScribe Agent isn't running"))?
                .call(call)
                .map_err(agent_call_error)
        }
    }
}

/// Only a current, explicitly normal-operational agent authorizes normal UI.
pub(crate) fn require_operational_agent(client: &mut Client) -> Result<(), IpcError> {
    let value = client.call(Call::Ping).map_err(agent_call_error)?;
    if let Some(protocol) = value["protocol"].as_u64()
        && protocol != u64::from(broscribe_ipc::PROTOCOL_VERSION)
    {
        return Err(IpcError::new(
            IpcError::UNSUPPORTED_VERSION,
            format!("Agent protocol {protocol}"),
        ));
    }
    let ping = AgentPing::from_value(value)?;
    if ping.mode != Mode::Normal || ping.readiness != AgentReadiness::NormalOperational {
        return Err(IpcError::new(
            AGENT_NOT_OPERATIONAL,
            "BroScribe Agent isn't ready to record yet.",
        ));
    }
    Ok(())
}

fn peer_protocol(message: &str) -> Option<u32> {
    let from_message = message.strip_prefix("Agent protocol ").or_else(|| {
        message
            .strip_prefix("protocol version ")
            .and_then(|rest| rest.split_once(' ').map(|(version, _)| version))
    });
    let from_refusal = message
        .split_once("this side speaks ")
        .and_then(|(_, version)| version.strip_suffix(')'));
    [from_message, from_refusal]
        .into_iter()
        .flatten()
        .filter_map(|version| version.parse().ok())
        .find(|version| *version != broscribe_ipc::PROTOCOL_VERSION)
}

/// Owns the stream across background reads, and reconnects after EOF/errors.
#[derive(Default)]
pub struct Connection {
    stream: Option<Box<dyn EventStream>>,
}
impl Connection {
    fn read(&mut self, transport: &dyn Transport) -> Result<Option<Event>, IpcError> {
        if self.stream.is_none() {
            self.stream = Some(transport.subscribe()?);
        }
        let result = self.stream.as_mut().expect("connected stream").next_event();
        if !matches!(result, Ok(Some(_))) || matches!(result, Ok(Some(Event::Lagged { .. }))) {
            self.stream = None;
        }
        result
    }
}

#[derive(Clone, Debug)]
pub enum DaemonEvent {
    Received(Event),
    Disconnected,
}

/// The recording's Me and Others levels (whole dBFS, at most 10 changes a
/// second), in their own entity: a meter change notifies only views that
/// render this entity, never [`DaemonClient`] and its observers (the panel's
/// bookkeeping, its transcript, menus and other windows).
pub struct LiveMeters {
    levels: [i16; 2],
    states: [crate::ui::kit::MeterState; 2],
}
impl LiveMeters {
    /// Me, then Others.
    pub fn levels(&self) -> [i16; 2] {
        self.levels
    }
}
impl Render for LiveMeters {
    /// The two tracks as separate labelled meters (never one combined meter),
    /// each named by its level in words.
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::ui::kit::{Track, level_meter};
        div()
            .id("audio-levels")
            .role(gpui::Role::Group)
            .aria_label("Audio levels")
            .flex()
            .flex_col()
            .gap_1()
            .child(level_meter(
                "meter-me",
                Track::Me,
                f32::from(self.levels[0]),
                self.states[0],
                cx,
            ))
            .child(level_meter(
                "meter-others",
                Track::Others,
                f32::from(self.levels[1]),
                self.states[1],
                cx,
            ))
    }
}

/// What an agent-level failure is about. Commands are told apart by method
/// and by the recording, capture or notification they act on, never by
/// their payload (titles, notes, names).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Origin {
    /// Reading or reopening the event stream.
    Connection,
    Command {
        method: &'static str,
        target: Option<String>,
    },
}
impl Origin {
    fn of(call: &Call) -> Self {
        let target = match call {
            Call::RecordTitle { session_id, .. }
            | Call::RecordTitleRefresh { session_id, .. }
            | Call::RecordDiscard { session_id, .. }
            | Call::RecordRetry { session_id, .. }
            | Call::RecordResolveJournal { session_id, .. }
            | Call::RecordParticipant { session_id, .. }
            | Call::RecordLive { session_id, .. }
            | Call::ReaderEvents { session_id, .. }
            | Call::ReaderAttach { session_id, .. }
            | Call::ReaderRefused { session_id, .. } => Some(session_id.as_str()),
            Call::RecordLiveAttribute(params) => Some(params.session_id.as_str()),
            Call::RecordAutoStop(params) => Some(params.session_id.as_str()),
            Call::CaptureSample { session, .. } | Call::CaptureSampleStop { session, .. } => {
                Some(session.as_str())
            }
            Call::NotificationAction {
                notification_id, ..
            } => Some(notification_id.as_str()),
            other => other.recording_session(),
        };
        Self::Command {
            method: call.method(),
            target: target.map(str::to_owned),
        }
    }
}

/// One request or connection failure, numbered in the order they happened.
#[derive(Debug)]
struct Ticket {
    origin: Origin,
    number: u64,
}

/// Issues [`Ticket`]s. A request takes its own when it is made, through
/// `&self`, since every [`DaemonClient::request`] caller holds a read.
#[derive(Default)]
struct Tickets {
    issued: Cell<u64>,
    /// The newest request of each command still out: only its answer counts.
    latest: RefCell<HashMap<Origin, u64>>,
}
impl Tickets {
    fn issue(&self, origin: Origin) -> Ticket {
        let number = self.issued.get() + 1;
        self.issued.set(number);
        if origin != Origin::Connection {
            self.latest.borrow_mut().insert(origin.clone(), number);
        }
        Ticket { origin, number }
    }

    /// Whether `ticket` is still its command's newest request; once
    /// answered it no longer is.
    fn retire(&self, ticket: &Ticket) -> bool {
        let mut latest = self.latest.borrow_mut();
        if latest.get(&ticket.origin) == Some(&ticket.number) {
            latest.remove(&ticket.origin);
            true
        } else {
            false
        }
    }
}

/// What [`DaemonClient::error`] is about.
#[derive(Debug)]
struct Shown {
    origin: Origin,
    number: u64,
    text: String,
}

/// Every failure of the request becomes the agent-level error.
fn every_failure(_: &IpcError) -> bool {
    true
}

/// The caller shows the request's failures itself (beside its control).
fn no_failure(_: &IpcError) -> bool {
    false
}

/// Where the connected recording agent runs (its retention passes and
/// workers), compared with the library this app opened (SET-01).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AgentLibrary {
    /// No agent state since the stream (re)opened: none is connected, or
    /// its first state hasn't arrived.
    Unknown = 0,
    /// The agent's newest state names the open library.
    Open = 1,
    /// The agent's newest state names another library, or a folder that
    /// can't be compared (see `state::agent_runs_elsewhere`), or the app
    /// hasn't said which library it opened.
    Elsewhere = 2,
}

/// The [`AgentLibrary`] the daemon client published last, as one atomic
/// value: reading it takes no lock, so a retention save reads it inside
/// `Config::update`, whose config lock must stay the last lock taken
/// (SET-01, design §14.1 CL08). The client compares the library each state
/// event reports with the open one when the event arrives, on the UI
/// thread, and publishes the result; it publishes
/// [`AgentLibrary::Unknown`] when the stream ends. Clones read it from any
/// thread.
#[derive(Clone, Debug, Default)]
pub struct AgentLibraryFlag(Arc<AtomicU8>);

impl AgentLibraryFlag {
    /// What the client published last.
    pub fn get(&self) -> AgentLibrary {
        match self.0.load(AtomicOrdering::SeqCst) {
            1 => AgentLibrary::Open,
            2 => AgentLibrary::Elsewhere,
            _ => AgentLibrary::Unknown,
        }
    }

    /// Whether the agent connected now runs on another library than the
    /// open one; `false` with no agent state, since the next agent
    /// normally opens the folder the settings name (a launch override can
    /// make it open another one).
    pub fn elsewhere(&self) -> bool {
        self.get() == AgentLibrary::Elsewhere
    }

    fn set(&self, value: AgentLibrary) {
        self.0.store(value as u8, AtomicOrdering::SeqCst);
    }
}

pub struct DaemonClient {
    pub connected: bool,
    pub(crate) connecting: bool,
    pub(crate) incompatible_agent: Option<Option<u32>>,
    pub(crate) menu_start_refusal: Option<StartRefusal>,
    pub panel: PanelModel,
    install: Option<broscribe_ipc::InstallHold>,
    /// The library the newest state reported, as reported; `None` before
    /// the first state and once the stream ends.
    reported_library: Option<PathBuf>,
    /// The library this app opened, absolute
    /// ([`DaemonClient::compare_agent_library_with`]).
    open_library: Option<PathBuf>,
    /// [`Self::reported_library`] compared with [`Self::open_library`],
    /// published again whenever either changes.
    agent_library: AgentLibraryFlag,
    /// The agent-level failure shown: the event stream could not be read, or
    /// a command sent with [`DaemonClient::send`] or
    /// [`DaemonClient::request_reporting`] failed. A later success of that
    /// same command (method and the recording or notification it acts on),
    /// through any of this client's requests, clears it; another command's
    /// success, meters, preview lines and state snapshots never do. A newer
    /// failure replaces it, an answer to an older request never does, and a
    /// reconnection clears it.
    pub error: Option<String>,
    /// The origin of [`Self::error`] while it holds what this client set.
    shown: Option<Shown>,
    tickets: Tickets,
    meters: Entity<LiveMeters>,
    this: WeakEntity<Self>,
    transport: Arc<dyn Transport>,
    readers: Option<Entity<AppReaders>>,
    admission: Option<Arc<ModeLease>>,
    _pump: Task<()>,
    meter_flush: Option<Task<()>>,
}
impl EventEmitter<DaemonEvent> for DaemonClient {}
impl DaemonClient {
    pub fn attach(data_dir: PathBuf, admission: Arc<ModeLease>, cx: &mut App) -> Entity<Self> {
        let readers = AppReaders::attach(data_dir.clone(), admission.clone(), cx);
        let transport: Arc<dyn Transport> = Arc::new(SocketTransport::new(data_dir));
        let reader_admission = admission.clone();
        cx.new(|cx: &mut Context<Self>| {
            let background_transport = transport.clone();
            let pump = cx.spawn(async move |this, cx| {
                let mut connection = Connection::default();
                let mut reconnect = Reconnect::default();
                loop {
                    let transport = background_transport.clone();
                    let admission = admission.clone();
                    let (returned, result) = cx
                        .background_executor()
                        .spawn(async move {
                            let _admission = admission;
                            let result = connection.read(transport.as_ref());
                            (connection, result)
                        })
                        .await;
                    connection = returned;
                    let disconnected = !matches!(result, Ok(Some(_)));
                    // While the agent stays unreachable with the same error,
                    // retries stay on the background thread: no main-loop wakeup.
                    let report = if disconnected {
                        reconnect.failed(result.as_ref().err())
                    } else {
                        reconnect.connected();
                        true
                    };
                    if !report {
                        cx.background_executor().timer(reconnect.delay()).await;
                        continue;
                    }
                    if this
                        .update(cx, |this, cx| match result {
                            Ok(Some(event)) => this.received(event, cx),
                            Ok(None) => this.lost(None, cx),
                            Err(error) => this.lost(Some(error), cx),
                        })
                        .is_err()
                    {
                        break;
                    }
                    if disconnected {
                        cx.background_executor().timer(reconnect.delay()).await;
                    }
                }
            });
            Self::new(transport, Some(readers), pump, Some(reader_admission), cx)
        })
    }

    fn new(
        transport: Arc<dyn Transport>,
        readers: Option<Entity<AppReaders>>,
        pump: Task<()>,
        admission: Option<Arc<ModeLease>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let panel = PanelModel::default();
        let levels = panel.meters;
        Self {
            connected: false,
            connecting: true,
            incompatible_agent: None,
            menu_start_refusal: None,
            panel,
            install: None,
            reported_library: None,
            open_library: None,
            agent_library: AgentLibraryFlag::default(),
            error: None,
            shown: None,
            tickets: Tickets::default(),
            meters: cx.new(|_| LiveMeters {
                levels,
                states: [crate::ui::kit::MeterState::Disconnected; 2],
            }),
            this: cx.weak_entity(),
            transport,
            readers,
            admission,
            _pump: pump,
            meter_flush: None,
        }
    }

    /// A client without the event pump or readers: tests deliver events with
    /// [`DaemonClient::received`] and answer calls through `transport`.
    #[doc(hidden)]
    pub fn detached(transport: Arc<dyn Transport>, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self::new(transport, None, Task::ready(()), None, cx))
    }

    pub(crate) fn admission(&self) -> Option<Arc<ModeLease>> {
        self.admission.clone()
    }

    /// Where the connected agent runs, compared with the open library;
    /// [`AgentLibrary::Unknown`] while disconnected or before its first
    /// state. Clones follow later state events and disconnections.
    pub fn agent_library(&self) -> &AgentLibraryFlag {
        &self.agent_library
    }

    /// The root the agent reported for its open library.
    pub(crate) fn reported_library(&self) -> Option<&std::path::Path> {
        self.reported_library.as_deref()
    }

    /// Compares the library every agent state reports with `open`, the
    /// library this app opened (absolute, fixed for the app's life), from
    /// now on and for the state already received. Until this is called a
    /// reported library counts as [`AgentLibrary::Elsewhere`].
    pub fn compare_agent_library_with(&mut self, open: PathBuf) {
        self.open_library = Some(open);
        self.publish_agent_library();
    }

    /// Compares here, when a state arrives or the stream ends, so whoever
    /// reads [`Self::agent_library`] (a retention save under the config
    /// lock) only loads an atomic value.
    fn publish_agent_library(&self) {
        let value = match (&self.reported_library, &self.open_library) {
            (None, _) => AgentLibrary::Unknown,
            (Some(agent), Some(open)) if !crate::state::agent_runs_elsewhere(agent, open) => {
                AgentLibrary::Open
            }
            (Some(_), _) => AgentLibrary::Elsewhere,
        };
        self.agent_library.set(value);
    }

    /// The recording's levels. Render this entity where they are shown: a
    /// meter change (at most 10 Hz) notifies it alone.
    pub fn meters(&self) -> &Entity<LiveMeters> {
        &self.meters
    }

    pub fn is_connecting(&self) -> bool {
        self.connecting
    }

    pub(crate) fn refresh_capture_access(&self, cx: &App) {
        // Unsupported platforms and older agents have no permission to re-read.
        if self.connected && self.panel.access != broscribe_ipc::CaptureAccess::default() {
            self.request(Call::PermissionsStatus, cx).detach();
        }
    }

    /// One event from the agent's stream. Only a reconnection clears
    /// [`DaemonClient::error`] here: meters arrive ten times a second and
    /// must not hide a failed command. Meters notify [`Self::meters`] only.
    pub fn received(&mut self, event: Event, cx: &mut Context<Self>) {
        let reconnected = !self.connected;
        self.connecting = false;
        self.incompatible_agent = None;
        self.connected = true;
        if reconnected {
            self.clear_error(cx);
            self.menu_start_refusal = None;
        }
        if matches!(&event, Event::State(state) if state.recording.is_some()) {
            self.menu_start_refusal = None;
        }
        if let Event::State(state) = &event {
            self.reported_library = Some(PathBuf::from(&state.library));
            self.publish_agent_library();
            if self.install != state.install {
                self.install = state.install;
                crate::views::update::admission_changed(self.install.is_some(), cx);
            }
        }
        if let Some(readers) = &self.readers {
            readers.update(cx, |readers, cx| {
                if let Err(error) = readers.handle_event(&event, cx) {
                    tracing::warn!(%error, "reader event");
                }
            });
        }
        let changed = self.panel.apply(&event, Instant::now());
        // A new recording resets the levels too.
        self.sync_meters(cx);
        if (changed && !matches!(event, Event::Meters { .. })) || reconnected {
            cx.notify();
        }
        // Rows missed while disconnected come back from the authoritative
        // snapshot (also after a lag or a new session, which `apply` marks).
        if reconnected {
            self.panel.want_snapshot();
        }
        self.refresh_live(cx);
        if self.meter_flush.is_none()
            && let Some(delay) = self.panel.meter_flush_delay(Instant::now())
        {
            self.meter_flush = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(delay).await;
                this.update(cx, |this, cx| {
                    this.meter_flush = None;
                    if this.panel.flush_meters(Instant::now()) {
                        this.sync_meters(cx);
                    }
                })
                .ok();
            }));
        }
        cx.emit(DaemonEvent::Received(event));
    }

    /// Copies the panel model's levels into [`Self::meters`], notifying it
    /// only when they changed.
    fn sync_meters(&self, cx: &mut App) {
        use crate::ui::kit::MeterState;
        let levels = self.panel.meters;
        let states = [broscribe_ipc::Track::Mic, broscribe_ipc::Track::System].map(|track| {
            if !self.connected {
                MeterState::Disconnected
            } else if self.panel.access.denied(track) {
                MeterState::NoAccess
            } else if let Some(recording) = &self.panel.recording {
                if recording.status == RecordingStatus::Paused {
                    MeterState::Paused
                } else if recording.health.tracks.iter().any(|health| {
                    health.track == track && health.state != broscribe_ipc::TrackState::Capturing
                }) {
                    MeterState::Retrying
                } else {
                    MeterState::Recording
                }
            } else {
                MeterState::Disconnected
            }
        });
        self.meters.update(cx, |meters, cx| {
            if meters.levels != levels || meters.states != states {
                meters.levels = levels;
                meters.states = states;
                cx.notify();
            }
        });
    }

    /// The stream ended (`error` is `None`) or failed. The panel says the
    /// agent is disconnected and claims nothing about the preview meanwhile;
    /// a failure is shown in words ([`agent_error_text`]) and logged by its
    /// code.
    pub fn lost(&mut self, error: Option<IpcError>, cx: &mut Context<Self>) {
        let incompatible = error
            .as_ref()
            .filter(|error| error.code == IpcError::UNSUPPORTED_VERSION)
            .map(|error| peer_protocol(&error.message));
        let changed = self.connecting || incompatible != self.incompatible_agent;
        self.connecting = false;
        self.incompatible_agent = incompatible;
        if let Some(readers) = &self.readers {
            readers.update(cx, |readers, cx| readers.disconnected(cx));
        }
        // The next agent may run on another library.
        self.reported_library = None;
        self.publish_agent_library();
        let was_connected = self.connected;
        if was_connected {
            self.connected = false;
            self.panel.disconnected();
            self.sync_meters(cx);
        }
        if changed || was_connected {
            cx.emit(DaemonEvent::Disconnected);
            cx.notify();
        }
        if let Some(error) = error {
            tracing::warn!(code = %error.code, "agent event stream failed");
            let ticket = self.tickets.issue(Origin::Connection);
            self.show(ticket, agent_error_text(&error), cx);
        }
    }

    /// Requests the `record.live` snapshot the panel model wants, if any;
    /// the answer replaces the live rows of that session only.
    pub fn refresh_live(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.panel.take_snapshot_request() else {
            return;
        };
        let request = self.request(
            Call::RecordLive {
                session_id: session.clone(),
            },
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = request.await;
            this.update(cx, |this, cx| {
                // A failed request is already logged once, by method and
                // code ([`Self::tracked`]); an unreadable answer by its
                // category only (its words may quote the transcript).
                let snapshot = result.ok().and_then(|value| {
                    serde_json::from_value::<LiveSnapshot>(value)
                        .inspect_err(|error| {
                            tracing::warn!(
                                method = "record.live",
                                category = ?error.classify(),
                                "reading the live snapshot"
                            )
                        })
                        .ok()
                });
                if this.panel.apply_snapshot(&session, snapshot) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Sends `call` off GPUI's loop. Its failure is the caller's to show;
    /// its success clears an agent-level error of the same command (the
    /// user did it again, here or elsewhere).
    pub fn request(&self, call: Call, cx: &App) -> Task<Result<Value, IpcError>> {
        self.tracked(call, no_failure, cx)
    }

    pub fn record_start_pending(&self) -> bool {
        self.tickets.latest.borrow().contains_key(&Origin::Command {
            method: "record.start",
            target: None,
        })
    }

    /// [`Self::request`] whose failures that `report` accepts become the
    /// agent-level error, as with [`Self::send`] (a notification action:
    /// every failure but a notification the agent no longer has).
    pub fn request_reporting(
        &self,
        call: Call,
        report: fn(&IpcError) -> bool,
        cx: &App,
    ) -> Task<Result<Value, IpcError>> {
        self.tracked(call, report, cx)
    }

    /// Sends `call`; its failure becomes [`DaemonClient::error`], and its
    /// success clears an earlier failure of the same command.
    pub fn send(&mut self, call: Call, cx: &mut Context<Self>) {
        if matches!(call, Call::RecordStart(_)) && self.menu_start_refusal.take().is_some() {
            cx.notify();
        }
        self.tracked(call, every_failure, cx).detach();
    }

    /// The request runs on the background executor; its answer reaches the
    /// agent-level error even when the caller stops waiting for it. Every
    /// failure is logged once, by method and code (never the payload or
    /// the agent's words, which may echo what the user typed), whoever
    /// shows it.
    fn tracked(
        &self,
        call: Call,
        report: fn(&IpcError) -> bool,
        cx: &App,
    ) -> Task<Result<Value, IpcError>> {
        let method = call.method();
        let ticket = self.tickets.issue(Origin::of(&call));
        let starting = matches!(call, Call::RecordStart(_));
        let transport = self.transport.clone();
        let admission = self.admission.clone();
        let answer = cx.background_executor().spawn(async move {
            let _admission = admission;
            transport.call(call)
        });
        let (answered, answer_rx) = oneshot::channel();
        let this = self.this.clone();
        cx.spawn(async move |cx| {
            if starting {
                this.update(cx, |_, cx| cx.notify()).ok();
            }
            let result = answer.await;
            if let Err(error) = &result {
                tracing::warn!(method, code = %error.code, "agent request failed");
            }
            this.update(cx, |this, cx| {
                let outcome = result.as_ref().map(|_| ());
                this.settle(ticket, outcome, report, cx);
            })
            .ok();
            answered.send(result).ok();
        })
        .detach();
        cx.foreground_executor().spawn(async move {
            answer_rx.await.unwrap_or_else(|_| {
                Err(IpcError::new(
                    AGENT_UNREACHABLE,
                    "The connection to BroScribe Agent closed",
                ))
            })
        })
    }

    /// Applies an answer to the agent-level error. Only the newest request
    /// of a command counts: an earlier one answering late changes nothing.
    fn settle(
        &mut self,
        ticket: Ticket,
        outcome: Result<(), &IpcError>,
        report: fn(&IpcError) -> bool,
        cx: &mut Context<Self>,
    ) {
        if !self.tickets.retire(&ticket) {
            return;
        }
        let starting = matches!(
            ticket.origin,
            Origin::Command {
                method: "record.start",
                ..
            }
        );
        if starting {
            cx.notify();
        }
        match outcome {
            Ok(()) => {
                if starting {
                    self.menu_start_refusal = None;
                }
                if self.shown().is_some_and(|shown| {
                    shown.origin == ticket.origin && shown.number < ticket.number
                }) {
                    self.clear_error(cx);
                }
            }
            Err(error) if report(error) && starting => {
                if self.panel.active_session().is_none() {
                    self.menu_start_refusal = Some(start_refusal(
                        error,
                        crate::ui::setup_window::capture_active(cx),
                    ));
                }
            }
            Err(error) if report(error) => self.show(ticket, agent_error_text(error), cx),
            Err(_) => {}
        }
    }

    /// What [`Self::error`] is about, unless something else wrote it.
    fn shown(&self) -> Option<&Shown> {
        self.shown
            .as_ref()
            .filter(|shown| self.error.as_deref() == Some(shown.text.as_str()))
    }

    /// Shows a failure unless a newer one is shown already.
    fn show(&mut self, ticket: Ticket, text: String, cx: &mut Context<Self>) {
        if self
            .shown()
            .is_some_and(|shown| shown.number > ticket.number)
        {
            return;
        }
        let changed = self.error.as_ref() != Some(&text);
        self.error = Some(text.clone());
        self.shown = Some(Shown {
            origin: ticket.origin,
            number: ticket.number,
            text,
        });
        if changed {
            cx.notify();
        }
    }

    fn clear_error(&mut self, cx: &mut Context<Self>) {
        self.shown = None;
        if self.error.take().is_some() {
            cx.notify();
        }
    }
}

/// Reconnection after the agent's stream ended or could not be opened: the
/// delay doubles from 1 s up to [`Reconnect::MAX_DELAY`], and a failure is
/// reported to the UI only when it differs from the previous one.
#[derive(Debug, Default)]
struct Reconnect {
    failures: u32,
    last_error: Option<IpcError>,
}

impl Reconnect {
    const MAX_DELAY: Duration = Duration::from_secs(8);

    /// Records a failed read; whether the UI must hear about it.
    fn failed(&mut self, error: Option<&IpcError>) -> bool {
        let report = self.failures == 0 || self.last_error.as_ref() != error;
        self.failures = self.failures.saturating_add(1);
        self.last_error = error.cloned();
        report
    }

    fn connected(&mut self) {
        self.failures = 0;
        self.last_error = None;
    }

    /// Wait before the next attempt.
    fn delay(&self) -> Duration {
        let doublings = self.failures.saturating_sub(1).min(8);
        Duration::from_secs(1u64 << doublings).min(Self::MAX_DELAY)
    }
}

/// An installed-but-stopped service must be offered a start, not replaced
/// with an app-owned processing child. The platform autostart is authoritative.
pub fn local_fallback_allowed(
    installed: Result<bool, broscribe_core::Error>,
    connected: bool,
) -> bool {
    !connected && matches!(installed, Ok(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    struct FakeStream(VecDeque<Result<Option<Event>, IpcError>>);
    impl EventStream for FakeStream {
        fn next_event(&mut self) -> Result<Option<Event>, IpcError> {
            self.0.pop_front().unwrap_or(Ok(None))
        }
    }
    struct FakeTransport {
        attempts: AtomicUsize,
        streams: Mutex<VecDeque<FakeStream>>,
    }
    impl Transport for FakeTransport {
        fn subscribe(&self) -> Result<Box<dyn EventStream>, IpcError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.streams
                .lock()
                .unwrap()
                .pop_front()
                .map(|s| Box::new(s) as Box<dyn EventStream>)
                .ok_or_else(|| IpcError::new(AGENT_UNREACHABLE, "offline"))
        }
        fn call(&self, _: Call) -> Result<Value, IpcError> {
            Err(IpcError::new("unused", "unused"))
        }
    }
    #[test]
    fn reconnects_after_eof_and_transport_failure() {
        let event = Event::Lagged { dropped: 2 };
        let transport = FakeTransport {
            attempts: AtomicUsize::new(0),
            streams: Mutex::new(VecDeque::from([
                FakeStream(VecDeque::from([Ok(None)])),
                FakeStream(VecDeque::from([Err(IpcError::new(
                    AGENT_UNREACHABLE,
                    "broken",
                ))])),
                FakeStream(VecDeque::from([Ok(Some(event.clone()))])),
            ])),
        };
        let mut connection = Connection::default();
        assert_eq!(connection.read(&transport), Ok(None));
        assert!(connection.read(&transport).is_err());
        assert_eq!(connection.read(&transport), Ok(Some(event)));
        assert_eq!(transport.attempts.load(Ordering::SeqCst), 3);
    }
    #[test]
    fn unreachable_agent_backs_off_and_reports_only_changes() {
        let offline = IpcError::new(AGENT_UNREACHABLE, "offline");
        let denied = IpcError::new(AGENT_UNREACHABLE, "permission denied");
        let mut r = Reconnect::default();
        assert!(r.failed(None), "the stream ending is reported");
        assert_eq!(r.delay(), Duration::from_secs(1));
        assert!(r.failed(Some(&offline)), "a new error is reported");
        assert_eq!(r.delay(), Duration::from_secs(2));
        // The same error again: retried quietly, ever less often.
        for _ in 0..10 {
            assert!(!r.failed(Some(&offline)));
        }
        assert_eq!(r.delay(), Reconnect::MAX_DELAY);
        assert!(r.failed(Some(&denied)));
        r.connected();
        assert_eq!(r.delay(), Duration::from_secs(1));
        assert!(r.failed(Some(&denied)), "first failure after a connection");
    }
    #[test]
    fn installed_stopped_agent_never_falls_back() {
        assert!(!local_fallback_allowed(Ok(true), false));
        assert!(!local_fallback_allowed(Ok(false), true));
        assert!(local_fallback_allowed(Ok(false), false));
    }

    const UNREACHABLE: &str = "BroScribe Agent isn't running.";

    /// WRT-04: an agent refusal shows the agent's message as a sentence,
    /// never its code; the log keeps the code.
    #[test]
    fn agent_failures_show_words_without_codes() {
        let busy = IpcError::new("busy", "capture device busy");
        assert_eq!(agent_error_text(&busy), "Capture device busy.");
        assert_eq!(busy.to_string(), "capture device busy (busy)");
        assert_eq!(
            agent_error_text(&IpcError::new("invalid_input", "invalid input: bad id")),
            "Bad id."
        );
        let remote = agent_call_error(CallError::Remote(IpcError::new("not_found", "gone")));
        assert_eq!(remote, IpcError::new("not_found", "gone"));
    }

    /// WRT-04: the agent builds its refusals from library errors
    /// (`IpcError::from_core`), whose text is the system's: a note added on a
    /// full disk names the journal's path and the system's error. The codes
    /// whose message can hold a path get the CLI's fixed sentences; any
    /// other refusal keeps its words without a path.
    #[test]
    fn agent_refusals_never_show_the_systems_text() {
        let private = "/Users/synthetic-private/Library/BroScribe/journal/s-zq3/notes.jsonl";
        let full_disk = IpcError::from_core(&broscribe_core::Error::io_at(
            std::path::Path::new(private),
            std::io::Error::from_raw_os_error(28),
        ));
        assert_eq!(full_disk.code, "io");
        assert!(full_disk.message.contains("os error 28"), "{full_disk}");
        for (error, words) in [
            (full_disk, "BroScribe couldn't read or write a file."),
            (
                IpcError::new("schema", format!("schema error: {private}: expected value")),
                "BroScribe couldn't read its library data.",
            ),
            (
                IpcError::new("decode", format!("decode error: {private}")),
                "BroScribe couldn't read its library data.",
            ),
            (
                IpcError::new(
                    "internal",
                    format!("internal error: {private} (os error 5)"),
                ),
                "Something went wrong inside BroScribe.",
            ),
            (
                IpcError::new("lease_lost", format!("lease lost: {private}")),
                "Another BroScribe process took over this work.",
            ),
            (
                IpcError::new("inference", format!("inference error: {private} exited")),
                "A model on this computer couldn't finish.",
            ),
            (
                IpcError::new(
                    "invalid_input",
                    format!("invalid input: {private} is not a note file"),
                ),
                "notes.jsonl is not a note file.",
            ),
        ] {
            let text = agent_error_text(&error);
            assert_eq!(text, words, "{error}");
            for raw in [
                "synthetic-private",
                "os error",
                "/Users",
                error.code.as_str(),
            ] {
                assert!(!text.contains(raw), "{raw} shown: {text}");
            }
        }
    }

    /// WRT-04: a failed `record.live` snapshot is logged once, by method
    /// and code, as every request is ([`DaemonClient::tracked`]); never a
    /// second time with the agent's words.
    #[gpui::test]
    fn a_failed_live_snapshot_is_logged_once_by_method_and_code(cx: &mut gpui::TestAppContext) {
        fn live_refused(call: &Call) -> Result<Value, IpcError> {
            match call {
                Call::RecordLive { .. } => Err(IpcError::new(
                    "io",
                    "/Users/synthetic-private/journal/s-zq3/live.jsonl: Input/output error (os \
                     error 5)",
                )),
                _ => Ok(Value::Null),
            }
        }
        let daemon = cx.update(|cx| DaemonClient::detached(scripted(live_refused), cx));
        let state = serde_json::from_value(serde_json::json!({
            "pid": 123,
            "version": "synthetic",
            "protocol": broscribe_ipc::PROTOCOL_VERSION,
            "library": "/synthetic/library",
            "recording": {
                "session_id": "s-zq3",
                "status": "recording",
                "started_at": "2026-11-14T10:00:00Z",
                "elapsed_s": 1.0,
                "language": null,
                "app": null,
                "title": null,
                "setup": null,
                "participants": [],
                "observed_roster": [],
                "tags": [],
                "health": { "tracks": [] }
            },
            "jobs": { "running": null, "waiting": 0, "deferred": 0 },
            "preview": { "state": "off" }
        }))
        .unwrap();
        let (log, _guard) = crate::test_support::LogCapture::start();
        // The first state after connecting asks for the live snapshot.
        daemon.update(cx, |d, cx| d.received(Event::State(Box::new(state)), cx));
        cx.run_until_parked();
        let text = log.text();
        assert_eq!(text.matches("record.live").count(), 1, "{text}");
        assert_eq!(text.matches("agent request failed").count(), 1, "{text}");
        assert!(text.contains("code=io"), "{text}");
        for raw in ["synthetic-private", "os error", "Input/output"] {
            assert!(!text.contains(raw), "{raw} logged: {text}");
        }
    }

    /// WRT-04: the setup assistant's helper calls, made without the shared
    /// client, log their failure once by method and code (or the
    /// connection's kind), never the system's words or a path.
    #[test]
    fn helper_call_failures_are_logged_by_method_and_code() {
        let (log, _guard) = crate::test_support::LogCapture::start();
        let refused = CallError::Remote(IpcError::new(
            "io",
            "/Users/synthetic-private/journal/s-zq3: No space left on device (os error 28)",
        ));
        let text = call_error_text(Call::KickQueue.method(), refused);
        assert_eq!(text, "BroScribe couldn't read or write a file.");
        let gone = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "/Users/synthetic-private/agent.sock: Permission denied (os error 13)",
        );
        assert_eq!(connect_error_text(Call::Ping.method(), &gone), UNREACHABLE);
        let logged = log.text();
        assert_eq!(logged.lines().count(), 2, "{logged}");
        let method = Call::KickQueue.method();
        let lines: Vec<&str> = logged.lines().collect();
        assert!(
            lines[0].contains("agent request failed")
                && lines[0].contains("method=")
                && lines[0].contains(method)
                && lines[0].contains("code=io"),
            "{logged}"
        );
        assert!(
            lines[1].contains("connecting to the agent failed")
                && lines[1].contains("method=")
                && lines[1].contains("ping")
                && lines[1].contains("kind=PermissionDenied"),
            "{logged}"
        );
        for raw in [
            "synthetic-private",
            "os error",
            "No space",
            "Permission denied",
        ] {
            assert!(!logged.contains(raw), "{raw} logged: {logged}");
        }
    }

    /// WRT-04: a request that never reached the agent, one whose outcome is
    /// unknown and the protocol's own refusals are said in words, never the
    /// system's text ("Connection refused (os error 61)") or the code.
    #[test]
    fn transport_failures_are_said_in_words() {
        let refused = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let gone = agent_call_error(CallError::NotSent(refused));
        assert_eq!(gone.code, AGENT_UNREACHABLE);
        assert_eq!(agent_error_text(&gone), UNREACHABLE);
        let raw = IpcError::new(
            AGENT_UNREACHABLE,
            "Daemon not reached: Connection refused (os error 61)",
        );
        assert_eq!(agent_error_text(&raw), UNREACHABLE);
        for (code, words) in [
            (
                AGENT_UNCERTAIN,
                "BroScribe Agent didn't confirm whether it happened. Check before trying again.",
            ),
            (
                AGENT_NOT_OPERATIONAL,
                "BroScribe Agent isn't ready to record yet.",
            ),
            (IpcError::SHUTTING_DOWN, "BroScribe Agent is shutting down."),
            (
                IpcError::IN_PROGRESS,
                "BroScribe Agent is still doing this.",
            ),
            (
                IpcError::UNSUPPORTED_VERSION,
                "BroScribe Agent is a different version. Restart BroScribe.",
            ),
            (
                IpcError::PROTOCOL,
                "BroScribe Agent didn't understand the request.",
            ),
            (
                IpcError::INVALID_REQUEST,
                "BroScribe Agent didn't understand the request.",
            ),
        ] {
            let error = IpcError::new(code, "connection reset by peer (os error 54)");
            let text = agent_error_text(&error);
            assert_eq!(text, words, "{code}");
            assert!(!text.contains("os error") && !text.contains(code), "{text}");
        }
        // A start whose capture stopped: the stop code stays out.
        let start = IpcError::new(CAPTURE_FAILED, "recording did not start: disk_low");
        assert_eq!(
            agent_error_text(&start),
            "The recording didn't start. Try again."
        );
    }

    /// WRT-04: every failed request is logged by method and code, also one
    /// whose failure a panel control shows itself (`request`); never with
    /// the agent's words or the call's payload.
    #[gpui::test]
    fn every_failed_request_is_logged_by_its_code(cx: &mut gpui::TestAppContext) {
        fn titles_refused(call: &Call) -> Result<Value, IpcError> {
            match call {
                Call::RecordTitle { .. } => Err(IpcError::new(
                    "stale_session",
                    "no recording titled Private Synthetic Title",
                )),
                _ => Ok(Value::Null),
            }
        }
        let daemon = cx.update(|cx| DaemonClient::detached(scripted(titles_refused), cx));
        let (log, _guard) = crate::test_support::LogCapture::start();
        daemon.update(cx, |d, cx| {
            d.request(
                Call::RecordTitle {
                    session_id: "synthetic-1".into(),
                    title: "Private Synthetic Title".into(),
                    request_id: broscribe_ipc::RequestId::random(),
                },
                cx,
            )
            .detach()
        });
        cx.run_until_parked();
        // The panel shows it itself: no agent-level error.
        assert_eq!(error(&daemon, cx), None);
        let text = log.text();
        assert_eq!(text.matches("agent request failed").count(), 1, "{text}");
        assert!(text.contains("code=stale_session"), "{text}");
        assert!(text.contains("record.title"), "{text}");
        assert!(!text.contains("Private Synthetic"), "{text}");
    }

    type ReplyHandler = fn(&Call) -> Result<Value, IpcError>;

    /// Answers every call with what `answer` says for it now.
    struct Scripted {
        answer: Mutex<ReplyHandler>,
    }
    impl Transport for Scripted {
        fn subscribe(&self) -> Result<Box<dyn EventStream>, IpcError> {
            Err(IpcError::new("unused", "unused"))
        }
        fn call(&self, call: Call) -> Result<Value, IpcError> {
            (self.answer.lock().unwrap())(&call)
        }
    }
    fn scripted(answer: ReplyHandler) -> Arc<Scripted> {
        Arc::new(Scripted {
            answer: Mutex::new(answer),
        })
    }
    fn all_ok(_: &Call) -> Result<Value, IpcError> {
        Ok(Value::Null)
    }
    fn stops_fail(call: &Call) -> Result<Value, IpcError> {
        match call {
            Call::RecordStop { .. } => Err(IpcError::new("busy", "capture device busy")),
            _ => Ok(Value::Null),
        }
    }
    fn refused(message: &str) -> IpcError {
        IpcError::new("synthetic", message)
    }
    fn stop(session: &str) -> Call {
        Call::RecordStop {
            session_id: session.into(),
        }
    }
    fn error(daemon: &Entity<DaemonClient>, cx: &mut gpui::TestAppContext) -> Option<String> {
        daemon.read_with(cx, |d, _| d.error.clone())
    }

    /// UI10: the queue kick sent whenever jobs change, another recording's
    /// stop and an unrelated control succeed without hiding a failed stop;
    /// that same stop succeeding (here from a panel control) clears it.
    #[gpui::test]
    fn only_the_same_command_succeeding_clears_its_failure(cx: &mut gpui::TestAppContext) {
        let transport = scripted(stops_fail);
        let daemon = cx.update(|cx| DaemonClient::detached(transport.clone(), cx));
        daemon.update(cx, |d, cx| d.send(stop("synthetic-1"), cx));
        cx.run_until_parked();
        let failed = Some("Capture device busy.".to_owned());
        assert_eq!(error(&daemon, cx), failed);
        daemon.update(cx, |d, cx| d.send(Call::KickQueue, cx));
        cx.run_until_parked();
        assert_eq!(error(&daemon, cx), failed);
        *transport.answer.lock().unwrap() = all_ok;
        daemon.update(cx, |d, cx| {
            d.send(stop("synthetic-2"), cx);
            d.request(
                Call::RecordPause {
                    session_id: "synthetic-1".into(),
                },
                cx,
            )
            .detach();
        });
        cx.run_until_parked();
        assert_eq!(error(&daemon, cx), failed);
        daemon.update(cx, |d, cx| d.request(stop("synthetic-1"), cx).detach());
        cx.run_until_parked();
        assert_eq!(error(&daemon, cx), None);
    }

    /// An answer to an older request, of the same command or another one,
    /// never replaces what a newer request decided.
    #[gpui::test]
    fn an_older_answer_never_replaces_a_newer_one(cx: &mut gpui::TestAppContext) {
        let daemon = cx.update(|cx| DaemonClient::detached(scripted(all_ok), cx));
        daemon.update(cx, |d, cx| {
            let stop_origin = || Origin::of(&stop("synthetic-1"));
            let first = d.tickets.issue(stop_origin());
            let second = d.tickets.issue(stop_origin());
            d.settle(
                second,
                Err(&refused("stop failed again")),
                every_failure,
                cx,
            );
            d.settle(first, Err(&refused("stop failed")), every_failure, cx);
            assert_eq!(d.error.as_deref(), Some("Stop failed again."));
            // The newest stop succeeded; an earlier one refused late.
            let third = d.tickets.issue(stop_origin());
            let fourth = d.tickets.issue(stop_origin());
            d.settle(fourth, Ok(()), every_failure, cx);
            assert_eq!(d.error, None);
            d.settle(
                third,
                Err(&IpcError::new("stale_session", "stale")),
                every_failure,
                cx,
            );
            assert_eq!(d.error, None);
            // Another command's older failure.
            let pause = d.tickets.issue(Origin::of(&Call::RecordPause {
                session_id: "synthetic-1".into(),
            }));
            let fifth = d.tickets.issue(stop_origin());
            d.settle(fifth, Err(&refused("stop failed")), every_failure, cx);
            d.settle(pause, Err(&refused("pause failed")), every_failure, cx);
            assert_eq!(d.error.as_deref(), Some("Stop failed."));
        });
    }

    /// A stream failure is its own: a command's success does not end it, an
    /// older command's late failure does not replace it, a reconnection does.
    #[gpui::test]
    fn connection_failures_stay_until_the_agent_reconnects(cx: &mut gpui::TestAppContext) {
        let daemon = cx.update(|cx| DaemonClient::detached(scripted(all_ok), cx));
        daemon.update(cx, |d, cx| {
            d.received(Event::Lagged { dropped: 0 }, cx);
            let before = d.tickets.issue(Origin::of(&stop("synthetic-1")));
            d.lost(
                Some(IpcError::new(
                    AGENT_UNREACHABLE,
                    "Daemon not reached: Connection refused (os error 61)",
                )),
                cx,
            );
            d.settle(before, Err(&refused("stop failed")), every_failure, cx);
            assert_eq!(d.error.as_deref(), Some(UNREACHABLE));
            d.send(Call::KickQueue, cx);
        });
        cx.run_until_parked();
        assert_eq!(error(&daemon, cx).as_deref(), Some(UNREACHABLE));
        daemon.update(cx, |d, cx| d.received(Event::Lagged { dropped: 0 }, cx));
        assert_eq!(error(&daemon, cx), None);
    }

    /// The idle agent's state, as it reports running on `library`.
    fn state_on(library: &str) -> Event {
        let state = serde_json::from_value(serde_json::json!({
            "pid": 123,
            "version": "synthetic",
            "protocol": broscribe_ipc::PROTOCOL_VERSION,
            "library": library,
            "recording": null,
            "jobs": { "running": null, "waiting": 0, "deferred": 0 },
            "preview": { "state": "off" }
        }))
        .unwrap();
        Event::State(Box::new(state))
    }

    /// SET-01: the client compares each library the agent reports with the
    /// open one when the state arrives and publishes the result as one
    /// atomic value (no lock: a retention save reads it under the config
    /// lock, which must stay the last lock taken); the stream ending
    /// publishes Unknown again. A clone taken before follows every change.
    #[gpui::test]
    fn agent_library_is_compared_when_a_state_arrives(cx: &mut gpui::TestAppContext) {
        let open = std::env::temp_dir().join("synthetic-open-library");
        let elsewhere = std::env::temp_dir().join("synthetic-other-library");
        let lossy = std::env::temp_dir().join("synthetic-\u{FFFD}-library");
        let daemon = cx.update(|cx| DaemonClient::detached(scripted(all_ok), cx));
        let flag = daemon.read_with(cx, |d, _| d.agent_library().clone());
        let receive = |cx: &mut gpui::TestAppContext, event: Event| {
            daemon.update(cx, |d, cx| d.received(event, cx));
        };
        assert_eq!(flag.get(), AgentLibrary::Unknown);

        // Reported before the app named its open library: fail closed, and
        // compared again once it does.
        receive(cx, state_on(&open.to_string_lossy()));
        assert_eq!(flag.get(), AgentLibrary::Elsewhere);
        daemon.update(cx, |d, _| d.compare_agent_library_with(open.clone()));
        assert_eq!(flag.get(), AgentLibrary::Open);
        assert!(!flag.elsewhere());

        for other in [
            elsewhere.to_string_lossy().into_owned(),
            lossy.to_string_lossy().into_owned(),
            "synthetic-open-library".to_owned(),
        ] {
            receive(cx, state_on(&other));
            assert_eq!(flag.get(), AgentLibrary::Elsewhere, "{other}");
            assert!(flag.elsewhere());
        }
        // Events other than a state leave it as it is.
        receive(cx, Event::Lagged { dropped: 0 });
        assert_eq!(flag.get(), AgentLibrary::Elsewhere);

        daemon.update(cx, |d, cx| d.lost(None, cx));
        assert_eq!(flag.get(), AgentLibrary::Unknown);
        assert!(!flag.elsewhere());
        // A reconnection that hasn't sent its state yet is still unknown.
        receive(cx, Event::Lagged { dropped: 0 });
        assert_eq!(flag.get(), AgentLibrary::Unknown);
        receive(cx, state_on(&open.to_string_lossy()));
        assert_eq!(flag.get(), AgentLibrary::Open);

        // A report holding U+FFFD never matches, even the same text.
        daemon.update(cx, |d, _| d.compare_agent_library_with(lossy.clone()));
        assert_eq!(flag.get(), AgentLibrary::Elsewhere);
        receive(cx, state_on(&lossy.to_string_lossy()));
        assert_eq!(flag.get(), AgentLibrary::Elsewhere);
    }

    /// UI34: meters, flushed at most 10 Hz, notify their own entity and
    /// never the client its observers (panel, menus, windows) follow.
    #[gpui::test]
    fn meters_notify_their_own_entity_only(cx: &mut gpui::TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;
        let daemon = cx.update(|cx| DaemonClient::detached(scripted(all_ok), cx));
        daemon.update(cx, |d, cx| d.received(Event::Lagged { dropped: 0 }, cx));
        cx.run_until_parked();
        let meters = daemon.read_with(cx, |d, _| d.meters().clone());
        let (client, levels) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let _observing = cx.update(|cx| {
            let (c, l) = (client.clone(), levels.clone());
            [
                cx.observe(&daemon, move |_, _| c.set(c.get() + 1)),
                cx.observe(&meters, move |_, _| l.set(l.get() + 1)),
            ]
        });
        let meter = |me_dbfs, others_dbfs| Event::Meters {
            me_dbfs,
            others_dbfs,
        };
        daemon.update(cx, |d, cx| d.received(meter(-20., -30.), cx));
        cx.run_until_parked();
        assert_eq!((client.get(), levels.get()), (0, 1));
        assert_eq!(meters.read_with(cx, |m, _| m.levels()), [-20, -30]);
        // A sample within 100 ms waits for the flush, which again notifies
        // the levels alone.
        daemon.update(cx, |d, cx| d.received(meter(-10., -40.), cx));
        std::thread::sleep(Duration::from_millis(110));
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!((client.get(), levels.get()), (0, 2));
        assert_eq!(meters.read_with(cx, |m, _| m.levels()), [-10, -40]);
    }
}
