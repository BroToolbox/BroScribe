//! The daemon: shared state, request dispatch, recording sessions,
//! retention schedule and startup recovery. The accept loop is in
//! [`crate::server`], job scheduling in [`crate::scheduler`].
//!
//! Locking rules: the library lock is taken only inside short
//! [`Library::write`]s on blocking threads, never while waiting on a child
//! process; the session mutex is never held across a library write it did
//! not find already held. Order is always library → session → stopped
//! list: participant confirmation and session titles take the library lock
//! first and the session mutex inside it, so Stop (session mutex only) can
//! never end a session halfway through their short metadata writes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use broscribe_capture::journal;
use broscribe_core::config::Config;
use broscribe_core::lifecycle::{self, DeleteOptions, SessionTitleOutcome};
use broscribe_core::process::ProcessProbe;
use broscribe_core::queue::{JobId, Queue, RECORD_FINAL_KIND, RECORD_PROVISIONAL_KIND};
use broscribe_core::schema;
use broscribe_core::{Error, Library};
use broscribe_ipc::recording_words::{
    interrupted_stop, meeting_title, stop_reason_text, transcription_cause,
};
use broscribe_ipc::{
    AgentBuild, AgentPing, AutoStopApp, Call, CaptureAccess, CaptureEngine, CaptureEvents,
    CaptureNotice, CaptureOutputs, CaptureWarning, DaemonState, Event, IndexState, IpcError,
    JobsState, JournalIssue, JournalResolution, NotificationButton, NotificationRequest,
    OrphanRecovery, PROTOCOL_VERSION, ParticipantAction, PreviewState, RecordAutoStopParams,
    RecordJobInput, RecordStartParams, RecordingState, RecordingStatus, Request, RequestState,
    Response, SAVED_RECORDINGS_MAX, SavedRecording, SessionConfig, SessionHandle, StoppedRecording,
    StoppedRecordingState, StoppedSession, SystemCalendarAction,
};
use broscribe_platform::CalendarProvider;
use chrono::{DateTime, FixedOffset};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::admission::{Admission, ConnectionId};
use crate::bus::{self, EventBus, Subscriber};
use crate::live::{self, LiveQueue, LiveRun, LiveSettings};
use crate::requests::{Begin, RequestLog};
use crate::shutdown::Shutdown;
use crate::worker_cmd::WorkerCommand;
use crate::{scheduler, writes};

/// Folder of capture journals inside the data folder.
pub use broscribe_core::lifecycle::JOURNAL_DIR;

fn accessory_args(at_login: bool) -> Vec<&'static OsStr> {
    let mut args = vec![OsStr::new("--accessory")];
    if at_login {
        args.push(OsStr::new("--at-login"));
    }
    args
}

#[test]
fn only_login_accessories_carry_the_login_flag() {
    assert_eq!(accessory_args(false), ["--accessory"]);
    assert_eq!(accessory_args(true), ["--accessory", "--at-login"]);
}

/// Daemon settings; [`DaemonConfig::new`] gives the production values.
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub library: PathBuf,
    pub data_dir: PathBuf,
    /// Whether the selected profile overrides native defaults (including cache placement).
    pub data_dir_overridden: bool,
    pub cache_dir: PathBuf,
    pub worker: WorkerCommand,
    /// Override the accessory executable (synthetic child processes in tests).
    pub accessory: Option<WorkerCommand>,
    /// A job is given up after this many worker crashes or claims.
    pub max_attempts: u32,
    /// First retry delay after a failure or crash; doubles per attempt.
    pub retry_backoff: Duration,
    /// Queue re-scan interval (jobs enqueued by other processes).
    pub poll_every: Duration,
    /// Retention runs at startup and then at this interval (design §7.2).
    pub retention_every: Duration,
    /// First retry of queueing a stopped recording's final job after it
    /// failed; doubles per attempt, at most hourly.
    pub journal_retry: Duration,
    /// Poll account expiry; the service enforces its daily/early refresh policy.
    pub integrations_every: Duration,
    pub live: LiveSettings,
    pub subscriber_capacity: usize,
    /// Meter events at most this often (10 Hz).
    pub meter_interval: Duration,
    /// The running agent's build identity ([`agent_build`]), read once at
    /// start by the native entry point; `None` when unknown.
    pub build: Option<AgentBuild>,
}

impl DaemonConfig {
    pub fn new(
        library: PathBuf,
        data_dir: PathBuf,
        cache_dir: PathBuf,
        worker: WorkerCommand,
    ) -> Self {
        Self {
            library,
            data_dir,
            data_dir_overridden: false,
            cache_dir,
            worker,
            accessory: None,
            max_attempts: 3,
            retry_backoff: Duration::from_secs(10),
            poll_every: Duration::from_secs(30),
            retention_every: Duration::from_secs(6 * 60 * 60),
            journal_retry: Duration::from_secs(30),
            integrations_every: Duration::from_secs(60),
            live: LiveSettings::default(),
            subscriber_capacity: bus::DEFAULT_CAPACITY,
            meter_interval: Duration::from_millis(100),
            build: None,
        }
    }

    pub fn journal_root(&self) -> PathBuf {
        self.data_dir.join(JOURNAL_DIR)
    }
}

/// The build identity an agent of version `app_version` reports, from the
/// build information its installation carried when it started: only a
/// present record naming this very version counts. Missing or invalid
/// information, or a record of another version (a replaced installation),
/// is unknown (`None`), never a guess from the version.
pub fn agent_build(
    app_version: &str,
    build: &broscribe_platform::about::BuildInfoState,
) -> Option<AgentBuild> {
    match build {
        broscribe_platform::about::BuildInfoState::Present(info) if info.version == app_version => {
            Some(AgentBuild {
                build: info.build,
                target: info.target.clone(),
            })
        }
        _ => None,
    }
}

/// `config.toml`, or the defaults when it does not load: capture (daemon
/// start-up, recording) must never depend on a readable config.
pub fn config_or_default(data_dir: &std::path::Path) -> Config {
    Config::load(data_dir).unwrap_or_else(|error| {
        tracing::warn!(%error, "config.toml does not load; using the defaults");
        Config::default()
    })
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The running session and what the daemon keeps beside it.
pub(crate) struct Active {
    pub handle: Box<dyn SessionHandle>,
    pub(crate) started_at: DateTime<FixedOffset>,
    title: Option<String>,
    pub app: Option<String>,
    pub reader: Option<(String, u32)>,
    pub observed_roster: Vec<broscribe_readers::RosterMember>,
    /// The matching calendar event's invitees (`RecordingState.expected_roster`).
    pub expected_roster: Vec<broscribe_readers::RosterMember>,
    live_queue: Arc<LiveQueue>,
    live_stop: Arc<Shutdown>,
    /// Live speaker preview of this session (plan D).
    pub(crate) live_preview: Arc<crate::live_identity::LivePreview>,
    /// The session offers an in-person setup and has no meeting-app
    /// evidence recorded yet.
    awaiting_remote_evidence: bool,
    /// Recording that evidence failed and was logged; later failures in this
    /// session stay quiet (the detector polls every second).
    evidence_warned: bool,
    /// Automatic stop of this session (plan H); owns its reader generation.
    pub(crate) auto_stop: crate::autostop::AutoStop,
    /// The current reader incarnation's last batch: when it arrived and
    /// whether its call state was usable (`RecordingState.reader`).
    pub(crate) reader_seen: Option<(Instant, bool)>,
    /// When the current reader incarnation was issued: one that sends no
    /// batch for long after (its start failed) shows as silent.
    pub(crate) reader_issued: Instant,
    /// When the current incarnation was issued or last offered again for
    /// bounded recovery after a failed snapshot or an abandoned host.
    pub(crate) reader_announced: Instant,
    /// Diagnostics of the current incarnation's latest accepted batch
    /// (`RecordingState.reader_health`); never call evidence.
    pub(crate) reader_health: Option<broscribe_readers::ReaderHealth>,
    /// Why the reader host could not start the current incarnation, as it
    /// reported (`reader.refused`), kept only once validated against the
    /// session, its reader and the current incarnation; cleared by every
    /// new incarnation. Diagnostics only, never call evidence.
    pub(crate) reader_refusal: Option<broscribe_readers::ReaderStartRefusal>,
    /// The reader status last published, so a reader going silent shows
    /// without waiting for another state change.
    pub(crate) reader_shown: Option<broscribe_ipc::ReaderStatus>,
}

/// A start admitted under the session mutex whose devices are opening
/// outside it ([`start_recording`]). It holds the capture slot
/// (`checking_audio`) meanwhile; a shutdown, an end of its detected meeting
/// or capture stopping on its own cancels it, and its commit then stops the
/// opened session instead of activating it.
pub(crate) struct Starting {
    session_id: String,
    cancelled: Option<String>,
}

/// The ASR preview state `DaemonState.preview` shows and the recording
/// whose live supervisor may change it (`None`: no recording, `off`).
#[derive(Default)]
struct PreviewSlot {
    owner: Option<String>,
    state: PreviewState,
}

/// A notification waiting for the user's answer.
enum Pending {
    /// Detection asked whether to record (policy `ask`).
    AskRecord {
        params: RecordStartParams,
        notification: NotificationRequest,
        created: Instant,
    },
    OpenMeeting(NotificationRequest),
    OpenIntegration(NotificationRequest, String),
    FailedRecording(NotificationRequest, String),
}

pub(crate) struct Shared {
    pub cfg: DaemonConfig,
    pub lib: Library,
    pub admission: Arc<broscribe_core::maintenance::ModeLease>,
    pub bus: EventBus,
    pub probe: Arc<dyn ProcessProbe>,
    engine: Arc<dyn CaptureEngine>,
    requests: Mutex<RequestLog>,
    pub jobs: Mutex<JobsState>,
    pub session: Mutex<Option<Active>>,
    /// `DaemonState.preview` and the recording that owns it: set at a
    /// start's commit and released at its stop, both under the session
    /// mutex, so a stopped recording's late supervisor never overwrites the
    /// next one's preview.
    preview: Mutex<PreviewSlot>,
    pending: Mutex<HashMap<String, Pending>>,
    /// Re-scan the queue now.
    pub detector: Mutex<Option<broscribe_detect::Detector>>,
    pub kick: Notify,
    pub integrations: Mutex<Option<Arc<dyn crate::integrations::IntegrationMaintenance>>>,
    pub integration_kick: Notify,
    pub audio_check: Mutex<Arc<dyn crate::audio_check::AudioCheck>>,
    /// Capture admission held by something other than a running session: an
    /// audio check, a setup capture or permission prompt, or a recording
    /// whose platform capture is still stopping.
    pub checking_audio: AtomicBool,
    pub devices: Mutex<Arc<dyn broscribe_platform::DeviceInfo>>,
    capture_access: Mutex<CaptureAccess>,
    pub sample_mic: Mutex<Arc<dyn broscribe_platform::MicCapture>>,
    pub reader_monitor: Mutex<Arc<dyn broscribe_platform::MicUsageMonitor>>,
    pub sample: Mutex<Option<Arc<crate::sample::Control>>>,
    pub sample_cancelled: Mutex<std::collections::VecDeque<String>>,
    pub permission_results: Mutex<HashMap<String, String>>,
    accessory_running: Arc<AtomicBool>,
    accessory_attempt: Mutex<Option<Instant>>,
    stopping: AtomicBool,
    pub shutdown: Shutdown,
    /// Stopped recordings not saved yet (`DaemonState.stopped`), oldest first.
    stopped: Mutex<Vec<StoppedRecording>>,
    /// Recordings saved during this run, newest first (`DaemonState.saved`).
    saved: Mutex<VecDeque<SavedRecording>>,
    /// Sessions taken out of `session` whose capture is still stopping (the
    /// journal writer may still write): never cleaned up meanwhile.
    stopping_sessions: Mutex<HashSet<String>>,
    /// Last issued reader generation: unique per reader incarnation.
    reader_generations: AtomicU64,
    /// Meeting apps on the mic at the last successful detector poll,
    /// linkable by `record.auto_stop`; cleared by a failed poll.
    pub(crate) auto_stop_candidates: Mutex<Vec<AutoStopApp>>,
    /// Serializes `record.tag` across its journal and provisional-meeting
    /// updates ([`record_tag`]); taken before the session mutex, never
    /// inside it or the library lock.
    tag_mutations: Mutex<()>,
    /// Startup maintenance, recovery and index repair have not finished:
    /// no capture is admitted until they have ([`Daemon::finish_startup`]).
    startup_pending: AtomicBool,
    /// Runs the startup work exactly once; a second caller waits for it.
    startup: std::sync::Once,
    /// A start whose devices are opening outside the session mutex.
    starting: Mutex<Option<Starting>>,
    /// Install admission (signed updater); mutated under the session mutex.
    install: Mutex<Admission>,
    /// Journals the last recovery sweep could not recover.
    journal_issues: Mutex<Vec<JournalIssue>>,
    // Serializes recovery scans and legacy journal resolution/cleanup.
    journal_ops: Mutex<()>,
    /// Rebuilt rows whose first job observation establishes the baseline
    /// without sending a failure notification again after restart.
    rebuilt_stopped: Mutex<HashSet<String>>,
    index: Mutex<IndexState>,
    /// Last issued client connection ID.
    connections: AtomicU64,
    /// The queue scheduler task; [`Daemon::finish`] awaits it, so a worker
    /// killed by a planned shutdown is handed back without an attempt.
    scheduler_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// When the next journal recovery sweep is due, while one is planned
    /// (`retry_in_s` of a stopped recording whose journal awaits recovery).
    journal_sweep_at: Mutex<Option<Instant>>,
    /// Holds a stop between its capture closing and its handover to the
    /// pending-save row.
    #[cfg(test)]
    stop_handover: Gate,
    /// The ID the next recording start uses instead of a generated one, so
    /// a test names consecutive recordings deterministically.
    #[cfg(test)]
    next_session_id: Mutex<Option<String>>,
}

/// A test barrier: the next [`Gate::pass`] signals `reached`, then waits
/// for `release`.
#[cfg(test)]
#[derive(Default)]
struct Gate(Mutex<Option<GateEnds>>);

/// The `reached` signal and the `release` wait of a [`Gate`].
#[cfg(test)]
type GateEnds = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

#[cfg(test)]
impl Gate {
    fn pass(&self) {
        let gate = lock(&self.0).take();
        if let Some((reached, release)) = gate {
            let _ = reached.send(());
            let _ = release.recv();
        }
    }
}

impl Shared {
    pub fn is_recording(&self) -> bool {
        lock(&self.session).is_some()
    }

    /// Re-read grants without prompting or holding the capture session lock.
    pub(crate) fn refresh_capture_access(&self) -> (CaptureAccess, bool) {
        let mut previous = lock(&self.capture_access);
        let current = crate::sample::capture_access(self);
        let changed = *previous != current;
        *previous = current;
        (current, changed)
    }

    /// A new reader generation (start, pause, resume, relink).
    pub(crate) fn next_reader_generation(&self) -> u64 {
        self.reader_generations.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn state(&self) -> DaemonState {
        let recording = lock(&self.session).as_ref().map(|a| {
            let st = a.handle.status();
            RecordingState {
                session_id: a.handle.session_id().to_owned(),
                status: st.status,
                started_at: a.started_at,
                elapsed_s: st.elapsed_s,
                language: st.language,
                app: a.app.clone(),
                title: a.title.clone(),
                setup: st.setup,
                participants: st.participants,
                observed_roster: a.observed_roster.clone(),
                expected_roster: a.expected_roster.clone(),
                tags: st.tags,
                auto_stop: Some(a.auto_stop.status(&lock(&self.auto_stop_candidates))),
                health: a.handle.health(),
                reader: crate::readers::status(a, st.status),
                reader_health: a.reader_health.clone(),
            }
        });
        DaemonState {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol: PROTOCOL_VERSION,
            library: self.lib.root().display().to_string(),
            recording,
            access: *lock(&self.capture_access),
            detection_enabled: lock(&self.detector).is_some(),
            jobs: lock(&self.jobs).clone(),
            preview: lock(&self.preview).state.clone(),
            pending_notifications: {
                let pending = lock(&self.pending);
                let mut ordered: Vec<_> = pending.iter().collect();
                ordered.sort_by_key(|(id, prompt)| {
                    let received = match prompt {
                        Pending::AskRecord { created, .. } => Some(*created),
                        _ => None,
                    };
                    (received, id.as_str())
                });
                ordered
                    .into_iter()
                    .map(|(_, prompt)| match prompt {
                        Pending::AskRecord {
                            notification: n, ..
                        }
                        | Pending::OpenMeeting(n)
                        | Pending::OpenIntegration(n, _)
                        | Pending::FailedRecording(n, _) => n.clone(),
                    })
                    .collect()
            },
            stopped: self.stopped_shown(),
            saved: lock(&self.saved).iter().cloned().collect(),
            journal_issues: lock(&self.journal_issues).clone(),
            index: lock(&self.index).clone(),
            install: lock(&self.install).hold(),
            build: self.cfg.build.clone(),
        }
    }

    /// `DaemonState.stopped`. A stopped journal whose recovery failed, or
    /// whose final job could not be queued (the library unwritable or busy),
    /// has no final job behind its row yet: it shows that retry, never a
    /// queue place nothing would ever advance. One that may pass (I/O, a
    /// busy library) is retried at the next sweep; a corrupt one is failed
    /// for good. Once a sweep queues its final job the row shows that job.
    fn stopped_shown(&self) -> Vec<StoppedRecording> {
        let mut stopped = lock(&self.stopped).clone();
        let issues = lock(&self.journal_issues);
        if issues.is_empty() {
            return stopped;
        }
        let retry_in_s = lock(&self.journal_sweep_at).map_or(0, |due| {
            let left = due.saturating_duration_since(Instant::now());
            left.as_secs() + u64::from(left.subsec_nanos() > 0)
        });
        for entry in stopped.iter_mut().filter(|r| r.state.is_pending_save()) {
            if let Some(issue) = issues.iter().find(|i| i.session_id == entry.session_id) {
                let error = Some(issue.code.clone());
                entry.state = if issue.corrupt {
                    StoppedRecordingState::Failed { error }
                } else {
                    StoppedRecordingState::Retrying {
                        attempts: 0,
                        retry_in_s,
                        error,
                    }
                };
            }
        }
        stopped
    }

    /// Admits one capture (a recording, setup sample, audio check or
    /// permission prompt) under the session mutex `session` guards: refused
    /// while the daemon stops, while an install admission is held, until
    /// startup recovery finished, while a recording runs and while another
    /// capture holds the slot. The slot (`checking_audio`) is held until the
    /// returned guard drops.
    pub(crate) fn admit_capture(
        &self,
        session: &Option<Active>,
        what: &str,
    ) -> Result<Reserved<'_>, IpcError> {
        if self.stopping.load(Ordering::Acquire) || self.shutdown.is_triggered() {
            return Err(IpcError::new(
                IpcError::SHUTTING_DOWN,
                "the daemon is stopping",
            ));
        }
        lock(&self.install).refuse_capture()?;
        if self.startup_pending.load(Ordering::Acquire) {
            return Err(IpcError::new(
                IpcError::BUSY,
                "BroScribe Agent is still recovering earlier recordings",
            ));
        }
        if session.is_some() || self.checking_audio.swap(true, Ordering::AcqRel) {
            return Err(IpcError::new(
                IpcError::BUSY,
                format!("{what} is unavailable while capturing"),
            ));
        }
        Ok(Reserved(&self.checking_audio))
    }

    /// Reserving a job and obtaining a library-switch admission are
    /// serialized on the jobs mutex. A switch can never cut off a worker.
    pub(crate) fn reserve_job(&self, progress: broscribe_ipc::JobProgress) -> bool {
        let mut jobs = lock(&self.jobs);
        if self.stopping.load(Ordering::Acquire)
            || self.shutdown.is_triggered()
            || lock(&self.install).fences_jobs()
        {
            return false;
        }
        jobs.running = Some(progress);
        true
    }

    pub fn publish_state(&self) {
        self.bus.publish(Event::State(Box::new(self.state())));
    }

    /// A preview state of recording `session_id`'s live supervisor: kept
    /// and published only while that recording owns the preview. The event
    /// is published under the slot lock, so it is ordered with the owner's
    /// start and release.
    fn set_preview(&self, session_id: &str, p: PreviewState) {
        let mut slot = lock(&self.preview);
        if slot.owner.as_deref() != Some(session_id) || slot.state == p {
            return;
        }
        slot.state = p.clone();
        self.bus.publish(Event::Preview {
            session_id: session_id.to_owned(),
            preview: p,
        });
    }

    /// Hands the preview to `owner` (a start's commit) or releases it (a
    /// stop), `off` until the owner's supervisor reports. Called under the
    /// session mutex that activates or takes the recording.
    fn own_preview(&self, owner: Option<&str>) {
        *lock(&self.preview) = PreviewSlot {
            owner: owner.map(str::to_owned),
            state: PreviewState::Off,
        };
    }

    pub(crate) fn launch_accessory(&self) {
        self.launch_accessory_with_reason(false);
    }

    fn launch_accessory_with_reason(&self, at_login: bool) {
        if self.bus.subscriber_count() != 0 {
            return;
        }
        let mut attempted = lock(&self.accessory_attempt);
        if attempted.is_some_and(|at| at.elapsed() < Duration::from_secs(5))
            || self.accessory_running.swap(true, Ordering::AcqRel)
        {
            return;
        }
        *attempted = Some(Instant::now());
        drop(attempted);
        let command = self.cfg.accessory.clone().map(Ok).unwrap_or_else(|| {
            WorkerCommand::bundled_executable("broscribe-app").map(WorkerCommand::new)
        });
        let child = command.and_then(|command| {
            let mut command = command;
            command.data_dir_override = self.cfg.worker.data_dir_override.clone();
            command
                .std_command(&self.cfg.library, &accessory_args(at_login))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        });
        match child {
            Ok(mut child) => {
                let running = self.accessory_running.clone();
                std::thread::spawn(move || {
                    let _ = child.wait();
                    running.store(false, Ordering::Release);
                });
            }
            Err(error) => {
                self.accessory_running.store(false, Ordering::Release);
                tracing::warn!(%error, "could not open accessory UI");
            }
        }
    }

    pub(crate) fn reconcile_integration_prompts(&self) {
        let accounts = lock(&self.integrations)
            .clone()
            .and_then(|service| service.list_accounts().ok());
        let mut pending = lock(&self.pending);
        pending.retain(|id, p| {
            let Pending::OpenIntegration(_, integration) = p else {
                return true;
            };
            if let Some((provider, account_id)) = integration.split_once('/') {
                let Some(accounts) = &accounts else {
                    return true;
                };
                let keep = accounts
                    .iter()
                    .find(|account| {
                        account.account.integration.as_str() == provider
                            && account.account.account_id.as_str() == account_id
                    })
                    .is_some_and(|account| {
                        account.status == broscribe_integrations::AccountStatus::ReauthRequired
                            || account.access_error.is_some()
                    });
                if !keep {
                    self.bus
                        .publish(Event::NotificationDismissed { id: id.clone() });
                }
                return keep;
            }
            let Ok(integration) = broscribe_integrations::IntegrationId::parse(integration) else {
                return true;
            };
            let path = broscribe_integrations::account_dir(&self.cfg.data_dir, integration)
                .join("state.json");
            let Ok(state) =
                broscribe_core::fsutil::read_json::<broscribe_integrations::AccountState>(&path)
            else {
                return true;
            };
            let keep = state.status == broscribe_integrations::AccountStatus::ReauthRequired;
            if !keep {
                self.bus
                    .publish(Event::NotificationDismissed { id: id.clone() });
            }
            keep
        });
    }

    /// Publishes a notification. Only actionable ones (meeting opens,
    /// integration re-auth; asks are kept by `ask_to_record`) stay in
    /// `pending_notifications`; action-less notices with no meeting
    /// ("Recording stopped", failures) are fire-and-forget, and the app
    /// keeps them until the user dismisses them.
    pub fn notify(&self, n: NotificationRequest) {
        if let Some(session) = n.id.strip_prefix("failed-") {
            // Retry changes the stopped state before withdrawing the notice.
            // Serialize publishing with that withdrawal and check the state
            // again in case retry won while we built the notice.
            let mut pending = lock(&self.pending);
            let still_failed = lock(&self.stopped).iter().any(|entry| {
                entry.session_id == session
                    && matches!(entry.state, StoppedRecordingState::Failed { .. })
            });
            if !still_failed || pending.contains_key(&n.id) {
                return;
            }
            pending.insert(
                n.id.clone(),
                Pending::FailedRecording(n.clone(), session.to_owned()),
            );
            self.bus.publish(Event::Notification(n));
            drop(pending);
            self.launch_accessory();
            return;
        }
        if n.meeting_id.is_some() {
            lock(&self.pending).insert(n.id.clone(), Pending::OpenMeeting(n.clone()));
        }
        if let Some(id) = n.id.strip_prefix("integration-reauth-") {
            let target = broscribe_integrations::IntegrationId::parse(id)
                .ok()
                .filter(|id| {
                    id.is_native_calendar() || *id == broscribe_integrations::IntegrationId::Google
                })
                .map(|id| id.as_str().to_owned())
                .or_else(|| {
                    let (provider, account_id) = id.rsplit_once('-')?;
                    let integration =
                        broscribe_integrations::IntegrationId::parse(provider).ok()?;
                    if !matches!(
                        integration,
                        broscribe_integrations::IntegrationId::MicrosoftGraph
                    ) {
                        return None;
                    }
                    let account_id = broscribe_core::calendar::AccountId::parse(account_id).ok()?;
                    Some(format!("{provider}/{account_id}"))
                });
            if let Some(target) = target {
                lock(&self.pending)
                    .insert(n.id.clone(), Pending::OpenIntegration(n.clone(), target));
            }
        }
        if !n.actions.is_empty() || n.meeting_id.is_some() {
            self.launch_accessory();
        }
        self.bus.publish(Event::Notification(n));
    }
}

/// Handle to a running daemon (cheap to clone).
#[derive(Clone)]
pub struct Daemon {
    pub(crate) shared: Arc<Shared>,
}

impl Daemon {
    /// Opens the library; nothing runs until [`Daemon::serve`].
    pub fn new(
        mut cfg: DaemonConfig,
        engine: Arc<dyn CaptureEngine>,
        probe: Arc<dyn ProcessProbe>,
        admission: Arc<broscribe_core::maintenance::ModeLease>,
    ) -> broscribe_core::Result<Self> {
        cfg.library = std::path::absolute(&cfg.library)?;
        cfg.data_dir = std::path::absolute(&cfg.data_dir)?;
        cfg.worker.data_dir_override = cfg.data_dir_overridden.then(|| cfg.data_dir.clone());
        admission.require_normal()?;
        if admission.data_dir() != cfg.data_dir.as_path() {
            return Err(Error::InvalidInput(
                "daemon admission names another data directory".into(),
            ));
        }
        Library::open(&cfg.library)?;
        let root = cfg
            .library
            .canonicalize()
            .map_err(|e| Error::io_at(&cfg.library, e))?;
        let lib = Library::open_existing(root)?.with_mode_lease(Arc::clone(&admission))?;
        // A confirmed install admission outlives the restart its replacement
        // caused: no capture opens until the updater releases it.
        let install_state = Admission::load(&cfg.data_dir);
        let bus = EventBus::new(cfg.subscriber_capacity);
        let daemon = Self {
            shared: Arc::new(Shared {
                cfg,
                lib,
                admission,
                bus,
                probe,
                engine,
                requests: Mutex::new(RequestLog::default()),
                session: Mutex::new(None),
                jobs: Mutex::new(JobsState::default()),
                preview: Mutex::new(PreviewSlot::default()),
                pending: Mutex::new(HashMap::new()),
                detector: Mutex::new(None),
                kick: Notify::new(),
                integrations: Mutex::new(None),
                integration_kick: Notify::new(),
                audio_check: Mutex::new(Arc::new(crate::audio_check::DeviceAudioCheck {
                    mic: Arc::from(broscribe_platform_native::NativePlatform::mic_capture()),
                    devices: Arc::from(broscribe_platform_native::NativePlatform::device_info()),
                    player: Arc::new(broscribe_platform_native::NativePlatform::audio_player),
                })),
                checking_audio: AtomicBool::new(false),
                devices: Mutex::new(Arc::from(
                    broscribe_platform_native::NativePlatform::device_info(),
                )),
                capture_access: Mutex::new(CaptureAccess::default()),
                sample_mic: Mutex::new(Arc::from(
                    broscribe_platform_native::NativePlatform::mic_capture(),
                )),
                reader_monitor: Mutex::new(Arc::from(
                    broscribe_platform_native::NativePlatform::mic_usage_monitor(),
                )),
                sample: Mutex::new(None),
                sample_cancelled: Mutex::new(std::collections::VecDeque::new()),
                permission_results: Mutex::new(HashMap::new()),
                accessory_running: Arc::new(AtomicBool::new(false)),
                accessory_attempt: Mutex::new(None),
                stopping: AtomicBool::new(false),
                shutdown: Shutdown::new(),
                stopped: Mutex::new(Vec::new()),
                saved: Mutex::new(VecDeque::new()),
                stopping_sessions: Mutex::new(HashSet::new()),
                reader_generations: AtomicU64::new(0),
                auto_stop_candidates: Mutex::new(Vec::new()),
                tag_mutations: Mutex::new(()),
                startup_pending: AtomicBool::new(true),
                startup: std::sync::Once::new(),
                starting: Mutex::new(None),
                install: Mutex::new(install_state),
                journal_issues: Mutex::new(Vec::new()),
                journal_ops: Mutex::new(()),
                rebuilt_stopped: Mutex::new(HashSet::new()),
                index: Mutex::new(IndexState::Checking),
                connections: AtomicU64::new(0),
                scheduler_task: Mutex::new(None),
                journal_sweep_at: Mutex::new(None),
                #[cfg(test)]
                stop_handover: Gate::default(),
                #[cfg(test)]
                next_session_id: Mutex::new(None),
            }),
        };
        daemon.shared.refresh_capture_access();
        Ok(daemon)
    }

    /// Startup maintenance, off the accept loop: finishes interrupted
    /// deletions and discards (a discarded journal must never be queued
    /// again), recovers crashed journals and queues their final jobs,
    /// rebuilds the stopped list, reclaims setup samples and repairs the
    /// index's pending marks under the library lock. No capture is admitted
    /// until it is done, so recovery never mistakes a just-started recording
    /// for an orphan. Blocking; [`Self::start_background`] runs it on a
    /// blocking thread. It runs once (startup assumes no capture is live);
    /// later calls wait for that run.
    pub fn finish_startup(&self) {
        let s = &self.shared;
        s.startup.call_once(|| {
            maintain_lifecycle(s, true);
            recover_sessions(s);
            rebuild_stopped(s);
            crate::sample::reclaim_after_restart(s);
            s.startup_pending.store(false, Ordering::Release);
            repair_index(s);
            s.publish_state();
            s.kick.notify_one();
        });
    }

    /// One periodic index pass ([`maintain_index`]), as the retention loop
    /// runs it. Blocking.
    pub fn maintain_index(&self) {
        maintain_index(&self.shared);
    }

    pub fn state(&self) -> DaemonState {
        self.shared.state()
    }

    /// Test seam for setup capture and permission/device facts.
    pub fn set_setup_devices(
        &self,
        devices: Arc<dyn broscribe_platform::DeviceInfo>,
        mic: Arc<dyn broscribe_platform::MicCapture>,
    ) {
        *lock(&self.shared.devices) = devices;
        *lock(&self.shared.sample_mic) = mic;
        self.shared.refresh_capture_access();
    }

    pub fn set_reader_monitor(&self, monitor: Arc<dyn broscribe_platform::MicUsageMonitor>) {
        *lock(&self.shared.reader_monitor) = monitor;
    }
    pub fn subscribe(&self) -> Subscriber {
        self.shared.bus.subscribe()
    }

    pub fn trigger_shutdown(&self) {
        self.shared.shutdown.trigger();
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shared.stopping.load(Ordering::Acquire) || self.shared.shutdown.is_triggered()
    }

    /// At agent start: opens the status app when
    /// `[app] open_at_login` is on. Like every accessory launch it does
    /// nothing while an app is subscribed; the app's instance lock stops a
    /// duplicate that raced it.
    pub fn launch_at_login(&self, config: &Config) {
        if config.open_at_login() {
            self.shared.launch_accessory_with_reason(true);
        }
    }

    /// Starts startup recovery (on a blocking thread, so the accept loop
    /// answers at once), the queue scheduler, retention schedule and
    /// detection poller.
    pub fn start_background(&self) {
        let startup = self.clone();
        tokio::task::spawn_blocking(move || startup.finish_startup());
        tokio::spawn(crate::detection::run(self.clone()));
        let s = Arc::clone(&self.shared);
        *lock(&self.shared.scheduler_task) = Some(tokio::spawn(scheduler::run(s)));
        let s = Arc::clone(&self.shared);
        tokio::spawn(retention_loop(s));
        tokio::spawn(crate::integrations::run(Arc::clone(&self.shared)));
        let reader_shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = reader_shared.shutdown.wait() => break,
                    () = tokio::time::sleep(Duration::from_secs(1)) => {
                        crate::readers::tick(&reader_shared);
                        if reader_shared.is_recording() {
                            let shared = Arc::clone(&reader_shared);
                            let _ = tokio::task::spawn_blocking(move || {
                                if shared.refresh_capture_access().1 {
                                    shared.publish_state();
                                }
                            }).await;
                        }
                    }
                }
            }
        });
    }

    /// Ends the active recording (queued for processing), cancels a start
    /// still opening its devices, waits for the scheduler to hand back an
    /// interrupted job without spending an attempt, and closes event
    /// streams; called once the accept loop stopped.
    pub async fn finish(&self) {
        cancel_start(&self.shared, "BroScribe Agent is stopping");
        if self.shared.is_recording()
            && let Err(e) = stop_recording(Arc::clone(&self.shared), None).await
        {
            tracing::warn!("stopping the recording at shutdown: {e}");
        }
        let scheduler = lock(&self.shared.scheduler_task).take();
        if let Some(scheduler) = scheduler {
            self.shared.shutdown.trigger();
            let _ = scheduler.await;
        }
        self.shared.bus.close();
    }

    pub(crate) fn expire_prompt(&self, app: &str) {
        let mut pending = lock(&self.shared.pending);
        pending.retain(|id, p| {
            let retain = match p {
                Pending::AskRecord { params, .. } => params.app.as_deref() != Some(app),
                Pending::OpenMeeting(_)
                | Pending::OpenIntegration(..)
                | Pending::FailedRecording(..) => true,
            };
            if !retain {
                self.shared
                    .bus
                    .publish(Event::NotificationDismissed { id: id.clone() });
            }
            retain
        });
    }

    /// What a new subscriber receives first: the state, pending prompts and
    /// the current reader incarnation (`start_reader`), which never resets a
    /// reader already running it.
    pub fn subscription_snapshot(&self) -> Vec<Event> {
        self.shared.reconcile_integration_prompts();
        let state = self.state();
        let notifications = state.pending_notifications.clone();
        let mut events = vec![Event::State(Box::new(state))];
        events.extend(notifications.into_iter().map(Event::Notification));
        if let Some(active) = lock(&self.shared.session).as_ref()
            && let Some(event) = crate::readers::event(active)
        {
            events.push(event);
        }
        events
    }

    /// What follows `event` on a subscription: after `lagged`, a fresh
    /// [`Self::subscription_snapshot`], so a subscriber that missed events
    /// (a reader stop, a new incarnation, a state change) resynchronizes at
    /// once; nothing after any other event.
    pub fn resync_after(&self, event: &Event) -> Vec<Event> {
        if matches!(event, Event::Lagged { .. }) {
            self.subscription_snapshot()
        } else {
            Vec::new()
        }
    }

    /// Asks the user whether to record (detection policy `ask`); the answer
    /// arrives as `notification.action` (`record` / `ignore`).
    pub fn ask_to_record(&self, params: RecordStartParams) -> String {
        let id = format!("ask-{}", params.app.as_deref().unwrap_or("unknown"));
        let app_name = meeting_app_name(&self.shared, &params);
        let notification = ask_notification(&id, &params, app_name.as_deref());
        let mut pending = lock(&self.shared.pending);
        if !pending.contains_key(&id) {
            pending.insert(
                id.clone(),
                Pending::AskRecord {
                    params,
                    notification: notification.clone(),
                    created: Instant::now(),
                },
            );
            self.shared.bus.publish(Event::Notification(notification));
            drop(pending);
            self.shared.launch_accessory();
        }
        id
    }

    /// A client connection: install admissions it acquires are bound to it
    /// and end, unless confirmed, when it is dropped ([`ClientConnection`]).
    pub fn connect(&self) -> ClientConnection {
        ClientConnection {
            daemon: self.clone(),
            id: ConnectionId(self.shared.connections.fetch_add(1, Ordering::AcqRel) + 1),
        }
    }

    /// Answers one request outside any connection (in-process callers):
    /// `install.acquire` needs a connection to bind to and is refused.
    pub async fn handle(&self, req: Request) -> Response {
        self.handle_on(None, req).await
    }

    /// Answers one request. Mutating requests are remembered by ID: a
    /// repeat returns the first outcome without running it again.
    async fn handle_on(&self, connection: Option<ConnectionId>, req: Request) -> Response {
        let Request { id, call } = req;
        if call
            .reconciliation_id()
            .is_some_and(|request_id| request_id != &id)
        {
            return Response {
                id,
                outcome: Err(IpcError::new(
                    IpcError::INVALID_REQUEST,
                    "reconciliation request ids do not match",
                )),
            };
        }
        // Install admission is fenced by its fence and holder connection.
        if call.is_install_admission() {
            let s = Arc::clone(&self.shared);
            let outcome = blocking(move || crate::daemon::install(&s, connection, &call)).await;
            return Response { id, outcome };
        }
        // Reader batches arrive every two seconds per reader and are fenced
        // by session, reader generation and sequence instead, so they never
        // crowd user actions out of the reconciliation log. `reader.attach`
        // (once per reader start) is logged: a host that lost its answer
        // asks `request.status` for the generation it was issued.
        if !call.is_mutating() || matches!(call, Call::ReaderEvents { .. }) {
            return Response {
                id,
                outcome: self.execute(call).await,
            };
        }
        match lock(&self.shared.requests).begin(&id) {
            Begin::Fresh => {}
            Begin::Seen(state) => {
                let outcome = match state {
                    RequestState::Done { result } => Ok(result),
                    RequestState::Failed { error } => Err(error),
                    RequestState::Running => Err(IpcError::new(
                        IpcError::IN_PROGRESS,
                        format!("request {id} is still running"),
                    )),
                    RequestState::Unknown => {
                        Err(IpcError::new("internal", "request log lost the request"))
                    }
                };
                return Response { id, outcome };
            }
        }
        let outcome = if self.is_shutting_down() {
            Err(IpcError::new(
                IpcError::SHUTTING_DOWN,
                "the daemon is stopping",
            ))
        } else {
            self.execute(call).await
        };
        lock(&self.shared.requests).finish(&id, &outcome);
        Response { id, outcome }
    }

    async fn execute(&self, call: Call) -> Result<Value, IpcError> {
        let s = Arc::clone(&self.shared);
        match call {
            Call::Ping => to_value(&AgentPing::normal(
                s.cfg.build.as_ref(),
                s.startup_pending.load(Ordering::Acquire),
            )),
            Call::Status => to_value(&s.state()),
            Call::RequestStatus { request_id } => to_value(&lock(&s.requests).state(&request_id)),
            Call::Subscribe => Ok(json!({ "subscribed": true })),
            Call::AppShow => Ok(json!({
                "subscribers": s.bus.publish(Event::ShowMainWindow),
            })),
            Call::Shutdown | Call::ShutdownIfIdle => {
                if matches!(call, Call::ShutdownIfIdle) {
                    let shared = Arc::clone(&s);
                    blocking(move || admit_idle_shutdown(&shared)).await?;
                } else {
                    s.stopping.store(true, Ordering::Release);
                    cancel_start(&s, "BroScribe Agent is stopping");
                }
                // Capture is stopped and durable before the CLI may unload
                // the login item. Offline processing remains queued.
                let stopped = if s.is_recording() {
                    stop_recording(s.clone(), None).await
                } else {
                    Ok(Value::Null)
                };
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    s.shutdown.trigger();
                });
                stopped?;
                Ok(json!({ "stopping": true }))
            }
            Call::RecordStart(params) => {
                let rt = tokio::runtime::Handle::current();
                blocking(move || start_recording(&s, params, rt, Admit::Always)).await
            }
            Call::RecordStop { session_id } => stop_recording(s, Some(session_id)).await,
            Call::RecordStatus => {
                let state = s.state();
                Ok(json!({ "recording": state.recording }))
            }
            Call::RecordPause { session_id } => {
                blocking(move || {
                    with_session_id(&s, &session_id, |h| {
                        h.pause()
                            .map(|()| json!({"session_id": h.session_id(), "paused": true}))
                    })
                })
                .await
            }
            Call::RecordResume { session_id } => {
                blocking(move || {
                    with_session_id(&s, &session_id, |h| {
                        h.resume()
                            .map(|()| json!({"session_id": h.session_id(), "paused": false}))
                    })
                })
                .await
            }
            Call::RecordLanguage {
                session_id,
                language,
            } => {
                let language = language.trim().to_owned();
                if !valid_language(&language) {
                    return Err(invalid_language());
                }
                blocking(move || {
                    with_active_session(&s, &session_id, |active| {
                        let at_s = active.handle.set_language(&language)?;
                        active
                            .live_queue
                            .set_language(live::sample_at(at_s), &language);
                        Ok(json!({ "session_id": active.handle.session_id(), "language": language, "at_s": at_s }))
                    })
                })
                .await
            }
            Call::RecordBookmark { session_id, label } => {
                blocking(move || {
                    with_session_id(&s, &session_id, |h| {
                        let at_s = h.bookmark(label.as_deref())?;
                        Ok(json!({ "session_id": h.session_id(), "at_s": at_s, "label": label }))
                    })
                })
                .await
            }
            Call::RecordNote { session_id, text } => {
                if text.trim().is_empty() {
                    return Err(IpcError::from_core(&Error::InvalidInput(
                        "empty note".into(),
                    )));
                }
                blocking(move || {
                    with_session_id(&s, &session_id, |h| {
                        let at_s = h.note(&text)?;
                        Ok(json!({ "session_id": h.session_id(), "at_s": at_s }))
                    })
                })
                .await
            }
            Call::RecordTag { tags, session_id } => {
                blocking(move || record_tag(&s, &session_id, &tags)).await
            }
            Call::RecordTitle {
                session_id, title, ..
            } => blocking(move || record_title(&s, &session_id, &title)).await,
            Call::RecordTitleRefresh { session_id } => {
                blocking(move || refresh_session_title(&s, &session_id)).await
            }
            Call::RecordDiscard { session_id, .. } => {
                let out = blocking({
                    let s = Arc::clone(&s);
                    move || discard_recording(&s, &session_id)
                })
                .await;
                s.kick.notify_one();
                out
            }
            Call::RecordProcessNow { session_id } => {
                blocking(move || process_now(&s, &session_id)).await
            }
            Call::RecordRetry { session_id, .. } => {
                let out = blocking({
                    let s = Arc::clone(&s);
                    move || retry_recording(&s, &session_id)
                })
                .await;
                s.kick.notify_one();
                out
            }
            Call::RecordResolveJournal {
                session_id,
                resolution,
                library_root,
                confirmed,
                ..
            } => {
                blocking(move || {
                    resolve_legacy_journal(&s, &session_id, resolution, &library_root, confirmed)
                })
                .await
            }
            call @ (Call::InstallAcquire
            | Call::ShutdownAcquire
            | Call::LibrarySwitchAcquire
            | Call::InstallConfirm { .. }
            | Call::InstallRelease { .. }) => blocking(move || install(&s, None, &call)).await,
            Call::ReaderAttach {
                session_id,
                reader_generation,
            } => blocking(move || crate::readers::attach(&s, &session_id, reader_generation)).await,
            Call::ReaderRefused {
                session_id,
                app_id,
                pid,
                reader_generation,
                refusal,
            } => {
                blocking(move || {
                    crate::readers::refused(
                        &s,
                        &session_id,
                        (app_id.as_str(), pid),
                        reader_generation,
                        refusal,
                    )
                })
                .await
            }
            Call::ReaderEvents { session_id, batch } => {
                blocking(move || crate::readers::append(&s, &session_id, batch)).await
            }
            Call::RecordParticipant {
                session_id,
                person_ref,
                action,
                ..
            } => blocking(move || record_participant(&s, &session_id, &person_ref, action)).await,
            Call::RecordLive { session_id } => {
                let preview = lock(&s.session)
                    .as_ref()
                    .map(|a| Arc::clone(&a.live_preview));
                crate::live_identity::snapshot_for(preview.as_deref(), &session_id)
            }
            Call::RecordLiveAttribute(params) => {
                blocking(move || record_live_attribute(&s, params)).await
            }
            Call::RecordAutoStop(params) => blocking(move || record_auto_stop(&s, params)).await,
            Call::KickQueue => {
                blocking(move || {
                    let ids = scheduler::pending(&s).map_err(IpcError::from)?;
                    s.kick.notify_one();
                    Ok(json!({ "job_ids": ids }))
                })
                .await
            }
            Call::DetectStatus => crate::detection::status(&s),
            Call::DetectReload => self.reload_detection().await,
            Call::AudioCheck { seconds } => {
                blocking(move || crate::audio_check::run(&s, seconds)).await
            }
            Call::CaptureSample {
                session,
                seconds,
                purpose,
            } => blocking(move || crate::sample::capture(&s, &session, seconds, &purpose)).await,
            Call::CaptureSampleStop { session, discard } => {
                crate::sample::stop(&s, &session, discard)
            }
            Call::PermissionsStatus => blocking(move || crate::sample::permissions(&s, None)).await,
            Call::PermissionsRequest { permission } => {
                blocking(move || crate::sample::permissions(&s, Some(&permission))).await
            }
            Call::IntegrationsSync { integration } => {
                blocking(move || {
                    let service = lock(&s.integrations).clone().ok_or_else(|| {
                        IpcError::new(
                            "not_configured",
                            "integration maintenance is not configured",
                        )
                    })?;
                    let results = service.sync(integration.as_deref(), true)?;
                    s.reconcile_integration_prompts();
                    Ok(json!({"synced": results}))
                })
                .await
            }
            Call::IntegrationsReload => {
                blocking(move || {
                    let service = lock(&s.integrations).clone().ok_or_else(|| {
                        IpcError::new(
                            "not_configured",
                            "integration maintenance is not configured",
                        )
                    })?;
                    service.reload()?;
                    s.reconcile_integration_prompts();
                    s.integration_kick.notify_one();
                    Ok(json!({"reloaded":true}))
                })
                .await
            }
            // The macOS prompt can wait up to two minutes: on a blocking thread, holding no
            // capture, session or library lock and never the capture admission slot, so
            // recording starts and continues meanwhile. The service single-flights prompts.
            Call::SystemCalendar { action } => {
                native_calendar(s, CalendarProvider::EventKit, action).await
            }
            // Only the live generation (never calendar labels or events) for snapshot consumers.
            Call::SystemCalendarAuthority => native_authority(s, CalendarProvider::EventKit).await,
            // The Google account and calendar-selection generations together,
            // read-only: priors stamped with another (or no) pair fail closed.
            Call::GoogleCalendarAuthority => {
                blocking(move || {
                    let authority = integration_service(&s)?.google_authority()?;
                    Ok(json!({ "authority": authority }))
                })
                .await
            }
            Call::MicrosoftCalendarAuthority { account } => {
                blocking(move || {
                    let authority = integration_service(&s)?.remote_authority(&account)?;
                    Ok(json!({ "authority": authority }))
                })
                .await
            }
            Call::QueueEnqueue { kind, key, input } => {
                let out = blocking({
                    let s = Arc::clone(&s);
                    move || {
                        // Record jobs go through the discard fence like every
                        // other route that queues them.
                        if kind == RECORD_FINAL_KIND || kind == RECORD_PROVISIONAL_KIND {
                            let record: RecordJobInput = serde_json::from_value(input)
                                .map_err(|e| IpcError::new("invalid_input", e.to_string()))?;
                            return match enqueue_record(&s, &kind, &key, &record)
                                .map_err(IpcError::from)?
                            {
                                Some(id) => Ok(json!({ "job_id": id })),
                                None => Err(IpcError::new(
                                    lifecycle::RECORDING_DISCARDED,
                                    "the recording was discarded",
                                )),
                            };
                        }
                        writes::enqueue(&s.lib, &kind, &key, input)
                            .map(|id| json!({ "job_id": id }))
                            .map_err(IpcError::from)
                    }
                })
                .await;
                s.kick.notify_one();
                out
            }
            Call::MeetingTag {
                meeting_id,
                add,
                remove,
            } => {
                write(s, move |lib, _| {
                    writes::meeting_tag(lib, &meeting_id, &add, &remove)
                })
                .await
            }
            Call::SpeakerName {
                meeting_id,
                speaker_id,
                person,
            } => {
                write(s, move |lib, config| {
                    writes::speaker_name(lib, config, &meeting_id, &speaker_id, &person)
                })
                .await
            }
            Call::SpeakerReject {
                meeting_id,
                speaker_id,
                person,
            } => {
                write(s, move |lib, config| {
                    writes::speaker_reject(lib, config, &meeting_id, &speaker_id, person.as_deref())
                })
                .await
            }
            Call::PersonForget { person, voice_only } => {
                let data_dir = s.cfg.data_dir.clone();
                write(s, move |lib, _| {
                    writes::person_forget(lib, &data_dir, &person, voice_only)
                })
                .await
            }
            Call::RetentionApply { meetings } => {
                let s2 = Arc::clone(&s);
                tokio::spawn(async move { run_retention(&s2, meetings).await });
                Ok(json!({ "started": true }))
            }
            Call::NotificationAction {
                notification_id,
                action,
            } => {
                let pending = lock(&s.pending).remove(&notification_id);
                match (pending, action.as_str()) {
                    (None, _) => Err(IpcError::from_core(&Error::NotFound(format!(
                        "notification {notification_id}"
                    )))),
                    (Some(Pending::OpenMeeting(notification)), "open") => {
                        blocking(move || {
                            let id = notification.meeting_id.ok_or_else(|| {
                                IpcError::new("invalid_input", "notification has no meeting")
                            })?;
                            let meeting = s
                                .lib
                                .load_meeting(
                                    &broscribe_core::MeetingId::parse(&id)
                                        .map_err(IpcError::from)?,
                                )
                                .map_err(IpcError::from)?;
                            s.bus.publish(Event::NotificationDismissed {
                                id: notification_id,
                            });
                            Ok(json!({"meeting_id":meeting.id}))
                        })
                        .await
                    }
                    (Some(Pending::OpenMeeting(_)), "dismiss") => {
                        s.bus.publish(Event::NotificationDismissed {
                            id: notification_id,
                        });
                        Ok(json!({"dismissed": true}))
                    }
                    (Some(Pending::OpenIntegration(_, integration)), "open") => {
                        s.bus.publish(Event::NotificationDismissed {
                            id: notification_id,
                        });
                        Ok(json!({"integration":integration}))
                    }
                    (Some(Pending::FailedRecording(_, session)), "open") => {
                        s.bus.publish(Event::NotificationDismissed {
                            id: notification_id,
                        });
                        Ok(json!({"recording_session": session}))
                    }
                    (Some(Pending::AskRecord { params, .. }), "record") => {
                        s.bus.publish(Event::NotificationDismissed {
                            id: notification_id,
                        });
                        if let (Some(d), Some(app)) =
                            (lock(&s.detector).as_mut(), params.app.as_deref())
                        {
                            d.answer(app, broscribe_detect::Answer::Record);
                        }
                        let rt = tokio::runtime::Handle::current();
                        let attempt = params.clone();
                        match blocking(move || start_recording(&s, attempt, rt, Admit::Answered))
                            .await
                        {
                            // A setup capture or permission prompt holds the
                            // start: wait it out as a detected start does.
                            Err(error) if error.code == "busy" => {
                                let d = self.clone();
                                tokio::spawn(async move {
                                    d.auto_record(params, AutoStart::Answered).await
                                });
                                Ok(json!({ "waiting": true }))
                            }
                            outcome => outcome,
                        }
                    }
                    (Some(Pending::AskRecord { params, .. }), "ignore") => {
                        s.bus.publish(Event::NotificationDismissed {
                            id: notification_id,
                        });
                        if let (Some(d), Some(app)) =
                            (lock(&s.detector).as_mut(), params.app.as_deref())
                        {
                            d.answer(app, broscribe_detect::Answer::Ignore);
                        }
                        Ok(json!({ "ignored": true }))
                    }
                    (Some(p), other) => {
                        lock(&s.pending).insert(notification_id, p);
                        Err(IpcError::from_core(&Error::InvalidInput(format!(
                            "unknown notification action {other:?}"
                        ))))
                    }
                }
            }
        }
    }
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, IpcError> {
    serde_json::to_value(v).map_err(|e| IpcError::new("internal", format!("serialising: {e}")))
}

/// The installed maintenance service; cloned so its lock is never held during the call.
fn integration_service(
    s: &Shared,
) -> Result<Arc<dyn crate::integrations::IntegrationMaintenance>, IpcError> {
    lock(&s.integrations).clone().ok_or_else(|| {
        IpcError::new(
            "not_configured",
            "integration maintenance is not configured",
        )
    })
}

/// Runs `f` on a blocking thread.
async fn blocking<F, T>(f: F) -> Result<T, IpcError>
where
    F: FnOnce() -> Result<T, IpcError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(IpcError::new("internal", format!("task failed: {e}"))))
}

/// A library write on a blocking thread, then retention for the meetings it
/// touched (a child process, not awaited) and a queue re-scan.
async fn write<F>(s: Arc<Shared>, f: F) -> Result<Value, IpcError>
where
    F: FnOnce(&Library, &Config) -> broscribe_core::Result<writes::WriteOutcome> + Send + 'static,
{
    let s2 = Arc::clone(&s);
    let out = tokio::task::spawn_blocking(move || {
        let config = Config::load(&s2.cfg.data_dir)?;
        f(&s2.lib, &config)
    })
    .await
    .map_err(|e| IpcError::new("internal", format!("task failed: {e}")))?
    .map_err(IpcError::from)?;
    if !out.touched.is_empty() {
        let ids = out.touched.iter().map(ToString::to_string).collect();
        let s3 = Arc::clone(&s);
        tokio::spawn(async move { run_retention(&s3, Some(ids)).await });
    }
    s.kick.notify_one();
    Ok(out.value)
}

/// Runs `f` on the session handle only while `session_id` is the active
/// recording, checked under the session mutex that `f` then runs under:
/// another session, or none running, is `stale_session` and nothing runs.
fn with_session_id(
    s: &Shared,
    session_id: &str,
    f: impl FnOnce(&mut dyn SessionHandle) -> broscribe_core::Result<Value>,
) -> Result<Value, IpcError> {
    with_active_session(s, session_id, |active| f(active.handle.as_mut()))
}

/// [`with_session_id`] with the whole [`Active`] (language reaches the
/// live queue too). A status change (pause, resume) starts a new reader
/// incarnation and clears automatic-stop evidence.
fn with_active_session(
    s: &Shared,
    session_id: &str,
    f: impl FnOnce(&mut Active) -> broscribe_core::Result<Value>,
) -> Result<Value, IpcError> {
    let out = {
        let mut session = lock(&s.session);
        let active = session
            .as_mut()
            .filter(|a| a.handle.session_id() == session_id)
            .ok_or_else(|| IpcError::from(lifecycle::stale_session(session_id)))?;
        let before = active.handle.status().status;
        let out = f(active).map_err(IpcError::from);
        if before != active.handle.status().status {
            // Pause and resume: a new reader incarnation, and no evidence
            // carries across (paused preview absence never counts as an end).
            crate::readers::new_incarnation(s, active);
            active.auto_stop.reset_evidence();
            crate::readers::lifecycle(s, active);
        }
        out
    };
    s.publish_state();
    out
}

/// `record.tag` of `session_id`: its journal, then its "Process now"
/// meeting, as one serialized mutation, so concurrent changes reach both in
/// the order the journal accepted them (a removal is never overtaken by an
/// earlier add). Lock order: tag mutations → session mutex (released before
/// the next) → library lock. Nothing holding the session or library lock
/// waits for the tag lock, and capture never takes it.
fn record_tag(s: &Arc<Shared>, session_id: &str, tags: &[String]) -> Result<Value, IpcError> {
    let (add, remove) = writes::tag_changes(tags).map_err(IpcError::from)?;
    let _serial = lock(&s.tag_mutations);
    let out = with_session_id(s, session_id, |h| {
        h.add_tags(&add)?;
        h.remove_tags(&remove)?;
        Ok(json!({
            "session_id": h.session_id(),
            "tags": h.status().tags,
            "added": add,
            "removed": remove,
        }))
    })?;
    // The final pass keeps the provisional meeting's tags and removals, so
    // a removed tag is never resurrected.
    lifecycle::apply_session_tags(&s.lib, &s.cfg.journal_root(), session_id, &add, &remove)
        .map_err(|e| {
            IpcError::new(
                e.code(),
                format!(
                    "the recording's tags changed, but its provisional meeting could not be \
                     updated; try again: {e}"
                ),
            )
        })?;
    Ok(out)
}

async fn native_calendar(
    s: Arc<Shared>,
    provider: CalendarProvider,
    action: SystemCalendarAction,
) -> Result<Value, IpcError> {
    let changes = action != SystemCalendarAction::Status;
    let out = blocking({
        let s = Arc::clone(&s);
        move || to_value(&integration_service(&s)?.native_calendar(provider, action)?)
    })
    .await;
    if changes && out.is_ok() {
        // Sync a new selection now, and re-snapshot a recording without
        // disconnected calendars.
        s.integration_kick.notify_one();
    }
    out
}

async fn native_authority(s: Arc<Shared>, provider: CalendarProvider) -> Result<Value, IpcError> {
    blocking(move || {
        let generation = integration_service(&s)?.native_generation(provider)?;
        Ok(json!({ "generation": generation }))
    })
    .await
}

// ---- participants, titles, discard ----

/// `record_participant`: confirms or removes attendance of a person in the
/// active session. Confirming a typed name nobody has creates a name-only
/// person (never a voiceprint). The library lock is taken first, then the
/// session mutex: the session is checked before anything is created (a
/// stale session creates nothing) and cannot be stopped until the short
/// person-file and journal writes are done. A person created when the
/// journal write then fails is kept and reported as `partial`.
fn record_participant(
    s: &Arc<Shared>,
    session_id: &str,
    person_ref: &str,
    action: ParticipantAction,
) -> Result<Value, IpcError> {
    let confirm = action == ParticipantAction::Confirm;
    let out = broscribe_core::ops::with_index(&s.lib, |tx, index| {
        let mut session = lock(&s.session);
        let Some(active) = session
            .as_mut()
            .filter(|a| a.handle.session_id() == session_id)
        else {
            return Err(lifecycle::stale_session(session_id));
        };
        let (person, created) = if confirm {
            broscribe_core::ops::resolve_or_new_person(tx.library(), person_ref)?
        } else {
            let person = broscribe_core::ops::find_person(tx.library(), person_ref)?
                .ok_or_else(|| Error::NotFound("person".into()))?;
            (person, false)
        };
        let partial = |error: &Error| {
            json!({
                "status": "partial",
                "person_id": person.id,
                "created": true,
                "attendance": false,
                "message": format!(
                    "the person was created, but attendance was not recorded ({})",
                    error.code()
                ),
            })
        };
        if created {
            // Persisted before the journal can name it: never a dangling participant.
            tx.save_person(&person)?;
            if let Err(error) = index.upsert_person(tx, &person) {
                return Ok(partial(&error));
            }
        }
        match active
            .handle
            .participant(person.id.as_str(), &person.name, confirm)
        {
            Ok(()) => Ok(json!({
                "status": if confirm { "confirmed" } else { "removed" },
                "person_id": person.id,
                "created": created,
            })),
            Err(error) if created => Ok(partial(&error)),
            Err(error) => Err(error),
        }
    });
    s.publish_state();
    out.map_err(IpcError::from)
}

/// `record.live_attribute`: a decision on one live turn of the active
/// session. As [`record_participant`]: the library lock first, then the
/// session mutex, held until the decision is saved (Stop waits; a stale
/// session creates nothing).
fn record_live_attribute(
    s: &Arc<Shared>,
    params: broscribe_ipc::live::LiveAttributeParams,
) -> Result<Value, IpcError> {
    let session_id = params.session_id.clone();
    let preview = lock(&s.session)
        .as_ref()
        .filter(|a| a.handle.session_id() == session_id)
        .map(|a| Arc::clone(&a.live_preview))
        .ok_or_else(|| IpcError::from(lifecycle::stale_session(&session_id)))?;
    crate::live_identity::attribute(&s.lib, &s.cfg.journal_root(), &preview, params, || {
        let session = lock(&s.session);
        if session.as_ref().is_some_and(|a| {
            a.handle.session_id() == session_id && Arc::ptr_eq(&a.live_preview, &preview)
        }) {
            Ok(session)
        } else {
            Err(lifecycle::stale_session(&session_id))
        }
    })
}

/// `record.auto_stop`: enables, disables or links automatic stop, fenced by
/// session and binding generation under the session mutex; a link takes
/// only a call detected at the last poll and resolves its reader anew.
fn record_auto_stop(s: &Arc<Shared>, params: RecordAutoStopParams) -> Result<Value, IpcError> {
    let candidates = lock(&s.auto_stop_candidates).clone();
    let status = {
        let mut session = lock(&s.session);
        let Some(active) = session
            .as_mut()
            .filter(|a| a.handle.session_id() == params.session_id)
        else {
            return Err(IpcError::from(lifecycle::stale_session(&params.session_id)));
        };
        if active.auto_stop.generation() != params.expected_generation {
            return Err(IpcError::new(
                crate::autostop::STALE_BINDING,
                "automatic stop changed meanwhile; read it again",
            ));
        }
        let link = match &params.app_id {
            None => None,
            Some(app_id) => Some(
                candidates
                    .iter()
                    .find(|c| &c.app_id == app_id)
                    .cloned()
                    .ok_or_else(|| {
                        IpcError::new(
                            "invalid_input",
                            format!("{app_id} is not a meeting detected on the microphone now"),
                        )
                    })?,
            ),
        };
        let relinked = active
            .auto_stop
            .configure(params.enabled, link)
            .map_err(|message| IpcError::new("invalid_input", message))?;
        if relinked {
            // The linked call's reader replaces any earlier one; without one
            // the status reports the microphone fallback only.
            let app = active.auto_stop.app().map(|a| a.app_id.clone());
            active.reader = crate::readers::resolve_for(s, app.as_deref());
            crate::readers::new_incarnation(s, active);
            if active.reader.is_none() {
                s.bus.publish(Event::StopReader {
                    session_id: params.session_id.clone(),
                });
            }
            crate::readers::lifecycle(s, active);
        }
        active.auto_stop.status(&candidates)
    };
    s.publish_state();
    to_value(&status)
}

/// Feeds the last detector poll to the running recording's evaluator and
/// returns an end decision whose grace has run out. Session mutex first,
/// then the detector (the order admission and reloads use).
pub(crate) fn evaluate_auto_stop(s: &Shared) -> Option<crate::autostop::StopDecision> {
    let (decision, changed) = {
        let mut session = lock(&s.session);
        let active = session.as_mut()?;
        let app = active.auto_stop.watched_app().map(str::to_owned);
        let paused = active.handle.status().status == RecordingStatus::Paused;
        let (at, sighting, timing) = {
            let detector = lock(&s.detector);
            let detector = detector.as_ref()?;
            let at = detector.last_observation()?.at;
            let sighting = app.as_deref().and_then(|app| detector.sighting(app));
            (at, sighting, detector.rules().timing())
        };
        let before = active.auto_stop.status(&[]);
        let decided = active.auto_stop.poll(at, sighting.as_ref(), timing, paused);
        let changed = before != active.auto_stop.status(&[]);
        let decision = decided.map(
            |(reason, evidence, mic_held)| crate::autostop::StopDecision {
                session_id: active.handle.session_id().to_owned(),
                generation: active.auto_stop.generation(),
                evidence,
                reason,
                mic_held,
                app_name: active
                    .auto_stop
                    .app()
                    .map(|a| a.app_name.clone())
                    .unwrap_or_default(),
            },
        );
        (decision, changed)
    };
    if changed {
        s.publish_state();
    }
    decision
}

/// A failed detector poll: the running recording's evidence, timers and
/// grace are cleared, and no call is linkable until a poll succeeds.
pub(crate) fn auto_stop_poll_failed(s: &Shared) {
    lock(&s.auto_stop_candidates).clear();
    let changed = lock(&s.session).as_mut().is_some_and(|active| {
        let before = active.auto_stop.status(&[]);
        active.auto_stop.poll_failed();
        before != active.auto_stop.status(&[])
    });
    if changed {
        s.publish_state();
    }
}

impl Daemon {
    /// Carries out an automatic end decision through the normal Stop and
    /// save, only while it is still current under the session mutex that
    /// takes the recording. A superseded decision does nothing; a failed
    /// stop is reported, never shown as saved.
    pub(crate) async fn auto_stop(&self, decision: crate::autostop::StopDecision) {
        let s = Arc::clone(&self.shared);
        let fenced = decision.clone();
        let outcome = blocking(
            move || match stop_selected(&s, Select::Decision(&fenced), false) {
                None => Ok(Value::Null),
                Some(outcome) => {
                    resume_detections(&s);
                    outcome
                }
            },
        )
        .await;
        match outcome {
            Ok(Value::Null) => {}
            Ok(_) => self.shared.notify(crate::autostop::saved_notice(&decision)),
            Err(error) => {
                tracing::warn!(code = %error.code, "automatic stop did not finish");
                self.shared
                    .notify(automatic_stop_failed_notice(&decision.session_id));
            }
        }
    }
}

/// `record.title`: the active session or a stopped one not saved yet.
fn record_title(s: &Arc<Shared>, session_id: &str, title: &str) -> Result<Value, IpcError> {
    let outcome =
        lifecycle::set_session_title(&s.lib, &s.cfg.journal_root(), session_id, title, || {
            // Under the library lock (library → session → stopped list).
            let session = lock(&s.session);
            let active = session
                .as_ref()
                .is_some_and(|a| a.handle.session_id() == session_id);
            let stopped = lock(&s.stopped)
                .iter()
                .any(|r| r.session_id == session_id && r.state.is_pending_save());
            if active || stopped {
                Ok(())
            } else {
                Err(lifecycle::stale_session(session_id))
            }
        })
        .map_err(IpcError::from)?;
    if matches!(outcome, SessionTitleOutcome::Saved { .. }) {
        // The title is saved; a session that ended meanwhile just has no
        // panel to update.
        let _ = refresh_session_title(s, session_id);
    }
    to_value(&outcome)
}

/// Shows the session's latest title from its sidecar, the authoritative
/// latest edit. Read under the session mutex, so concurrent edits leave the
/// shown title at the newest one whatever order their refreshes run in.
fn refresh_session_title(s: &Shared, session_id: &str) -> Result<Value, IpcError> {
    let dir = lifecycle::session_dir(&s.cfg.journal_root(), session_id).map_err(IpcError::from)?;
    let title = {
        let mut session = lock(&s.session);
        let latest = lifecycle::read_session_title(&dir)
            .map_err(IpcError::from)?
            .map(|t| t.title);
        let mut known = false;
        if let Some(active) = session
            .as_mut()
            .filter(|a| a.handle.session_id() == session_id)
        {
            if latest.is_some() {
                active.title.clone_from(&latest);
            }
            known = true;
        }
        if let Some(stopped) = lock(&s.stopped)
            .iter_mut()
            .find(|r| r.session_id == session_id)
        {
            if latest.is_some() {
                stopped.title.clone_from(&latest);
            }
            known = true;
        }
        if !known {
            return Err(IpcError::from(lifecycle::stale_session(session_id)));
        }
        latest
    };
    s.publish_state();
    Ok(json!({ "session_id": session_id, "title": title }))
}

/// `record.discard`: the intent is durable before anything else. An active
/// session then stops at once without a final job; a stopped one is checked
/// against its final commit under the library lock. The journal, jobs and
/// any provisional meeting go in the background ([`spawn_discard_cleanup`]),
/// never turning into a save.
fn discard_recording(s: &Arc<Shared>, session_id: &str) -> Result<Value, IpcError> {
    let root = s.cfg.journal_root();
    let journal = lifecycle::session_dir(&root, session_id).map_err(IpcError::from)?;
    let discarded =
        || json!({ "status": "discarded", "session_id": session_id, "cleanup": "pending" });
    // Active: the intent under the session mutex, so a concurrent Stop either
    // finds it (and queues nothing) or this finds no active session.
    let was_active = {
        let session = lock(&s.session);
        let active = session
            .as_ref()
            .is_some_and(|a| a.handle.session_id() == session_id);
        if active {
            lifecycle::write_discard_intent(&root, session_id).map_err(IpcError::from)?;
        }
        active
    };
    if was_active {
        if let Some(Err(error)) = stop_session(s, Some(session_id), true) {
            tracing::warn!(code = %error.code, "stopping a discarded recording");
        }
        resume_detections(s);
        spawn_discard_cleanup(s, session_id.to_owned());
        return Ok(discarded());
    }
    // Stopped: fenced against the final job's commit by the library lock.
    let saved = s
        .lib
        .write(|tx| {
            if let Some(saved) = lifecycle::session_meetings(tx.library(), &root, session_id)?
                .into_iter()
                .find(|m| m.processing.status == schema::ProcessingStatus::Done)
            {
                return Ok(Some(saved.id));
            }
            if lifecycle::has_discard_intent(&root, session_id) {
                return Ok(None);
            }
            let known = lock(&s.stopped).iter().any(|r| r.session_id == session_id);
            if !known || lifecycle::publication_blocked(&journal, session_id) {
                return Err(lifecycle::stale_session(session_id));
            }
            lifecycle::write_discard_intent(&root, session_id)?;
            Ok(None)
        })
        .map_err(IpcError::from)?;
    if let Some(meeting_id) = saved {
        return Ok(json!({ "status": "already_saved", "meeting_id": meeting_id }));
    }
    mark_discarding(s, session_id, None);
    spawn_discard_cleanup(s, session_id.to_owned());
    Ok(discarded())
}

/// Resolve a journal that predates library identity. The chosen library,
/// the manifest and capture ownership are rechecked before writing anything.
fn resolve_legacy_journal(
    s: &Arc<Shared>,
    session_id: &str,
    resolution: JournalResolution,
    library_root: &std::path::Path,
    confirmed: bool,
) -> Result<Value, IpcError> {
    if !confirmed {
        return Err(IpcError::new(
            "confirmation_required",
            "Confirm the journal resolution first.",
        ));
    }
    if library_root != s.lib.root() {
        return Err(IpcError::new(
            "library_changed",
            "BroScribe Agent is using a different library.",
        ));
    }
    let root = s.cfg.journal_root();
    let dir = lifecycle::session_dir(&root, session_id).map_err(IpcError::from)?;
    let journal_ops = lock(&s.journal_ops);
    let reserved = {
        let session = lock(&s.session);
        s.admit_capture(&session, "Recording recovery")?
    };
    let adopted = s.lib.write(|tx| {
        let manifest = journal::load_manifest(&dir)?;
        if manifest.session_id != session_id || lock(&s.stopping_sessions).contains(session_id) {
            return Err(lifecycle::stale_session(session_id));
        }
        match journal::check_library(&manifest, tx.root()) {
            Err(error) if error.code() == journal::LIBRARY_UNKNOWN => {}
            Err(error) => return Err(error),
            Ok(()) => return Err(lifecycle::stale_session(session_id)),
        }
        if manifest.state == journal::SessionState::Recording
            && manifest.owner_pid != s.probe.current_pid()
            && s.probe.is_alive(manifest.owner_pid)
        {
            return Err(Error::LibraryBusy("Another process may still be writing this recording.".into()));
        }
        match resolution {
            JournalResolution::Adopt => {
                if lifecycle::has_discard_intent(&root, session_id) {
                    return Err(Error::Service {
                        code: lifecycle::RECORDING_DISCARDED,
                        message: "This recording already has a discard request. Choose Discard recording to finish it.".into(),
                    });
                }
                let mut manifest = journal::recover(&dir)?;
                manifest.meta.library_root = Some(tx.root().to_path_buf());
                journal::write_manifest(&dir, &manifest)?;
                Ok(Some(manifest))
            }
            JournalResolution::Discard => {
                lifecycle::write_legacy_discard_intent(&root, session_id)?;
                Ok(None)
            }
        }
    });
    drop(reserved);
    let adopted = match adopted {
        Ok(adopted) => adopted,
        Err(error) => {
            // A failed sync can have left a binding or an intent on disk.
            sweep_due_within(s, Duration::ZERO);
            return Err(error.into());
        }
    };
    lock(&s.journal_issues).retain(|issue| issue.session_id != session_id);
    if let Some(manifest) = adopted {
        lock(&s.stopped).push(stopped_journal(&dir, &manifest, false));
        let issues = queue_recovered(
            s,
            vec![StoppedSession {
                session_id: session_id.to_owned(),
                journal_dir: dir,
            }],
        );
        if let Some(issue) = issues.first() {
            note_unqueued(s, session_id, &issue.code);
        } else {
            s.publish_state();
        }
        drop(journal_ops);
        if !issues.is_empty() {
            retry_journal_recovery(s);
        }
        s.kick.notify_one();
        Ok(json!({ "status": "adopted", "session_id": session_id }))
    } else {
        mark_discarding(s, session_id, None);
        drop(journal_ops);
        spawn_discard_cleanup(s, session_id.to_owned());
        Ok(json!({ "status": "discarded", "session_id": session_id, "cleanup": "pending" }))
    }
}

/// Shows `session_id` as discarding in the stopped list (adding it when the
/// stop raced the discard).
fn mark_discarding(s: &Shared, session_id: &str, entry: Option<StoppedRecording>) {
    set_discarding(s, session_id, entry);
    withdraw_failed_recording(s, session_id);
    s.publish_state();
}

/// [`mark_discarding`] without publishing: for callers holding the session
/// mutex, which publish once they released it.
fn set_discarding(s: &Shared, session_id: &str, entry: Option<StoppedRecording>) {
    let mut stopped = lock(&s.stopped);
    match stopped.iter_mut().find(|r| r.session_id == session_id) {
        Some(r) => r.state = StoppedRecordingState::Discarding,
        None => stopped.push(entry.unwrap_or_else(|| StoppedRecording {
            session_id: session_id.to_owned(),
            title: None,
            tags: Vec::new(),
            stopped_at: chrono::Utc::now(),
            stop_reason: None,
            state: StoppedRecordingState::Discarding,
        })),
    }
}

fn delete_options(s: &Shared) -> DeleteOptions {
    DeleteOptions {
        journal_root: s.cfg.journal_root(),
        cache_dir: Some(s.cfg.cache_dir.clone()),
    }
}

/// One cleanup attempt of a discarded session; `true` once it is complete.
/// At startup (`at_startup`, under the agent's instance lock, before any
/// capture is admitted) no capture runs, so a journal left `recording` by a
/// crash is an orphan. Later the session counts as live while it is the
/// active one or still stopping; core also refuses a journal another live
/// process owns.
fn discard_cleanup(s: &Shared, session_id: &str, at_startup: bool) -> broscribe_core::Result<bool> {
    let _journal_ops = lock(&s.journal_ops);
    let root = s.cfg.journal_root();
    let ownership = if at_startup {
        lifecycle::CaptureOwnership::NoneRunning
    } else {
        let active = lock(&s.session)
            .as_ref()
            .is_some_and(|a| a.handle.session_id() == session_id);
        let stopping = lock(&s.stopping_sessions).contains(session_id);
        lifecycle::CaptureOwnership::Agent {
            live: active || stopping,
        }
    };
    let dir = lifecycle::session_dir(&root, session_id)?;
    if !lifecycle::is_legacy_discard(&root, session_id)?
        && dir.join(journal::MANIFEST_FILE).try_exists()?
    {
        let manifest = journal::load_manifest(&dir)?;
        journal::check_library(&manifest, s.lib.root())?;
    }
    let report = lifecycle::discard_session(
        &s.lib,
        s.probe.as_ref(),
        &delete_options(s),
        session_id,
        ownership,
    )?;
    if report.complete {
        lock(&s.stopped).retain(|r| r.session_id != session_id);
        lock(&s.journal_issues).retain(|issue| issue.session_id != session_id);
        if let Some(meeting) = report.saved_meeting {
            push_saved(s, session_id, meeting.to_string(), None);
        }
        s.publish_state();
    } else {
        tracing::warn!(
            failures = report.failures.len(),
            "discarded recording cleanup pending; retrying"
        );
    }
    Ok(report.complete)
}

/// Retries [`discard_cleanup`] now and after `journal_retry`, doubling up
/// to hourly, until it completes; maintenance also retries every intent.
fn spawn_discard_cleanup(s: &Arc<Shared>, session_id: String) {
    const LONGEST: Duration = Duration::from_secs(60 * 60);
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        if let Err(error) = discard_cleanup(s, &session_id, false) {
            tracing::warn!(code = error.code(), "cleaning up a discarded recording");
        }
        return;
    };
    let s = Arc::clone(s);
    rt.spawn(async move {
        let mut delay = s.cfg.journal_retry;
        loop {
            let attempt = Arc::clone(&s);
            let id = session_id.clone();
            match tokio::task::spawn_blocking(move || discard_cleanup(&attempt, &id, false)).await {
                Ok(Ok(true)) | Err(_) => return,
                Ok(Ok(false)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(code = error.code(), "cleaning up a discarded recording")
                }
            }
            tokio::select! {
                () = s.shutdown.wait() => return,
                () = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(LONGEST);
        }
    });
}

/// Finishes interrupted meeting deletions and discarded recordings (startup
/// and periodic maintenance), and drops stopped entries whose journal is
/// gone. Failures are logged and retried next time; capture never waits.
fn maintain_lifecycle(s: &Shared, at_startup: bool) {
    if let Err(error) = lifecycle::finish_pending_deletions(&s.lib, s.probe.as_ref()) {
        tracing::warn!(code = error.code(), "finishing meeting deletions");
    }
    // Atomic-write temps of writers that died long ago (never a live one).
    if let Err(error) = lifecycle::sweep_orphan_temps(&s.lib, &s.cfg.data_dir, s.probe.as_ref()) {
        tracing::warn!(code = error.code(), "removing abandoned temporary files");
    }
    match lifecycle::discard_intents(&s.cfg.journal_root()) {
        Ok(sessions) => {
            for session in sessions {
                if let Err(error) = discard_cleanup(s, &session, at_startup) {
                    tracing::warn!(code = error.code(), "cleaning up a discarded recording");
                }
            }
        }
        Err(error) => tracing::warn!(code = error.code(), "listing discarded recordings"),
    }
    refresh_stopped(s);
}

/// The stopped list after a restart: every stopped journal not saved yet,
/// discarding ones included.
fn rebuild_stopped(s: &Shared) {
    let root = s.cfg.journal_root();
    let sessions = journal::list_sessions(&root).unwrap_or_default();
    let blocked: HashSet<_> = sessions
        .iter()
        .filter(|session| journal::check_library(&session.manifest, s.lib.root()).is_err())
        .map(|session| session.manifest.session_id.clone())
        .collect();
    let mut stopped = Vec::new();
    for journal in sessions {
        if blocked.contains(&journal.manifest.session_id) {
            continue;
        }
        stopped.push(stopped_journal(
            &journal.dir,
            &journal.manifest,
            lifecycle::has_discard_intent(&root, &journal.manifest.session_id),
        ));
    }
    for id in lifecycle::discard_intents(&root).unwrap_or_default() {
        if (!blocked.contains(&id) || lifecycle::is_legacy_discard(&root, &id).unwrap_or(false))
            && !stopped.iter().any(|r| r.session_id == id)
        {
            stopped.push(StoppedRecording {
                session_id: id,
                title: None,
                tags: Vec::new(),
                stopped_at: chrono::Utc::now(),
                stop_reason: None,
                state: StoppedRecordingState::Discarding,
            });
        }
    }
    {
        let mut entries = lock(&s.stopped);
        *lock(&s.rebuilt_stopped) = stopped
            .iter()
            .map(|entry| entry.session_id.clone())
            .collect();
        *entries = stopped;
    }
    s.publish_state();
}

fn stopped_journal(
    dir: &std::path::Path,
    manifest: &journal::SessionManifest,
    discarding: bool,
) -> StoppedRecording {
    let title = lifecycle::read_session_title(dir)
        .ok()
        .flatten()
        .map(|title| title.title)
        .or_else(|| manifest.meta.title.clone());
    StoppedRecording {
        state: if discarding {
            StoppedRecordingState::Discarding
        } else {
            StoppedRecordingState::Queued { ahead: 0 }
        },
        session_id: manifest.session_id.clone(),
        title,
        tags: manifest.meta.tags.clone(),
        stopped_at: manifest.stopped_at.unwrap_or_else(chrono::Utc::now),
        stop_reason: interrupted_stop(manifest.stop_reason.as_deref()).map(str::to_owned),
    }
}

/// Moves stopped entries whose journal is gone to `saved` (when their final
/// meeting exists) or drops them; discarding ones leave once cleaned up.
/// One pass over the meetings for all of them.
fn refresh_stopped(s: &Shared) {
    let root = s.cfg.journal_root();
    let gone: Vec<String> = lock(&s.stopped)
        .iter()
        .filter(|r| {
            r.state.is_pending_save()
                && !root
                    .join(&r.session_id)
                    .join(lifecycle::SESSION_MANIFEST)
                    .is_file()
        })
        .map(|r| r.session_id.clone())
        .collect();
    if gone.is_empty() {
        return;
    }
    let mut saved = saved_meetings(s, &gone);
    for id in gone {
        let meeting = saved.remove(&id);
        recording_saved_as(s, &id, meeting);
    }
    s.publish_state();
}

/// The saved (Done) meeting of each of `sessions`, `(meeting ID, title)`,
/// in one pass over the library's meetings. An unreadable meeting is
/// skipped and never hides another.
fn saved_meetings(s: &Shared, sessions: &[String]) -> HashMap<String, (String, Option<String>)> {
    let mut found = HashMap::new();
    let dirs = match s.lib.list_meeting_dirs() {
        Ok(dirs) => dirs,
        Err(error) => {
            tracing::warn!(code = error.code(), "listing meetings for saved recordings");
            return found;
        }
    };
    for dir in dirs {
        let Ok(meeting) = s.lib.load_meeting_at(&dir) else {
            continue;
        };
        if meeting.processing.status != schema::ProcessingStatus::Done {
            continue;
        }
        let Some(session) = meeting
            .source_id
            .as_deref()
            .and_then(|id| id.strip_prefix("recording:"))
            .filter(|id| sessions.iter().any(|s| s == id))
        else {
            continue;
        };
        found.insert(
            session.to_owned(),
            (meeting.id.to_string(), meeting.title.clone()),
        );
    }
    found
}

/// After a final job or a cleanup: the session leaves the stopped list and,
/// when its meeting was saved (`saved`: meeting ID and title), joins
/// `saved`. Without a saved meeting it leaves only once its journal is
/// gone. Publishes nothing; the caller does.
pub(crate) fn recording_saved_as(
    s: &Shared,
    session_id: &str,
    saved: Option<(String, Option<String>)>,
) {
    match saved {
        Some((meeting_id, title)) => {
            lock(&s.stopped).retain(|r| r.session_id != session_id);
            push_saved(s, session_id, meeting_id, title);
        }
        None => {
            let journal = s.cfg.journal_root().join(session_id);
            if !journal.join(lifecycle::SESSION_MANIFEST).is_file() {
                lock(&s.stopped).retain(|r| r.session_id != session_id);
            }
        }
    }
}

/// The scheduler's view of each stopped recording's final job (by
/// session); entries being discarded and sessions absent from `states`
/// keep theirs. Returns whether anything shown changed; publishes nothing.
pub(crate) fn set_stopped_states(
    s: &Shared,
    states: &HashMap<String, StoppedRecordingState>,
) -> bool {
    let mut changed = false;
    let mut failures = Vec::new();
    let mut entries = lock(&s.stopped);
    let mut rebuilt = lock(&s.rebuilt_stopped);
    for entry in entries.iter_mut().filter(|e| e.state.is_pending_save()) {
        let Some(state) = states.get(&entry.session_id) else {
            continue;
        };
        let first_observation = rebuilt.remove(&entry.session_id);
        if *state != entry.state {
            if !first_observation
                && let StoppedRecordingState::Failed { error } = state
                && !matches!(entry.state, StoppedRecordingState::Failed { .. })
            {
                let title = meeting_title(entry.title.as_deref());
                let cause = transcription_cause(error.as_deref())
                    .map_or(String::new(), |cause| format!(" {cause}"));
                failures.push(NotificationRequest {
                    id: format!("failed-{}", entry.session_id),
                    title: if broscribe_platform_native::NativePlatform::host_platform()
                        == broscribe_platform::HostPlatform::MacOs
                    {
                        "Couldn't Transcribe".into()
                    } else {
                        "Couldn't transcribe".into()
                    },
                    body: format!(
                        "BroScribe couldn't transcribe “{title}”.{cause} The recording is kept."
                    ),
                    actions: Vec::new(),
                    meeting_id: None,
                });
            }
            entry.state = state.clone();
            changed = true;
        }
    }
    drop(rebuilt);
    drop(entries);
    for notification in failures {
        s.notify(notification);
    }
    changed
}

/// `record.retry`: runs the final job of a stopped recording whose
/// processing failed for good again, with fresh attempts, under the library
/// lock that the discard and final-commit fences use. Only an explicit user
/// request does this; nothing retries a failed job automatically.
fn retry_recording(s: &Shared, session_id: &str) -> Result<Value, IpcError> {
    let journal_dir =
        lifecycle::session_dir(&s.cfg.journal_root(), session_id).map_err(IpcError::from)?;
    // No final job can run over a journal whose recovery failed: a damaged
    // one stays for repair, another is recovered again by the next sweep.
    if let Some(issue) = lock(&s.journal_issues)
        .iter()
        .find(|i| i.session_id == session_id)
    {
        return Err(IpcError::new(
            IpcError::NOT_RETRYABLE,
            if issue.corrupt {
                "this recording's journal is damaged; it is kept for repair"
            } else if journal::library_blocked(&issue.code) {
                "this recording's journal could not be assigned to the open library; it is kept in place"
            } else {
                "this recording's journal is recovered again automatically"
            },
        ));
    }
    let failed = lock(&s.stopped)
        .iter()
        .find(|r| r.session_id == session_id)
        .map(|r| matches!(r.state, StoppedRecordingState::Failed { .. }));
    match failed {
        None => return Err(IpcError::from(lifecycle::stale_session(session_id))),
        Some(false) => {
            return Err(IpcError::new(
                IpcError::NOT_RETRYABLE,
                "this recording's processing has not failed",
            ));
        }
        Some(true) => {}
    }
    let input = serde_json::to_value(RecordJobInput {
        session_id: session_id.to_owned(),
        journal_dir: journal_dir.clone(),
        cutoff: None,
    })
    .map_err(|e| IpcError::new("internal", e.to_string()))?;
    let job = s
        .lib
        .write(|tx| {
            if lifecycle::publication_blocked(&journal_dir, session_id) {
                return Err(lifecycle::stale_session(session_id));
            }
            journal::check_library(&journal::load_manifest(&journal_dir)?, tx.root())?;
            let queue = Queue::open(tx.root())?;
            let id = JobId::for_key(RECORD_FINAL_KIND, session_id);
            // Fenced on the record as stored now: only a job failed for good
            // restarts (a retrying or running one is left alone).
            match queue.get(&id) {
                Ok(job)
                    if job.state == broscribe_core::queue::JobState::Failed
                        && (job.attempts
                            >= s.cfg.max_attempts.min(broscribe_core::queue::MAX_ATTEMPTS)
                            || job.is_interactive()) => {}
                Ok(_) => {
                    return Err(Error::Service {
                        code: IpcError::NOT_RETRYABLE,
                        message: "this recording's processing has not failed".into(),
                    });
                }
                Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
            queue.enqueue(tx, RECORD_FINAL_KIND, session_id, input)
        })
        .map_err(IpcError::from)?;
    {
        let mut stopped = lock(&s.stopped);
        if let Some(entry) = stopped.iter_mut().find(|r| r.session_id == session_id) {
            entry.state = StoppedRecordingState::Queued { ahead: 0 };
        }
    }
    withdraw_failed_recording(s, session_id);
    s.publish_state();
    Ok(json!({ "session_id": session_id, "job_id": job }))
}

fn push_saved(s: &Shared, session_id: &str, meeting_id: String, title: Option<String>) {
    withdraw_failed_recording(s, session_id);
    let mut saved = lock(&s.saved);
    saved.retain(|r| r.session_id != session_id);
    saved.push_front(SavedRecording {
        session_id: session_id.to_owned(),
        meeting_id,
        title,
        saved_at: chrono::Utc::now(),
    });
    saved.truncate(SAVED_RECORDINGS_MAX);
}

fn withdraw_failed_recording(s: &Shared, session_id: &str) {
    let id = format!("failed-{session_id}");
    if lock(&s.pending).remove(&id).is_some() {
        s.bus.publish(Event::NotificationDismissed { id });
    }
}

/// Queues a record job unless its session is discarded or its journal is
/// gone, checked under the library lock that queues it (the discard and
/// final-commit fences check under the same lock). `None`: fenced.
fn enqueue_record(
    s: &Shared,
    kind: &str,
    key: &str,
    input: &RecordJobInput,
) -> broscribe_core::Result<Option<JobId>> {
    let value = serde_json::to_value(input).map_err(|e| Error::Internal(e.to_string()))?;
    s.lib.write(|tx| {
        if lifecycle::publication_blocked(&input.journal_dir, &input.session_id) {
            return Ok(None);
        }
        let manifest = journal::load_manifest(&input.journal_dir)?;
        journal::check_library(&manifest, tx.root())?;
        Queue::open(tx.root())?
            .enqueue(tx, kind, key, value)
            .map(Some)
    })
}

// ---- recording ----

fn new_session_id(at: &DateTime<FixedOffset>) -> String {
    format!(
        "s-{}-{:04x}",
        at.format("%Y%m%d-%H%M%S"),
        rand::random::<u16>()
    )
}

/// Capture notices → events. Never blocks: meters are throttled and only
/// sent when they change; health changes are coalesced into one state
/// publication at a time; a self-stop is handed to an async task.
struct DaemonEvents {
    shared: Weak<Shared>,
    rt: tokio::runtime::Handle,
    session_id: String,
    last_meter: Mutex<Option<(Instant, f32, f32)>>,
    /// A state publication for changed capture health is scheduled and
    /// has not read the state yet: later changes join it.
    health_dirty: Arc<AtomicBool>,
}

impl DaemonEvents {
    /// `RecordingState.health` changed: publishes the current state once,
    /// off this capture thread and outside every capture and session lock
    /// (the state reads the session mutex, which a command may hold while
    /// it waits for this very thread). Changes arriving before that
    /// publication reads the state join it; later ones schedule another.
    fn health_changed(&self, s: Arc<Shared>) {
        if self.health_dirty.swap(true, Ordering::AcqRel) {
            return;
        }
        let dirty = Arc::clone(&self.health_dirty);
        self.rt.spawn(async move {
            dirty.store(false, Ordering::Release);
            let _ = tokio::task::spawn_blocking(move || {
                s.refresh_capture_access();
                s.publish_state();
            })
            .await;
        });
    }
}

impl CaptureEvents for DaemonEvents {
    fn notice(&self, notice: CaptureNotice) {
        let Some(s) = self.shared.upgrade() else {
            return;
        };
        match notice {
            CaptureNotice::Meters {
                me_dbfs,
                others_dbfs,
            } => {
                let clamp = |v: f32| {
                    if v.is_finite() {
                        v.clamp(-120.0, 0.0)
                    } else {
                        -120.0
                    }
                };
                let (me, others) = (clamp(me_dbfs), clamp(others_dbfs));
                {
                    let mut last = lock(&self.last_meter);
                    if let Some((at, m, o)) = *last
                        && (at.elapsed() < s.cfg.meter_interval || (m == me && o == others))
                    {
                        return;
                    }
                    *last = Some((Instant::now(), me, others));
                }
                s.bus.publish(Event::Meters {
                    me_dbfs: me,
                    others_dbfs: others,
                });
            }
            CaptureNotice::Warning(warning) => {
                tracing::warn!(kind = warning_kind(&warning), "capture warning");
                if let CaptureWarning::DiskLow {
                    free_bytes,
                    headroom_s,
                    ..
                } = &warning
                {
                    s.notify(disk_low_notice(&self.session_id, *free_bytes, *headroom_s));
                }
                s.bus.publish(Event::CaptureWarning {
                    session_id: self.session_id.clone(),
                    warning,
                });
                // `RecordingState.health` changed with it.
                self.health_changed(s);
            }
            CaptureNotice::HealthChanged => self.health_changed(s),
            CaptureNotice::PreviewGap {
                track,
                from_s,
                to_s,
            } => {
                s.bus.publish(Event::PreviewGap {
                    session_id: self.session_id.clone(),
                    track,
                    from_s,
                    to_s,
                });
            }
            CaptureNotice::Stopped { reason } => {
                tracing::warn!("capture stopped: {reason}");
                // Still opening: its commit stops it instead of activating it
                // (no session mutex here: capture never waits on it).
                {
                    let mut starting = lock(&s.starting);
                    if let Some(start) = starting
                        .as_mut()
                        .filter(|st| st.session_id == self.session_id)
                    {
                        start.cancelled = Some(reason);
                        return;
                    }
                }
                let session = self.session_id.clone();
                let notice_id = format!("stopped-{session}");
                self.rt.spawn(async move {
                    let stopped = blocking({
                        let s = s.clone();
                        let reason = reason.clone();
                        move || {
                            let stopped =
                                stop_selected(&s, Select::Interrupted(&session, &reason), false);
                            resume_detections(&s);
                            stopped.unwrap_or(Ok(Value::Null))
                        }
                    })
                    .await;
                    if let Err(e) = stopped {
                        tracing::warn!("closing the stopped recording: {e}");
                    }
                    // The code stays in the log above; the notice says it in
                    // words. No action and no meeting: a state snapshot never
                    // withdraws it (design §4, Automation).
                    s.notify(NotificationRequest {
                        id: notice_id,
                        title: broscribe_platform_native::NativePlatform::notification_title(
                            "Recording Stopped",
                            "Recording stopped",
                        ),
                        body: stop_reason_text(&reason).into(),
                        actions: Vec::new(),
                        meeting_id: None,
                    });
                });
            }
        }
    }
}

/// Which starts `start_recording` admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admit {
    /// A user's start.
    Always,
    /// A detected start, or a Record answer waiting for its turn: only while
    /// the detected meeting is still on. Checked under the session lock, so
    /// a `MeetingEnded` handled at the same time either refuses this start
    /// or finds the recording and stops it.
    WhileDetected,
    /// The user answered Record to a detection prompt: admitted like a
    /// user's start, bound to the detected app like a detected one.
    Answered,
}

impl Admit {
    /// The start comes from a detection, so the recording binds to its app.
    fn detected(self) -> bool {
        self != Admit::Always
    }
}

/// `start_recording` refused an [`Admit::WhileDetected`] start: the meeting
/// is no longer detected. Internal to the daemon; never sent to a client.
const MEETING_ENDED: &str = "meeting_ended";

/// Starts a recording. Admission is decided under the session mutex and
/// held as a reservation (the capture slot plus [`Starting`]); the slow part
/// (config, device classification, opening both devices) runs without the
/// mutex, so status, IPC and other commands never wait on a device. The
/// start is acknowledged only after the devices opened and the commit,
/// under the session mutex again, found the reservation still current: a
/// shutdown, the end of its detected meeting or capture stopping on its own
/// meanwhile stops the opened session instead, and nothing is activated.
fn start_recording(
    s: &Arc<Shared>,
    params: RecordStartParams,
    rt: tokio::runtime::Handle,
    admit: Admit,
) -> Result<Value, IpcError> {
    let remote_confirmed = remote_confirmed(s, &params);
    if let Some(language) = &params.language
        && !valid_language(language)
    {
        return Err(invalid_language());
    }
    let started_at = schema::now();
    let session_id = new_session_id(&started_at);
    #[cfg(test)]
    let session_id = lock(&s.next_session_id).take().unwrap_or(session_id);
    let slot = {
        let session = lock(&s.session);
        if s.stopping.load(Ordering::Acquire) || s.shutdown.is_triggered() {
            return Err(IpcError::new(
                IpcError::SHUTTING_DOWN,
                "the daemon is stopping",
            ));
        }
        if let Some(a) = session.as_ref() {
            return Err(IpcError::from_core(&Error::AlreadyExists(format!(
                "recording {} is running",
                a.handle.session_id()
            ))));
        }
        // Before `busy`: a start held off by a setup capture stops retrying
        // once its meeting ends.
        if admit == Admit::WhileDetected && !detected_meeting_on(s, &params) {
            return Err(IpcError::new(
                MEETING_ENDED,
                "the detected meeting is no longer on",
            ));
        }
        let slot = s.admit_capture(&session, "recording")?;
        *lock(&s.starting) = Some(Starting {
            session_id: session_id.clone(),
            cancelled: None,
        });
        slot
    };
    s.refresh_capture_access();
    let opened = open_session(
        s,
        &params,
        &rt,
        admit,
        &session_id,
        started_at,
        remote_confirmed,
    );
    let (handle, prepared) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            lock(&s.starting).take();
            return Err(error);
        }
    };
    // Commit: the reservation must still be this start's and uncancelled.
    let mut session = lock(&s.session);
    let cancelled = {
        let mut starting = lock(&s.starting);
        let ours = starting
            .as_ref()
            .is_some_and(|st| st.session_id == session_id);
        let reason = match starting.as_ref() {
            _ if !ours => Some("the start was superseded".to_owned()),
            Some(Starting {
                cancelled: Some(reason),
                ..
            }) => Some(reason.clone()),
            _ if s.stopping.load(Ordering::Acquire) || s.shutdown.is_triggered() => {
                Some("BroScribe Agent is stopping".to_owned())
            }
            _ if admit == Admit::WhileDetected && !detected_meeting_on(s, &params) => {
                Some(MEETING_ENDED.to_owned())
            }
            _ => None,
        };
        if ours {
            starting.take();
        }
        reason
    };
    if let Some(reason) = cancelled {
        drop(session);
        // Never activated and never acknowledged: nobody knows this
        // recording, so its journal is closed and removed (as a start that
        // failed to open), never queued. The slot is held until then, so no
        // recovery sweep takes it for an orphan meanwhile.
        match handle.stop() {
            Ok(stopped) => {
                if let Err(error) = broscribe_capture::journal::remove(&stopped.journal_dir) {
                    tracing::warn!(code = error.code(), "removing a cancelled start's journal");
                }
            }
            Err(error) => tracing::warn!(code = error.code(), "stopping a cancelled start"),
        }
        drop(slot);
        return Err(if reason == MEETING_ENDED {
            IpcError::new(MEETING_ENDED, "the detected meeting is no longer on")
        } else if s.stopping.load(Ordering::Acquire) || s.shutdown.is_triggered() {
            IpcError::new(IpcError::SHUTTING_DOWN, "the daemon is stopping")
        } else {
            IpcError::new(
                "capture_failed",
                format!("recording did not start: {reason}"),
            )
        });
    }
    let Prepared {
        live_queue,
        resolved_in_person,
        binding,
    } = prepared;
    // The preview belongs to this recording from now on: a stopped one's
    // supervisor still finishing never overwrites it.
    s.own_preview(Some(&session_id));
    let live_stop = Arc::new(Shutdown::new());
    let live_preview = crate::live_identity::LivePreview::new(
        session_id.clone(),
        s.cfg.journal_root().join(&session_id),
        s.cfg.live.identity.clone(),
        s.bus.clone(),
    );
    let weak = Arc::downgrade(s);
    rt.spawn(live::supervise(LiveRun {
        worker: s.cfg.worker.clone(),
        library: s.lib.root().to_owned(),
        admission: Arc::clone(&s.admission),
        session_id: session_id.clone(),
        settings: s.cfg.live.clone(),
        queue: Arc::clone(&live_queue),
        bus: s.bus.clone(),
        stop: Arc::clone(&live_stop),
        on_preview: Arc::new(move |session_id: &str, p| {
            if let Some(s) = weak.upgrade() {
                s.set_preview(session_id, p);
            }
        }),
        preview: Some(Arc::clone(&live_preview)),
    }));
    rt.spawn(Arc::clone(&live_preview).supervise(
        s.cfg.worker.clone(),
        s.lib.root().to_owned(),
        s.cfg.data_dir.clone(),
        Arc::clone(&live_stop),
        Arc::clone(&s.admission),
    ));
    *session = Some(Active {
        handle,
        started_at,
        title: params.title,
        reader: crate::readers::resolve_for(s, params.app.as_deref()),
        app: params.app,
        observed_roster: Vec::new(),
        expected_roster: Vec::new(),
        live_queue,
        live_stop,
        live_preview,
        awaiting_remote_evidence: resolved_in_person,
        evidence_warned: false,
        auto_stop: crate::autostop::AutoStop::new(binding, s.next_reader_generation()),
        reader_seen: None,
        reader_issued: Instant::now(),
        reader_announced: Instant::now(),
        reader_health: None,
        reader_refusal: None,
        reader_shown: None,
    });
    // The session owns admission now: every admission refuses while it
    // exists, so the slot is released under the same mutex.
    drop(slot);
    drop(session);
    if let Err(error) = crate::integrations::snapshot_calendar(s) {
        tracing::warn!(code = error.code, "caching recording calendar prior");
    }
    s.publish_state();
    crate::readers::restart(s);
    s.integration_kick.notify_one();
    Ok(json!({ "session_id": session_id }))
}

/// What an opened session needs at its commit.
struct Prepared {
    live_queue: Arc<LiveQueue>,
    resolved_in_person: bool,
    binding: Option<AutoStopApp>,
}

/// The slow part of a start, outside the session mutex: the configuration,
/// device classification on the microphone capture will use, and opening
/// both devices into a new journal.
fn open_session(
    s: &Arc<Shared>,
    params: &RecordStartParams,
    rt: &tokio::runtime::Handle,
    admit: Admit,
    session_id: &str,
    started_at: DateTime<FixedOffset>,
    remote_confirmed: bool,
) -> Result<(Box<dyn SessionHandle>, Prepared), IpcError> {
    let config = config_or_default(&s.cfg.data_dir);
    let language = match &params.language {
        Some(language) => Some(language.clone()),
        None => config.default_language.clone().filter(|language| {
            let valid = valid_language(language);
            if !valid {
                tracing::warn!("config.toml default_language is not a language tag; ignoring it");
            }
            valid
        }),
    };
    let devices = lock(&s.devices).clone();
    let mic = lock(&s.sample_mic).clone();
    let resolved = crate::setup::resolve(
        devices.as_ref(),
        &s.cfg.data_dir,
        &config,
        params.setup.as_deref(),
        remote_confirmed,
        crate::setup::capture_mic(mic.as_ref(), &config).as_deref(),
    )?;
    // A detected start binds automatic stop to its detection app (a rule
    // ID); a manual start stays manual until the user links a call.
    let binding = params
        .app
        .as_deref()
        .filter(|_| admit.detected())
        .and_then(|id| {
            lock(&s.detector).as_ref().and_then(|d| {
                d.rules().app(id).map(|rule| AutoStopApp {
                    app_id: rule.id.clone(),
                    app_name: rule.name.clone(),
                })
            })
        });
    let live_queue = Arc::new(LiveQueue::with_seconds(s.cfg.live.queue_seconds));
    if let Some(language) = &language {
        live_queue.set_language(0, language);
    }
    let events = Arc::new(DaemonEvents {
        shared: Arc::downgrade(s),
        rt: rt.clone(),
        session_id: session_id.to_owned(),
        last_meter: Mutex::new(None),
        health_dirty: Arc::new(AtomicBool::new(false)),
    });
    let handle = s
        .engine
        .start(
            SessionConfig {
                session_id: session_id.to_owned(),
                journal_root: s.cfg.journal_root(),
                library_root: s.lib.root().to_path_buf(),
                started_at,
                title: params.title.clone(),
                app: params.app.clone(),
                language,
                setup: Some(resolved.setup),
                tags: params.tags.clone(),
                title_detected: params.title_detected,
                echo_handling: resolved.echo_handling,
                in_person_setup: resolved.in_person_setup.clone(),
                mic_device: config.audio.pinned_mic.clone(),
            },
            CaptureOutputs {
                live: live_queue.clone(),
                events,
            },
        )
        .map_err(IpcError::from)?;
    Ok((
        handle,
        Prepared {
            live_queue,
            resolved_in_person: resolved.in_person_setup.is_some(),
            binding,
        },
    ))
}

/// Cancels a start still opening its devices: its commit stops what it
/// opened instead of activating it.
fn cancel_start(s: &Shared, reason: &str) {
    if let Some(start) = lock(&s.starting).as_mut()
        && start.cancelled.is_none()
    {
        start.cancelled = Some(reason.to_owned());
    }
}

/// A capture warning's kind, for logs (never its device text).
fn warning_kind(warning: &CaptureWarning) -> &'static str {
    match warning {
        CaptureWarning::TrackDown { .. } => "track_down",
        CaptureWarning::TrackResumed { .. } => "track_resumed",
        CaptureWarning::DiskLow { .. } => "disk_low",
        CaptureWarning::PreviewUnavailable => "preview_unavailable",
    }
}

/// A BCP 47-shaped tag (`en`, `pt-BR`, `zh-Hant-TW`) short enough for a live
/// language frame, so a session language never breaks the live preview.
fn valid_language(tag: &str) -> bool {
    let mut parts = tag.split('-');
    tag.len() <= broscribe_ipc::live::MAX_LANGUAGE_BYTES
        && parts.next().is_some_and(|l| {
            (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_alphabetic())
        })
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn invalid_language() -> IpcError {
    IpcError::from_core(&Error::InvalidInput(format!(
        "the language must be a BCP 47 tag such as en or pt-BR, at most {} bytes",
        broscribe_ipc::live::MAX_LANGUAGE_BYTES
    )))
}

/// Whether a start is known to be for a remote meeting (design §5): a
/// detected meeting app started it, or a meeting app holds the mic right now
/// (any rule-matched app, whatever its policy). Without a detector nothing
/// can be observed later, so the start stays remote. Otherwise the start is
/// still classified as remote; the final job may settle on in person.
fn remote_confirmed(s: &Shared, params: &RecordStartParams) -> bool {
    params.app.is_some()
        || lock(&s.detector)
            .as_ref()
            .is_none_or(broscribe_detect::Detector::meeting_app_on_mic)
}

/// Records remote-meeting evidence for the final job's setup: a meeting app
/// held the mic during the recording.
pub(crate) fn meeting_app_on_mic(s: &Shared) {
    let mut session = lock(&s.session);
    let Some(active) = session.as_mut().filter(|a| a.awaiting_remote_evidence) else {
        return;
    };
    match active.handle.mark_meeting_app_seen() {
        Ok(()) => active.awaiting_remote_evidence = false,
        // Retried on every poll; warned about once per session.
        Err(error) if !active.evidence_warned => {
            active.evidence_warned = true;
            tracing::warn!(%error, "recording meeting-app evidence");
        }
        Err(_) => {}
    }
}

/// How often a detected start held off by a setup capture is retried.
const AUTO_RECORD_RETRY: Duration = Duration::from_millis(250);

/// What asked [`Daemon::auto_record`] to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoStart {
    /// Detection (policy `auto`).
    Detected,
    /// The user answered Record on an ask prompt while a setup capture or
    /// permission prompt held the start.
    Answered,
}

impl Daemon {
    /// Starts recording a detected meeting while it is still detected. A
    /// start refused as `busy` because a helper sample capture or permission
    /// prompt holds `checking_audio` is retried until admitted or the
    /// meeting ends. Any other failure is shown to the user, never dropped,
    /// and so is the end of a meeting the user asked to record.
    pub(crate) async fn auto_record(&self, params: RecordStartParams, origin: AutoStart) {
        loop {
            let s = Arc::clone(&self.shared);
            let rt = tokio::runtime::Handle::current();
            let attempt = params.clone();
            let error = match blocking(move || {
                start_recording(&s, attempt, rt, Admit::WhileDetected)
            })
            .await
            {
                Ok(_) => return,
                Err(error) => error,
            };
            match error.code.as_str() {
                "busy" => {
                    tokio::select! {
                        () = self.shared.shutdown.wait() => return,
                        () = tokio::time::sleep(AUTO_RECORD_RETRY) => continue,
                    }
                }
                MEETING_ENDED => {
                    if origin == AutoStart::Answered {
                        self.shared.notify(answered_start_ended(
                            meeting_app_name(&self.shared, &params).as_deref(),
                        ));
                    }
                    return;
                }
                // Another start won the race, or the daemon is going away.
                "already_exists" | IpcError::SHUTTING_DOWN => return,
                _ => {}
            }
            tracing::warn!(code = %error.code, "auto-record start failed: {}", error.message);
            self.shared.notify(auto_record_failed(
                meeting_app_name(&self.shared, &params).as_deref(),
                &error,
            ));
            return;
        }
    }
}

/// The detected meeting is still on: its app is `Active` (detected, or the
/// user answered Record while it was asking). Without a detector or an app
/// nothing can end it.
fn detected_meeting_on(s: &Shared, params: &RecordStartParams) -> bool {
    match (lock(&s.detector).as_ref(), params.app.as_deref()) {
        (Some(detector), Some(app)) => detector.state(app) == broscribe_detect::DetectState::Active,
        _ => true,
    }
}

fn meeting_app_name(s: &Shared, params: &RecordStartParams) -> Option<String> {
    let app = params.app.as_deref()?;
    if let Some(name) = lock(&s.detector)
        .as_ref()
        .and_then(|detector| detector.rules().app(app).map(|rule| rule.name.clone()))
    {
        return Some(name);
    }
    Some(match app {
        "zoom" => "Zoom".into(),
        "teams" => "Microsoft Teams".into(),
        "meet" => "Google Meet".into(),
        "slack" => "Slack huddle".into(),
        "discord" => "Discord".into(),
        "webex" => "Webex".into(),
        other => {
            let mut letters = other.chars();
            let first = letters.next()?;
            format!("{}{}", first.to_uppercase(), letters.as_str())
        }
    })
}

fn automatic_stop_failed_notice(session: &str) -> NotificationRequest {
    NotificationRequest {
        id: format!("auto-stop-failed-{session}"),
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Recording Needs Attention",
            "Recording needs attention",
        ),
        body: "Recording stopped, but saving couldn't start. BroScribe will try to recover it."
            .into(),
        actions: Vec::new(),
        meeting_id: None,
    }
}

fn disk_low_notice(session: &str, free_bytes: u64, headroom_s: f64) -> NotificationRequest {
    NotificationRequest {
        id: format!("disk-low-{session}"),
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Disk Space Is Low",
            "Disk space is low",
        ),
        body: format!(
            "{} free, {} of recording. Recording stops and is saved before the disk fills.",
            broscribe_core::display_units::bytes(free_bytes),
            broscribe_core::display_units::estimate(headroom_s),
        ),
        actions: Vec::new(),
        meeting_id: None,
    }
}

fn answered_start_ended(app: Option<&str>) -> NotificationRequest {
    NotificationRequest {
        id: format!("autorecord-ended-{:08x}", rand::random::<u32>()),
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Recording Didn't Start",
            "Recording didn't start",
        ),
        body: match app {
            Some(app) => format!("The {app} call ended before recording started."),
            None => "The call ended before recording started.".into(),
        },
        actions: Vec::new(),
        meeting_id: None,
    }
}

fn auto_record_failed(app: Option<&str>, error: &IpcError) -> NotificationRequest {
    let cause = broscribe_ipc::recording_words::start_refusal(error, false).map_or_else(
        || "Try starting the recording again.".into(),
        |(cause, _)| cause,
    );
    let call = match app {
        Some(app) => format!("A {app} call"),
        None => "A call".into(),
    };
    NotificationRequest {
        id: format!("autorecord-failed-{:08x}", rand::random::<u32>()),
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Couldn't Start Recording",
            "Couldn't start recording",
        ),
        body: format!("{call} was found, but BroScribe Agent couldn't record it. {cause}"),
        actions: Vec::new(),
        meeting_id: None,
    }
}

/// Holds capture admission (`checking_audio`) until dropped.
pub(crate) struct Reserved<'a>(&'a AtomicBool);
impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Stops the running session (only `expect`, when given) and queues its
/// final job; then starts or asks about meetings detected meanwhile. A
/// named session that is not the active one is `stale_session`: a stop meant
/// for an earlier recording never stops a newer one.
async fn stop_recording(s: Arc<Shared>, expect: Option<String>) -> Result<Value, IpcError> {
    blocking(move || {
        let stopped = stop_session(&s, expect.as_deref(), false).ok_or_else(|| match &expect {
            Some(id) => IpcError::from(lifecycle::stale_session(id)),
            None => IpcError::from_core(&Error::NotFound("no recording is running".into())),
        })?;
        resume_detections(&s);
        stopped
    })
    .await
}

/// Which session a stop may take.
#[derive(Clone, Copy)]
enum Select<'a> {
    Any,
    Session(&'a str),
    Interrupted(&'a str, &'a str),
    /// An automatic stop: only the decided session while that decision is
    /// still current ([`crate::autostop::AutoStop::confirms`]).
    Decision(&'a crate::autostop::StopDecision),
}

/// `None` when there is no session (or not `expect`) to stop. Capture stops
/// without waiting for inference or the library. `discard` (or a discard
/// intent written meanwhile) queues no final job; otherwise the final job
/// is queued through the discard fence ([`enqueue_record`]).
fn stop_session(
    s: &Arc<Shared>,
    expect: Option<&str>,
    discard: bool,
) -> Option<Result<Value, IpcError>> {
    stop_selected(s, expect.map_or(Select::Any, Select::Session), discard)
}

/// [`stop_session`] for `select`, checked under the session mutex that
/// takes the recording.
fn stop_selected(
    s: &Arc<Shared>,
    select: Select<'_>,
    discard: bool,
) -> Option<Result<Value, IpcError>> {
    let active = {
        let mut session = lock(&s.session);
        match (session.as_ref(), select) {
            (None, _) => None,
            (Some(a), Select::Session(want) | Select::Interrupted(want, _))
                if a.handle.session_id() != want =>
            {
                None
            }
            (Some(a), Select::Decision(decision))
                if a.handle.session_id() != decision.session_id
                    || !a.auto_stop.confirms(decision) =>
            {
                None
            }
            (Some(a), _) => {
                // `checking_audio` is clear while a session runs: every
                // admission refuses under the session lock while one exists.
                s.checking_audio.store(true, Ordering::Release);
                // Marked stopping before the session mutex is released, so a
                // discard cleanup always sees it active or stopping until
                // its journal is closed (session → stopping set order).
                lock(&s.stopping_sessions).insert(a.handle.session_id().to_owned());
                // Its supervisors finish on their own; what they still
                // report is no longer this daemon's preview.
                s.own_preview(None);
                session.take()
            }
        }
    }?;
    // No capture starts until this one has stopped.
    let reserved = Reserved(&s.checking_audio);
    let session_id = active.handle.session_id().to_owned();
    let mut entry = StoppedRecording {
        session_id: session_id.clone(),
        title: active.title.clone(),
        tags: active.handle.status().tags,
        stopped_at: chrono::Utc::now(),
        stop_reason: match select {
            Select::Interrupted(_, reason) => interrupted_stop(Some(reason)).map(str::to_owned),
            _ => None,
        },
        state: StoppedRecordingState::Queued { ahead: 0 },
    };
    active.live_stop.trigger();
    s.bus.publish(Event::StopReader {
        session_id: session_id.clone(),
    });
    if let Some(d) = lock(&s.detector).as_mut() {
        dismiss_covered(
            d,
            active.app.as_deref(),
            active.auto_stop.app().map(|a| a.app_id.as_str()),
        );
    }
    active.live_queue.close();
    let stopped = active.handle.stop();
    if entry.stop_reason.is_none()
        && let Ok(stopped) = &stopped
        && let Ok(manifest) = broscribe_capture::journal::load_manifest(&stopped.journal_dir)
    {
        entry.stop_reason = interrupted_stop(manifest.stop_reason.as_deref()).map(str::to_owned);
    }
    #[cfg(test)]
    s.stop_handover.pass();
    let discarding = discard || lifecycle::has_discard_intent(&s.cfg.journal_root(), &session_id);
    {
        // The handover: the pending-save (or discarding) row appears, and
        // only then the stop's marker and capture reservation end, under
        // the session mutex install admission is decided under, so an
        // admission sees this recording in one form or the other.
        let _session = lock(&s.session);
        if discarding {
            set_discarding(
                s,
                &session_id,
                Some(StoppedRecording {
                    state: StoppedRecordingState::Discarding,
                    ..entry
                }),
            );
        } else {
            let mut list = lock(&s.stopped);
            list.retain(|r| r.session_id != session_id);
            list.push(entry);
        }
        lock(&s.stopping_sessions).remove(&session_id);
        drop(reserved);
    }
    s.publish_state();
    if discarding {
        if let Err(error) = &stopped {
            tracing::warn!(
                code = error.code(),
                "stopping discarded recording {session_id}"
            );
        }
        return Some(Ok(json!({ "session_id": session_id, "discarded": true })));
    }
    let queued = stopped.and_then(|stopped| {
        let input = RecordJobInput {
            session_id: stopped.session_id.clone(),
            journal_dir: stopped.journal_dir,
            cutoff: None,
        };
        enqueue_record(s, RECORD_FINAL_KIND, &stopped.session_id, &input)
    });
    let outcome = match queued {
        Ok(Some(job)) => Ok(json!({ "session_id": session_id, "job_id": job })),
        // Discarded meanwhile: the discard's cleanup owns the journal.
        Ok(None) => {
            mark_discarding(s, &session_id, None);
            Ok(json!({ "session_id": session_id, "discarded": true }))
        }
        Err(error) => {
            // The journal outlives the failure: recover it now if no capture
            // started meanwhile, else retry shortly. Until a sweep queues
            // its final job the row shows that retry and its code, never a
            // queue place nothing would advance.
            tracing::warn!(%error, "closing recording {session_id}; recovering its journal");
            note_unqueued(s, &session_id, error.code());
            retry_journal_recovery(s);
            Err(IpcError::from(error))
        }
    };
    s.kick.notify_one();
    Some(outcome)
}

/// What a stopped recording covered stays quiet until its app releases the
/// mic, whatever stopped it: the app that started it and, when the user
/// linked another call by hand (relink), that bound app too, so neither is
/// started again automatically for the call just saved. A recording with
/// neither (started by hand, never linked) covers every app detected
/// meanwhile, which that recording was taking. (After an automatic end its
/// own app is already cooling down.)
fn dismiss_covered(
    detector: &mut broscribe_detect::Detector,
    started_by: Option<&str>,
    bound: Option<&str>,
) {
    if started_by.is_none() && bound.is_none() {
        for open in detector.open_detections() {
            if open.state == broscribe_detect::DetectState::Active {
                detector.dismiss(&open.app_id);
            }
        }
        return;
    }
    for app in started_by
        .into_iter()
        .chain(bound.filter(|b| Some(*b) != started_by))
    {
        detector.dismiss(app);
    }
}

/// After a stop: starts every other meeting app detected while recording
/// (still `Active`) and asks about every one still `Asking` without a
/// prompt, so a meeting that began during another's recording is not lost.
/// The detector's state already follows the current policies
/// (`Detector::set_policies` reclassifies), and admission checks it again.
fn resume_detections(s: &Arc<Shared>) {
    if s.stopping.load(Ordering::Acquire) || s.shutdown.is_triggered() {
        return;
    }
    let open = lock(&s.detector)
        .as_ref()
        .map(broscribe_detect::Detector::open_detections)
        .unwrap_or_default();
    let daemon = Daemon {
        shared: Arc::clone(s),
    };
    for open in open {
        let params = crate::detection::detected_start(open.app_id, open.title);
        match open.state {
            broscribe_detect::DetectState::Active => {
                let daemon = daemon.clone();
                tokio::spawn(async move { daemon.auto_record(params, AutoStart::Detected).await });
            }
            broscribe_detect::DetectState::Asking => {
                let asked = lock(&s.pending)
                    .values()
                    .any(|p| matches!(p, Pending::AskRecord { params: asked, .. } if asked.app == params.app));
                if !asked {
                    daemon.ask_to_record(params);
                }
            }
            _ => {}
        }
    }
}

/// What one recovery sweep left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Swept {
    /// A journal could not be recovered for a reason that may pass (I/O),
    /// or its final job could not be queued: sweep again later. Corrupt
    /// journals never ask for this.
    retry: bool,
}

/// Queues final jobs for journals no session owns; `None` when a capture
/// runs or is being admitted, so nothing was swept. Admission is held while
/// the journals are scanned and repaired, so none starts and a live journal
/// is never taken for an orphan, but not while queueing waits on the
/// library lock: the scan only names journals that existed before it. One
/// journal failing (unreadable, I/O, queueing) never hides the others;
/// failures (a final job that could not be queued included) are kept in
/// `DaemonState.journal_issues` until a sweep gets past them, corrupt
/// journals are left untouched.
fn sweep_orphans(s: &Shared) -> broscribe_core::Result<Option<Swept>> {
    let _journal_ops = lock(&s.journal_ops);
    {
        let session = lock(&s.session);
        if s.stopping.load(Ordering::Acquire)
            || session.is_some()
            || s.checking_audio.swap(true, Ordering::AcqRel)
        {
            return Ok(None);
        }
    }
    let OrphanRecovery { recovered, failed } = {
        let _reserved = Reserved(&s.checking_audio);
        s.engine
            .recover_orphans(&s.cfg.journal_root(), s.lib.root())?
    };
    let root = s.cfg.journal_root();
    let mut issues: Vec<JournalIssue> = failed
        .iter()
        .filter(|failure| {
            !lifecycle::is_legacy_discard(&root, &failure.session_id).unwrap_or(false)
        })
        .map(|f| JournalIssue {
            session_id: f.session_id.clone(),
            code: f.code.clone(),
            corrupt: f.corrupt,
        })
        .collect();
    issues.extend(queue_recovered(s, recovered));
    let retry = issues
        .iter()
        .any(|i| !i.corrupt && !journal::library_blocked(&i.code));
    record_journal_issues(s, issues);
    Ok(Some(Swept { retry }))
}

/// Keeps the sweep's failures for diagnosis (IDs and codes only), in place
/// of the earlier sweep's; a newly corrupt journal is logged once, not at
/// every sweep.
fn record_journal_issues(s: &Shared, mut issues: Vec<JournalIssue>) {
    issues.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    let changed = {
        let mut current = lock(&s.journal_issues);
        for issue in issues.iter().filter(|i| !current.contains(i)) {
            tracing::warn!(
                session = %issue.session_id,
                code = %issue.code,
                corrupt = issue.corrupt,
                "recording journal not recovered; kept in place"
            );
        }
        let changed = *current != issues;
        *current = issues;
        changed
    };
    if changed {
        s.publish_state();
    }
}

/// A stopped recording whose final job could not be queued (`code`): kept
/// as a journal issue that may pass, so its row shows the retry, until a
/// sweep queues the job (or finds another issue).
fn note_unqueued(s: &Shared, session_id: &str, code: &str) {
    {
        let mut issues = lock(&s.journal_issues);
        issues.retain(|i| i.session_id != session_id);
        issues.push(JournalIssue {
            session_id: session_id.to_owned(),
            code: code.to_owned(),
            corrupt: false,
        });
        issues.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    }
    s.publish_state();
}

/// Sweeps journals now, then again after `journal_retry`, doubling up to
/// hourly, until a sweep runs through with nothing left to retry (a
/// journal whose final job could not be queued at stop, or one whose
/// recovery failed for a reason that may pass), instead of leaving it to
/// the periodic sweep.
fn retry_journal_recovery(s: &Arc<Shared>) {
    const LONGEST: Duration = Duration::from_secs(60 * 60);
    let s = Arc::clone(s);
    let mut delay = s.cfg.journal_retry;
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        if let Err(error) = sweep_orphans(&s) {
            tracing::warn!(code = error.code(), "recovering recording journals");
        }
        return;
    };
    rt.spawn(async move {
        loop {
            let swept = Arc::clone(&s);
            match tokio::task::spawn_blocking(move || sweep_orphans(&swept)).await {
                Ok(Ok(Some(Swept { retry: false, .. }))) | Err(_) => return,
                // A capture runs or is being admitted, or something is left
                // to retry: try again later.
                Ok(Ok(Some(_) | None)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(code = error.code(), "recovering recording journals")
                }
            }
            sweep_due_within(&s, delay);
            tokio::select! {
                () = s.shutdown.wait() => return,
                () = tokio::time::sleep(delay) => {}
            }
            delay = (delay * 2).min(LONGEST);
        }
    });
}

/// The next journal recovery sweep runs within `after`; an earlier one
/// already planned stays the one shown.
fn sweep_due_within(s: &Shared, after: Duration) {
    let now = Instant::now();
    let at = now + after;
    let mut next = lock(&s.journal_sweep_at);
    if next.is_none_or(|due| due <= now || due > at) {
        *next = Some(at);
    }
}

/// "Process now" (plan §14.2): a provisional job over the durable chunks so
/// far of `session_id`, only while it is the active recording (else
/// `stale_session`); capture continues. The cutoff is taken under the
/// session mutex that checked the session.
fn process_now(s: &Shared, session_id: &str) -> Result<Value, IpcError> {
    let (session_id, cutoff, journal_dir) = {
        let mut session = lock(&s.session);
        let active = session
            .as_mut()
            .filter(|a| a.handle.session_id() == session_id)
            .ok_or_else(|| IpcError::from(lifecycle::stale_session(session_id)))?;
        let cutoff = active.handle.snapshot_cutoff().map_err(IpcError::from)?;
        let id = active.handle.session_id().to_owned();
        let dir = s.cfg.journal_root().join(&id);
        (id, cutoff, dir)
    };
    let key = RecordJobInput::provisional_key(&session_id, &cutoff);
    let input = RecordJobInput {
        session_id: session_id.clone(),
        journal_dir,
        cutoff: Some(cutoff),
    };
    let job = enqueue_record(s, RECORD_PROVISIONAL_KIND, &key, &input)
        .map_err(IpcError::from)?
        .ok_or_else(|| {
            IpcError::new(
                lifecycle::RECORDING_DISCARDED,
                "the recording was discarded",
            )
        })?;
    s.kick.notify_one();
    Ok(json!({ "session_id": session_id, "job_id": job }))
}

/// Startup recovery: queues final jobs for journals left by a crash. A
/// journal whose final job already exists keeps that job as it is:
/// re-enqueueing would restart failed or exhausted work with fresh
/// attempts, bypassing its backoff. Failures that may pass are retried in
/// the background; an unlistable journal root is retried the same way.
fn recover_sessions(s: &Arc<Shared>) {
    match sweep_orphans(s) {
        Ok(Some(Swept { retry: false, .. })) => {}
        Ok(_) => retry_journal_recovery(s),
        Err(error) => {
            tracing::warn!(code = error.code(), "recovering recording journals");
            retry_journal_recovery(s);
        }
    }
}

/// Queues a final job for each of `orphans` that has none; returns the ones
/// that could not be queued, as issues that may pass (kept for a retry; one
/// failing never stops the others).
fn queue_recovered(s: &Shared, orphans: Vec<StoppedSession>) -> Vec<JournalIssue> {
    let mut failed = Vec::new();
    for o in orphans {
        // A discarded journal (or one already gone) is never queued again.
        if lifecycle::publication_blocked(&o.journal_dir, &o.session_id) {
            continue;
        }
        let input = RecordJobInput {
            session_id: o.session_id.clone(),
            journal_dir: o.journal_dir.clone(),
            cutoff: None,
        };
        let id = JobId::for_key(RECORD_FINAL_KIND, &o.session_id);
        let created = serde_json::to_value(input)
            .map_err(|e| Error::Internal(e.to_string()))
            .and_then(|input| {
                s.lib.write(|tx| {
                    // Checked again under the lock a discard writes its
                    // intent under.
                    if lifecycle::publication_blocked(&o.journal_dir, &o.session_id) {
                        return Ok(false);
                    }
                    let manifest = journal::load_manifest(&o.journal_dir)?;
                    journal::check_library(&manifest, tx.root())?;
                    let queue = Queue::open(tx.root())?;
                    match queue.get(&id) {
                        Ok(_) => Ok(false),
                        Err(Error::NotFound(_)) => queue
                            .enqueue(tx, RECORD_FINAL_KIND, &o.session_id, input)
                            .map(|_| true),
                        Err(e) => Err(e),
                    }
                })
            });
        if let Err(error) = created {
            tracing::warn!(
                session = %o.session_id,
                code = error.code(),
                "could not queue a recovered recording"
            );
            failed.push(JournalIssue {
                session_id: o.session_id,
                code: error.code().to_owned(),
                corrupt: matches!(error, Error::Schema(_)),
            });
        }
    }
    failed
}

/// Repairs what the index's pending marks list (or rebuilds it when they ask
/// for that) under the library lock, and only then reports it current; files
/// it could not read stay listed and are reported, never as current.
fn repair_index(s: &Shared) {
    let state = match s
        .lib
        .write(|tx| broscribe_core::index::Index::repair_pending(tx))
    {
        Ok(report) => {
            let skipped =
                report.skipped.len() + report.rebuilt.as_ref().map_or(0, |r| r.skipped.len());
            if skipped == 0 {
                IndexState::Current
            } else {
                tracing::warn!(skipped, "index repair skipped unreadable files");
                IndexState::Pending { skipped }
            }
        }
        Err(error) => {
            tracing::warn!(code = error.code(), "repairing the index");
            IndexState::Failed {
                code: error.code().to_owned(),
            }
        }
    };
    *lock(&s.index) = state;
}

/// The periodic index pass: repairs the index unless the on-disk verdict
/// (marks, database presence and completeness, read without a lock and
/// never creating anything) says it is current and this agent already
/// shows it current. Marks a busy library, unreadable files or another
/// process (a CLI write that crashed or found the library busy) left, and
/// a missing or incomplete database of a nonempty library, are read from
/// disk, never only from what this agent last saw. Publishes a changed
/// state.
fn maintain_index(s: &Shared) {
    let current =
        broscribe_core::index::Index::health(s.lib.root()).is_ok_and(|health| health.is_current());
    let before = lock(&s.index).clone();
    if current && before == IndexState::Current {
        return;
    }
    repair_index(s);
    if *lock(&s.index) != before {
        s.publish_state();
    }
}

// ---- install admission ----

/// One client connection of the agent ([`Daemon::connect`]). Dropping it
/// (the connection closed) ends an install admission it acquired and did
/// not confirm.
pub struct ClientConnection {
    daemon: Daemon,
    id: ConnectionId,
}

impl ClientConnection {
    /// Answers one request on this connection.
    pub async fn handle(&self, req: Request) -> Response {
        self.daemon.handle_on(Some(self.id), req).await
    }
}

impl Drop for ClientConnection {
    fn drop(&mut self) {
        let s = &self.daemon.shared;
        let ended = {
            let _session = lock(&s.session);
            lock(&s.install).connection_closed(self.id)
        };
        if ended {
            s.kick.notify_one();
            s.publish_state();
        }
    }
}

/// Reserves shutdown under the same locks as capture and job admission.
/// Queued work stays durable for the next launch.
fn admit_idle_shutdown(s: &Shared) -> Result<(), IpcError> {
    let session = lock(&s.session);
    let jobs = lock(&s.jobs);
    check_idle_shutdown(s, &session, &jobs)?;
    s.stopping.store(true, Ordering::Release);
    Ok(())
}

fn check_idle_shutdown(
    s: &Shared,
    session: &Option<Active>,
    jobs: &JobsState,
) -> Result<(), IpcError> {
    if s.stopping.load(Ordering::Acquire) || s.shutdown.is_triggered() {
        return Err(IpcError::new(
            IpcError::SHUTTING_DOWN,
            "the daemon is stopping",
        ));
    }
    if let Some(recording) = session.as_ref() {
        return Err(IpcError::new(
            IpcError::RECORDING_ACTIVE,
            format!(
                "recording {} is active; stop it before stopping BroScribe Agent",
                recording.handle.session_id()
            ),
        ));
    }
    lock(&s.install).refuse_capture()?;
    if s.startup_pending.load(Ordering::Acquire)
        || s.checking_audio.load(Ordering::Acquire)
        || jobs.running.is_some()
    {
        return Err(IpcError::new(
            IpcError::BUSY,
            "BroScribe Agent has work in progress",
        ));
    }
    let queue = Queue::open_existing(s.lib.root()).map_err(IpcError::from)?;
    if let Some(queue) = queue
        && queue
            .snapshot()
            .map_err(IpcError::from)?
            .jobs
            .iter()
            .any(|job| queue.is_held(job, s.probe.as_ref()))
    {
        return Err(IpcError::new(
            IpcError::BUSY,
            "the library has a running job",
        ));
    }
    Ok(())
}

/// Serializes a switch or restarted-agent hold's queue inspection with writers. Keep lock order library → session → jobs → install.
fn install(s: &Shared, connection: Option<ConnectionId>, call: &Call) -> Result<Value, IpcError> {
    if matches!(call, Call::LibrarySwitchAcquire | Call::ShutdownAcquire) {
        // Keep the IPC result intact: the outer transaction can fail while
        // taking the library lock, independently of the admission refusal.
        s.lib
            .write(|_| Ok(install_locked(s, connection, call)))
            .map_err(|error| match error {
                Error::LibraryBusy(message) => IpcError::new(IpcError::BUSY, message),
                other => IpcError::from(other),
            })?
    } else {
        install_locked(s, connection, call)
    }
}

fn install_locked(
    s: &Shared,
    connection: Option<ConnectionId>,
    call: &Call,
) -> Result<Value, IpcError> {
    let session = lock(&s.session);
    // Capturing, paused or in its stop grace (all an active session),
    // stopping (its pending-save row appears, under this mutex, only as
    // the stop's marker goes) or stopped and not saved yet; the stopped
    // list is known once startup recovery rebuilt it.
    let recording = session.is_some()
        || s.startup_pending.load(Ordering::Acquire)
        || !lock(&s.stopping_sessions).is_empty()
        || lock(&s.stopped).iter().any(|r| r.state.is_pending_save());
    let outcome = match call {
        Call::ShutdownAcquire => {
            let connection = connection.ok_or_else(|| {
                IpcError::new(
                    IpcError::INVALID_REQUEST,
                    "daemon.stop_acquire needs a client connection to bind the hold to",
                )
            })?;
            let jobs = lock(&s.jobs);
            check_idle_shutdown(s, &session, &jobs)?;
            lock(&s.install)
                .acquire_job_fence(connection)
                .map(|fence| json!({ "fence": fence }))
        }
        Call::InstallAcquire | Call::LibrarySwitchAcquire => {
            let connection = connection.ok_or_else(|| {
                IpcError::new(
                    IpcError::INVALID_REQUEST,
                    "install.acquire needs a client connection to bind the admission to",
                )
            })?;
            if s.stopping.load(Ordering::Acquire) || s.shutdown.is_triggered() {
                return Err(IpcError::new(
                    IpcError::SHUTTING_DOWN,
                    "the daemon is stopping",
                ));
            }
            if recording {
                return Err(IpcError::new(
                    IpcError::RECORDING_ACTIVE,
                    "a recording is active or not saved yet; updates install after it is saved",
                ));
            }
            // A setup capture, audio check, permission prompt, a start
            // opening its devices or a capture still stopping.
            if s.checking_audio.load(Ordering::Acquire) || lock(&s.starting).is_some() {
                return Err(IpcError::new(
                    IpcError::BUSY,
                    "another audio capture is in progress",
                ));
            }
            if matches!(call, Call::LibrarySwitchAcquire) {
                let jobs = lock(&s.jobs);
                let queue = Queue::open(s.lib.root()).map_err(IpcError::from)?;
                let snapshot = queue.snapshot().map_err(IpcError::from)?;
                let pending = snapshot.pending_work(s.probe.as_ref(), &|_| Duration::ZERO);
                if jobs.running.is_some()
                    || !pending.runnable.is_empty()
                    || pending.wake_at.is_some()
                    || snapshot
                        .jobs
                        .iter()
                        .any(|job| queue.is_held(job, s.probe.as_ref()))
                {
                    return Err(IpcError::new(
                        IpcError::BUSY,
                        "the library has work in progress",
                    ));
                }
                lock(&s.install).acquire_job_fence(connection)
            } else {
                lock(&s.install).acquire(connection)
            }
            .map(|fence| json!({ "fence": fence }))
        }
        Call::InstallConfirm { fence } => {
            if recording {
                return Err(IpcError::new(
                    IpcError::ADMISSION_LOST,
                    "a recording is active or not saved yet",
                ));
            }
            lock(&s.install)
                .confirm(*fence)
                .map(|()| json!({ "fence": fence, "durable": true }))
        }
        Call::InstallRelease { fence } => lock(&s.install)
            .release(*fence)
            .map(|()| json!({ "released": fence })),
        other => Err(IpcError::new(
            "internal",
            format!("{} is not an install admission call", other.method()),
        )),
    };
    drop(session);
    if outcome.is_ok() {
        if matches!(call, Call::InstallRelease { .. }) {
            s.kick.notify_one();
        }
        s.publish_state();
    }
    outcome
}

// ---- retention ----

/// Retention and housekeeping at startup, then every `retention_every`.
/// Between those passes an index shown current is checked at the queue's
/// re-scan cadence (`poll_every`, 30 s by default), so one another process
/// lost, damaged or left marked stops showing current within about one
/// poll, not hours later. That bound is the timer's: the check waits for a
/// blocking thread and its repair for the library lock, so a loaded host or
/// a long library write delays it. It is a read-only check that takes no lock, a repair
/// under the library lock only when the index is not current, and a state
/// published only when it changed; never per status or meter, and never a
/// session or capture lock. An index already shown pending or failed is
/// left to the next pass rather than repaired at every poll.
async fn retention_loop(s: Arc<Shared>) {
    loop {
        run_retention(&s, None).await;
        let swept = s.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::sample::reclaim_abandoned(&swept);
            // Journals whose final job could not be queued when they stopped.
            if let Err(error) = sweep_orphans(&swept) {
                tracing::warn!(code = error.code(), "recovering recording journals");
            }
            // Deletions and discards a failure or crash left unfinished.
            maintain_lifecycle(&swept, false);
            maintain_index(&swept);
        })
        .await;
        sweep_due_within(&s, s.cfg.retention_every);
        let due = tokio::time::Instant::now() + s.cfg.retention_every;
        loop {
            let left = due.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep(left.min(s.cfg.poll_every)) => {}
                () = s.shutdown.wait() => return,
            }
            if tokio::time::Instant::now() >= due {
                break;
            }
            // Only an index shown current is checked here: one already
            // pending or failed (unreadable files, a busy library) is
            // repaired again at the next pass, never every poll.
            let checked = s.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if *lock(&checked.index) == IndexState::Current {
                    maintain_index(&checked);
                }
            })
            .await;
        }
    }
}

/// `broscribe-worker retention [--meeting <id>]…` as a child process.
async fn run_retention(s: &Shared, meetings: Option<Vec<String>>) {
    let mut args: Vec<OsString> = vec!["retention".into()];
    for m in meetings.into_iter().flatten() {
        args.push("--meeting".into());
        args.push(m.into());
    }
    let refs: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let mut cmd = s.cfg.worker.tokio_command(s.lib.root(), &refs);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    match cmd.status().await {
        Ok(st) if st.success() => {}
        Ok(st) => tracing::warn!("retention run ended with {st}"),
        Err(e) => tracing::warn!("retention run did not start: {e}"),
    }
}

fn ask_notification(
    id: &str,
    params: &RecordStartParams,
    app_name: Option<&str>,
) -> NotificationRequest {
    let what = match (
        params
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty()),
        app_name,
    ) {
        (Some(title), Some(app)) => format!("“{title}” in {app}"),
        (Some(title), None) => format!("“{title}”"),
        (None, app) => app.unwrap_or("A meeting").to_owned(),
    };
    NotificationRequest {
        id: id.to_owned(),
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Meeting Detected",
            "Meeting detected",
        ),
        body: format!("{what} is using the microphone."),
        actions: vec![
            NotificationButton {
                id: "record".into(),
                label: "Record".into(),
            },
            NotificationButton {
                id: "ignore".into(),
                label: "Ignore".into(),
            },
        ],
        meeting_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::queue::JobState;
    use broscribe_ipc::{ChunkCutoff, RecordingStatus, SessionStatus, StoppedSession};
    use broscribe_platform::{DeviceDetails, DeviceInfo};

    #[test]
    fn recording_notices_keep_event_titles_and_plain_bodies() {
        let ask = ask_notification("ask-synthetic", &detected(), Some("Zoom"));
        let native = broscribe_platform_native::NativePlatform::notification_title;
        assert_eq!(ask.title, native("Meeting Detected", "Meeting detected"));
        assert_eq!(
            ask.body,
            "“Synthetic standup” in Zoom is using the microphone."
        );
        let untitled =
            ask_notification("ask-synthetic", &RecordStartParams::default(), Some("Zoom"));
        assert_eq!(untitled.body, "Zoom is using the microphone.");

        let disk = disk_low_notice("synthetic", 1_200_000_000, 2400.);
        assert_eq!(disk.title, native("Disk Space Is Low", "Disk space is low"));
        assert_eq!(
            disk.body,
            "1.2 GB free, about 40 min of recording. Recording stops and is saved before the disk fills."
        );
        assert!(
            disk_low_notice("synthetic", 180_000_000, 30.)
                .body
                .contains("under 1 min of recording")
        );

        let stop_failed = automatic_stop_failed_notice("synthetic");
        assert_eq!(
            stop_failed.title,
            native("Recording Needs Attention", "Recording needs attention")
        );
        assert_eq!(
            stop_failed.body,
            "Recording stopped, but saving couldn't start. BroScribe will try to recover it."
        );
    }

    #[tokio::test]
    async fn pending_asks_replay_oldest_first_with_their_caption() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path(), Arc::new(Engine::default()));
        daemon.set_detector(zoom_in_meeting());
        let first = daemon.ask_to_record(RecordStartParams {
            app: Some("zoom".into()),
            title: Some("Synthetic orbit review".into()),
            ..Default::default()
        });
        let duplicate = daemon.ask_to_record(RecordStartParams {
            app: Some("zoom".into()),
            title: Some("Synthetic duplicate".into()),
            ..Default::default()
        });
        let second = daemon.ask_to_record(RecordStartParams {
            app: Some("teams".into()),
            title: Some("Synthetic sprint review".into()),
            ..Default::default()
        });
        assert_eq!(first, "ask-zoom");
        assert_eq!(duplicate, first);
        assert_eq!(second, "ask-teams");
        let events = daemon.subscription_snapshot();
        let Event::State(state) = &events[0] else {
            panic!("snapshot begins with state")
        };
        let asks: Vec<_> = state
            .pending_notifications
            .iter()
            .filter(|notice| notice.is_recording_prompt())
            .collect();
        assert_eq!(
            asks.iter()
                .map(|notice| notice.id.as_str())
                .collect::<Vec<_>>(),
            [first, second]
        );
        assert_eq!(
            asks[0].body,
            "“Synthetic orbit review” in Zoom is using the microphone."
        );
        let replayed: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Notification(notice) if notice.is_recording_prompt() => Some(notice),
                _ => None,
            })
            .collect();
        assert_eq!(replayed, asks);
    }

    /// The session the daemon shows as recording now (empty when none):
    /// what a client names in a fenced recording command.
    fn current(d: &Daemon) -> String {
        d.state()
            .recording
            .map(|r| r.session_id)
            .unwrap_or_default()
    }

    /// "Stop and save" of the recording shown now.
    fn stop_active(d: &Daemon) -> Request {
        Request::new(Call::RecordStop {
            session_id: current(d),
        })
    }

    #[derive(Default)]
    struct Engine {
        fail: bool,
        /// Recording meeting-app evidence fails (the recorder thread is gone).
        fail_mark: bool,
        started: Mutex<Vec<SessionConfig>>,
        /// Sessions marked (or, with `fail_mark`, tried to mark) as having
        /// seen a meeting app on the mic.
        app_seen: Arc<Mutex<Vec<String>>>,
        /// Journals `recover_orphans` reports.
        orphans: Mutex<Vec<StoppedSession>>,
        /// Journals `recover_orphans` reports it could not recover.
        orphan_failures: Mutex<Vec<broscribe_ipc::OrphanFailure>>,
        /// The next start signals the first channel once its devices are
        /// "opening" and waits on the second before they open.
        start_gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
        /// The next session's `stop` waits for a message on this channel.
        stop_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        /// Attendance decisions the sessions journaled: (session, person, confirm).
        participants: Arc<Mutex<Vec<(String, String, bool)>>>,
        /// Journaling an attendance decision fails (metadata write fault).
        fail_participant: bool,
        /// The next session's attendance write signals the first channel and
        /// waits on the second.
        participant_gate:
            Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
        /// The latest session's notice sink, as the recorder holds it.
        events: Mutex<Option<Arc<dyn CaptureEvents>>>,
        /// What the sessions report as their health (`None`: starting).
        health: Arc<Mutex<Option<broscribe_ipc::CaptureHealth>>>,
        /// The next start's capture stops on its own with this code once
        /// open, before the start returns (and so before it is committed).
        stop_while_starting: Mutex<Option<&'static str>>,
    }
    struct Session {
        id: String,
        app_seen: Arc<Mutex<Vec<String>>>,
        fail_mark: bool,
        stop_gate: Option<std::sync::mpsc::Receiver<()>>,
        /// A synthetic journal folder with a manifest, as the recorder keeps.
        dir: PathBuf,
        participants: Arc<Mutex<Vec<(String, String, bool)>>>,
        fail_participant: bool,
        participant_gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
        tags: Vec<String>,
        paused: bool,
        health: Arc<Mutex<Option<broscribe_ipc::CaptureHealth>>>,
    }
    impl CaptureEngine for Engine {
        fn start(
            &self,
            cfg: SessionConfig,
            outputs: CaptureOutputs,
        ) -> broscribe_core::Result<Box<dyn SessionHandle>> {
            if let Some((opening, open)) = lock(&self.start_gate).take() {
                let _ = opening.send(());
                let _ = open.recv();
            }
            if self.fail {
                return Err(Error::Internal("synthetic capture failure".into()));
            }
            let id = cfg.session_id.clone();
            let dir = cfg.journal_root.join(&id);
            let mut writer = create_journal(&cfg.journal_root, &id, cfg.library_root.clone())?;
            writer.set_started_at(cfg.started_at.to_utc())?;
            let tags = cfg.tags.clone();
            lock(&self.started).push(cfg);
            if let Some(reason) = lock(&self.stop_while_starting).take() {
                outputs.events.notice(CaptureNotice::Stopped {
                    reason: reason.into(),
                });
            }
            *lock(&self.events) = Some(outputs.events);
            Ok(Box::new(Session {
                id,
                app_seen: self.app_seen.clone(),
                fail_mark: self.fail_mark,
                stop_gate: lock(&self.stop_gate).take(),
                dir,
                participants: self.participants.clone(),
                fail_participant: self.fail_participant,
                participant_gate: lock(&self.participant_gate).take(),
                tags,
                paused: false,
                health: Arc::clone(&self.health),
            }))
        }
        fn recover_orphans(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
        ) -> broscribe_core::Result<broscribe_ipc::OrphanRecovery> {
            Ok(broscribe_ipc::OrphanRecovery {
                recovered: lock(&self.orphans).clone(),
                failed: lock(&self.orphan_failures).clone(),
            })
        }
    }
    impl SessionHandle for Session {
        fn session_id(&self) -> &str {
            &self.id
        }
        fn health(&self) -> broscribe_ipc::CaptureHealth {
            lock(&self.health)
                .clone()
                .unwrap_or_else(broscribe_ipc::CaptureHealth::starting)
        }
        fn status(&self) -> SessionStatus {
            SessionStatus {
                status: if self.paused {
                    RecordingStatus::Paused
                } else {
                    RecordingStatus::Recording
                },
                elapsed_s: 0.0,
                language: None,
                setup: None,
                participants: lock(&self.participants)
                    .iter()
                    .filter(|(s, _, confirm)| *s == self.id && *confirm)
                    .map(|(_, p, _)| p.clone())
                    .collect(),
                tags: self.tags.clone(),
            }
        }
        fn pause(&mut self) -> broscribe_core::Result<()> {
            self.paused = true;
            Ok(())
        }
        fn resume(&mut self) -> broscribe_core::Result<()> {
            self.paused = false;
            Ok(())
        }
        fn set_language(&mut self, _: &str) -> broscribe_core::Result<f64> {
            Ok(0.0)
        }
        fn bookmark(&mut self, _: Option<&str>) -> broscribe_core::Result<f64> {
            Ok(0.0)
        }
        fn note(&mut self, _: &str) -> broscribe_core::Result<f64> {
            Ok(0.0)
        }
        fn add_tags(&mut self, tags: &[String]) -> broscribe_core::Result<()> {
            for tag in tags {
                if !self.tags.contains(tag) {
                    self.tags.push(tag.clone());
                }
            }
            Ok(())
        }
        fn remove_tags(&mut self, tags: &[String]) -> broscribe_core::Result<()> {
            let fold = |t: &str| t.trim().to_lowercase();
            self.tags
                .retain(|t| !tags.iter().any(|r| fold(r) == fold(t)));
            Ok(())
        }
        fn mark_meeting_app_seen(&mut self) -> broscribe_core::Result<()> {
            lock(&self.app_seen).push(self.id.clone());
            if self.fail_mark {
                return Err(Error::Internal("synthetic recorder gone".into()));
            }
            Ok(())
        }
        fn participant(
            &mut self,
            person: &str,
            _: &str,
            confirm: bool,
        ) -> broscribe_core::Result<()> {
            if let Some((entered, release)) = self.participant_gate.take() {
                let _ = entered.send(());
                let _ = release.recv();
            }
            if self.fail_participant {
                return Err(Error::io(
                    "synthetic journal",
                    std::io::Error::other("synthetic metadata write fault"),
                ));
            }
            let mut journaled = lock(&self.participants);
            journaled.retain(|(s, p, _)| !(*s == self.id && p == person));
            journaled.push((self.id.clone(), person.to_owned(), confirm));
            Ok(())
        }
        fn snapshot_cutoff(&mut self) -> broscribe_core::Result<ChunkCutoff> {
            Ok(ChunkCutoff {
                chunks: Default::default(),
                recording_s: 0.0,
            })
        }
        fn stop(self: Box<Self>) -> broscribe_core::Result<StoppedSession> {
            if let Some(gate) = &self.stop_gate {
                let _ = gate.recv();
            }
            let mut manifest = journal::load_manifest(&self.dir)?;
            manifest.state = journal::SessionState::Stopped;
            journal::write_manifest(&self.dir, &manifest)?;
            Ok(StoppedSession {
                session_id: self.id,
                journal_dir: self.dir,
            })
        }
    }

    fn create_journal(
        root: &std::path::Path,
        session: &str,
        library_root: PathBuf,
    ) -> broscribe_core::Result<journal::JournalWriter> {
        journal::JournalWriter::create(
            root,
            session,
            journal::SessionMeta {
                library_root: Some(library_root),
                ..Default::default()
            },
            5.0,
            std::process::id(),
        )
    }

    /// A permission prompt the "user" answers when the test says so.
    struct SlowPrompt(Mutex<std::sync::mpsc::Receiver<()>>);
    impl DeviceInfo for SlowPrompt {
        fn devices(&self) -> broscribe_platform::Result<Vec<DeviceDetails>> {
            Ok(Vec::new())
        }
        fn request_permission(&self, _: &str) -> broscribe_platform::Result<String> {
            let _ = lock(&self.0).recv();
            Ok("granted".into())
        }
    }

    fn daemon(tmp: &std::path::Path, engine: Arc<Engine>) -> Daemon {
        daemon_with_worker(tmp, engine, WorkerCommand::new("/bin/false"))
    }

    fn daemon_with_worker(
        tmp: &std::path::Path,
        engine: Arc<Engine>,
        worker: WorkerCommand,
    ) -> Daemon {
        let mut cfg = DaemonConfig::new(
            tmp.join("library"),
            tmp.join("data"),
            tmp.join("cache"),
            worker,
        );
        cfg.data_dir_overridden = true;
        cfg.journal_retry = Duration::from_millis(20);
        let daemon = Daemon::new(
            cfg,
            engine,
            Arc::new(broscribe_platform::fakes::FakeProcessProbe::new(
                std::process::id(),
            )),
            Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
        )
        .unwrap();
        daemon.set_setup_devices(
            Arc::new(broscribe_platform::devices::FakeDeviceInfo::default()),
            Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000)),
        );
        daemon.finish_startup();
        daemon
    }

    async fn until(what: &str, mut check: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    fn detected() -> RecordStartParams {
        RecordStartParams {
            app: Some("zoom".into()),
            title: Some("Synthetic standup".into()),
            ..Default::default()
        }
    }

    /// A detector that saw a Zoom meeting start (policy `auto`).
    fn zoom_in_meeting() -> broscribe_detect::Detector {
        let mut detector = broscribe_detect::Detector::new(
            broscribe_detect::DetectRules::builtin().unwrap(),
            Box::new(broscribe_platform::fakes::FakeMicUsageMonitor::new(vec![
                broscribe_platform::MicUser {
                    pid: 8,
                    bundle_id: Some("us.zoom.CptHost".into()),
                    responsible_pid: Some(7),
                    responsible_bundle_id: Some("us.zoom.xos".into()),
                },
            ])),
            Box::new(broscribe_platform::fakes::FakeWindowInspector::default()),
            Box::new(broscribe_detect::FakeClock::default()),
        );
        detector.poll().unwrap();
        assert_eq!(
            detector.state("zoom"),
            broscribe_detect::DetectState::Active
        );
        detector
    }

    /// Runs `auto_record` while `hold` keeps `checking_audio` set; the start
    /// must wait and then be admitted once `release` runs.
    async fn assert_auto_record_waits(d: &Daemon, release: impl FnOnce()) {
        until("setup capture admission", || {
            d.shared.checking_audio.load(Ordering::Acquire)
        })
        .await;
        let auto = tokio::spawn({
            let d = d.clone();
            async move { d.auto_record(detected(), AutoStart::Detected).await }
        });
        tokio::time::sleep(AUTO_RECORD_RETRY * 3).await;
        assert!(
            !d.shared.is_recording(),
            "start admitted during setup capture"
        );
        assert!(!auto.is_finished(), "detected start was dropped as busy");
        release();
        tokio::time::timeout(Duration::from_secs(10), auto)
            .await
            .unwrap()
            .unwrap();
        assert!(d.shared.is_recording());
        assert!(d.state().pending_notifications.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_auto_start_waits_out_helper_sample_capture() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let sample = tokio::spawn({
            let d = d.clone();
            async move {
                d.handle(Request::new(Call::CaptureSample {
                    session: broscribe_ipc::RequestId::random().to_string(),
                    seconds: 1.0,
                    purpose: "enrollment".into(),
                }))
                .await
            }
        });
        assert_auto_record_waits(&d, || {}).await;
        let _ = sample.await.unwrap();
        assert_eq!(lock(&engine.started).len(), 1);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_auto_start_waits_out_permission_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let (answer, prompt) = std::sync::mpsc::channel();
        d.set_setup_devices(
            Arc::new(SlowPrompt(Mutex::new(prompt))),
            Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000)),
        );
        let request = tokio::spawn({
            let d = d.clone();
            async move {
                d.handle(Request::new(Call::PermissionsRequest {
                    permission: "microphone".into(),
                }))
                .await
            }
        });
        assert_auto_record_waits(&d, move || answer.send(()).unwrap()).await;
        assert!(request.await.unwrap().outcome.is_ok());
        assert_eq!(lock(&engine.started).len(), 1);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_auto_start_dropped_when_meeting_ends_during_retry_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        d.set_detector(zoom_in_meeting());
        let (answer, prompt) = std::sync::mpsc::channel();
        d.set_setup_devices(
            Arc::new(SlowPrompt(Mutex::new(prompt))),
            Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000)),
        );
        let request = tokio::spawn({
            let d = d.clone();
            async move {
                d.handle(Request::new(Call::PermissionsRequest {
                    permission: "microphone".into(),
                }))
                .await
            }
        });
        until("permission prompt admission", || {
            d.shared.checking_audio.load(Ordering::Acquire)
        })
        .await;
        let auto = tokio::spawn({
            let d = d.clone();
            async move { d.auto_record(detected(), AutoStart::Detected).await }
        });
        // The first attempt is refused as busy; the start now waits a retry.
        tokio::time::sleep(AUTO_RECORD_RETRY / 4).await;
        assert!(!auto.is_finished(), "detected start was dropped as busy");
        // The meeting ends and the prompt is answered within the wait.
        lock(&d.shared.detector).as_mut().unwrap().dismiss("zoom");
        answer.send(()).unwrap();
        assert!(request.await.unwrap().outcome.is_ok());
        assert!(!d.shared.checking_audio.load(Ordering::Acquire));
        tokio::time::timeout(Duration::from_secs(10), auto)
            .await
            .unwrap()
            .unwrap();
        assert!(!d.shared.is_recording(), "ended meeting was recorded");
        assert!(lock(&engine.started).is_empty());
        d.finish().await;
    }

    /// `MeetingEnded` handled after the start's checks but before it holds
    /// the session lock finds nothing to stop; the start itself must then
    /// see that the meeting ended.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_auto_start_refused_when_meeting_ends_before_admission() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        d.set_detector(zoom_in_meeting());
        let held = lock(&d.shared.session);
        let auto = tokio::spawn({
            let d = d.clone();
            async move { d.auto_record(detected(), AutoStart::Detected).await }
        });
        // The start is now waiting for the session lock.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!auto.is_finished());
        // The meeting ends in that gap; nothing is recording to stop.
        lock(&d.shared.detector).as_mut().unwrap().dismiss("zoom");
        drop(held);
        tokio::time::timeout(Duration::from_secs(10), auto)
            .await
            .unwrap()
            .unwrap();
        assert!(!d.shared.is_recording(), "ended meeting was recorded");
        assert!(lock(&engine.started).is_empty());
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_waiting_answer_notifies_when_meeting_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        d.set_detector(zoom_in_meeting());
        let (answer, prompt) = std::sync::mpsc::channel();
        d.set_setup_devices(
            Arc::new(SlowPrompt(Mutex::new(prompt))),
            Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000)),
        );
        let request = tokio::spawn({
            let d = d.clone();
            async move {
                d.handle(Request::new(Call::PermissionsRequest {
                    permission: "microphone".into(),
                }))
                .await
            }
        });
        until("permission prompt admission", || {
            d.shared.checking_audio.load(Ordering::Acquire)
        })
        .await;
        let mut events = d.subscribe();
        let ask = d.ask_to_record(detected());
        let outcome = d
            .handle(Request::new(Call::NotificationAction {
                notification_id: ask,
                action: "record".into(),
            }))
            .await
            .outcome
            .unwrap();
        assert_eq!(outcome["waiting"], true);
        // The meeting ends while the answered start waits.
        lock(&d.shared.detector).as_mut().unwrap().dismiss("zoom");
        let notice = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(Event::Notification(n)) = events.next().await
                    && n.id.starts_with("autorecord-ended-")
                {
                    break n;
                }
            }
        })
        .await
        .expect("the user who clicked Record was told nothing");
        assert_eq!(
            notice.title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Recording Didn't Start",
                "Recording didn't start"
            )
        );
        assert_eq!(notice.body, "The Zoom call ended before recording started.");
        assert!(notice.actions.is_empty() && notice.meeting_id.is_none());
        assert!(!d.shared.is_recording());
        answer.send(()).unwrap();
        assert!(request.await.unwrap().outcome.is_ok());
        assert!(lock(&engine.started).is_empty());
        d.finish().await;
    }

    #[tokio::test]
    async fn record_auto_start_failure_is_notified_not_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(
            tmp.path(),
            Arc::new(Engine {
                fail: true,
                ..Default::default()
            }),
        );
        let mut events = d.subscribe();
        d.auto_record(detected(), AutoStart::Detected).await;
        let notice = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(Event::Notification(n)) = events.next().await {
                    break n;
                }
            }
        })
        .await
        .unwrap();
        assert!(notice.id.starts_with("autorecord-failed-"));
        assert!(
            notice
                .body
                .starts_with("A Zoom call was found, but BroScribe Agent couldn't record it.")
        );
        assert!(notice.actions.is_empty() && notice.meeting_id.is_none());
        assert!(!d.shared.is_recording());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_ask_answer_waits_out_permission_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let (answer, prompt) = std::sync::mpsc::channel();
        d.set_setup_devices(
            Arc::new(SlowPrompt(Mutex::new(prompt))),
            Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000)),
        );
        let request = tokio::spawn({
            let d = d.clone();
            async move {
                d.handle(Request::new(Call::PermissionsRequest {
                    permission: "microphone".into(),
                }))
                .await
            }
        });
        until("permission prompt admission", || {
            d.shared.checking_audio.load(Ordering::Acquire)
        })
        .await;
        let ask = d.ask_to_record(detected());
        let outcome = d
            .handle(Request::new(Call::NotificationAction {
                notification_id: ask,
                action: "record".into(),
            }))
            .await
            .outcome
            .unwrap();
        assert_eq!(outcome["waiting"], true);
        tokio::time::sleep(AUTO_RECORD_RETRY * 2).await;
        assert!(!d.shared.is_recording(), "start admitted during prompt");
        answer.send(()).unwrap();
        assert!(request.await.unwrap().outcome.is_ok());
        until("answered start admission", || d.shared.is_recording()).await;
        assert_eq!(lock(&engine.started).len(), 1);
        assert!(d.state().pending_notifications.is_empty());
        d.finish().await;
    }

    #[tokio::test]
    async fn record_manual_start_is_remote_until_the_final_job_settles_it() {
        use broscribe_platform::{DeviceTransport, MicUser, devices::FakeDeviceInfo};
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let on_mic = |bundle: &str| MicUser {
            pid: 8,
            bundle_id: Some(bundle.into()),
            responsible_pid: Some(7),
            responsible_bundle_id: Some(bundle.into()),
        };
        let detector = |users: Vec<MicUser>| {
            broscribe_detect::Detector::new(
                broscribe_detect::DetectRules::builtin().unwrap(),
                Box::new(broscribe_platform::fakes::FakeMicUsageMonitor::new(users)),
                Box::new(broscribe_platform::fakes::FakeWindowInspector::default()),
                Box::new(broscribe_detect::FakeClock::default()),
            )
        };
        // The admitted setup and the in-person alternative offered.
        let start = |params: RecordStartParams| {
            let d = d.clone();
            let engine = engine.clone();
            async move {
                d.handle(Request::new(Call::RecordStart(params)))
                    .await
                    .outcome
                    .unwrap();
                let started = lock(&engine.started);
                let cfg = started.last().unwrap();
                (cfg.setup.clone().unwrap(), cfg.in_person_setup.clone())
            }
        };
        let stop = || {
            let d = d.clone();
            async move {
                d.handle(stop_active(&d)).await.outcome.unwrap();
            }
        };
        let remote = |setup: &str| (setup.to_owned(), None);
        let unsettled = |setup: &str| (setup.to_owned(), Some("in_person".to_owned()));
        // Manual start, no meeting app on the mic: classified as remote; the
        // final job may settle on in person.
        d.set_detector(detector(Vec::new()));
        assert_eq!(start(Default::default()).await, unsettled("speakers_mic"));
        // Started before joining: the app takes the mic later, and that is
        // recorded as evidence once.
        d.set_detector(detector(vec![on_mic("us.zoom.xos")]));
        crate::detection::poll(&d.shared).unwrap();
        crate::detection::poll(&d.shared).unwrap();
        let session = d.state().recording.unwrap().session_id;
        assert_eq!(*lock(&engine.app_seen), [session]);
        stop().await;
        // Teams with detection policy off still holds the mic: remote.
        let mut teams = detector(vec![on_mic("com.microsoft.teams2")]);
        let events = teams.set_policies(
            [("teams".to_owned(), broscribe_detect::Policy::Off)].into(),
            None,
        );
        assert!(events.is_empty());
        teams.poll().unwrap();
        assert_eq!(teams.state("teams"), broscribe_detect::DetectState::Idle);
        d.set_detector(teams);
        assert_eq!(start(Default::default()).await, remote("speakers_mic"));
        stop().await;
        // A detected app start is remote.
        d.set_detector(detector(Vec::new()));
        assert_eq!(start(detected()).await, remote("speakers_mic"));
        stop().await;
        // A headphones pair: remote headphones, in person still possible.
        let mut headphones = FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true);
        headphones.output_data_source = Some("Headphones".into());
        d.set_setup_devices(
            Arc::new(FakeDeviceInfo::new(vec![
                FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
                headphones,
            ])),
            Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000)),
        );
        assert_eq!(start(Default::default()).await, unsettled("headphones"));
        stop().await;
        // An explicit setup always wins.
        let explicit = RecordStartParams {
            setup: Some("in_person".into()),
            ..Default::default()
        };
        assert_eq!(start(explicit).await, remote("in_person"));
        stop().await;
        // Without a detector nothing is observable later: remote.
        *lock(&d.shared.detector) = None;
        assert_eq!(start(Default::default()).await, remote("headphones"));
        stop().await;
        // Only the one unsettled session was ever marked.
        assert_eq!(lock(&engine.app_seen).len(), 1);
        d.finish().await;
    }

    #[tokio::test]
    async fn record_meeting_app_evidence_failure_is_warned_once_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine {
            fail_mark: true,
            ..Default::default()
        });
        let d = daemon(tmp.path(), engine.clone());
        let idle = || {
            broscribe_detect::Detector::new(
                broscribe_detect::DetectRules::builtin().unwrap(),
                Box::new(broscribe_platform::fakes::FakeMicUsageMonitor::new(
                    Vec::new(),
                )),
                Box::new(broscribe_platform::fakes::FakeWindowInspector::default()),
                Box::new(broscribe_detect::FakeClock::default()),
            )
        };
        let evidence = || {
            lock(&d.shared.session)
                .as_ref()
                .map(|a| (a.awaiting_remote_evidence, a.evidence_warned))
        };
        for _ in 0..2 {
            // A manual start with no meeting app on the mic awaits evidence.
            d.set_detector(idle());
            d.handle(Request::new(Call::RecordStart(Default::default())))
                .await
                .outcome
                .unwrap();
            assert_eq!(evidence(), Some((true, false)));
            // The app joins; every poll retries, the first failure warns.
            d.set_detector(zoom_in_meeting());
            for _ in 0..3 {
                crate::detection::poll(&d.shared).unwrap();
                assert_eq!(evidence(), Some((true, true)));
            }
            d.handle(stop_active(&d)).await.outcome.unwrap();
        }
        // Each session retried on every poll, and the next one warned anew.
        assert_eq!(lock(&engine.app_seen).len(), 6);
        d.finish().await;
    }

    #[test]
    fn recovery_keeps_an_existing_failed_final_job() {
        let tmp = tempfile::tempdir().unwrap();
        let library = tmp.path().join("library");
        let lib = Library::open(&library).unwrap();
        let probe = broscribe_platform::fakes::FakeProcessProbe::new(std::process::id());
        let session = "s-20260101-000000-abcd";
        let id = writes::enqueue(&lib, RECORD_FINAL_KIND, session, json!({})).unwrap();
        let queue = Queue::open(&library).unwrap();
        let lease = queue.claim(&lib, &id, &probe).unwrap().unwrap();
        lease.fail(&lib, "synthetic failure").unwrap();
        lease.release(&lib).unwrap();
        let before = queue.get(&id).unwrap();
        assert_eq!((before.state, before.attempts), (JobState::Failed, 1));
        let engine = Arc::new(Engine {
            orphans: Mutex::new(vec![
                StoppedSession {
                    session_id: session.into(),
                    journal_dir: journal(tmp.path(), session),
                },
                StoppedSession {
                    session_id: "s-20260101-000001-abcd".into(),
                    journal_dir: journal(tmp.path(), "s-20260101-000001-abcd"),
                },
            ]),
            ..Default::default()
        });
        let _d = daemon(tmp.path(), engine);
        let after = queue.get(&id).unwrap();
        assert_eq!(after, before, "recovery reset the failed job");
        let missing = queue
            .get(&JobId::for_key(RECORD_FINAL_KIND, "s-20260101-000001-abcd"))
            .unwrap();
        assert_eq!(missing.state, JobState::Queued);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn crash_recovery_only_touches_the_originating_library() {
        use broscribe_platform::fakes::{
            FakeCapture, FakeMicCapture, FakeProcessProbe, FakeSystemInfo,
        };
        use broscribe_platform::priority::FakeProcessPriority;

        let tmp = tempfile::tempdir().unwrap();
        let original = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&original).await;
        let root = original.shared.cfg.journal_root();
        let path = root.join(&session).join(journal::MANIFEST_FILE);
        let before = std::fs::read(&path).unwrap();
        let legacy = "s-20260101-000001-abcd";
        let legacy = journal::JournalWriter::create(
            &root,
            legacy,
            journal::SessionMeta::default(),
            5.0,
            std::process::id(),
        )
        .unwrap()
        .dir()
        .to_owned();
        let legacy_path = legacy.join(journal::MANIFEST_FILE);
        let legacy_before = std::fs::read(&legacy_path).unwrap();
        let mut config = original.shared.cfg.clone();
        drop(original); // No graceful stop or final job.
        let library_a = config.library.clone();
        config.library = tmp.path().join("other-library");
        let restart = |cfg: DaemonConfig| {
            let lease = broscribe_core::maintenance::ModeLease::acquire(
                &cfg.data_dir,
                broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
            )
            .unwrap();
            let d = Daemon::new(
                cfg,
                Arc::new(broscribe_capture::Recorder::new(
                    Arc::new(FakeMicCapture::new(16_000)),
                    Arc::new(FakeCapture::new(16_000, Vec::new())),
                    Arc::new(FakeSystemInfo::default()),
                    Arc::new(FakeProcessPriority::default()),
                )),
                Arc::new(FakeProcessProbe::new(std::process::id())),
                Arc::new(lease),
            )
            .unwrap();
            d.finish_startup();
            d
        };
        let other = restart(config.clone());
        assert!(
            Queue::open(other.shared.lib.root())
                .unwrap()
                .jobs()
                .unwrap()
                .is_empty()
        );
        assert!(other.state().stopped.is_empty());
        let issues = other.state().journal_issues;
        assert!(
            issues.iter().any(|i| i.session_id == session
                && i.code == journal::LIBRARY_MISMATCH
                && !i.corrupt)
        );
        assert!(
            issues
                .iter()
                .any(|i| i.code == journal::LIBRARY_UNKNOWN && !i.corrupt)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(&legacy_path).unwrap(), legacy_before);
        other.finish().await;
        drop(other);

        config.library = library_a;
        let original = restart(config);
        let jobs = Queue::open(original.shared.lib.root())
            .unwrap()
            .jobs()
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].key, session);
        assert_eq!(
            journal::load_manifest(path.parent().unwrap())
                .unwrap()
                .state,
            journal::SessionState::Stopped
        );
        assert_eq!(std::fs::read(&legacy_path).unwrap(), legacy_before);
        original.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_stop_holds_capture_admission_until_capture_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let engine = Arc::new(Engine {
            stop_gate: Mutex::new(Some(gate)),
            ..Default::default()
        });
        let d = daemon(tmp.path(), engine.clone());
        d.handle(Request::new(
            Call::RecordStart(RecordStartParams::default()),
        ))
        .await
        .outcome
        .unwrap();
        let stop = tokio::spawn({
            let d = d.clone();
            async move { d.handle(stop_active(&d)).await }
        });
        until("the stop to take the session", || !d.shared.is_recording()).await;
        // Platform capture is still stopping: every capture is refused.
        for call in [
            Call::AudioCheck { seconds: 0.1 },
            Call::CaptureSample {
                session: broscribe_ipc::RequestId::random().to_string(),
                seconds: 0.1,
                purpose: "enrollment".into(),
            },
            Call::RecordStart(RecordStartParams::default()),
        ] {
            let outcome = d.handle(Request::new(call)).await.outcome;
            assert_eq!(outcome.unwrap_err().code, "busy");
        }
        release.send(()).unwrap();
        assert!(stop.await.unwrap().outcome.is_ok());
        assert!(!d.shared.checking_audio.load(Ordering::Acquire));
        d.handle(Request::new(
            Call::RecordStart(RecordStartParams::default()),
        ))
        .await
        .outcome
        .unwrap();
        assert_eq!(lock(&engine.started).len(), 2);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_language_reaches_the_live_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        d.handle(Request::new(Call::RecordStart(RecordStartParams {
            language: Some("en".into()),
            ..Default::default()
        })))
        .await
        .outcome
        .unwrap();
        d.handle(Request::new(Call::RecordLanguage {
            session_id: current(&d),
            language: "pt".into(),
        }))
        .await
        .outcome
        .unwrap();
        let queue = Arc::clone(&lock(&d.shared.session).as_ref().unwrap().live_queue);
        assert_eq!(
            queue.languages_after(0),
            [(0, "en".to_owned()), (0, "pt".to_owned())]
        );
        d.finish().await;
    }

    /// Returns once an event `sub` delivered is `want` (one already in
    /// `seen` included); every event read meanwhile goes to `seen`.
    #[cfg(unix)]
    async fn wait_for(
        sub: &mut Subscriber,
        seen: &mut Vec<Event>,
        what: &str,
        want: impl Fn(&Event) -> bool,
    ) {
        if seen.iter().any(&want) {
            return;
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = sub.next().await.expect("the event stream is open");
                let found = want(&event);
                seen.push(event);
                if found {
                    return;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}: {seen:?}"));
    }

    /// A stopped recording's live supervisors are never joined before the
    /// next recording starts, so they may still publish while it runs: its
    /// ASR worker's buffered output and its identity companion's final
    /// `idle`. Those events name the stopped recording (never the running
    /// one), its late preview state never replaces the running one's, and
    /// the running recording's own transcript, identity and `idle` still
    /// arrive. Each old worker forks a helper holding its output before it
    /// reports `ready`; the helper writes the late lines only once the test
    /// writes its gate (a FIFO the test holds open), so the late output
    /// deterministically arrives after the next recording runs, and any
    /// earlier failure closes the gates and lets the helpers exit.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn record_late_live_output_of_a_stopped_recording_names_it_never_the_next() {
        use broscribe_ipc::live::{LiveIdentityStatus, SpeakersEvent};
        use broscribe_ipc::{LiveEvent, LiveSegment, LiveSpeaker, Track};
        let tmp = tempfile::tempdir().unwrap();
        let gates = tmp.path().join("gates");
        std::fs::create_dir_all(&gates).unwrap();
        // Held open for reading and writing from before the first start: a
        // helper's open never waits, a release never blocks, and dropping
        // them (the test ending, or failing) gives a waiting helper EOF.
        let mut gate_files = ["live", "live-speakers"].map(|mode| {
            let fifo = gates.join(mode);
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap();
            assert!(status.success());
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&fifo)
                .unwrap()
        });
        let ready = |model: &str| {
            LiveEvent::Ready {
                model: model.into(),
            }
            .to_line()
            .unwrap()
        };
        let segment = |text: &str| {
            LiveEvent::Segment(LiveSegment {
                track: Track::Mic,
                start_s: 0.0,
                end_s: 1.0,
                text: text.into(),
                speaker: LiveSpeaker::Me,
                words: Vec::new(),
            })
            .to_line()
            .unwrap()
        };
        let speakers_ready = SpeakersEvent::Ready {
            epoch: 1,
            diarizer: "synthetic".into(),
            embedder: "synthetic".into(),
        }
        .to_line()
        .unwrap();
        // `$3` is the worker subcommand, `live` or `live-speakers`. The
        // first worker of each kind serves the old recording: before it
        // reports `ready` it forks a helper that keeps the output open and,
        // once its gate opens, writes the late lines (a late `ready` and a
        // segment for the ASR preview); on EOF it writes nothing.
        let script = format!(
            "mode=$3
             case $mode in
               live) ready='{}'; late='{}
{}'; own='{}' ;;
               *) ready='{}'; late=''; own='' ;;
             esac
             first=
             if mkdir '{gates}'/\"$mode.first\" 2>/dev/null; then
               first=1
               ( read go < '{gates}'/\"$mode\" && [ -n \"$late\" ] &&
                 printf '%s\\n' \"$late\" ) </dev/null &
             fi
             printf '%s\\n' \"$ready\"
             [ -n \"$first\" ] || [ -z \"$own\" ] || printf '%s\\n' \"$own\"
             exec cat >/dev/null",
            ready("synthetic"),
            ready("stale"),
            segment("late from the stopped recording"),
            segment("from the running recording"),
            speakers_ready,
            gates = gates.display(),
        );
        let d = daemon_with_worker(
            tmp.path(),
            Arc::new(Engine::default()),
            WorkerCommand {
                program: "/bin/sh".into(),
                prefix: vec!["-c".into(), script.into(), "worker".into()],
                data_dir_override: None,
            },
        );
        let mut sub = d.subscribe();
        let mut seen = Vec::new();
        // Named through the start seam: the two recordings always differ.
        let start = |id: &str| {
            *lock(&d.shared.next_session_id) = Some(id.to_owned());
            Request::new(Call::RecordStart(RecordStartParams::default()))
        };
        d.handle(start("s-20261117-100000-000a"))
            .await
            .outcome
            .unwrap();
        let a = current(&d);
        assert_eq!(a, "s-20261117-100000-000a");
        let ready_model = |model: &'static str| PreviewState::Ready {
            model: model.into(),
        };
        wait_for(&mut sub, &mut seen, "the old preview ready", |e| {
            matches!(e, Event::Preview { session_id, preview }
                if *session_id == a && *preview == ready_model("synthetic"))
        })
        .await;
        wait_for(&mut sub, &mut seen, "the old identity ready", |e| {
            matches!(e, Event::LiveIdentity { session_id, identity }
                if *session_id == a && identity.status == LiveIdentityStatus::Ready)
        })
        .await;

        // Stop the old recording and start the next while the old
        // supervisors still wait on their workers' output.
        d.handle(stop_active(&d)).await.outcome.unwrap();
        assert_eq!(d.state().preview, PreviewState::Off);
        d.handle(start("s-20261117-100000-000b"))
            .await
            .outcome
            .unwrap();
        let b = current(&d);
        assert_eq!(b, "s-20261117-100000-000b");
        assert_ne!(a, b);
        wait_for(&mut sub, &mut seen, "the running preview ready", |e| {
            matches!(e, Event::Preview { session_id, preview }
                if *session_id == b && *preview == ready_model("synthetic"))
        })
        .await;
        wait_for(&mut sub, &mut seen, "the running transcript", |e| {
            matches!(e, Event::LiveTranscript { session_id, segment }
                if *session_id == b && segment.text == "from the running recording")
        })
        .await;
        wait_for(&mut sub, &mut seen, "the running identity ready", |e| {
            matches!(e, Event::LiveIdentity { session_id, identity }
                if *session_id == b && identity.status == LiveIdentityStatus::Ready)
        })
        .await;

        // Release the old workers' late output.
        for gate in &mut gate_files {
            std::io::Write::write_all(gate, b"go\n").unwrap();
        }
        // The old reader handles its late `ready` before this segment.
        wait_for(&mut sub, &mut seen, "the late transcript", |e| {
            matches!(e, Event::LiveTranscript { segment, .. }
                if segment.text == "late from the stopped recording")
        })
        .await;
        wait_for(&mut sub, &mut seen, "the old identity's final idle", |e| {
            matches!(e, Event::LiveIdentity { session_id, identity }
                if *session_id == a && identity.status == LiveIdentityStatus::Idle)
        })
        .await;
        for event in &seen {
            match event {
                Event::LiveTranscript {
                    session_id,
                    segment,
                } => assert_eq!(
                    *session_id == a,
                    segment.text == "late from the stopped recording",
                    "{event:?}"
                ),
                Event::LiveTurn(turn) => assert_eq!(
                    turn.session_id == a,
                    turn.text == "late from the stopped recording",
                    "{event:?}"
                ),
                Event::LiveIdentity {
                    session_id,
                    identity,
                } if *session_id == b => {
                    assert_ne!(identity.status, LiveIdentityStatus::Idle, "{event:?}");
                }
                Event::Preview { preview, .. } => {
                    assert_ne!(*preview, ready_model("stale"), "{event:?}");
                }
                _ => {}
            }
        }
        // The old worker's late `ready` never became the running preview.
        let state = d.state();
        assert_eq!(state.recording.map(|r| r.session_id), Some(b.clone()));
        assert_eq!(state.preview, ready_model("synthetic"));

        // The running recording's own final `idle` is still delivered.
        d.handle(stop_active(&d)).await.outcome.unwrap();
        wait_for(
            &mut sub,
            &mut seen,
            "the running identity's final idle",
            |e| {
                matches!(e, Event::LiveIdentity { session_id, identity }
                if *session_id == b && identity.status == LiveIdentityStatus::Idle)
            },
        )
        .await;
        d.finish().await;
    }

    /// Zoom (recording), Teams (auto) and Discord (ask) all hold the mic.
    fn three_meeting_apps() -> broscribe_detect::Detector {
        let users = ["us.zoom.xos", "com.microsoft.teams2", "com.hnc.Discord"]
            .into_iter()
            .enumerate()
            .map(|(n, bundle)| broscribe_platform::MicUser {
                pid: 10 + n as u32,
                bundle_id: Some(bundle.into()),
                responsible_pid: Some(10 + n as u32),
                responsible_bundle_id: Some(bundle.into()),
            })
            .collect();
        let mut detector = broscribe_detect::Detector::new(
            broscribe_detect::DetectRules::builtin().unwrap(),
            Box::new(broscribe_platform::fakes::FakeMicUsageMonitor::new(users)),
            Box::new(broscribe_platform::fakes::FakeWindowInspector::default()),
            Box::new(broscribe_detect::FakeClock::default()),
        );
        detector.poll().unwrap();
        assert_eq!(
            detector.state("teams"),
            broscribe_detect::DetectState::Active
        );
        assert_eq!(
            detector.state("discord"),
            broscribe_detect::DetectState::Asking
        );
        detector
    }

    /// Meetings detected while another is recorded: once it ends, the auto
    /// one records and the ask one is asked about.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_back_to_back_meeting_records_after_the_first_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        d.set_detector(three_meeting_apps());
        d.auto_record(detected(), AutoStart::Detected).await;
        let app = || d.state().recording.and_then(|r| r.app);
        assert_eq!(app().as_deref(), Some("zoom"));
        assert!(d.state().pending_notifications.is_empty());
        d.handle(stop_active(&d)).await.outcome.unwrap();
        until("the next meeting's recording", || {
            app().as_deref() == Some("teams")
        })
        .await;
        let asks: Vec<_> = d
            .state()
            .pending_notifications
            .into_iter()
            .filter(|n| n.id.starts_with("ask-"))
            .collect();
        assert_eq!(asks.len(), 1, "{asks:?}");
        assert_eq!(asks[0].body, "Discord is using the microphone.");
        assert_eq!(lock(&engine.started).len(), 2);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_bad_settings_fall_back_and_capture_still_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(
            broscribe_detect::DetectRules::user_path(&data),
            "[[apps]]\nnot_a_field = 1\n",
        )
        .unwrap();
        std::fs::write(Config::path(&data), "library = [unterminated").unwrap();
        assert!(config_or_default(&data).library.is_none());
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        d.start_detection(
            Box::new(broscribe_platform::fakes::FakeMicUsageMonitor::new(
                Vec::new(),
            )),
            Box::new(broscribe_platform::fakes::FakeWindowInspector::default()),
            Box::new(broscribe_detect::FakeClock::default()),
        )
        .unwrap();
        let status = d
            .handle(Request::new(Call::DetectStatus))
            .await
            .outcome
            .unwrap();
        assert_eq!(status["enabled"], true);
        assert!(status["problem"].is_string(), "{status}");
        assert!(
            status["apps"]
                .as_array()
                .is_some_and(|apps| !apps.is_empty())
        );
        d.handle(Request::new(
            Call::RecordStart(RecordStartParams::default()),
        ))
        .await
        .outcome
        .unwrap();
        d.finish().await;
    }

    fn write_config(tmp: &std::path::Path, text: &str) {
        let data = tmp.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(Config::path(&data), text).unwrap();
    }

    fn ask_bodies(d: &Daemon) -> Vec<String> {
        d.state()
            .pending_notifications
            .into_iter()
            .filter(|n| n.id.starts_with("ask-"))
            .map(|n| n.body)
            .collect()
    }

    /// Teams (`auto`) is detected while Zoom records; the user sets Teams'
    /// policy to `teams` and reloads before Zoom's recording stops. Returns
    /// how many recordings started and the ask prompts shown.
    async fn back_to_back_after_policy_change(teams: &str) -> (usize, Vec<String>) {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        d.set_detector(three_meeting_apps());
        d.auto_record(detected(), AutoStart::Detected).await;
        write_config(
            tmp.path(),
            &format!("[detect.policies]\nteams = \"{teams}\"\n"),
        );
        d.handle(Request::new(Call::DetectReload))
            .await
            .outcome
            .unwrap();
        assert!(ask_bodies(&d).is_empty(), "prompted while recording");
        d.handle(stop_active(&d)).await.outcome.unwrap();
        if teams == "auto" {
            until("the next meeting's recording", || {
                lock(&engine.started).len() == 2
            })
            .await;
        } else {
            // Any start resume_detections spawned has landed by then.
            tokio::time::sleep(AUTO_RECORD_RETRY * 2).await;
        }
        let outcome = (lock(&engine.started).len(), ask_bodies(&d));
        d.finish().await;
        outcome
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_deferred_meeting_follows_a_policy_changed_to_off() {
        let (started, asks) = back_to_back_after_policy_change("off").await;
        assert_eq!(started, 1, "Teams recorded after its policy became off");
        assert!(
            asks.iter().all(|a| !a.contains("Microsoft Teams")),
            "{asks:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_deferred_meeting_follows_a_policy_changed_to_ask() {
        let (started, asks) = back_to_back_after_policy_change("ask").await;
        assert_eq!(started, 1, "Teams recorded after its policy became ask");
        let teams: Vec<_> = asks
            .iter()
            .filter(|a| a.contains("Microsoft Teams"))
            .collect();
        assert_eq!(teams.len(), 1, "{asks:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_deferred_meeting_with_unchanged_auto_policy_still_records() {
        let (started, asks) = back_to_back_after_policy_change("auto").await;
        assert_eq!(started, 2);
        assert!(
            asks.iter().all(|a| !a.contains("Microsoft Teams")),
            "{asks:?}"
        );
    }

    /// Settings that do not load never weaken the user's policies: broken
    /// rules keep the valid overrides, an invalid override is skipped alone,
    /// an unreadable config caps every app at `ask`.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_broken_detection_settings_never_weaken_user_policies() {
        let zoom_on_mic = || {
            Box::new(broscribe_platform::fakes::FakeMicUsageMonitor::new(vec![
                broscribe_platform::MicUser {
                    pid: 7,
                    bundle_id: Some("us.zoom.xos".into()),
                    responsible_pid: Some(7),
                    responsible_bundle_id: Some("us.zoom.xos".into()),
                },
            ]))
        };
        for (detect_toml, config, want_ask, problems) in [
            (
                Some("[[apps]]\nnot_a_field = 1\n"),
                "[detect.policies]\nzoom = \"off\"\n",
                false,
                &["detect.toml"][..],
            ),
            (
                None,
                "[detect.policies]\nzoom = \"off\"\nteams = \"sometimes\"\n",
                false,
                &["\"teams\""][..],
            ),
            (
                None,
                "library = [unterminated",
                true,
                &["capped at ask"][..],
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let engine = Arc::new(Engine::default());
            let d = daemon(tmp.path(), engine.clone());
            write_config(tmp.path(), config);
            if let Some(rules) = detect_toml {
                std::fs::write(
                    broscribe_detect::DetectRules::user_path(&tmp.path().join("data")),
                    rules,
                )
                .unwrap();
            }
            d.start_detection(
                zoom_on_mic(),
                Box::new(broscribe_platform::fakes::FakeWindowInspector::default()),
                Box::new(broscribe_detect::FakeClock::default()),
            )
            .unwrap();
            let events = crate::detection::poll(&d.shared).unwrap().unwrap();
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, broscribe_detect::DetectEvent::MeetingDetected { .. })),
                "{config}: {events:?}"
            );
            let asked = events
                .iter()
                .any(|e| matches!(e, broscribe_detect::DetectEvent::AskUser { app_id, .. } if app_id == "zoom"));
            assert_eq!(asked, want_ask, "{config}: {events:?}");
            let status = d
                .handle(Request::new(Call::DetectStatus))
                .await
                .outcome
                .unwrap();
            let problem = status["problem"].as_str().unwrap_or_default();
            for want in problems {
                assert!(problem.contains(want), "{problem}");
            }
            assert!(lock(&engine.started).is_empty());
        }
    }

    /// A recording stopped by the user or by a capture failure covered the
    /// meeting detected meanwhile: nothing restarts it.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_stop_never_restarts_the_meeting_it_covered() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let zoom = || {
            lock(&d.shared.detector)
                .as_ref()
                .map(|det| det.state("zoom"))
        };
        // Started by hand while Zoom was detected, then stopped by the user.
        d.set_detector(zoom_in_meeting());
        d.handle(Request::new(
            Call::RecordStart(RecordStartParams::default()),
        ))
        .await
        .outcome
        .unwrap();
        d.handle(stop_active(&d)).await.outcome.unwrap();
        tokio::time::sleep(AUTO_RECORD_RETRY * 2).await;
        assert!(!d.shared.is_recording());
        assert_eq!(lock(&engine.started).len(), 1);
        assert_eq!(zoom(), Some(broscribe_detect::DetectState::Ignored));
        // Zoom's own recording stopped by the capture itself.
        d.set_detector(zoom_in_meeting());
        d.auto_record(detected(), AutoStart::Detected).await;
        let session = d.state().recording.unwrap().session_id;
        stop_recording(Arc::clone(&d.shared), Some(session))
            .await
            .unwrap();
        tokio::time::sleep(AUTO_RECORD_RETRY * 2).await;
        assert!(!d.shared.is_recording());
        assert_eq!(lock(&engine.started).len(), 2);
        assert_eq!(zoom(), Some(broscribe_detect::DetectState::Ignored));
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_language_tags_fit_a_live_language_frame() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let too_long = format!("en{}", "-abcdefgh".repeat(7));
        assert_eq!(too_long.len(), broscribe_ipc::live::MAX_LANGUAGE_BYTES + 1);
        for bad in ["", "e", "english", "en_US", "en-", too_long.as_str()] {
            let start = d
                .handle(Request::new(Call::RecordStart(RecordStartParams {
                    language: Some(bad.into()),
                    ..Default::default()
                })))
                .await
                .outcome;
            assert_eq!(start.unwrap_err().code, "invalid_input", "{bad:?}");
        }
        // An invalid default is ignored; the recording starts without it.
        write_config(tmp.path(), &format!("default_language = \"{too_long}\"\n"));
        d.handle(Request::new(
            Call::RecordStart(RecordStartParams::default()),
        ))
        .await
        .outcome
        .unwrap();
        let queue = Arc::clone(&lock(&d.shared.session).as_ref().unwrap().live_queue);
        assert!(queue.languages_after(0).is_empty());
        for bad in ["english", too_long.as_str()] {
            let set = d
                .handle(Request::new(Call::RecordLanguage {
                    session_id: current(&d),
                    language: bad.into(),
                }))
                .await
                .outcome;
            assert_eq!(set.unwrap_err().code, "invalid_input", "{bad:?}");
        }
        d.handle(Request::new(Call::RecordLanguage {
            session_id: current(&d),
            language: "zh-Hant-TW".into(),
        }))
        .await
        .outcome
        .unwrap();
        assert_eq!(queue.languages_after(0), [(0, "zh-Hant-TW".to_owned())]);
        d.finish().await;
    }

    /// A final job that could not be queued at stop is queued by a retry
    /// soon after, not by the six-hourly sweep.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_failed_final_job_is_queued_by_a_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        d.handle(Request::new(
            Call::RecordStart(RecordStartParams::default()),
        ))
        .await
        .outcome
        .unwrap();
        let session = d.state().recording.unwrap().session_id;
        *lock(&engine.orphans) = vec![StoppedSession {
            session_id: session.clone(),
            journal_dir: tmp.path().join("data/journal").join(&session),
        }];
        // The queue folder is a file: queueing fails until it is restored.
        let library = tmp.path().join("library");
        let queue_dir = library.join(broscribe_core::paths::QUEUE_DIR);
        std::fs::remove_dir_all(&queue_dir).unwrap();
        std::fs::write(&queue_dir, b"").unwrap();
        assert!(d.handle(stop_active(&d)).await.outcome.is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::remove_file(&queue_dir).unwrap();
        let id = JobId::for_key(RECORD_FINAL_KIND, &session);
        until("the retried final job", || {
            Queue::open(&library).is_ok_and(|q| q.get(&id).is_ok())
        })
        .await;
        d.finish().await;
    }

    /// A synthetic journal a crash left `recording`, owned by this (live)
    /// process: at a restart its PID says nothing, as PIDs are reused.
    fn journal(tmp: &std::path::Path, session: &str) -> PathBuf {
        let lib = Library::open(tmp.join("library")).unwrap();
        create_journal(
            &tmp.join("data/journal"),
            session,
            lib.root().canonicalize().unwrap(),
        )
        .unwrap()
        .dir()
        .to_owned()
    }

    async fn start(d: &Daemon) -> String {
        d.handle(Request::new(
            Call::RecordStart(RecordStartParams::default()),
        ))
        .await
        .outcome
        .unwrap();
        d.state().recording.unwrap().session_id
    }

    fn participant(session: &str, person: &str, action: ParticipantAction) -> Request {
        Request::new(Call::RecordParticipant {
            session_id: session.into(),
            request_id: broscribe_ipc::RequestId::random(),
            person_ref: person.into(),
            action,
        })
    }

    fn final_jobs(tmp: &std::path::Path) -> usize {
        Queue::open(tmp.join("library"))
            .unwrap()
            .list(RECORD_FINAL_KIND)
            .unwrap()
            .len()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_participant_creates_a_name_only_person_and_shows_it_confirmed() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let session = start(&d).await;
        let req = participant(&session, "  Quinn Example ", ParticipantAction::Confirm);
        let first = d.handle(req.clone()).await;
        let out = first.outcome.clone().unwrap();
        assert_eq!(out["status"], "confirmed");
        assert_eq!(out["created"], true);
        // A duplicate request (same id) returns the first outcome, creating nothing more.
        assert_eq!(d.handle(req).await, first);
        let people = d.shared.lib.list_people().unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].name, "Quinn Example");
        assert_eq!(out["person_id"], json!(people[0].id));
        assert!(
            d.shared
                .lib
                .load_voiceprints(&people[0].id)
                .unwrap()
                .embeddings
                .is_empty(),
            "attendance never enrolls a voice"
        );
        let recording = d.state().recording.unwrap();
        assert_eq!(recording.participants, [people[0].id.to_string()]);
        // Confirming the same name again finds that person.
        let again = d
            .handle(participant(
                &session,
                "quinn example",
                ParticipantAction::Confirm,
            ))
            .await
            .outcome
            .unwrap();
        assert_eq!(again["created"], false);
        // Removing never creates; an unknown e-mail or ID is not a name.
        for (reference, action) in [
            ("Nobody Example", ParticipantAction::Remove),
            ("nobody@example.invalid", ParticipantAction::Confirm),
            ("p-0000ff", ParticipantAction::Confirm),
        ] {
            let err = d
                .handle(participant(&session, reference, action))
                .await
                .outcome
                .unwrap_err();
            assert_eq!(err.code, "not_found", "{reference}");
        }
        assert_eq!(d.shared.lib.list_people().unwrap().len(), 1);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_participant_refuses_stale_and_ambiguous_without_creating() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let session = start(&d).await;
        let err = d
            .handle(participant(
                "s-20200101-000000-dead",
                "Riley Example",
                ParticipantAction::Confirm,
            ))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, lifecycle::STALE_SESSION);
        assert!(d.shared.lib.list_people().unwrap().is_empty());
        for name in ["Sam One", "Sam Two"] {
            let mut p = broscribe_core::schema::Person::new(d.shared.lib.new_person_id(), name);
            p.aliases = vec!["Sam".into()];
            d.shared.lib.write(|tx| tx.save_person(&p)).unwrap();
        }
        let err = d
            .handle(participant(&session, "Sam", ParticipantAction::Confirm))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, "invalid_input");
        assert_eq!(d.shared.lib.list_people().unwrap().len(), 2);
        assert!(lock(&engine.participants).is_empty());
        // After stop, the ended session creates nothing either.
        d.handle(stop_active(&d)).await.outcome.unwrap();
        let err = d
            .handle(participant(
                &session,
                "Late Example",
                ParticipantAction::Confirm,
            ))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, lifecycle::STALE_SESSION);
        assert_eq!(d.shared.lib.list_people().unwrap().len(), 2);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_participant_journal_fault_reports_the_partial_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine {
            fail_participant: true,
            ..Default::default()
        });
        let d = daemon(tmp.path(), engine.clone());
        let session = start(&d).await;
        let out = d
            .handle(participant(
                &session,
                "Avery Example",
                ParticipantAction::Confirm,
            ))
            .await
            .outcome
            .unwrap();
        assert_eq!(out["status"], "partial");
        assert_eq!(out["attendance"], false);
        let people = d.shared.lib.list_people().unwrap();
        assert_eq!(people.len(), 1, "the created person is kept");
        assert_eq!(out["person_id"], json!(people[0].id));
        assert!(d.state().recording.unwrap().participants.is_empty());
        // An existing person whose journal write fails is a plain error.
        let err = d
            .handle(participant(
                &session,
                "Avery Example",
                ParticipantAction::Confirm,
            ))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, "io");
        d.finish().await;
    }

    /// Stop waits for an attendance write in flight (library → session
    /// order), so the journal holds it before the session ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_stop_waits_for_an_attendance_write_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let engine = Arc::new(Engine {
            participant_gate: Mutex::new(Some((entered_tx, release_rx))),
            ..Default::default()
        });
        let d = daemon(tmp.path(), engine.clone());
        let session = start(&d).await;
        let confirm = tokio::spawn({
            let d = d.clone();
            let req = participant(&session, "Morgan Example", ParticipantAction::Confirm);
            async move { d.handle(req).await }
        });
        tokio::task::spawn_blocking(move || entered.recv().unwrap())
            .await
            .unwrap();
        let stop = tokio::spawn({
            let d = d.clone();
            async move { d.handle(stop_active(&d)).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!stop.is_finished(), "stop ended the session mid-write");
        release.send(()).unwrap();
        assert_eq!(
            confirm.await.unwrap().outcome.unwrap()["status"],
            "confirmed"
        );
        stop.await.unwrap().outcome.unwrap();
        assert_eq!(lock(&engine.participants).len(), 1);
        assert_eq!(final_jobs(tmp.path()), 1);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_tags_reach_the_snapshot_and_refuse_another_session() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        let out = d
            .handle(Request::new(Call::RecordTag {
                tags: vec!["+alpha".into(), "+-literal".into()],
                session_id: session.clone(),
            }))
            .await
            .outcome
            .unwrap();
        assert_eq!(out["tags"], json!(["alpha", "-literal"]));
        assert_eq!(d.state().recording.unwrap().tags, ["alpha", "-literal"]);
        let err = d
            .handle(Request::new(Call::RecordTag {
                tags: vec!["beta".into()],
                session_id: "s-20200101-000000-dead".into(),
            }))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, lifecycle::STALE_SESSION);
        assert_eq!(d.state().recording.unwrap().tags, ["alpha", "-literal"]);
        d.finish().await;
    }

    /// UI01: a command meant for an earlier recording A (a panel or menu
    /// still showing it, a CLI that resolved it before A ended) never acts on
    /// the newer recording B: each is `stale_session` and B is untouched.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_stale_session_commands_never_touch_the_next_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        *lock(&d.shared.next_session_id) = Some("s-20261117-100000-000a".into());
        let a = start(&d).await;
        d.handle(stop_active(&d)).await.outcome.unwrap();
        *lock(&d.shared.next_session_id) = Some("s-20261117-100000-000b".into());
        let b = start(&d).await;
        assert_eq!(
            (a.as_str(), b.as_str()),
            ("s-20261117-100000-000a", "s-20261117-100000-000b")
        );
        assert_ne!(a, b);
        d.handle(Request::new(Call::RecordTag {
            tags: vec!["+kept".into()],
            session_id: b.clone(),
        }))
        .await
        .outcome
        .unwrap();
        let stale = [
            Call::RecordPause {
                session_id: a.clone(),
            },
            Call::RecordResume {
                session_id: a.clone(),
            },
            Call::RecordLanguage {
                session_id: a.clone(),
                language: "pt".into(),
            },
            Call::RecordBookmark {
                session_id: a.clone(),
                label: None,
            },
            Call::RecordNote {
                session_id: a.clone(),
                text: "synthetic".into(),
            },
            Call::RecordTag {
                tags: vec!["-kept".into()],
                session_id: a.clone(),
            },
            Call::RecordProcessNow {
                session_id: a.clone(),
            },
            Call::RecordStop {
                session_id: a.clone(),
            },
        ];
        for call in stale {
            let method = call.method();
            let err = d.handle(Request::new(call)).await.outcome.unwrap_err();
            assert_eq!(err.code, lifecycle::STALE_SESSION, "{method}");
            let recording = d.state().recording.unwrap();
            assert_eq!(recording.session_id, b, "{method}");
            assert_eq!(recording.status, RecordingStatus::Recording, "{method}");
            assert_eq!(recording.tags, ["kept"], "{method}");
        }
        assert_eq!(final_jobs(tmp.path()), 1, "only A's own stop saved");
        // B's own commands still work.
        d.handle(Request::new(Call::RecordPause {
            session_id: b.clone(),
        }))
        .await
        .outcome
        .unwrap();
        assert_eq!(d.state().recording.unwrap().status, RecordingStatus::Paused);
        d.handle(stop_active(&d)).await.outcome.unwrap();
        assert_eq!(final_jobs(tmp.path()), 2);
        d.finish().await;
    }

    /// RT02: concurrent tag changes reach the journal and the "Process now"
    /// meeting in one order: the meeting ends with exactly the journal's tags.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_concurrent_tag_changes_keep_journal_and_meeting_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        let id = provisional_meeting(&d, &session, &[]);
        let mut tasks = Vec::new();
        for i in 0..24 {
            let d = d.clone();
            let session = session.clone();
            let change = if i % 2 == 0 { "+flip" } else { "-flip" };
            tasks.push(tokio::spawn(async move {
                d.handle(Request::new(Call::RecordTag {
                    tags: vec![change.into()],
                    session_id: session,
                }))
                .await
                .outcome
                .unwrap()
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let journal = d.state().recording.unwrap().tags;
        let meeting = d.shared.lib.load_meeting(&id).unwrap();
        assert_eq!(meeting.tags, journal);
        d.finish().await;
    }

    fn title(session: &str, title: &str) -> Request {
        Request::new(Call::RecordTitle {
            session_id: session.into(),
            request_id: broscribe_ipc::RequestId::random(),
            title: title.into(),
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_title_follows_the_session_until_it_is_saved() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        let out = d
            .handle(title(&session, "Live rename"))
            .await
            .outcome
            .unwrap();
        assert_eq!(out["status"], "saved");
        assert_eq!(
            d.state().recording.unwrap().title.as_deref(),
            Some("Live rename")
        );
        let journal = d.shared.cfg.journal_root().join(&session);
        assert_eq!(
            lifecycle::read_session_title(&journal)
                .unwrap()
                .unwrap()
                .title,
            "Live rename"
        );
        d.handle(stop_active(&d)).await.outcome.unwrap();
        let stopped = d.state().stopped;
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].title.as_deref(), Some("Live rename"));
        assert_eq!(stopped[0].state, StoppedRecordingState::Queued { ahead: 0 });
        // Stopped but not saved: still renamable.
        d.handle(title(&session, "After stop"))
            .await
            .outcome
            .unwrap();
        assert_eq!(d.state().stopped[0].title.as_deref(), Some("After stop"));
        let err = d
            .handle(title("s-20200101-000000-dead", "Stale"))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, lifecycle::STALE_SESSION);
        // Saved: routed to the meeting.
        let meeting = saved_meeting(&d, &session);
        let out = d.handle(title(&session, "Too late")).await.outcome.unwrap();
        assert_eq!(out["status"], "already_published");
        assert_eq!(out["meeting_id"], json!(meeting));
        d.finish().await;
    }

    /// Publishes a final meeting for `session`, as its final job would.
    fn saved_meeting(d: &Daemon, session: &str) -> broscribe_core::MeetingId {
        let lib = &d.shared.lib;
        let mut meeting = broscribe_core::schema::Meeting::new(
            lib.new_meeting_id().unwrap(),
            broscribe_core::schema::Source::Recording,
            journal_started_at(d, session),
        );
        meeting.source_id = Some(format!("recording:{session}"));
        meeting.processing.status = schema::ProcessingStatus::Done;
        lib.write(|tx| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
        meeting.id
    }

    fn journal_started_at(d: &Daemon, session: &str) -> DateTime<FixedOffset> {
        journal::load_manifest(&d.shared.cfg.journal_root().join(session))
            .unwrap()
            .started_at
            .fixed_offset()
    }

    fn discard(session: &str) -> Request {
        Request::new(Call::RecordDiscard {
            session_id: session.into(),
            request_id: broscribe_ipc::RequestId::random(),
        })
    }

    /// Discard after "process now": no final job, the provisional job and
    /// the journal go, and the intent is removed last.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_discard_of_the_active_session_after_process_now_saves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        let provisional = d
            .handle(Request::new(Call::RecordProcessNow {
                session_id: current(&d),
            }))
            .await
            .outcome
            .unwrap();
        let out = d.handle(discard(&session)).await.outcome.unwrap();
        assert_eq!(out["status"], "discarded");
        assert!(d.state().recording.is_none(), "capture stopped at once");
        assert_eq!(final_jobs(tmp.path()), 0);
        let root = d.shared.cfg.journal_root();
        until("the discard cleanup and state publication", || {
            !lifecycle::has_discard_intent(&root, &session) && d.state().stopped.is_empty()
        })
        .await;
        assert!(!root.join(&session).exists());
        let queue = Queue::open(tmp.path().join("library")).unwrap();
        let job = JobId::parse(provisional["job_id"].as_str().unwrap()).unwrap();
        assert!(queue.get(&job).is_err());
        assert!(d.state().stopped.is_empty());
        // The session is gone: a late discard is stale.
        let err = d.handle(discard(&session)).await.outcome.unwrap_err();
        assert_eq!(err.code, lifecycle::STALE_SESSION);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_discard_of_a_stopped_session_revokes_its_final_job() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        d.handle(stop_active(&d)).await.outcome.unwrap();
        assert_eq!(
            final_jobs(tmp.path()),
            1,
            "an ordinary stop queues exactly once"
        );
        let out = d.handle(discard(&session)).await.outcome.unwrap();
        assert_eq!(out["status"], "discarded");
        let root = d.shared.cfg.journal_root();
        until("the discard cleanup and state publication", || {
            !lifecycle::has_discard_intent(&root, &session) && d.state().stopped.is_empty()
        })
        .await;
        assert_eq!(final_jobs(tmp.path()), 0);
        assert!(!root.join(&session).exists());
        assert!(d.state().stopped.is_empty());
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_discard_after_saving_points_to_the_meeting() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        d.handle(stop_active(&d)).await.outcome.unwrap();
        let meeting = saved_meeting(&d, &session);
        let out = d.handle(discard(&session)).await.outcome.unwrap();
        assert_eq!(out["status"], "already_saved");
        assert_eq!(out["meeting_id"], json!(meeting));
        assert!(!lifecycle::has_discard_intent(
            &d.shared.cfg.journal_root(),
            &session
        ));
        assert!(d.shared.lib.meeting_exists(&meeting).unwrap());
        d.finish().await;
    }

    /// A crash after the intent but before capture closed the journal (still
    /// `recording`, naming a live PID): the restarted daemon finishes the
    /// discard before recovery, so the journal is never queued or published.
    #[test]
    fn record_discard_interrupted_by_a_crash_finishes_at_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let session = "s-20260101-000000-abcd";
        let dir = journal(tmp.path(), session);
        let root = tmp.path().join("data/journal");
        lifecycle::write_discard_intent(&root, session).unwrap();
        let engine = Arc::new(Engine {
            orphans: Mutex::new(vec![StoppedSession {
                session_id: session.into(),
                journal_dir: dir.clone(),
            }]),
            ..Default::default()
        });
        let d = daemon(tmp.path(), engine);
        assert!(!dir.exists());
        assert!(!lifecycle::has_discard_intent(&root, session));
        assert_eq!(final_jobs(tmp.path()), 0);
        assert!(d.state().stopped.is_empty());
    }

    /// A live capture is never erased under it: cleanup waits while the
    /// session is active, and finishes once it stopped.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_discard_cleanup_never_erases_a_live_capture() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        let root = d.shared.cfg.journal_root();
        lifecycle::write_discard_intent(&root, &session).unwrap();
        assert!(!discard_cleanup(&d.shared, &session, false).unwrap());
        assert!(
            root.join(&session)
                .join(lifecycle::SESSION_MANIFEST)
                .exists()
        );
        // A stop in progress protects it too, with no active session.
        let out = d.handle(stop_active(&d)).await.outcome.unwrap();
        assert_eq!(
            out["discarded"], true,
            "a stop finding the intent queues nothing"
        );
        assert!(lock(&d.shared.stopping_sessions).is_empty());
        lock(&d.shared.stopping_sessions).insert(session.clone());
        assert!(!discard_cleanup(&d.shared, &session, false).unwrap());
        assert!(root.join(&session).exists());
        lock(&d.shared.stopping_sessions).remove(&session);
        assert!(discard_cleanup(&d.shared, &session, false).unwrap());
        assert!(!root.join(&session).exists());
        assert_eq!(final_jobs(tmp.path()), 0);
        d.finish().await;
    }

    /// After a restart, a stopped recording not saved yet is listed again.
    #[test]
    fn record_stopped_sessions_are_listed_again_after_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data/journal");
        let mut writer = broscribe_capture::journal::JournalWriter::create(
            &root,
            "s-20260101-000000-abcd",
            broscribe_capture::journal::SessionMeta {
                library_root: Some(
                    Library::open(tmp.path().join("library"))
                        .unwrap()
                        .root()
                        .canonicalize()
                        .unwrap(),
                ),
                title: Some("Synthetic sync".into()),
                tags: vec!["alpha".into()],
                ..Default::default()
            },
            5.0,
            std::process::id(),
        )
        .unwrap();
        writer.set_started_at(chrono::Utc::now()).unwrap();
        writer.finish(0, "user").unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let stopped = d.state().stopped;
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].title.as_deref(), Some("Synthetic sync"));
        assert_eq!(stopped[0].tags, ["alpha"]);
        assert_eq!(stopped[0].state, StoppedRecordingState::Queued { ahead: 0 });
    }

    #[test]
    fn rebuilt_failed_recording_stays_quiet_but_a_new_failure_notifies() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "s-20260101-000000-abcd";
        let dir = journal(tmp.path(), id);
        journal::recover(&dir).unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let failed =
            HashMap::from([(id.to_owned(), StoppedRecordingState::Failed { error: None })]);
        assert!(set_stopped_states(&d.shared, &failed));
        assert_eq!(d.state().stopped[0].state, failed[id]);
        assert!(d.state().pending_notifications.is_empty());

        let queued = HashMap::from([(id.to_owned(), StoppedRecordingState::Queued { ahead: 0 })]);
        assert!(set_stopped_states(&d.shared, &queued));
        assert!(set_stopped_states(&d.shared, &failed));
        assert_eq!(
            d.state().pending_notifications[0].id,
            format!("failed-{id}")
        );
    }

    // ---- automatic stop (plan H) ----

    struct SharedMicUsers(Arc<broscribe_platform::fakes::FakeMicUsageMonitor>);
    impl broscribe_platform::MicUsageMonitor for SharedMicUsers {
        fn mic_users(&self) -> broscribe_platform::Result<Vec<broscribe_platform::MicUser>> {
            self.0.mic_users()
        }
    }
    struct SharedWindows(Arc<Mutex<Vec<broscribe_platform::WindowInfo>>>);
    impl broscribe_platform::WindowInspector for SharedWindows {
        fn windows(&self) -> broscribe_platform::Result<Vec<broscribe_platform::WindowInfo>> {
            Ok(lock(&self.0).clone())
        }
    }
    struct SharedClock(Arc<broscribe_detect::FakeClock>);
    impl broscribe_detect::Clock for SharedClock {
        fn now(&self) -> Instant {
            self.0.now()
        }
        fn wall(&self) -> std::time::SystemTime {
            self.0.wall()
        }
    }

    /// Synthetic meeting apps on the mic and on screen, a controlled clock,
    /// and the daemon's real detector, evaluator and fenced stop.
    struct Calls {
        mic: Arc<broscribe_platform::fakes::FakeMicUsageMonitor>,
        windows: Arc<Mutex<Vec<broscribe_platform::WindowInfo>>>,
        clock: Arc<broscribe_detect::FakeClock>,
    }

    fn app_identity(app: &str) -> (u32, &'static str, &'static str) {
        match app {
            "zoom" => (7, "us.zoom.xos", "Zoom"),
            _ => (9, "com.microsoft.teams2", "Microsoft Teams"),
        }
    }

    impl Calls {
        fn install(d: &Daemon) -> Self {
            let calls = Self {
                mic: Arc::new(broscribe_platform::fakes::FakeMicUsageMonitor::default()),
                windows: Arc::new(Mutex::new(Vec::new())),
                clock: Arc::new(broscribe_detect::FakeClock::new()),
            };
            d.set_detector(broscribe_detect::Detector::new(
                broscribe_detect::DetectRules::builtin().unwrap(),
                Box::new(SharedMicUsers(calls.mic.clone())),
                Box::new(SharedWindows(calls.windows.clone())),
                Box::new(SharedClock(calls.clock.clone())),
            ));
            d.set_reader_monitor(Arc::new(SharedMicUsers(calls.mic.clone())));
            calls
        }
        /// `apps` hold the mic now (others released it).
        fn on_mic(&self, apps: &[&str]) {
            self.mic.set(
                apps.iter()
                    .map(|app| {
                        let (pid, bundle, _) = app_identity(app);
                        broscribe_platform::MicUser {
                            pid,
                            bundle_id: Some(bundle.into()),
                            ..Default::default()
                        }
                    })
                    .collect(),
            );
        }
        /// `apps` show a window now.
        fn on_screen(&self, apps: &[&str]) {
            *lock(&self.windows) = apps
                .iter()
                .map(|app| {
                    let (pid, bundle, name) = app_identity(app);
                    broscribe_platform::WindowInfo {
                        pid,
                        app: name.into(),
                        bundle_id: Some(bundle.into()),
                        title: None,
                    }
                })
                .collect();
        }
        /// One second passes and the detector polls.
        async fn tick(&self, d: &Daemon) {
            self.clock.advance(Detector::POLL_INTERVAL);
            crate::detection::tick(d).await;
        }
        /// Ticks until the recording stops (`Some(tick)`) or `n` passed.
        async fn ticks_until_stopped(&self, d: &Daemon, n: u32) -> Option<u32> {
            for i in 1..=n {
                self.tick(d).await;
                if !d.shared.is_recording() {
                    return Some(i);
                }
            }
            None
        }
    }

    use broscribe_detect::{Clock as _, Detector};

    fn auto_status(d: &Daemon) -> broscribe_ipc::AutoStopStatus {
        d.state().recording.unwrap().auto_stop.unwrap()
    }

    /// A detected Zoom call recording, bound and enabled.
    async fn detected_zoom(d: &Daemon, calls: &Calls) -> String {
        calls.on_mic(&["zoom"]);
        calls.on_screen(&["zoom"]);
        calls.tick(d).await;
        until("the detected recording", || d.shared.is_recording()).await;
        let status = auto_status(d);
        assert!(status.enabled);
        assert_eq!(status.app.as_ref().unwrap().app_id, "zoom");
        d.state().recording.unwrap().session_id
    }

    /// WRT-71: the automatic stop notice when Zoom showed the call ended and
    /// still has the microphone.
    const ZOOM_ENDED_HOLDING_MIC: &str = "Zoom showed the call ended. BroScribe is transcribing \
         it. Zoom still has the microphone, so its next call won't start recording by itself. \
         Start it yourself if you need it.";

    fn notices(sub: &crate::bus::Subscriber) -> Vec<NotificationRequest> {
        let mut out = Vec::new();
        while let Some(event) = sub.try_next() {
            if let Event::Notification(n) = event {
                out.push(n);
            }
        }
        out
    }

    fn auto_stop(session: &str, generation: u64, enabled: bool, app: Option<&str>) -> Request {
        Request::new(Call::RecordAutoStop(RecordAutoStopParams {
            session_id: session.into(),
            request_id: broscribe_ipc::RequestId::random(),
            expected_generation: generation,
            enabled,
            app_id: app.map(str::to_owned),
        }))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_stop_saves_exactly_once_after_the_detected_call_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let sub = d.subscribe();
        let calls = Calls::install(&d);
        detected_zoom(&d, &calls).await;
        calls.on_mic(&[]);
        calls.on_screen(&[]);
        // 20 s debounce, then the visible 10 s grace.
        for _ in 0..21 {
            calls.tick(&d).await;
        }
        let pending = auto_status(&d).pending.unwrap();
        assert_eq!(
            pending.reason,
            broscribe_ipc::AutoStopReason::MicReleasedRuleChanged
        );
        assert!(pending.remaining_s <= 10 && pending.remaining_s > 0);
        let stopped = calls.ticks_until_stopped(&d, 20).await.unwrap();
        assert!(stopped <= 10, "the grace is 10 s: {stopped}");
        assert_eq!(final_jobs(tmp.path()), 1, "saved through Stop and save");
        assert_eq!(d.state().stopped.len(), 1);
        let saved = notices(&sub);
        assert!(
            saved.iter().any(|n| n.title
                == broscribe_platform_native::NativePlatform::notification_title(
                    "Recording Stopped Automatically",
                    "Recording stopped automatically"
                )
                && n.body == "Zoom released the microphone. BroScribe is transcribing it."),
            "{saved:?}"
        );
        // Nothing more happens: exactly one stop, one final job.
        for _ in 0..40 {
            calls.tick(&d).await;
        }
        assert_eq!(final_jobs(tmp.path()), 1);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_stop_ignores_other_apps_brief_releases_and_poll_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        detected_zoom(&d, &calls).await;
        // Another app's call starts and ends: not this recording's.
        calls.on_mic(&["zoom", "teams"]);
        calls.on_screen(&["zoom", "teams"]);
        for _ in 0..5 {
            calls.tick(&d).await;
        }
        calls.on_mic(&["zoom"]);
        calls.on_screen(&["zoom"]);
        assert!(calls.ticks_until_stopped(&d, 90).await.is_none());
        // Brief releases and reconnections never add up.
        for _ in 0..4 {
            calls.on_mic(&[]);
            assert!(calls.ticks_until_stopped(&d, 15).await.is_none());
            calls.on_mic(&["zoom"]);
            assert!(calls.ticks_until_stopped(&d, 2).await.is_none());
        }
        // A detector stall (sleep) restarts the count instead of finishing it.
        calls.on_mic(&[]);
        calls.on_screen(&[]);
        assert!(calls.ticks_until_stopped(&d, 25).await.is_none());
        assert!(auto_status(&d).pending.is_some());
        calls.clock.advance(Duration::from_secs(30));
        assert!(calls.ticks_until_stopped(&d, 25).await.is_none());
        assert!(d.shared.is_recording());
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_stop_keep_recording_cancels_and_a_fresh_enable_counts_anew() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        calls.on_mic(&[]);
        calls.on_screen(&[]);
        for _ in 0..22 {
            calls.tick(&d).await;
        }
        let status = auto_status(&d);
        assert!(status.pending.is_some());
        // Keep recording.
        let kept = d
            .handle(auto_stop(&session, status.generation, false, None))
            .await
            .outcome
            .unwrap();
        let kept: broscribe_ipc::AutoStopStatus = serde_json::from_value(kept).unwrap();
        assert!(!kept.enabled && kept.pending.is_none());
        assert_eq!(kept.generation, status.generation + 1);
        assert!(calls.ticks_until_stopped(&d, 90).await.is_none());
        // The old generation is stale and changes nothing.
        let err = d
            .handle(auto_stop(&session, status.generation, true, None))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, crate::autostop::STALE_BINDING);
        assert!(!auto_status(&d).enabled);
        // Re-enabled: counting starts from fresh evidence.
        d.handle(auto_stop(&session, kept.generation, true, None))
            .await
            .outcome
            .unwrap();
        let stopped = calls.ticks_until_stopped(&d, 60).await.unwrap();
        assert!(stopped >= 30, "debounce + grace from the enable: {stopped}");
        assert_eq!(final_jobs(tmp.path()), 1);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_stop_manual_recordings_stay_manual_until_explicitly_linked() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = start(&d).await;
        let status = auto_status(&d);
        assert!(!status.enabled && status.app.is_none());
        // Zoom on the mic is a candidate, never a guessed binding.
        calls.on_mic(&["zoom"]);
        calls.on_screen(&["zoom"]);
        calls.tick(&d).await;
        let status = auto_status(&d);
        assert_eq!(status.candidates.len(), 1);
        assert!(!status.enabled);
        // Enabling without a link, linking an app not on the mic, a stale
        // session or generation: refused, nothing changes.
        for (request, code) in [
            (
                auto_stop(&session, status.generation, true, None),
                "invalid_input",
            ),
            (
                auto_stop(&session, status.generation, true, Some("teams")),
                "invalid_input",
            ),
            (
                auto_stop(&session, status.generation, false, Some("zoom")),
                "invalid_input",
            ),
            (
                auto_stop(
                    "s-20200101-000000-dead",
                    status.generation,
                    true,
                    Some("zoom"),
                ),
                lifecycle::STALE_SESSION,
            ),
            (
                auto_stop(&session, status.generation + 7, true, Some("zoom")),
                crate::autostop::STALE_BINDING,
            ),
        ] {
            let err = d.handle(request).await.outcome.unwrap_err();
            assert_eq!(err.code, code);
            assert_eq!(auto_status(&d).generation, status.generation);
            assert!(!auto_status(&d).enabled);
        }
        let linked = d
            .handle(auto_stop(&session, status.generation, true, Some("zoom")))
            .await
            .outcome
            .unwrap();
        assert_eq!(linked["app"]["app_id"], "zoom");
        assert_eq!(linked["enabled"], true);
        calls.on_mic(&[]);
        calls.on_screen(&[]);
        assert!(calls.ticks_until_stopped(&d, 40).await.is_some());
        assert_eq!(final_jobs(tmp.path()), 1);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_stop_with_no_detection_at_all_never_stops_a_manual_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let calls = Calls::install(&d);
        start(&d).await;
        calls.on_mic(&["zoom"]);
        calls.on_screen(&["zoom"]);
        calls.tick(&d).await;
        calls.on_mic(&[]);
        calls.on_screen(&[]);
        assert!(calls.ticks_until_stopped(&d, 100).await.is_none());
        d.finish().await;
    }

    /// A decision made for one session never stops a replacement, even of
    /// the same app, nor the same session once anything changed.
    #[tokio::test(flavor = "multi_thread")]
    async fn auto_stop_stale_decisions_cannot_stop_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        detected_zoom(&d, &calls).await;
        calls.on_mic(&[]);
        calls.on_screen(&[]);
        // Drive the evaluator without its own stop, to hold a due decision.
        let mut decision = None;
        for _ in 0..40 {
            calls.clock.advance(Detector::POLL_INTERVAL);
            crate::detection::poll(&d.shared).unwrap();
            if let Some(found) = evaluate_auto_stop(&d.shared) {
                decision = Some(found);
                break;
            }
        }
        let old = decision.unwrap();
        // The user stops it and a new Zoom call is recorded and linked.
        d.handle(stop_active(&d)).await.outcome.unwrap();
        let replacement = start(&d).await;
        calls.on_mic(&["zoom"]);
        calls.on_screen(&["zoom"]);
        calls.tick(&d).await;
        let generation = auto_status(&d).generation;
        d.handle(auto_stop(&replacement, generation, true, Some("zoom")))
            .await
            .outcome
            .unwrap();
        d.auto_stop(old.clone()).await;
        assert_eq!(d.state().recording.unwrap().session_id, replacement);
        // Same session, same app, but the decision's evidence changed.
        let forged = crate::autostop::StopDecision {
            session_id: replacement.clone(),
            ..old
        };
        d.auto_stop(forged).await;
        assert!(d.shared.is_recording());
        assert_eq!(final_jobs(tmp.path()), 1, "only the user's stop saved");
        d.finish().await;
    }

    /// The reader's explicit post-call screen stops a recording whose app
    /// keeps the mic; the app then stays ignored until it releases the mic,
    /// so a held-mic back-to-back call is not recorded automatically.
    #[tokio::test(flavor = "multi_thread")]
    async fn auto_stop_on_an_explicit_call_end_while_the_mic_stays_held() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (reader, generation) = {
            let guard = lock(&d.shared.session);
            let active = guard.as_ref().unwrap();
            (
                active.reader.clone().unwrap(),
                active.auto_stop.reader_generation(),
            )
        };
        let mut sequence = 0;
        let mut observe = |state: broscribe_readers::CallState| {
            sequence += 1;
            let wall_ms = calls
                .clock
                .wall()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            Request::new(Call::ReaderEvents {
                session_id: session.clone(),
                batch: broscribe_readers::ReaderBatch {
                    session_id: session.clone(),
                    app: reader.0.clone(),
                    pid: reader.1,
                    reader_generation: generation,
                    call_observation: broscribe_readers::CallObservation {
                        sequence,
                        snapshot_started_at_ms: wall_ms,
                        state,
                        complete: true,
                    },
                    events: Vec::new(),
                    health: Default::default(),
                },
            })
        };
        d.handle(observe(broscribe_readers::CallState::Active))
            .await
            .outcome
            .unwrap();
        assert!(auto_status(&d).reader_available);
        // The post-call screen, re-read every 2 s; the mic stays held.
        let mut stopped = None;
        for i in 1..=60u32 {
            if i % 2 == 0 {
                d.handle(observe(broscribe_readers::CallState::Ended))
                    .await
                    .outcome
                    .unwrap();
            }
            calls.tick(&d).await;
            if !d.shared.is_recording() {
                stopped = Some(i);
                break;
            }
        }
        let stopped = stopped.unwrap();
        assert!(
            (30..=32).contains(&stopped),
            "20 s debounce + 10 s grace: {stopped}"
        );
        assert_eq!(final_jobs(tmp.path()), 1);
        let saved = notices(&sub);
        assert!(
            saved.iter().any(|n| n.title
                == broscribe_platform_native::NativePlatform::notification_title(
                    "Recording Stopped Automatically",
                    "Recording stopped automatically"
                )
                && n.body == ZOOM_ENDED_HOLDING_MIC),
            "{saved:?}"
        );
        assert_eq!(
            lock(&d.shared.detector).as_ref().unwrap().state("zoom"),
            broscribe_detect::DetectState::Ignored
        );
        // Still on the mic: never re-armed by the post-call screen.
        for _ in 0..40 {
            calls.tick(&d).await;
        }
        assert!(!d.shared.is_recording());
        assert_eq!(lock(&engine.started).len(), 1);
        d.finish().await;
    }

    /// Pause clears pending evidence and starts a new reader incarnation:
    /// batches from before it are refused, and nothing ends while paused.
    #[tokio::test(flavor = "multi_thread")]
    async fn auto_stop_pause_resets_evidence_and_rejects_earlier_reader_batches() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (reader, before) = {
            let guard = lock(&d.shared.session);
            let active = guard.as_ref().unwrap();
            (
                active.reader.clone().unwrap(),
                active.auto_stop.reader_generation(),
            )
        };
        calls.on_mic(&[]);
        calls.on_screen(&[]);
        for _ in 0..22 {
            calls.tick(&d).await;
        }
        assert!(auto_status(&d).pending.is_some());
        d.handle(Request::new(Call::RecordPause {
            session_id: current(&d),
        }))
        .await
        .outcome
        .unwrap();
        assert!(auto_status(&d).pending.is_none());
        d.handle(Request::new(Call::RecordResume {
            session_id: current(&d),
        }))
        .await
        .outcome
        .unwrap();
        let after = lock(&d.shared.session)
            .as_ref()
            .unwrap()
            .auto_stop
            .reader_generation();
        assert!(after > before);
        let err = d
            .handle(Request::new(Call::ReaderEvents {
                session_id: session.clone(),
                batch: broscribe_readers::ReaderBatch {
                    session_id: session.clone(),
                    app: reader.0,
                    pid: reader.1,
                    reader_generation: before,
                    call_observation: broscribe_readers::CallObservation {
                        sequence: 1,
                        snapshot_started_at_ms: 0,
                        state: broscribe_readers::CallState::Active,
                        complete: true,
                    },
                    events: Vec::new(),
                    health: Default::default(),
                },
            }))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, IpcError::STALE_READER);
        // Counting restarts after resume: no stop within the old remainder.
        assert!(calls.ticks_until_stopped(&d, 15).await.is_none());
        d.finish().await;
    }

    /// A reader batch of `generation` at `sequence`, stamped by the
    /// detector's wall clock.
    fn reader_batch(
        calls: &Calls,
        session: &str,
        reader: &(String, u32),
        generation: u64,
        sequence: u64,
        state: broscribe_readers::CallState,
    ) -> Request {
        let wall_ms = calls
            .clock
            .wall()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        Request::new(Call::ReaderEvents {
            session_id: session.into(),
            batch: broscribe_readers::ReaderBatch {
                session_id: session.into(),
                app: reader.0.clone(),
                pid: reader.1,
                reader_generation: generation,
                call_observation: broscribe_readers::CallObservation {
                    sequence,
                    snapshot_started_at_ms: wall_ms,
                    state,
                    complete: true,
                },
                events: Vec::new(),
                health: Default::default(),
            },
        })
    }

    fn current_reader(d: &Daemon) -> ((String, u32), u64) {
        let guard = lock(&d.shared.session);
        let active = guard.as_ref().unwrap();
        (
            active.reader.clone().unwrap(),
            active.auto_stop.reader_generation(),
        )
    }

    /// RT01 + USR01: a reader host that restarts (its own crash, a
    /// reconnect, a lagged stream) restarts its sequences at 1. Without a
    /// fresh incarnation every later observation was "old" and the call's
    /// end was never usable. `reader.attach` issues one: the old
    /// incarnation's batches are `stale_reader`, the new sequence 1 is
    /// evidence, and the real end goes through grace to Stop and save and
    /// one queued final job.
    #[tokio::test(flavor = "multi_thread")]
    async fn auto_stop_restarted_reader_takes_a_fresh_incarnation_and_the_call_end_saves() {
        use broscribe_readers::CallState;
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (reader, first) = current_reader(&d);
        for sequence in 1..=40 {
            let out = d
                .handle(reader_batch(
                    &calls,
                    &session,
                    &reader,
                    first,
                    sequence,
                    CallState::Active,
                ))
                .await
                .outcome
                .unwrap();
            assert_eq!(out["call_state_usable"], true);
        }
        // The host restarted without a new incarnation: its sequence 1 is
        // not usable evidence (the bug a restart left behind).
        let replayed = d
            .handle(reader_batch(
                &calls,
                &session,
                &reader,
                first,
                1,
                CallState::Active,
            ))
            .await
            .outcome
            .unwrap();
        assert_eq!(replayed["call_state_usable"], false);
        let attached = d
            .handle(Request::new(Call::ReaderAttach {
                session_id: session.clone(),
                reader_generation: first,
            }))
            .await
            .outcome
            .unwrap();
        let second = attached["reader_generation"].as_u64().unwrap();
        assert!(second > first);
        assert_eq!(attached["app_id"], reader.0.as_str());
        assert_eq!(attached["pid"], reader.1);
        // The earlier incarnation is fenced off, and a second host naming it
        // gets no incarnation of its own.
        let err = d
            .handle(reader_batch(
                &calls,
                &session,
                &reader,
                first,
                41,
                CallState::Active,
            ))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, IpcError::STALE_READER);
        for (session_id, told, code) in [
            (session.clone(), first, IpcError::STALE_READER),
            (
                "s-20200101-000000-dead".to_owned(),
                second,
                lifecycle::STALE_SESSION,
            ),
        ] {
            let err = d
                .handle(Request::new(Call::ReaderAttach {
                    session_id,
                    reader_generation: told,
                }))
                .await
                .outcome
                .unwrap_err();
            assert_eq!(err.code, code);
        }
        assert_eq!(current_reader(&d).1, second, "refusals issue nothing");
        // A plain subscription reports the incarnation and resets nothing.
        let snapshot = d.subscription_snapshot();
        assert!(snapshot.iter().any(|e| matches!(
            e,
            Event::StartReader { reader_generation, .. } if *reader_generation == second
        )));
        assert_eq!(current_reader(&d).1, second);
        // The new reader's sequence 1 is evidence; the call then ends while
        // Zoom keeps its microphone.
        let mut sequence = 1;
        let out = d
            .handle(reader_batch(
                &calls,
                &session,
                &reader,
                second,
                sequence,
                CallState::Active,
            ))
            .await
            .outcome
            .unwrap();
        assert_eq!(out["call_state_usable"], true);
        let mut stopped = None;
        for i in 1..=60u32 {
            if i % 2 == 0 {
                sequence += 1;
                d.handle(reader_batch(
                    &calls,
                    &session,
                    &reader,
                    second,
                    sequence,
                    CallState::Ended,
                ))
                .await
                .outcome
                .unwrap();
            }
            calls.tick(&d).await;
            if i == 25 {
                // The grace is shown, not acted on early.
                assert!(auto_status(&d).pending.is_some());
                assert!(d.shared.is_recording());
            }
            if !d.shared.is_recording() {
                stopped = Some(i);
                break;
            }
        }
        let stopped = stopped.expect("the ended call was saved");
        assert!((30..=32).contains(&stopped), "debounce + grace: {stopped}");
        assert_eq!(final_jobs(tmp.path()), 1, "one durable final job");
        let state = d.state();
        assert!(state.recording.is_none());
        assert_eq!(state.stopped.len(), 1);
        assert_eq!(state.stopped[0].session_id, session);
        assert!(notices(&sub).iter().any(|n| n.title
            == broscribe_platform_native::NativePlatform::notification_title(
                "Recording Stopped Automatically",
                "Recording stopped automatically"
            )
            && n.body == ZOOM_ENDED_HOLDING_MIC));
        d.finish().await;
    }

    /// NA35/RT23: a reader's diagnostics reach the state (and a new
    /// subscriber's snapshot) only from a batch of its current incarnation;
    /// a new incarnation forgets them and an earlier one's batch changes
    /// nothing. A reader that never sends a batch and whose host never said
    /// why (a lost host, a lost report) shows silent once its heartbeat is
    /// overdue, not starting for the rest of the recording.
    #[tokio::test(flavor = "multi_thread")]
    async fn reader_health_follows_its_incarnation_and_a_silent_start_degrades() {
        use broscribe_ipc::{ReaderDegraded, ReaderStatus};
        use broscribe_readers::{CallState, IncompleteReason, ReaderHealth};
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (reader, first) = current_reader(&d);
        let recording = |d: &Daemon| d.state().recording.unwrap();
        assert_eq!(recording(&d).reader, Some(ReaderStatus::Starting));
        assert_eq!(recording(&d).reader_health, None);
        // Issued long ago and never heard of.
        lock(&d.shared.session).as_mut().unwrap().reader_issued =
            Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        assert_eq!(
            recording(&d).reader,
            Some(ReaderStatus::Degraded {
                reason: ReaderDegraded::Silent
            })
        );

        let with_health = |generation, sequence, health: &ReaderHealth| {
            let mut request = reader_batch(
                &calls,
                &session,
                &reader,
                generation,
                sequence,
                CallState::Active,
            );
            if let Call::ReaderEvents { batch, .. } = &mut request.call {
                batch.health = health.clone();
            }
            request
        };
        let truncated = ReaderHealth {
            incomplete: Some(IncompleteReason::NodeCap),
            ..ReaderHealth::default()
        };
        d.handle(with_health(first, 1, &truncated))
            .await
            .outcome
            .unwrap();
        assert_eq!(recording(&d).reader_health.as_ref(), Some(&truncated));
        let snapshot = d.subscription_snapshot();
        let Some(Event::State(snapshot)) = snapshot.first() else {
            panic!("a subscription starts with the state");
        };
        assert_eq!(
            snapshot.recording.as_ref().unwrap().reader_health.as_ref(),
            Some(&truncated)
        );

        let attached = d
            .handle(Request::new(Call::ReaderAttach {
                session_id: session.clone(),
                reader_generation: first,
            }))
            .await
            .outcome
            .unwrap();
        let second = attached["reader_generation"].as_u64().unwrap();
        assert_eq!(recording(&d).reader_health, None);
        assert_eq!(recording(&d).reader, Some(ReaderStatus::Starting));
        let backing_off = ReaderHealth {
            incomplete: Some(IncompleteReason::Transient),
            consecutive_failures: 2,
            retry_after_s: Some(12.0),
            ..ReaderHealth::default()
        };
        let stale = d
            .handle(with_health(first, 2, &backing_off))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(stale.code, IpcError::STALE_READER);
        assert_eq!(recording(&d).reader_health, None);
        d.handle(with_health(second, 1, &backing_off))
            .await
            .outcome
            .unwrap();
        assert_eq!(recording(&d).reader_health, Some(backing_off));
        d.finish().await;
    }

    /// NA35: a reader host's typed start refusal is kept only once it names
    /// the active session, its reader's app and process and the current,
    /// unpaused incarnation nothing was heard from; it shows as refused
    /// (never silent) for that incarnation only, changes no automatic-stop
    /// evidence or health, leaves the bounded re-announcement running, and
    /// a new incarnation or a batch of it clears it. A report naming an
    /// earlier (or guessed) incarnation changes nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn reader_start_refusals_are_typed_validated_and_end_with_their_incarnation() {
        use broscribe_ipc::ReaderStatus;
        use broscribe_readers::{CallState, ReaderStartRefusal};
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (reader, first) = current_reader(&d);
        let recording = |d: &Daemon| d.state().recording.unwrap();
        let refuse = |session: &str, app: &str, pid, generation, refusal| {
            Request::new(Call::ReaderRefused {
                session_id: session.into(),
                app_id: app.into(),
                pid,
                reader_generation: generation,
                refusal,
            })
        };
        let code = |d: &Daemon, request: Request| {
            let d = d.clone();
            async move { d.handle(request).await.outcome.unwrap_err().code }
        };
        let auto_before = auto_status(&d);
        for refusal in [
            ReaderStartRefusal::Untrusted,
            ReaderStartRefusal::NoRule,
            ReaderStartRefusal::Prepare,
            ReaderStartRefusal::Attach,
            ReaderStartRefusal::Observe,
        ] {
            let kept = d
                .handle(refuse(&session, &reader.0, reader.1, first, refusal))
                .await
                .outcome
                .unwrap();
            assert_eq!(kept, json!({ "retained": true }));
            assert_eq!(
                recording(&d).reader,
                Some(ReaderStatus::Refused { refusal })
            );
            let snapshot = d.subscription_snapshot();
            let Some(Event::State(snapshot)) = snapshot.first() else {
                panic!("a subscription starts with the state");
            };
            assert_eq!(
                snapshot.recording.as_ref().unwrap().reader,
                Some(ReaderStatus::Refused { refusal })
            );
        }
        assert_eq!(auto_status(&d), auto_before, "never call evidence");
        assert_eq!(recording(&d).reader_health, None);
        // Overdue: still the reason it gave, not "stopped reporting".
        lock(&d.shared.session).as_mut().unwrap().reader_issued =
            Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        let refused = Some(ReaderStatus::Refused {
            refusal: ReaderStartRefusal::Observe,
        });
        assert_eq!(recording(&d).reader, refused);
        // Another session, app, process or incarnation changes nothing.
        let untrusted = ReaderStartRefusal::Untrusted;
        for (request, want) in [
            (
                refuse(
                    "s-20200101-000000-dead",
                    &reader.0,
                    reader.1,
                    first,
                    untrusted,
                ),
                broscribe_core::lifecycle::STALE_SESSION,
            ),
            (
                refuse(&session, "teams", reader.1, first, untrusted),
                "invalid_input",
            ),
            (
                refuse(&session, &reader.0, reader.1 + 1, first, untrusted),
                "invalid_input",
            ),
            (
                refuse(&session, &reader.0, reader.1, first + 1, untrusted),
                IpcError::STALE_READER,
            ),
        ] {
            assert_eq!(code(&d, request).await, want);
            assert_eq!(recording(&d).reader, refused);
        }
        // The incarnation is still announced again, once per silent period.
        while sub.try_next().is_some() {}
        lock(&d.shared.session).as_mut().unwrap().reader_announced =
            Instant::now().checked_sub(Duration::from_secs(11)).unwrap();
        crate::readers::tick(&d.shared);
        let mut announced = Vec::new();
        while let Some(event) = sub.try_next() {
            if let Event::StartReader {
                reader_generation, ..
            } = event
            {
                announced.push(reader_generation);
            }
        }
        assert_eq!(announced, [first]);

        // A new incarnation clears it; a late report of the old one is stale.
        let attached = d
            .handle(Request::new(Call::ReaderAttach {
                session_id: session.clone(),
                reader_generation: first,
            }))
            .await
            .outcome
            .unwrap();
        let second = attached["reader_generation"].as_u64().unwrap();
        assert_eq!(recording(&d).reader, Some(ReaderStatus::Starting));
        assert_eq!(
            code(&d, refuse(&session, &reader.0, reader.1, first, untrusted)).await,
            IpcError::STALE_READER
        );
        assert_eq!(recording(&d).reader, Some(ReaderStatus::Starting));
        // A batch of the incarnation proves it started: the refusal goes and
        // a later one is stale.
        d.handle(refuse(
            &session,
            &reader.0,
            reader.1,
            second,
            ReaderStartRefusal::Observe,
        ))
        .await
        .outcome
        .unwrap();
        d.handle(reader_batch(
            &calls,
            &session,
            &reader,
            second,
            1,
            CallState::Active,
        ))
        .await
        .outcome
        .unwrap();
        assert_eq!(recording(&d).reader, Some(ReaderStatus::Reading));
        assert_eq!(
            code(&d, refuse(&session, &reader.0, reader.1, second, untrusted)).await,
            IpcError::STALE_READER
        );
        assert_eq!(recording(&d).reader, Some(ReaderStatus::Reading));
        // Paused: no reader runs, so nothing of it is refused.
        d.handle(Request::new(Call::RecordPause {
            session_id: session.clone(),
        }))
        .await
        .outcome
        .unwrap();
        let paused = current_reader(&d).1;
        assert_eq!(
            code(&d, refuse(&session, &reader.0, reader.1, paused, untrusted)).await,
            IpcError::STALE_READER
        );
        assert_eq!(recording(&d).reader, Some(ReaderStatus::Paused));
        assert!(d.shared.is_recording(), "a refusal never stops anything");
        d.finish().await;
    }

    /// NA35: a full identity registry reaches the state as the reader's
    /// typed incomplete cause from a batch of its current incarnation only,
    /// is never call evidence (an incomplete observation cannot end the
    /// call) and goes with its incarnation.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_identity_cap_is_reader_health_only_and_ends_with_its_incarnation() {
        use broscribe_readers::{CallState, IncompleteReason, ReaderHealth};
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (reader, first) = current_reader(&d);
        let capped = ReaderHealth {
            incomplete: Some(IncompleteReason::IdentityCap),
            ..ReaderHealth::default()
        };
        let capped_batch = |generation, sequence| {
            let mut request = reader_batch(
                &calls,
                &session,
                &reader,
                generation,
                sequence,
                CallState::Unknown,
            );
            if let Call::ReaderEvents { batch, .. } = &mut request.call {
                batch.call_observation.complete = false;
                batch.health = capped.clone();
            }
            request
        };
        d.handle(capped_batch(first, 1)).await.outcome.unwrap();
        let recording = |d: &Daemon| d.state().recording.unwrap();
        assert_eq!(recording(&d).reader_health, Some(capped.clone()));
        let snapshot = d.subscription_snapshot();
        let Some(Event::State(snapshot)) = snapshot.first() else {
            panic!("a subscription starts with the state");
        };
        assert_eq!(
            snapshot.recording.as_ref().unwrap().reader_health,
            Some(capped.clone())
        );
        // The app still holds the mic: nothing ends the recording.
        assert!(calls.ticks_until_stopped(&d, 35).await.is_none());
        assert!(auto_status(&d).pending.is_none());
        let attached = d
            .handle(Request::new(Call::ReaderAttach {
                session_id: session.clone(),
                reader_generation: first,
            }))
            .await
            .outcome
            .unwrap();
        let second = attached["reader_generation"].as_u64().unwrap();
        assert_eq!(recording(&d).reader_health, None);
        assert_eq!(
            d.handle(capped_batch(first, 2))
                .await
                .outcome
                .unwrap_err()
                .code,
            IpcError::STALE_READER
        );
        assert_eq!(recording(&d).reader_health, None);
        d.handle(reader_batch(
            &calls,
            &session,
            &reader,
            second,
            1,
            CallState::Active,
        ))
        .await
        .outcome
        .unwrap();
        assert_eq!(recording(&d).reader_health, Some(ReaderHealth::default()));
        d.finish().await;
    }

    /// NA33/F3: a host that lost the answer of its `reader.attach` asks
    /// `request.status` for that request ID and learns the generation it
    /// was issued; the same request sent again is answered from the log,
    /// never issued a second time.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_reader_attach_answer_is_reconciled_by_its_request_id() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (_, first) = current_reader(&d);
        let attach = Request::new(Call::ReaderAttach {
            session_id: session.clone(),
            reader_generation: first,
        });
        let issued = d.handle(attach.clone()).await.outcome.unwrap();
        let status = d
            .handle(Request::new(Call::RequestStatus {
                request_id: attach.id.clone(),
            }))
            .await
            .outcome
            .unwrap();
        assert_eq!(
            serde_json::from_value::<RequestState>(status).unwrap(),
            RequestState::Done {
                result: issued.clone()
            }
        );
        let again = d.handle(attach).await.outcome.unwrap();
        assert_eq!(again, issued);
        assert_eq!(
            Some(current_reader(&d).1),
            issued["reader_generation"].as_u64()
        );
        d.finish().await;
    }

    /// NA33/F3: an incarnation nothing was heard from (its host failed to
    /// start it or lost the attach answer and forgot it) is announced again
    /// once per silent period, naming the incarnation current now; one that
    /// sends batches is never announced again.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unheard_reader_incarnation_is_announced_again() {
        use broscribe_readers::CallState;
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let mut sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (reader, generation) = current_reader(&d);
        let announced = |sub: &crate::bus::Subscriber| {
            let mut generations = Vec::new();
            while let Some(event) = sub.try_next() {
                if let Event::StartReader {
                    reader_generation, ..
                } = event
                {
                    generations.push(reader_generation);
                }
            }
            generations
        };
        // Recording state becomes visible before start_recording publishes
        // its reader lifecycle event. Consume that initial event explicitly,
        // rather than racing its publication with the reannouncement checks.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::StartReader {
                    session_id,
                    reader_generation,
                    ..
                } = sub.next().await.expect("the reader event stream is open")
                {
                    assert_eq!(session_id, session);
                    assert_eq!(reader_generation, generation);
                    break;
                } else {
                    continue;
                }
            }
        })
        .await
        .expect("the initial reader announcement");
        // Every check runs at one instant: when the incarnation was issued
        // (and last announced). However long the test takes to get here,
        // "recently" and "silent for long" are decided against it alone.
        let now = lock(&d.shared.session).as_ref().unwrap().reader_announced;
        let silent_for_long = |d: &Daemon| {
            lock(&d.shared.session).as_mut().unwrap().reader_announced =
                now.checked_sub(Duration::from_secs(11)).unwrap();
        };
        assert_eq!(
            announced(&sub),
            Vec::<u64>::new(),
            "no duplicate initial reader"
        );
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(announced(&sub), Vec::<u64>::new(), "heard of recently");
        silent_for_long(&d);
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(announced(&sub), [generation]);
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(announced(&sub), Vec::<u64>::new(), "once per silent period");

        d.handle(reader_batch(
            &calls,
            &session,
            &reader,
            generation,
            1,
            CallState::Active,
        ))
        .await
        .outcome
        .unwrap();
        silent_for_long(&d);
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(announced(&sub), Vec::<u64>::new(), "its reader runs");
        d.finish().await;
    }

    /// RT01/USR01 (REV02 runtime F1, native F2): a reader its host stopped
    /// after a failed snapshot, or abandoned without a word, is announced
    /// again — bounded, once per silent period, naming the incarnation
    /// current then — so the host restarts it through `reader.attach` as a
    /// new incarnation whose batches are evidence while the stopped one's
    /// are `stale_reader`. A reader whose heartbeats keep arriving is never
    /// announced again, and an announcement never issues an incarnation.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_or_abandoned_reader_is_announced_again_and_restarts_as_a_new_incarnation() {
        use broscribe_readers::{CallState, IncompleteReason, ReaderHealth};
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let mut sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (reader, first) = current_reader(&d);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(
                sub.next().await.expect("the reader event stream is open"),
                Event::StartReader { reader_generation, .. } if reader_generation == first
            ) {}
        })
        .await
        .expect("the initial reader announcement");
        let announced = |sub: &crate::bus::Subscriber| {
            let mut generations = Vec::new();
            while let Some(event) = sub.try_next() {
                if let Event::StartReader {
                    reader_generation, ..
                } = event
                {
                    generations.push(reader_generation);
                }
            }
            generations
        };
        assert_eq!(announced(&sub), Vec::<u64>::new());
        // Every check runs at one instant; batches are stamped on receipt,
        // never before it.
        let now = lock(&d.shared.session).as_ref().unwrap().reader_announced;
        let long_ago = now.checked_sub(Duration::from_secs(11)).unwrap();
        let announced_at = |d: &Daemon, at: Instant| {
            lock(&d.shared.session).as_mut().unwrap().reader_announced = at;
        };
        let batch = |generation, sequence, state, health: Option<IncompleteReason>| {
            let mut request = reader_batch(&calls, &session, &reader, generation, sequence, state);
            if let Call::ReaderEvents { batch, .. } = &mut request.call {
                batch.call_observation.complete = health.is_none();
                batch.health = ReaderHealth {
                    incomplete: health,
                    consecutive_failures: u32::from(health.is_some()),
                    ..ReaderHealth::default()
                };
            }
            request
        };

        // A reader sending heartbeats, even one backing off a transient
        // failure, is never announced again.
        for (sequence, state, health) in [
            (1, CallState::Active, None),
            (2, CallState::Unknown, Some(IncompleteReason::Transient)),
        ] {
            d.handle(batch(first, sequence, state, health))
                .await
                .outcome
                .unwrap();
            announced_at(&d, long_ago);
            crate::readers::tick_at(&d.shared, now);
            assert_eq!(announced(&sub), Vec::<u64>::new(), "its reader runs");
        }

        // The snapshot failed: the host stopped the reader and forgot it.
        announced_at(&d, now);
        d.handle(batch(
            first,
            3,
            CallState::Unknown,
            Some(IncompleteReason::Failed),
        ))
        .await
        .outcome
        .unwrap();
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(
            announced(&sub),
            Vec::<u64>::new(),
            "at most once per silent period since it was last announced"
        );
        announced_at(&d, long_ago);
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(announced(&sub), [first]);
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(announced(&sub), Vec::<u64>::new(), "once per silent period");
        assert_eq!(
            current_reader(&d).1,
            first,
            "an announcement issues nothing"
        );

        // The host restarts it through `reader.attach`.
        let attached = d
            .handle(Request::new(Call::ReaderAttach {
                session_id: session.clone(),
                reader_generation: first,
            }))
            .await
            .outcome
            .unwrap();
        let second = attached["reader_generation"].as_u64().unwrap();
        assert!(second > first);
        assert_eq!(attached["app_id"], reader.0.as_str());
        assert_eq!(attached["pid"], reader.1);
        let late = d
            .handle(batch(first, 4, CallState::Active, None))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(late.code, IpcError::STALE_READER);
        let restarted = d
            .handle(batch(second, 1, CallState::Active, None))
            .await
            .outcome
            .unwrap();
        assert_eq!(restarted["call_state_usable"], true);
        announced_at(&d, long_ago);
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(announced(&sub), Vec::<u64>::new(), "the new reader runs");

        // A host that abandoned it without a word: silent for long.
        lock(&d.shared.session).as_mut().unwrap().reader_seen = Some((long_ago, true));
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(announced(&sub), [second]);
        crate::readers::tick_at(&d.shared, now);
        assert_eq!(announced(&sub), Vec::<u64>::new(), "once per silent period");
        // A host that still ran it ignored the announcement: its next
        // heartbeat is the same incarnation's, still evidence.
        assert_eq!(current_reader(&d).1, second);
        let heartbeat = d
            .handle(batch(second, 2, CallState::Active, None))
            .await
            .outcome
            .unwrap();
        assert_eq!(heartbeat["call_state_usable"], true);
        d.finish().await;
    }

    /// A subscriber that lagged gets the authoritative snapshot (state and
    /// the current `start_reader`) right after `lagged`; other events are
    /// followed by nothing, and neither resets the reader incarnation.
    #[tokio::test(flavor = "multi_thread")]
    async fn lagged_subscribers_resync_without_resetting_the_reader() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        let (_, generation) = current_reader(&d);
        let resync = d.resync_after(&Event::Lagged { dropped: 3 });
        assert!(matches!(&resync[0], Event::State(state)
                if state.recording.as_ref().is_some_and(|r| r.session_id == session)));
        assert!(resync.iter().any(|e| matches!(
            e,
            Event::StartReader { session_id, reader_generation, .. }
                if *session_id == session && *reader_generation == generation
        )));
        assert!(
            d.resync_after(&Event::Meters {
                me_dbfs: -20.0,
                others_dbfs: -30.0,
            })
            .is_empty()
        );
        assert_eq!(current_reader(&d).1, generation);
        d.finish().await;
    }

    /// RT03: a detected Zoom recording the user relinked to the Teams call
    /// covers both: after Stop neither app is started again for the call
    /// just saved, until each releases the microphone.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_stop_after_relink_keeps_both_covered_apps_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let _sub = d.subscribe();
        let calls = Calls::install(&d);
        let session = detected_zoom(&d, &calls).await;
        calls.on_mic(&["zoom", "teams"]);
        calls.on_screen(&["zoom", "teams"]);
        calls.tick(&d).await;
        let generation = auto_status(&d).generation;
        d.handle(auto_stop(&session, generation, true, Some("teams")))
            .await
            .outcome
            .unwrap();
        assert_eq!(auto_status(&d).app.unwrap().app_id, "teams");
        d.handle(stop_active(&d)).await.outcome.unwrap();
        for app in ["zoom", "teams"] {
            assert_eq!(
                lock(&d.shared.detector).as_ref().unwrap().state(app),
                broscribe_detect::DetectState::Ignored,
                "{app}"
            );
        }
        for _ in 0..10 {
            calls.tick(&d).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!d.shared.is_recording());
        assert_eq!(lock(&engine.started).len(), 1);
        d.finish().await;
    }

    /// RT08: the reader is resolved by the rules the detector runs, so a
    /// broken `detect.toml` (detection fell back to its rules) never leaves
    /// a detected recording without its meeting reader.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_reader_follows_running_detector_rules_despite_a_broken_detect_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let _sub = d.subscribe();
        std::fs::create_dir_all(&d.shared.cfg.data_dir).unwrap();
        std::fs::write(
            d.shared.cfg.data_dir.join("detect.toml"),
            "version = 1\n[[apps]\nid = ",
        )
        .unwrap();
        assert!(broscribe_detect::DetectRules::load(&d.shared.cfg.data_dir).is_err());
        let calls = Calls::install(&d);
        detected_zoom(&d, &calls).await;
        let (reader, _) = current_reader(&d);
        assert_eq!(reader.1, 7, "the Zoom process on the mic");
        d.finish().await;
    }

    // ---- council corrections (A1, B1) ----

    /// A "Process now" meeting of `session` tagged `tags`.
    fn provisional_meeting(d: &Daemon, session: &str, tags: &[&str]) -> broscribe_core::MeetingId {
        let lib = &d.shared.lib;
        let mut meeting = broscribe_core::schema::Meeting::new(
            lib.new_meeting_id().unwrap(),
            broscribe_core::schema::Source::Recording,
            journal_started_at(d, session),
        );
        meeting.source_id = Some(format!("recording:{session}"));
        meeting.processing.status = schema::ProcessingStatus::Provisional;
        for tag in tags {
            meeting.add_tag(tag);
        }
        lib.write(|tx| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
        meeting.id
    }

    /// Removing a tag after "Process now" records the removal on the
    /// provisional meeting, which the final pass keeps (no resurrection).
    #[tokio::test(flavor = "multi_thread")]
    async fn record_tag_removal_reaches_the_provisional_meeting() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        d.handle(Request::new(Call::RecordTag {
            tags: vec!["alpha".into()],
            session_id: session.clone(),
        }))
        .await
        .outcome
        .unwrap();
        let id = provisional_meeting(&d, &session, &["alpha"]);
        d.handle(Request::new(Call::RecordTag {
            tags: vec!["-alpha".into()],
            session_id: session.clone(),
        }))
        .await
        .outcome
        .unwrap();
        let meeting = d.shared.lib.load_meeting(&id).unwrap();
        assert!(meeting.effective_tags().is_empty());
        assert!(meeting.tags_removed.iter().any(|t| t == "alpha"));
        assert!(d.state().recording.unwrap().tags.is_empty());
        d.finish().await;
    }

    /// A rename through the CLI (shared service, then a refresh) racing a
    /// panel rename: the late refresh re-reads the files and never replays
    /// the older title over the newer edit.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_title_refresh_never_overwrites_a_newer_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        let id = provisional_meeting(&d, &session, &[]);
        let cli = lifecycle::set_meeting_title(
            &d.shared.lib,
            &d.shared.cfg.journal_root(),
            &id,
            "From the command line",
        )
        .unwrap();
        assert_eq!(cli.session_id.as_deref(), Some(session.as_str()));
        d.handle(title(&session, "From the panel"))
            .await
            .outcome
            .unwrap();
        // The CLI's notification arrives late.
        let out = d
            .handle(Request::new(Call::RecordTitleRefresh {
                session_id: session.clone(),
            }))
            .await
            .outcome
            .unwrap();
        assert_eq!(out["title"], "From the panel");
        assert_eq!(
            d.state().recording.unwrap().title.as_deref(),
            Some("From the panel")
        );
        let journal = d.shared.cfg.journal_root().join(&session);
        assert_eq!(
            lifecycle::read_session_title(&journal)
                .unwrap()
                .unwrap()
                .title,
            "From the panel"
        );
        assert_eq!(
            d.shared.lib.load_meeting(&id).unwrap().title.as_deref(),
            Some("From the panel")
        );
        let err = d
            .handle(Request::new(Call::RecordTitleRefresh {
                session_id: "s-20200101-000000-dead".into(),
            }))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(err.code, lifecycle::STALE_SESSION);
        d.finish().await;
    }

    // ---- wave 2A runtime contracts ----

    /// RT12: a start opens its devices without the session mutex (status
    /// answers meanwhile, no other capture slips in) and is acknowledged
    /// only after they opened; a start cancelled meanwhile never becomes
    /// the active recording.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_start_opens_devices_outside_the_session_mutex_and_a_cancel_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let (opening_tx, opening_rx) = std::sync::mpsc::channel();
        let (open_tx, open_rx) = std::sync::mpsc::channel();
        let engine = Arc::new(Engine {
            start_gate: Mutex::new(Some((opening_tx, open_rx))),
            ..Default::default()
        });
        let d = daemon(tmp.path(), engine);
        let starting = tokio::spawn({
            let d = d.clone();
            async move {
                d.handle(Request::new(
                    Call::RecordStart(RecordStartParams::default()),
                ))
                .await
            }
        });
        tokio::task::spawn_blocking(move || opening_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let state = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking({
                let d = d.clone();
                move || d.state()
            }),
        )
        .await
        .expect("status waited on a device")
        .unwrap();
        assert!(
            state.recording.is_none(),
            "not acknowledged before it opened"
        );
        let audio = d
            .handle(Request::new(Call::AudioCheck { seconds: 1.0 }))
            .await;
        assert_eq!(audio.outcome.unwrap_err().code, IpcError::BUSY);
        cancel_start(&d.shared, "synthetic cancellation");
        open_tx.send(()).unwrap();
        let error = starting.await.unwrap().outcome.unwrap_err();
        assert_eq!(error.code, "capture_failed");
        assert!(d.state().recording.is_none());
        assert_eq!(final_jobs(tmp.path()), 0);
        assert!(!d.shared.checking_audio.load(Ordering::Acquire));
        // The next start is admitted as usual.
        start(&d).await;
        d.finish().await;
    }

    fn call(call: Call) -> Request {
        Request::new(call)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn idle_shutdown_and_capture_start_share_admission() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let id = start(&d).await;
        let refusal = d
            .handle(call(Call::ShutdownIfIdle))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(refusal.code, IpcError::RECORDING_ACTIVE);
        assert_eq!(d.state().recording.unwrap().session_id, id);
        d.finish().await;

        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let progress = broscribe_ipc::JobProgress {
            job_id: "synthetic".into(),
            kind: "transcribe".into(),
            meeting_id: None,
            phase: "started".into(),
            fraction: None,
            message: None,
        };
        assert!(d.shared.reserve_job(progress.clone()));
        let refusal = d
            .handle(call(Call::ShutdownIfIdle))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(refusal.code, IpcError::BUSY);
        lock(&d.shared.jobs).running = None;
        d.handle(call(Call::ShutdownIfIdle)).await.outcome.unwrap();
        assert!(!d.shared.reserve_job(progress));
        let refusal = d
            .handle(call(Call::RecordStart(RecordStartParams::default())))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(refusal.code, IpcError::SHUTTING_DOWN);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restarted_agent_holds_stop_admission_with_queued_work() {
        let tmp = tempfile::tempdir().unwrap();
        let first = daemon(tmp.path(), Arc::new(Engine::default()));
        let id = writes::enqueue(&first.shared.lib, "transcribe", "synthetic", json!({})).unwrap();
        first
            .handle(call(Call::ShutdownIfIdle))
            .await
            .outcome
            .unwrap();
        first.finish().await;
        drop(first);

        let restarted = daemon(tmp.path(), Arc::new(Engine::default()));
        let s = &restarted.shared;
        let queue = Queue::open(s.lib.root()).unwrap();
        let progress = broscribe_ipc::JobProgress {
            job_id: id.to_string(),
            kind: "transcribe".into(),
            meeting_id: None,
            phase: "started".into(),
            fraction: None,
            message: None,
        };
        let client = restarted.connect();
        assert_eq!(
            client
                .handle(call(Call::LibrarySwitchAcquire))
                .await
                .outcome
                .unwrap_err()
                .code,
            IpcError::BUSY,
        );
        assert!(s.reserve_job(progress.clone()));
        assert_eq!(
            client
                .handle(call(Call::ShutdownAcquire))
                .await
                .outcome
                .unwrap_err()
                .code,
            IpcError::BUSY,
        );
        lock(&s.jobs).running = None;
        client
            .handle(call(Call::ShutdownAcquire))
            .await
            .outcome
            .unwrap();
        assert!(!s.reserve_job(progress.clone()));
        assert_eq!(
            restarted
                .handle(call(Call::RecordStart(RecordStartParams::default())))
                .await
                .outcome
                .unwrap_err()
                .code,
            IpcError::INSTALLING,
        );
        assert_eq!(queue.get(&id).unwrap().state, JobState::Queued);
        drop(client);
        assert!(s.reserve_job(progress));
        restarted.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn library_switch_checks_the_disk_queue_and_wakes_after_release() {
        use broscribe_core::queue::Next;

        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let s = &d.shared;
        let client = d.connect();
        let queue = Queue::open(s.lib.root()).unwrap();
        let id = writes::enqueue(&s.lib, "transcribe", "synthetic", json!({})).unwrap();
        assert_eq!(d.state().jobs.waiting, 0);
        let refusal = client
            .handle(call(Call::LibrarySwitchAcquire))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(refusal.code, IpcError::BUSY, "an unscanned queued job");

        let lease = queue.claim(&s.lib, &id, s.probe.as_ref()).unwrap().unwrap();
        let refusal = client
            .handle(call(Call::LibrarySwitchAcquire))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(refusal.code, IpcError::BUSY, "an external held job");
        s.lib
            .write(|tx| {
                lease.update_in(
                    tx,
                    Some(Next::Requeue {
                        not_before: Some(schema::now() + chrono::TimeDelta::hours(1)),
                    }),
                    |_| Ok(()),
                )
            })
            .unwrap();
        lease.release(&s.lib).unwrap();
        let refusal = client
            .handle(call(Call::LibrarySwitchAcquire))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(refusal.code, IpcError::BUSY, "a deferred job");

        s.lib
            .write(|tx| queue.set_state(tx, &id, JobState::Done, None))
            .unwrap();
        s.kick.notified().await; // Startup's wake.
        let acquired = client
            .handle(call(Call::LibrarySwitchAcquire))
            .await
            .outcome
            .unwrap();
        let fence = acquired["fence"].as_u64().unwrap();
        client
            .handle(call(Call::InstallRelease { fence }))
            .await
            .outcome
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), s.kick.notified())
            .await
            .unwrap();
        client
            .handle(call(Call::LibrarySwitchAcquire))
            .await
            .outcome
            .unwrap();
        drop(client);
        tokio::time::timeout(Duration::from_secs(1), s.kick.notified())
            .await
            .unwrap();
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn parity_library_switch_admission_waits_and_fences_new_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let progress = broscribe_ipc::JobProgress {
            job_id: "synthetic-job".into(),
            kind: "transcribe".into(),
            meeting_id: None,
            phase: "started".into(),
            fraction: Some(0.0),
            message: None,
        };
        let client = d.connect();
        assert!(d.shared.reserve_job(progress.clone()));
        let busy = client
            .handle(call(Call::LibrarySwitchAcquire))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(busy.code, IpcError::BUSY);
        lock(&d.shared.jobs).running = None;
        let result = client
            .handle(call(Call::LibrarySwitchAcquire))
            .await
            .outcome
            .unwrap();
        let fence = result["fence"].as_u64().unwrap();
        assert!(!d.shared.reserve_job(progress.clone()));
        let capture = d
            .handle(call(Call::RecordStart(RecordStartParams::default())))
            .await
            .outcome
            .unwrap_err();
        assert_eq!(capture.code, IpcError::INSTALLING);
        assert_eq!(
            client
                .handle(call(Call::InstallConfirm { fence }))
                .await
                .outcome
                .unwrap_err()
                .code,
            IpcError::ADMISSION_LOST
        );
        drop(client);
        assert!(d.shared.reserve_job(progress));
        d.finish().await;
    }

    /// SH23: an admission binds to its connection, refuses every capture
    /// start while held, and ends with that connection unless confirmed;
    /// it is refused while a recording runs or is not saved yet.
    #[tokio::test(flavor = "multi_thread")]
    async fn install_admission_fences_every_capture_and_ends_with_its_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let unbound = d.handle(call(Call::InstallAcquire)).await;
        assert_eq!(unbound.outcome.unwrap_err().code, IpcError::INVALID_REQUEST);
        let holder = d.connect();
        let fence = holder
            .handle(call(Call::InstallAcquire))
            .await
            .outcome
            .unwrap()["fence"]
            .as_u64()
            .unwrap();
        assert_eq!(
            d.state().install,
            Some(broscribe_ipc::InstallHold {
                fence,
                durable: false
            })
        );
        for capture in [
            Call::RecordStart(RecordStartParams::default()),
            Call::AudioCheck { seconds: 1.0 },
            Call::CaptureSample {
                session: "sample-1".into(),
                seconds: 1.0,
                purpose: "enrollment".into(),
            },
            Call::PermissionsRequest {
                permission: "microphone".into(),
            },
        ] {
            let method = capture.method();
            let out = d.handle(call(capture)).await.outcome.unwrap_err();
            assert_eq!(out.code, IpcError::INSTALLING, "{method}");
        }
        let second = d.connect().handle(call(Call::InstallAcquire)).await;
        assert_eq!(second.outcome.unwrap_err().code, IpcError::BUSY);
        drop(holder);
        assert_eq!(d.state().install, None);
        let lost = d.handle(call(Call::InstallConfirm { fence })).await;
        assert_eq!(lost.outcome.unwrap_err().code, IpcError::ADMISSION_LOST);
        let session = start(&d).await;
        let updater = d.connect();
        let refused = updater.handle(call(Call::InstallAcquire)).await;
        assert_eq!(
            refused.outcome.unwrap_err().code,
            IpcError::RECORDING_ACTIVE
        );
        d.handle(stop_active(&d)).await.outcome.unwrap();
        assert!(d.state().stopped.iter().any(|r| r.session_id == session));
        let unsaved = updater.handle(call(Call::InstallAcquire)).await;
        assert_eq!(
            unsaved.outcome.unwrap_err().code,
            IpcError::RECORDING_ACTIVE
        );
        d.finish().await;
    }

    /// SH23: a stop held between its capture closing and its pending-save
    /// row (neither active nor listed yet) still refuses install admission;
    /// the row then keeps refusing it until the recording is saved.
    #[tokio::test(flavor = "multi_thread")]
    async fn install_admission_is_refused_across_a_stops_handover() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        let (reached_tx, reached) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        *lock(&d.shared.stop_handover.0) = Some((reached_tx, release_rx));
        let stop = stop_active(&d);
        let stopping = {
            let d = d.clone();
            tokio::spawn(async move { d.handle(stop).await })
        };
        tokio::task::spawn_blocking(move || reached.recv())
            .await
            .unwrap()
            .unwrap();
        let state = d.state();
        assert!(state.recording.is_none());
        assert!(!state.stopped.iter().any(|r| r.session_id == session));
        let updater = d.connect();
        let handover = updater.handle(call(Call::InstallAcquire)).await;
        assert_eq!(
            handover.outcome.unwrap_err().code,
            IpcError::RECORDING_ACTIVE
        );
        assert_eq!(d.state().install, None);
        release.send(()).unwrap();
        stopping.await.unwrap().outcome.unwrap();
        assert!(d.state().stopped.iter().any(|r| r.session_id == session));
        let unsaved = updater.handle(call(Call::InstallAcquire)).await;
        assert_eq!(
            unsaved.outcome.unwrap_err().code,
            IpcError::RECORDING_ACTIVE
        );
        d.finish().await;
    }

    /// The next state event whose recording health is `want`.
    async fn health_shown(sub: &mut Subscriber, want: &broscribe_ipc::CaptureHealth) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match sub.next().await {
                    Some(Event::State(state))
                        if state.recording.as_ref().map(|r| &r.health) == Some(want) =>
                    {
                        return;
                    }
                    Some(_) => {}
                    None => panic!("the subscription ended"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no state showed {want:?}"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn denied_capture_permission_is_reported_and_rechecked_without_prompting() {
        use broscribe_ipc::{CapturePermission, Track};
        use broscribe_platform::devices::FakeDeviceInfo;
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let daemon = daemon(tmp.path(), engine.clone());
        let devices = Arc::new(FakeDeviceInfo::default());
        lock(&devices.permissions).extend([
            ("microphone".into(), "denied".into()),
            ("system_audio".into(), "granted".into()),
        ]);
        {
            *lock(&daemon.shared.devices) = devices.clone();
        }
        let session = start(&daemon).await;
        let started = daemon.state();
        assert!(started.access.denied(Track::Mic));
        assert_eq!(started.access.system_audio, CapturePermission::Granted);
        assert_eq!(started.recording.unwrap().health.tracks.len(), 2);
        assert_eq!(lock(&devices.permissions)["microphone"], "denied");
        let mut subscription = daemon.subscribe();
        lock(&devices.permissions).insert("microphone".into(), "granted".into());
        lock(&engine.events)
            .clone()
            .unwrap()
            .notice(CaptureNotice::HealthChanged);
        let rechecked = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(Event::State(state)) = subscription.next().await
                    && state.access.microphone == CapturePermission::Granted
                {
                    break state;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(rechecked.recording.unwrap().session_id, session);
    }

    /// RT21: capture health changes reach subscribers on their own, with no
    /// command or other state change: losses and a disk warning, then the
    /// warning clearing once space was freed.
    #[tokio::test(flavor = "multi_thread")]
    async fn capture_health_changes_reach_subscribers_on_their_own() {
        use broscribe_ipc::{CaptureHealth, DiskHealth, DiskLevel};
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        start(&d).await;
        let mut sub = d.subscribe();
        let events = lock(&engine.events).clone().unwrap();
        let mut lossy = CaptureHealth::starting();
        lossy.tracks[0].dropped_frames = 480;
        lossy.disk = Some(DiskHealth {
            free_bytes: 900 << 20,
            headroom_s: 1_500.0,
            level: DiskLevel::Warning,
        });
        *lock(&engine.health) = Some(lossy.clone());
        events.notice(CaptureNotice::HealthChanged);
        health_shown(&mut sub, &lossy).await;
        let freed = CaptureHealth {
            disk: Some(DiskHealth {
                free_bytes: 50 << 30,
                headroom_s: 90_000.0,
                level: DiskLevel::Ok,
            }),
            ..lossy
        };
        *lock(&engine.health) = Some(freed.clone());
        events.notice(CaptureNotice::HealthChanged);
        health_shown(&mut sub, &freed).await;
        d.finish().await;
    }

    /// The sentence of each stop code the recorder emits
    /// (`broscribe-capture/src/session.rs`), with a cause as that code
    /// carries one.
    const STOP_SENTENCES: [(&str, &str); 6] = [
        (
            "disk_low",
            "The disk is almost full, so recording stopped to protect what was already \
             recorded. Free some space to record again.",
        ),
        (
            "disk_check_failed",
            "BroScribe couldn't check free disk space, so recording stopped to protect what \
             was already recorded.",
        ),
        (
            "capture_device_unavailable",
            "The microphone or audio output was disconnected. What was recorded until then \
             is kept.",
        ),
        (
            "journal_error: writing /synthetic/journal/mic/7.f32: No space left on device \
             (os error 28)",
            "BroScribe couldn't write the recording to disk. What was written until then is \
             kept. Check that the library folder is still available.",
        ),
        (
            "start_failed",
            "The audio devices didn't start, so nothing was recorded. Try again, or choose \
             another microphone in Settings.",
        ),
        (
            "owner_gone",
            "Recording stopped unexpectedly. What was recorded until then is kept.",
        ),
    ];

    /// The codes a recording that started can stop with, and so the only
    /// ones that reach the "Recording stopped" notice. The recorder sets
    /// `start_failed` only before its session is admitted, and then fails
    /// the start instead of stopping it: it removes the journal and
    /// announces nothing (broscribe-capture's recorder tests, and
    /// [`a_start_that_fails_is_not_announced_as_a_stop`] here). `owner_gone`
    /// needs the session's owner to let go of it without a stop, which
    /// dropping a `RecorderSession` never does. Both keep their sentence
    /// ([`STOP_SENTENCES`]) without a path to this notice.
    const STOPS_WHILE_RECORDING: [&str; 4] = [
        "disk_low",
        "disk_check_failed",
        "capture_device_unavailable",
        "journal_error",
    ];

    /// The code of a stop reason: what precedes its cause, if it has one.
    fn stop_code(reason: &str) -> &str {
        reason.split_once(':').map_or(reason, |(code, _)| code)
    }

    /// WRT-01: every stop code has its sentence, and no sentence shows a code.
    #[test]
    fn every_stop_reason_is_said_in_words() {
        for (reason, sentence) in STOP_SENTENCES {
            assert_eq!(stop_reason_text(reason), sentence, "{reason}");
        }
        // A journal error reads the same whatever its cause, or without one.
        let journal = stop_reason_text("journal_error: synthetic");
        for reason in ["journal_error", "journal_error:", "journal_error:no space"] {
            assert_eq!(stop_reason_text(reason), journal, "{reason}");
        }
        // Anything else, a manual stop's own code included, is the general
        // sentence: there is no fallback to the code.
        let unknown = stop_reason_text("owner_gone");
        let unknowns = [
            "",
            "user",
            "synthetic_new_code",
            "synthetic_new_code: with a cause",
            "journal_errors",
            "Disk_Low",
            " disk_low",
            "error: synthetic",
        ];
        for reason in unknowns {
            assert_eq!(stop_reason_text(reason), unknown, "{reason:?}");
        }
        let reasons = STOP_SENTENCES.iter().map(|(reason, _)| *reason);
        for reason in reasons.chain(unknowns) {
            let text = stop_reason_text(reason);
            assert!(text.ends_with('.'), "{text}");
            // No code, cause or other machine text.
            for raw in ["_", "error:", "os error", "/synthetic", "No space left"] {
                assert!(!text.contains(raw), "{reason:?} shows {raw:?}: {text}");
            }
            let code = stop_code(reason).trim();
            assert!(
                code.is_empty() || !text.contains(code),
                "{reason:?} shows its code: {text}"
            );
        }
        // Only a start that failed keeps nothing, and only it says so.
        for (reason, sentence) in STOP_SENTENCES {
            assert_eq!(
                sentence.contains("nothing was recorded"),
                reason == "start_failed",
                "{reason}"
            );
        }
    }

    /// WRT-01: the sentences cover exactly the codes the recorder can stop
    /// with, read from its source, so a new code is given its own sentence
    /// (or the general one on purpose) instead of falling through unseen.
    #[test]
    fn stop_sentences_cover_the_codes_the_recorder_emits() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../broscribe-capture/src/session.rs");
        let source = std::fs::read_to_string(&path).unwrap();
        let mut emitted = std::collections::BTreeSet::new();
        for assigned in source.split("stopped_reason = Some(").skip(1) {
            let literal = assigned.split('"').nth(1).expect("a reason literal");
            emitted.insert(stop_code(literal).to_owned());
        }
        let known: std::collections::BTreeSet<String> = STOP_SENTENCES
            .iter()
            .map(|(reason, _)| stop_code(reason).to_owned())
            .collect();
        assert_eq!(emitted, known);
        // Each is a stop of a recording that started, or one of the two that
        // never reach the notice.
        let reaching: std::collections::BTreeSet<String> = STOPS_WHILE_RECORDING
            .iter()
            .chain(&["start_failed", "owner_gone"])
            .map(|code| (*code).to_owned())
            .collect();
        assert_eq!(emitted, reaching);
    }

    /// What `tracing` wrote on this thread while `run` ran.
    fn logged(run: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                lock(&self.0).extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lines = Lines::default();
        let sink = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(lock(&lines.0).clone()).unwrap()
    }

    /// WRT-01: a recording whose capture stops on its own is closed and
    /// queued for its final transcript, its code goes to the log (for
    /// Diagnostics), and the one "Recording stopped" notice says why in
    /// words. Having no action and no meeting, the notice is never listed
    /// as pending, so no state snapshot withdraws it. Only the codes such a
    /// recording can stop with are sent ([`STOPS_WHILE_RECORDING`]).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_capture_that_stops_on_its_own_is_announced_in_words() {
        let stops: Vec<_> = STOP_SENTENCES
            .into_iter()
            .filter(|(reason, _)| STOPS_WHILE_RECORDING.contains(&stop_code(reason)))
            .collect();
        assert_eq!(stops.len(), STOPS_WHILE_RECORDING.len());
        for (reason, sentence) in stops {
            let tmp = tempfile::tempdir().unwrap();
            let engine = Arc::new(Engine::default());
            let d = daemon(tmp.path(), engine.clone());
            let session = start(&d).await;
            let sub = d.subscribe();
            let events = lock(&engine.events).clone().unwrap();
            let log = logged(|| {
                events.notice(CaptureNotice::Stopped {
                    reason: reason.into(),
                });
            });
            assert!(log.contains(&format!("capture stopped: {reason}")), "{log}");
            let mut shown = Vec::new();
            until("the stop notice", || {
                shown.extend(notices(&sub));
                !shown.is_empty()
            })
            .await;
            assert_eq!(shown.len(), 1, "{shown:?}");
            let notice = &shown[0];
            assert_eq!(
                notice.title,
                broscribe_platform_native::NativePlatform::notification_title(
                    "Recording Stopped",
                    "Recording stopped"
                )
            );
            assert_eq!(notice.body, sentence);
            assert!(!notice.body.contains(stop_code(reason)), "{notice:?}");
            assert!(notice.id.starts_with("stopped-"), "{notice:?}");
            assert!(notice.actions.is_empty() && notice.meeting_id.is_none());
            // Closed and queued like a stop the user asked for.
            let state = d.state();
            assert!(state.recording.is_none());
            assert_eq!(state.stopped.len(), 1);
            assert_eq!(state.stopped[0].session_id, session);
            assert_eq!(
                state.stopped[0].stop_reason.as_deref(),
                Some(stop_code(reason))
            );
            assert_eq!(final_jobs(tmp.path()), 1, "{reason}");
            assert!(state.pending_notifications.is_empty());
            d.shared.publish_state();
            assert!(d.state().pending_notifications.is_empty());
            assert!(notices(&sub).is_empty(), "announced once");
            d.finish().await;
        }
    }

    /// WRT-01: a start that fails is never announced as a stop, so
    /// `start_failed`'s sentence is not shown for it. When the devices don't
    /// start, the recorder fails the start itself, which the engine's
    /// refusal stands for here; and a capture that stops on its own before
    /// its start is committed fails that start. Either way the caller gets
    /// the error, and nothing is kept or queued.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_start_that_fails_is_not_announced_as_a_stop() {
        let record = || Request::new(Call::RecordStart(RecordStartParams::default()));
        let nothing_kept = |d: &Daemon, tmp: &std::path::Path| {
            let state = d.state();
            assert!(state.recording.is_none());
            assert!(state.stopped.is_empty());
            assert_eq!(final_jobs(tmp), 0);
        };
        // The devices don't start.
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine {
            fail: true,
            ..Default::default()
        });
        let d = daemon(tmp.path(), engine);
        let sub = d.subscribe();
        assert!(d.handle(record()).await.outcome.is_err());
        nothing_kept(&d, tmp.path());
        assert!(notices(&sub).is_empty());
        d.finish().await;
        // The capture stops on its own while the start is still committing.
        for code in STOPS_WHILE_RECORDING {
            let tmp = tempfile::tempdir().unwrap();
            let engine = Arc::new(Engine::default());
            *lock(&engine.stop_while_starting) = Some(code);
            let d = daemon(tmp.path(), engine.clone());
            let sub = d.subscribe();
            let error = d.handle(record()).await.outcome.unwrap_err();
            assert_eq!(error.code, "capture_failed", "{code}");
            nothing_kept(&d, tmp.path());
            assert!(notices(&sub).is_empty(), "{code}");
            // The slot is free again: the next start is admitted.
            start(&d).await;
            d.finish().await;
        }
    }

    /// SH23: a confirmed admission survives its connection and the agent
    /// restart its replacement causes; no capture opens until it is
    /// released, and fences keep growing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_confirmed_install_admission_outlives_a_restart_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let holder = d.connect();
        let fence = holder
            .handle(call(Call::InstallAcquire))
            .await
            .outcome
            .unwrap()["fence"]
            .as_u64()
            .unwrap();
        let confirmed = holder
            .handle(call(Call::InstallConfirm { fence }))
            .await
            .outcome
            .unwrap();
        assert_eq!(confirmed["durable"], true);
        drop(holder);
        d.finish().await;
        drop(d);
        let restarted = daemon(tmp.path(), Arc::new(Engine::default()));
        assert_eq!(
            restarted.state().install,
            Some(broscribe_ipc::InstallHold {
                fence,
                durable: true
            })
        );
        let start_refused = restarted
            .handle(call(Call::RecordStart(RecordStartParams::default())))
            .await;
        assert_eq!(
            start_refused.outcome.unwrap_err().code,
            IpcError::INSTALLING
        );
        let stale = restarted
            .handle(call(Call::InstallRelease { fence: fence + 1 }))
            .await;
        assert_eq!(stale.outcome.unwrap_err().code, IpcError::ADMISSION_LOST);
        restarted
            .handle(call(Call::InstallRelease { fence }))
            .await
            .outcome
            .unwrap();
        start(&restarted).await;
        restarted
            .handle(stop_active(&restarted))
            .await
            .outcome
            .unwrap();
        restarted.finish().await;
    }

    /// RT24: a stopped recording whose processing failed for good is never
    /// retried on its own; `record.retry` restarts its final job with fresh
    /// attempts, and only then.
    #[tokio::test(flavor = "multi_thread")]
    async fn record_retry_restarts_only_a_final_job_failed_for_good() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let session = start(&d).await;
        d.handle(stop_active(&d)).await.outcome.unwrap();
        let retry = |session: &str| {
            call(Call::RecordRetry {
                session_id: session.into(),
                request_id: broscribe_ipc::RequestId::random(),
            })
        };
        let queued = d.handle(retry(&session)).await;
        assert_eq!(queued.outcome.unwrap_err().code, IpcError::NOT_RETRYABLE);
        let library = tmp.path().join("library");
        let queue = Queue::open(&library).unwrap();
        let id = JobId::for_key(RECORD_FINAL_KIND, &session);
        let probe = broscribe_platform::fakes::FakeProcessProbe::new(std::process::id());
        for _ in 0..d.shared.cfg.max_attempts {
            let lease = queue.claim(&d.shared.lib, &id, &probe).unwrap().unwrap();
            lease.fail(&d.shared.lib, "synthetic failure").unwrap();
            lease.release(&d.shared.lib).unwrap();
        }
        // The scheduler shows it failed for good.
        let failed = StoppedRecordingState::Failed {
            error: Some("decode error: /synthetic/journal".into()),
        };
        assert!(set_stopped_states(
            &d.shared,
            &HashMap::from([(session.clone(), failed.clone())])
        ));
        assert_eq!(d.state().stopped[0].state, failed);
        let notice_id = format!("failed-{session}");
        let state = d.state();
        let notice = state
            .pending_notifications
            .iter()
            .find(|notice| notice.id == notice_id)
            .unwrap();
        assert!(notice.actions.is_empty());
        assert_eq!(
            notice.body,
            "BroScribe couldn't transcribe “Untitled meeting”. Its saved data couldn't be read. The recording is kept."
        );
        assert_eq!(
            d.state()
                .pending_notifications
                .iter()
                .filter(|notice| notice.id == notice_id)
                .count(),
            1,
        );
        assert!(!set_stopped_states(
            &d.shared,
            &HashMap::from([(session.clone(), failed)])
        ));
        let out = d.handle(retry(&session)).await.outcome.unwrap();
        assert_eq!(out["job_id"], json!(id));
        let job = queue.get(&id).unwrap();
        assert_eq!((job.state, job.attempts), (JobState::Queued, 0));
        assert_eq!(
            d.state().stopped[0].state,
            StoppedRecordingState::Queued { ahead: 0 }
        );
        assert!(
            d.state()
                .pending_notifications
                .iter()
                .all(|notice| notice.id != notice_id)
        );
        let unknown = d.handle(retry("s-20200101-000000-dead")).await;
        assert_eq!(unknown.outcome.unwrap_err().code, lifecycle::STALE_SESSION);
        d.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_journal_resolution_requires_confirmation_and_the_open_library() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        let root = d.shared.cfg.journal_root();
        let library = d.shared.lib.root();
        let other = Library::open(tmp.path().join("elsewhere")).unwrap();
        let legacy = "s-20260101-000000-abcd";
        let foreign = "s-20260101-000001-abcd";
        let make_journal = |session, library_root| {
            journal::JournalWriter::create(
                &root,
                session,
                journal::SessionMeta {
                    library_root,
                    ..Default::default()
                },
                5.0,
                std::process::id(),
            )
            .unwrap()
            .dir()
            .to_path_buf()
        };
        let legacy_dir = make_journal(legacy, None);
        let foreign_dir = make_journal(foreign, Some(other.root().to_path_buf()));
        let request = |session: &str, resolution, target: &std::path::Path, confirmed| {
            call(Call::RecordResolveJournal {
                session_id: session.into(),
                request_id: broscribe_ipc::RequestId::random(),
                resolution,
                library_root: target.to_path_buf(),
                confirmed,
            })
        };
        for resolution in [JournalResolution::Adopt, JournalResolution::Discard] {
            for (session, target, confirmed, code) in [
                (legacy, library, false, "confirmation_required"),
                (legacy, other.root(), true, "library_changed"),
                (foreign, library, true, journal::LIBRARY_MISMATCH),
            ] {
                assert_eq!(
                    d.handle(request(session, resolution, target, confirmed))
                        .await
                        .outcome
                        .unwrap_err()
                        .code,
                    code
                );
            }
        }
        assert!(
            journal::load_manifest(&legacy_dir)
                .unwrap()
                .meta
                .library_root
                .is_none()
        );
        assert!(foreign_dir.is_dir());
        assert_eq!(final_jobs(tmp.path()), 0);
        assert!(
            journal::recover_orphans(&root, library)
                .unwrap()
                .recovered
                .is_empty()
        );
        assert_eq!(
            journal::recover_orphans(&root, other.root())
                .unwrap()
                .recovered[0]
                .dir,
            foreign_dir
        );

        let result = d
            .handle(request(legacy, JournalResolution::Adopt, library, true))
            .await
            .outcome
            .unwrap();
        assert_eq!(result["status"], "adopted");
        let manifest = journal::load_manifest(&legacy_dir).unwrap();
        assert_eq!(manifest.meta.library_root.as_deref(), Some(library));
        assert_eq!(manifest.state, journal::SessionState::Stopped);
        assert_eq!(d.state().stopped[0].session_id, legacy);
        assert_eq!(final_jobs(tmp.path()), 1);

        let discarded = "s-20260101-000002-abcd";
        let discarded_dir = make_journal(discarded, None);
        // A queued job in the current library is not proof it owns this journal.
        let job = writes::enqueue(
            &d.shared.lib,
            RECORD_FINAL_KIND,
            discarded,
            json!({ "session_id": discarded }),
        )
        .unwrap();
        let result = d
            .handle(request(
                discarded,
                JournalResolution::Discard,
                library,
                true,
            ))
            .await
            .outcome
            .unwrap();
        assert_eq!(result["status"], "discarded");
        until("the confirmed legacy discard", || {
            !discarded_dir.exists() && !lifecycle::has_discard_intent(&root, discarded)
        })
        .await;
        let queue = Queue::open(library).unwrap();
        assert_eq!(queue.get(&job).unwrap().state, JobState::Queued);
        assert!(foreign_dir.is_dir());
        d.finish().await;
    }

    /// RT04: a journal recovery could not bring back is reported by ID and
    /// code, never hides the healthy one, and the index is only reported
    /// current after its startup repair.
    #[test]
    fn unrecoverable_journals_are_reported_without_hiding_healthy_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let healthy = "s-20260101-000000-abcd";
        let corrupt = "s-20260101-000001-abcd";
        let engine = Arc::new(Engine {
            orphans: Mutex::new(vec![StoppedSession {
                session_id: healthy.into(),
                journal_dir: journal(tmp.path(), healthy),
            }]),
            orphan_failures: Mutex::new(vec![broscribe_ipc::OrphanFailure {
                session_id: corrupt.into(),
                journal_dir: tmp.path().join("data/journal").join(corrupt),
                code: "schema".into(),
                corrupt: true,
            }]),
            ..Default::default()
        });
        let d = daemon(tmp.path(), engine);
        assert_eq!(final_jobs(tmp.path()), 1);
        assert_eq!(
            d.state().journal_issues,
            [JournalIssue {
                session_id: corrupt.into(),
                code: "schema".into(),
                corrupt: true,
            }]
        );
        assert_eq!(d.state().index, IndexState::Current);
    }

    /// RT04/RT24: a stopped journal whose recovery failed for a reason that
    /// may pass has no final job: it shows its recovery being retried with
    /// its code, never a queue place, and refuses a manual retry. Once a
    /// sweep recovers it, its final job is queued and shown.
    #[test]
    fn a_journal_awaiting_recovery_is_never_shown_queued_without_a_job() {
        let tmp = tempfile::tempdir().unwrap();
        let session = "s-20260101-000002-abcd";
        let root = tmp.path().join("data/journal");
        let mut writer = broscribe_capture::journal::JournalWriter::create(
            &root,
            session,
            journal::SessionMeta {
                library_root: Some(
                    Library::open(tmp.path().join("library"))
                        .unwrap()
                        .root()
                        .canonicalize()
                        .unwrap(),
                ),
                ..Default::default()
            },
            5.0,
            std::process::id(),
        )
        .unwrap();
        writer.set_started_at(chrono::Utc::now()).unwrap();
        writer.finish(0, "user").unwrap();
        let engine = Arc::new(Engine {
            orphan_failures: Mutex::new(vec![broscribe_ipc::OrphanFailure {
                session_id: session.into(),
                journal_dir: root.join(session),
                code: "io".into(),
                corrupt: false,
            }]),
            ..Default::default()
        });
        let d = daemon(tmp.path(), engine.clone());
        assert_eq!(final_jobs(tmp.path()), 0);
        let stopped = d.state().stopped;
        assert_eq!(stopped.len(), 1);
        assert!(
            matches!(
                &stopped[0].state,
                StoppedRecordingState::Retrying { error: Some(code), .. } if code == "io"
            ),
            "{stopped:?}"
        );
        let retry = retry_recording(&d.shared, session).unwrap_err();
        assert_eq!(retry.code, IpcError::NOT_RETRYABLE);

        lock(&engine.orphan_failures).clear();
        *lock(&engine.orphans) = vec![StoppedSession {
            session_id: session.into(),
            journal_dir: root.join(session),
        }];
        sweep_orphans(&d.shared).unwrap();
        assert_eq!(final_jobs(tmp.path()), 1);
        assert_eq!(
            d.state().stopped[0].state,
            StoppedRecordingState::Queued { ahead: 0 }
        );
    }

    /// RT24/N1: a stopped recording whose final job cannot be queued (a
    /// persistent library write fault) shows the retry with the stop's
    /// error code, never "queued, 0 ahead" with no job, keeps its closed
    /// journal and refuses a manual retry; every sweep that still cannot
    /// queue it keeps that, and the first one that queues it durably shows
    /// the job with nothing lost.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_final_job_that_could_not_be_queued_is_shown_retrying_until_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::default());
        let d = daemon(tmp.path(), engine.clone());
        let session = start(&d).await;
        let journal_dir = tmp.path().join("data/journal").join(&session);
        // The closed journal is what every sweep finds.
        *lock(&engine.orphans) = vec![StoppedSession {
            session_id: session.clone(),
            journal_dir: journal_dir.clone(),
        }];
        // The queue folder is a file: queueing fails until it is restored.
        let library = tmp.path().join("library");
        let queue_dir = library.join(broscribe_core::paths::QUEUE_DIR);
        std::fs::remove_dir_all(&queue_dir).unwrap();
        std::fs::write(&queue_dir, b"").unwrap();
        let code = d.handle(stop_active(&d)).await.outcome.unwrap_err().code;
        let retrying = |d: &Daemon| {
            let state = d.state();
            matches!(
                &state.stopped[..],
                [row] if row.session_id == session
                    && matches!(&row.state, StoppedRecordingState::Retrying { error: Some(c), .. } if *c == code)
            ) && state.journal_issues
                == [JournalIssue {
                    session_id: session.clone(),
                    code: code.clone(),
                    corrupt: false,
                }]
        };
        assert!(retrying(&d), "{:?}", d.state().stopped);
        assert!(d.state().recording.is_none());
        let refused = retry_recording(&d.shared, &session).unwrap_err();
        assert_eq!(refused.code, IpcError::NOT_RETRYABLE);
        // Background sweeps run into the same fault: still retrying.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(retrying(&d), "{:?}", d.state().stopped);
        assert!(journal_dir.is_dir(), "the closed journal is kept");

        std::fs::remove_file(&queue_dir).unwrap();
        until("the final job queued after the fault", || {
            final_jobs(tmp.path()) == 1
                && d.state().journal_issues.is_empty()
                && d.state().stopped[0].state == StoppedRecordingState::Queued { ahead: 0 }
        })
        .await;
        assert!(journal_dir.is_dir(), "the recording is kept");
        d.finish().await;
    }

    /// RT12/startup: until startup recovery finished, no capture is
    /// admitted (a just-started journal is never taken for an orphan).
    #[tokio::test(flavor = "multi_thread")]
    async fn captures_wait_for_startup_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let d = Daemon::new(
            DaemonConfig::new(
                tmp.path().join("library"),
                tmp.path().join("data"),
                tmp.path().join("cache"),
                WorkerCommand::new("/bin/false"),
            ),
            Arc::new(Engine::default()),
            Arc::new(broscribe_platform::fakes::FakeProcessProbe::new(
                std::process::id(),
            )),
            Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.path().join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
        )
        .unwrap();
        d.set_setup_devices(
            Arc::new(broscribe_platform::devices::FakeDeviceInfo::default()),
            Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000)),
        );
        assert_eq!(d.state().index, IndexState::Checking);
        let early = d
            .handle(call(Call::RecordStart(RecordStartParams::default())))
            .await;
        assert_eq!(early.outcome.unwrap_err().code, IpcError::BUSY);
        let installing = d.connect().handle(call(Call::InstallAcquire)).await;
        assert_eq!(
            installing.outcome.unwrap_err().code,
            IpcError::RECORDING_ACTIVE
        );
        d.finish_startup();
        start(&d).await;
        d.finish().await;
    }

    /// SH23: an agent is identified by its build, not its version: two
    /// builds of one version report different identities, and unknown or
    /// mismatched build information is never guessed from the version.
    #[tokio::test(flavor = "multi_thread")]
    async fn agents_of_one_version_report_their_own_build() {
        use broscribe_platform::about::{BuildInfo, BuildInfoState};
        let version = env!("CARGO_PKG_VERSION");
        let info = |build: u64, version: &str| {
            BuildInfoState::Present(BuildInfo {
                product: "BroScribe".into(),
                version: version.into(),
                build,
                target: "aarch64-apple-darwin".into(),
            })
        };
        assert_eq!(agent_build(version, &BuildInfoState::Missing), None);
        assert_eq!(
            agent_build(
                version,
                &BuildInfoState::Invalid {
                    reason: "synthetic".into()
                }
            ),
            None
        );
        // A record of another version (a replaced installation) proves nothing.
        assert_eq!(agent_build(version, &info(42, "0.0.0-other")), None);
        let ping_build = |d: &Daemon| {
            let d = d.clone();
            async move {
                let pong = d.handle(call(Call::Ping)).await.outcome.unwrap();
                (pong["version"].clone(), pong["build"].clone())
            }
        };
        let mut builds = Vec::new();
        for build in [41, 42] {
            let tmp = tempfile::tempdir().unwrap();
            let mut cfg = DaemonConfig::new(
                tmp.path().join("library"),
                tmp.path().join("data"),
                tmp.path().join("cache"),
                WorkerCommand::new("/bin/false"),
            );
            cfg.build = agent_build(version, &info(build, version));
            let d = Daemon::new(
                cfg,
                Arc::new(Engine::default()),
                Arc::new(broscribe_platform::fakes::FakeProcessProbe::new(
                    std::process::id(),
                )),
                Arc::new(
                    broscribe_core::maintenance::ModeLease::acquire(
                        &tmp.path().join("data"),
                        broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                    )
                    .unwrap(),
                ),
            )
            .unwrap();
            let expected = AgentBuild {
                build,
                target: "aarch64-apple-darwin".into(),
            };
            assert_eq!(d.state().build, Some(expected.clone()));
            let ping =
                AgentPing::from_value(d.handle(call(Call::Ping)).await.outcome.unwrap()).unwrap();
            assert_eq!(ping.version, version);
            assert_eq!(ping.build.as_deref(), Some(&expected));
            assert_eq!(ping.mode, broscribe_core::maintenance::Mode::Normal);
            assert_eq!(
                ping.readiness,
                broscribe_ipc::AgentReadiness::NormalStarting
            );
            d.finish_startup();
            let operational =
                AgentPing::from_value(d.handle(call(Call::Ping)).await.outcome.unwrap()).unwrap();
            assert_eq!(
                operational.readiness,
                broscribe_ipc::AgentReadiness::NormalOperational
            );
            builds.push(json!(ping.build));
        }
        assert_ne!(builds[0], builds[1], "same version, different builds");
        // Unknown stays unknown: the default configuration reports none.
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path(), Arc::new(Engine::default()));
        assert_eq!(d.state().build, None);
        assert_eq!(ping_build(&d).await.1, Value::Null);
    }
}
