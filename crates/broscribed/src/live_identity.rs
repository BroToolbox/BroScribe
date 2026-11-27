//! Live-preview speaker identity in the daemon (plan D, R1).
//!
//! One [`LivePreview`] per recording session:
//! - Every ASR preview segment (from the live ASR worker, with word times)
//!   becomes a source-anchored turn on the [`PreviewBoard`], published as
//!   `live_turn`, even when speaker analysis is off (track-only preview).
//! - A supervised companion `broscribe-worker live-speakers <session>`
//!   reads closed journal chunks and returns windows of provisional voices
//!   and suggestions; the board revises the turns they cover. Before it is
//!   ready only its `loading` lines are signs of life, and it must be ready
//!   within the total load deadline ([`LiveSettings::load_deadline`]). A
//!   worker that is silent, or has pending audio but completes no window,
//!   for `progress_timeout_s` is killed and restarted with backoff; capture
//!   and ASR never wait for it. Availability travels as `live_identity`.
//! - `[live] identity` in the config is watched while the session lasts:
//!   turning it off stops only the companion (the board, its turns and the
//!   user's decisions stay; turns are track-only from then on); turning it
//!   back on starts a fresh worker epoch.
//! - `record.live_attribute` decisions are fenced (session, turn revision,
//!   epoch, voice-evidence fingerprint) under the library lock, then the
//!   session mutex, before any person is created; they are persisted in the
//!   session sidecar (`core::live_identity`) and apply only to the turn's
//!   exact range. A Name/Confirm hands the range to the worker as a
//!   transient session-local sample (suggestions only, never a voiceprint).
//!
//! No embeddings, audio or credentials cross the worker pipe or IPC; logs
//! carry codes and counts only.

mod board;

#[cfg(test)]
mod tests;

pub use board::{Names, PreviewBoard, STALE_EVIDENCE, STALE_TURN};

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use broscribe_capture::anchors::{journal_point, placements};
use broscribe_capture::journal::{self, Track as CaptureTrack};
use broscribe_core::config::Config;
use broscribe_core::live_identity::{
    LiveAction, LiveRange, NewDecision, evidence_fingerprint, forget_fence as live_forget_fence,
    record_decision,
};
use broscribe_core::{Error, Library, PersonId, lifecycle, ops, paths};
use broscribe_ipc::live::{
    LiveAttributeOutcome, LiveAttributeParams, LiveIdentitySettings, LiveIdentityState,
    LiveIdentityStatus, LivePerson, LiveSegment, LiveSnapshot, LiveTurn, SpeakerWindow,
    SpeakersEvent, SpeakersInput, SpeakersStart,
};
use broscribe_ipc::{Event, IpcError, Track, WorkerProblem};
use serde_json::Value;

use crate::bus::EventBus;
use crate::live::{LiveSettings, backoff};
use crate::shutdown::Shutdown;
use crate::worker_cmd::WorkerCommand;

/// A name-only person was created, but the decision could not be saved:
/// the message carries the person ID.
pub const PARTIAL_OUTCOME: &str = "partial_outcome";

/// Live sample rate.
const RATE: u32 = broscribe_ipc::live::LIVE_RATE;

pub(crate) fn seconds(sample: u64) -> f64 {
    sample as f64 / f64::from(RATE)
}

pub(crate) fn sample(seconds: f64) -> u64 {
    (seconds.max(0.0) * f64::from(RATE)).round() as u64
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn capture_track(track: Track) -> CaptureTrack {
    match track {
        Track::Mic => CaptureTrack::Mic,
        Track::System => CaptureTrack::System,
    }
}

/// The display name of a person ID in `lib`; `None` when it does not exist.
fn name_of(lib: Option<&Library>, id: &str) -> Option<String> {
    let lib = lib?;
    let id = PersonId::parse(id).ok()?;
    lib.load_person(&id).ok().map(|p| p.name)
}

/// Size and modification time of a file; `None` when it cannot be read.
type Stamp = Option<(SystemTime, u64)>;

fn file_stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Person display names already read, each with the `person.md` stamp it
/// was read at; all dropped when the voice evidence changes.
#[derive(Default)]
struct NameCache {
    evidence: String,
    names: HashMap<String, (Stamp, Option<String>)>,
}

/// How often the config is checked for a changed `[live] identity`.
const CONFIG_POLL: Duration = Duration::from_secs(1);

/// `[live] identity` as last applied from `<data_dir>/config.toml`. The
/// file is re-read only when its size or modification time changes; a read
/// that fails keeps the last applied value, and one older than it (by
/// config revision) is ignored. Missing: on.
struct ConfigWatch {
    data_dir: PathBuf,
    stamp: Stamp,
    revision: u64,
    enabled: bool,
    checked: Instant,
}

impl ConfigWatch {
    fn new(data_dir: PathBuf) -> Self {
        let mut watch = Self {
            stamp: file_stamp(&Config::path(&data_dir)),
            data_dir,
            revision: 0,
            enabled: true,
            checked: Instant::now(),
        };
        watch.apply();
        watch
    }

    /// Whether live speaker analysis is on; checks the file at most once
    /// per [`CONFIG_POLL`].
    fn enabled(&mut self) -> bool {
        if self.checked.elapsed() >= CONFIG_POLL {
            self.checked = Instant::now();
            let stamp = file_stamp(&Config::path(&self.data_dir));
            if stamp != self.stamp {
                self.stamp = stamp;
                self.apply();
            }
        }
        self.enabled
    }

    fn apply(&mut self) {
        match Config::load(&self.data_dir) {
            Ok(config) if config.revision() >= self.revision => {
                self.revision = config.revision();
                self.enabled = config.live_identity_enabled();
            }
            Ok(config) => tracing::debug!(
                read = config.revision(),
                applied = self.revision,
                "live speakers: older config ignored"
            ),
            Err(e) => tracing::warn!(
                code = e.code(),
                "live speakers: config unreadable; keeping the last setting"
            ),
        }
    }
}

/// Progress counters of one worker, in milliseconds since it started.
#[derive(Clone)]
struct Progress {
    started: Instant,
    /// Last sign of life: before readiness a `loading` or `ready` line,
    /// after it any valid line.
    last_line: Arc<AtomicU64>,
    /// Last completed window, gap or readiness; `u64::MAX` before ready.
    last_progress: Arc<AtomicU64>,
    /// Pending closed audio the worker last reported.
    pending_ms: Arc<AtomicU64>,
    /// How long the worker's current window had been running at its last
    /// heartbeat (0: idle). Heartbeats come from a thread independent of
    /// inference, so a stuck window shows here even when nothing is pending.
    busy_ms: Arc<AtomicU64>,
    /// The worker reported `ready`.
    ready: Arc<AtomicBool>,
}

impl Progress {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_line: Arc::new(AtomicU64::new(0)),
            last_progress: Arc::new(AtomicU64::new(u64::MAX)),
            pending_ms: Arc::new(AtomicU64::new(0)),
            busy_ms: Arc::new(AtomicU64::new(0)),
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    fn now(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
}

/// A running live-speakers worker.
struct SpeakersChild {
    child: Child,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    progress: Progress,
}

/// How one live-speakers worker ended.
enum Ended {
    /// The session ended.
    Stopped,
    /// Turned off in the config; stopped through end of stdin.
    Disabled,
    /// It exited successfully on its own: the session's audio is all
    /// analysed, or its journal is gone. Nothing to restart.
    Finished,
    Failed(WorkerProblem),
}

/// How one enabled stretch of the session ended.
enum Run {
    Stopped,
    Disabled,
    Finished,
    /// Consecutive load timeouts: not restarted while it stays enabled.
    GaveUp,
}

/// One session's live speaker preview.
pub struct LivePreview {
    session_id: String,
    journal_dir: PathBuf,
    settings: LiveIdentitySettings,
    bus: EventBus,
    board: Mutex<PreviewBoard>,
    library: Mutex<Option<Library>>,
    names: Mutex<NameCache>,
    /// Stdin of the current live-speakers worker.
    input: Mutex<Option<mpsc::Sender<SpeakersInput>>>,
    epoch: AtomicU64,
}

impl LivePreview {
    pub fn new(
        session_id: String,
        journal_dir: PathBuf,
        settings: LiveIdentitySettings,
        bus: EventBus,
    ) -> Arc<Self> {
        let board = PreviewBoard::new(session_id.clone(), settings.clone());
        Arc::new(Self {
            session_id,
            journal_dir,
            settings,
            bus,
            board: Mutex::new(board),
            library: Mutex::new(None),
            names: Mutex::new(NameCache::default()),
            input: Mutex::new(None),
            epoch: AtomicU64::new(0),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The authoritative rows (reconnect, lag, stale picker), in
    /// publication order (see [`PreviewBoard::snapshot`]).
    pub fn snapshot(&self) -> LiveSnapshot {
        lock(&self.board).snapshot()
    }

    /// The library person names and the evidence fingerprint come from;
    /// stamps the board with the current evidence.
    fn attach_library(&self, lib: Option<Library>) {
        let fingerprint = lib.as_ref().and_then(|l| evidence_fingerprint(l).ok());
        *lock(&self.library) = lib.clone();
        if let Some(fingerprint) = fingerprint {
            let names = |id: &str| self.name(lib.as_ref(), &fingerprint, id);
            let changed = lock(&self.board).refresh_evidence(&fingerprint, &names);
            self.publish_turns(changed);
        }
    }

    /// The display name of person `id` with the board at `evidence`. A
    /// cached name is reused only while the person's `person.md` keeps the
    /// stamp it was read at and the evidence is the same, so a rename or a
    /// forget shows on the next lookup.
    fn name(&self, lib: Option<&Library>, evidence: &str, id: &str) -> Option<String> {
        let lib = lib?;
        let person = PersonId::parse(id).ok()?;
        // Taken before reading: a change after it changes the stamp again.
        let stamp = file_stamp(&lib.person_dir(&person).join(paths::PERSON_MD));
        let mut cache = lock(&self.names);
        if cache.evidence != evidence {
            cache.evidence = evidence.to_owned();
            cache.names.clear();
        }
        if let Some((cached, name)) = cache.names.get(id)
            && *cached == stamp
        {
            return name.clone();
        }
        let name = stamp
            .and_then(|_| lib.load_person(&person).ok())
            .map(|p| p.name);
        cache.names.insert(id.to_owned(), (stamp, name.clone()));
        name
    }

    fn library(&self) -> Option<Library> {
        lock(&self.library).clone()
    }

    fn publish_turns(&self, turns: Vec<LiveTurn>) {
        for turn in turns {
            self.bus.publish(Event::LiveTurn(Box::new(turn)));
        }
    }

    /// Records and publishes this session's identity status; the event
    /// names this session, never whichever one is active when it arrives.
    fn set_identity(&self, status: LiveIdentityStatus, epoch: u64) {
        let state = LiveIdentityState { status, epoch };
        let changed = lock(&self.board).set_identity(state.clone());
        if changed {
            self.bus.publish(Event::LiveIdentity {
                session_id: self.session_id.clone(),
                identity: state,
            });
        }
    }

    /// An ASR preview segment: a new turn (published).
    pub fn on_segment(&self, seg: &LiveSegment) {
        let lib = self.library();
        let mut board = lock(&self.board);
        let evidence = board.evidence().to_owned();
        let names = |id: &str| self.name(lib.as_ref(), &evidence, id);
        let turn = board.ingest_segment(seg, &names);
        drop(board);
        self.publish_turns(vec![turn]);
    }

    /// An analysed window: revises the turns it covers (published).
    fn on_window(&self, window: &SpeakerWindow) {
        let lib = self.library();
        let names = |id: &str| self.name(lib.as_ref(), &window.evidence, id);
        let changed = lock(&self.board).ingest_window(window, &names);
        self.publish_turns(changed);
    }

    /// Hands a message to the current worker, if any (dropped otherwise:
    /// a restarted worker holds no audio of earlier turns anyway).
    fn send(&self, message: SpeakersInput) {
        if let Some(tx) = lock(&self.input).as_ref() {
            let _ = tx.send(message);
        }
    }

    fn on_event(&self, event: SpeakersEvent, epoch: u64, progress: &Progress) {
        let now = progress.now();
        match event {
            SpeakersEvent::Loading { .. } => {
                if !progress.is_ready() {
                    self.set_identity(LiveIdentityStatus::Loading { elapsed_ms: now }, epoch);
                }
            }
            SpeakersEvent::Ready { .. } => {
                progress.ready.store(true, Ordering::SeqCst);
                progress.last_progress.store(now, Ordering::SeqCst);
                self.set_identity(LiveIdentityStatus::Ready, epoch);
            }
            SpeakersEvent::Heartbeat { pending_s, busy_ms } => {
                // Not progress, and before readiness not even a sign of life.
                if !progress.is_ready() {
                    return;
                }
                progress
                    .pending_ms
                    .store((pending_s.max(0.0) * 1000.0) as u64, Ordering::SeqCst);
                progress.busy_ms.store(busy_ms, Ordering::SeqCst);
            }
            SpeakersEvent::Window(window) => {
                if window.epoch != epoch {
                    return;
                }
                progress.last_progress.store(now, Ordering::SeqCst);
                self.on_window(&window);
                let status = if window.behind {
                    LiveIdentityStatus::Behind
                } else {
                    LiveIdentityStatus::Ready
                };
                self.set_identity(status, epoch);
            }
            SpeakersEvent::Gap { .. } => {
                progress.last_progress.store(now, Ordering::SeqCst);
                self.set_identity(LiveIdentityStatus::Behind, epoch);
            }
            SpeakersEvent::SessionSample { used, .. } => {
                tracing::debug!(used, "live speakers: session sample");
            }
            SpeakersEvent::Warning { message } => {
                tracing::warn!("live speakers: {message}");
            }
        }
    }

    /// Runs live-speakers workers until `stop` (the session ended) while
    /// the settings and `[live] identity` in `data_dir`'s config allow it,
    /// restarting a failed one with backoff; preview turns stay track-only
    /// meanwhile. Turning it off stops only this companion (`Disabled`);
    /// turning it on again starts a fresh epoch.
    pub async fn supervise(
        self: Arc<Self>,
        worker: WorkerCommand,
        library: PathBuf,
        data_dir: PathBuf,
        stop: Arc<Shutdown>,
        admission: Arc<broscribe_core::maintenance::ModeLease>,
    ) {
        // Reader-owned previews retain the same process admission through their library.
        if let Err(error) = admission.require_normal() {
            tracing::warn!(code = error.code(), "live speakers admission failed");
            self.set_identity(
                LiveIdentityStatus::Unavailable {
                    reason: WorkerProblem::StartFailed,
                },
                0,
            );
            return;
        }
        if admission.data_dir() != data_dir.as_path() {
            self.set_identity(
                LiveIdentityStatus::Unavailable {
                    reason: WorkerProblem::InvalidSettings,
                },
                0,
            );
            return;
        }
        self.attach_library(
            Library::open_existing(&library)
                .and_then(|library| library.with_mode_lease(Arc::clone(&admission)))
                .ok(),
        );
        if !self.settings.enabled {
            self.set_identity(LiveIdentityStatus::Disabled, 0);
            return;
        }
        if let Err(e) = self.settings.validate() {
            tracing::warn!("live speakers: invalid settings ({e})");
            self.set_identity(
                LiveIdentityStatus::Unavailable {
                    reason: WorkerProblem::InvalidSettings,
                },
                0,
            );
            return;
        }
        let mut config = ConfigWatch::new(data_dir);
        'session: while !stop.is_triggered() {
            if !config.enabled() {
                self.set_identity(LiveIdentityStatus::Disabled, self.epoch());
                while !config.enabled() {
                    tokio::select! {
                        () = tokio::time::sleep(CONFIG_POLL) => {}
                        () = stop.wait() => break 'session,
                    }
                }
            }
            match self
                .run_enabled(&worker, &library, &mut config, &stop, &admission)
                .await
            {
                Run::Stopped | Run::Finished => break,
                Run::Disabled => {}
                Run::GaveUp => {
                    // Unavailable until the session ends or it is turned off.
                    while config.enabled() {
                        tokio::select! {
                            () = tokio::time::sleep(CONFIG_POLL) => {}
                            () = stop.wait() => break 'session,
                        }
                    }
                }
            }
        }
        *lock(&self.input) = None;
        self.set_identity(LiveIdentityStatus::Idle, self.epoch());
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Workers of fresh epochs, restarted with backoff, until the session
    /// ends, the config turns analysis off, one finishes, or the models
    /// miss the load deadline [`LiveSettings::load_attempts`] times in a
    /// row.
    async fn run_enabled(
        self: &Arc<Self>,
        worker: &WorkerCommand,
        library: &Path,
        config: &mut ConfigWatch,
        stop: &Shutdown,
        admission: &Arc<broscribe_core::maintenance::ModeLease>,
    ) -> Run {
        let restart = LiveSettings::default();
        let mut failures: u32 = 0;
        let mut load_timeouts: u32 = 0;
        loop {
            let started = Instant::now();
            let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
            // Before the reader thread exists, so it never overwrites what
            // the worker reports.
            self.set_identity(LiveIdentityStatus::Loading { elapsed_ms: 0 }, epoch);
            let (ended, ready) = match self.spawn(worker, library, epoch, admission) {
                Ok(child) => self.watch(child, stop, config, restart.load_deadline).await,
                Err(e) => {
                    tracing::warn!("live speakers worker did not start: {e}");
                    (Ended::Failed(WorkerProblem::StartFailed), false)
                }
            };
            let reason = match ended {
                Ended::Stopped => return Run::Stopped,
                Ended::Disabled => return Run::Disabled,
                Ended::Finished => return Run::Finished,
                Ended::Failed(reason) => reason,
            };
            if stop.is_triggered() {
                return Run::Stopped;
            }
            if ready {
                load_timeouts = 0;
            }
            if reason == WorkerProblem::LoadTimeout {
                load_timeouts = load_timeouts.saturating_add(1);
                if load_timeouts >= restart.load_attempts.max(1) {
                    tracing::warn!(
                        attempts = load_timeouts,
                        "live speakers models not ready within the load deadline; giving up"
                    );
                    self.set_identity(LiveIdentityStatus::Unavailable { reason }, epoch);
                    return Run::GaveUp;
                }
            }
            if started.elapsed() >= restart.healthy_after {
                failures = 0;
            }
            failures = failures.saturating_add(1);
            self.set_identity(LiveIdentityStatus::Restarting { reason }, epoch);
            let delay = backoff(
                restart.restart_backoff,
                restart.restart_backoff_max,
                failures,
            );
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = stop.wait() => return Run::Stopped,
            }
            if !config.enabled() {
                return Run::Disabled;
            }
        }
    }

    fn spawn(
        self: &Arc<Self>,
        worker: &WorkerCommand,
        library: &Path,
        epoch: u64,
        admission: &Arc<broscribe_core::maintenance::ModeLease>,
    ) -> std::io::Result<SpeakersChild> {
        let mut cmd = worker.std_command(
            library,
            &[OsStr::new("live-speakers"), OsStr::new(&self.session_id)],
        );
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn()?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::other("live speakers worker pipes missing"));
        };
        let start = serde_json::to_string(&SpeakersStart {
            session_id: self.session_id.clone(),
            journal_dir: self.journal_dir.clone(),
            epoch,
            settings: self.settings.clone(),
        })
        .map_err(std::io::Error::other)?;
        let (tx, rx) = mpsc::channel::<SpeakersInput>();
        *lock(&self.input) = Some(tx);
        let writer_admission = Arc::clone(admission);
        let writer = std::thread::Builder::new()
            .name("speakers-writer".into())
            .spawn(move || {
                let _admission = writer_admission;
                if writeln!(stdin, "{start}")
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    return;
                }
                // Ends when the sender is dropped (stop, restart): stdin
                // closes and the worker exits.
                for message in rx {
                    let Ok(line) = serde_json::to_string(&message) else {
                        continue;
                    };
                    if writeln!(stdin, "{line}")
                        .and_then(|()| stdin.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            })?;
        let progress = Progress::new();
        let reader = {
            let admission = Arc::clone(admission);
            let preview = Arc::clone(self);
            let progress = progress.clone();
            std::thread::Builder::new()
                .name("speakers-reader".into())
                .spawn(move || {
                    let _admission = admission;
                    for line in BufReader::new(stdout).lines() {
                        let Ok(line) = line else {
                            return;
                        };
                        // Before readiness only `loading`/`ready` are signs
                        // of life; after it, any valid line.
                        match SpeakersEvent::parse(&line) {
                            Ok(event) => {
                                if progress.is_ready()
                                    || matches!(
                                        event,
                                        SpeakersEvent::Loading { .. } | SpeakersEvent::Ready { .. }
                                    )
                                {
                                    progress.last_line.store(progress.now(), Ordering::SeqCst);
                                }
                                preview.on_event(event, epoch, &progress);
                            }
                            Err(e) => tracing::warn!("live speakers: {e}"),
                        }
                    }
                })?
        };
        Ok(SpeakersChild {
            child,
            reader,
            writer,
            progress,
        })
    }

    /// Waits for the worker to exit; kills it when it is not ready within
    /// `load_deadline`, is silent, or makes no useful progress while audio
    /// is pending; stops it through end of stdin when the session ends or
    /// the config turns analysis off. Returns how it ended and whether it
    /// had got ready.
    async fn watch(
        &self,
        mut w: SpeakersChild,
        stop: &Shutdown,
        config: &mut ConfigWatch,
        load_deadline: Duration,
    ) -> (Ended, bool) {
        let limit = (self.settings.progress_timeout_s * 1000.0) as u64;
        let hop_ms = (self.settings.hop_s * 1000.0) as u64;
        let load_ms = u64::try_from(load_deadline.as_millis()).unwrap_or(u64::MAX);
        let tick = Duration::from_millis(limit.clamp(1, 1000));
        let ended = loop {
            match w.child.try_wait() {
                Ok(Some(status)) if status.success() => break Ended::Finished,
                Ok(Some(status)) => {
                    tracing::warn!("live speakers worker exited ({status})");
                    break Ended::Failed(WorkerProblem::Exited);
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("live speakers worker: {e}");
                    let _ = w.child.kill();
                    break Ended::Failed(WorkerProblem::Exited);
                }
            }
            if stop.is_triggered() {
                self.close(&mut w.child).await;
                break Ended::Stopped;
            }
            if !config.enabled() {
                tracing::info!("live speakers turned off; stopping the worker");
                self.close(&mut w.child).await;
                break Ended::Disabled;
            }
            let p = &w.progress;
            let now = p.now();
            if !p.is_ready() && now > load_ms {
                tracing::warn!(
                    "live speakers worker not ready within {load_deadline:?}; restarting it"
                );
                let _ = w.child.kill();
                break Ended::Failed(WorkerProblem::LoadTimeout);
            }
            let heard = p.last_line.load(Ordering::SeqCst);
            if now.saturating_sub(heard) > limit {
                tracing::warn!("live speakers worker silent; restarting it");
                let _ = w.child.kill();
                break Ended::Failed(WorkerProblem::Silent);
            }
            // Heartbeats alone are not progress: audio pending and no
            // window completed for the limit, or one window (the first
            // included) running longer than the limit.
            let progressed = p.last_progress.load(Ordering::SeqCst);
            let pending = p.pending_ms.load(Ordering::SeqCst);
            let busy = p.busy_ms.load(Ordering::SeqCst);
            let stalled = progressed != u64::MAX
                && pending >= hop_ms
                && now.saturating_sub(progressed) > limit;
            if stalled || busy > limit {
                tracing::warn!("live speakers worker made no progress; restarting it");
                let _ = w.child.kill();
                break Ended::Failed(WorkerProblem::Stalled);
            }
            tokio::select! {
                () = tokio::time::sleep(tick) => {}
                () = stop.wait() => {}
            }
        };
        // Close stdin (the writer ends), reap the child, join the threads:
        // nothing of this worker is published after this returns.
        *lock(&self.input) = None;
        let ready = w.progress.is_ready();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = w.child.wait();
            let _ = w.writer.join();
            let _ = w.reader.join();
        })
        .await;
        (ended, ready)
    }

    /// End of stdin asks the worker to exit; kills it if it does not
    /// within two seconds.
    async fn close(&self, child: &mut Child) {
        *lock(&self.input) = None;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = child.kill();
    }
}

/// `record.live`: the snapshot of the active session's preview; another
/// session (or none) is `stale_session`.
pub fn snapshot_for(preview: Option<&LivePreview>, session_id: &str) -> Result<Value, IpcError> {
    match preview.filter(|p| p.session_id == session_id) {
        Some(p) => serde_json::to_value(p.snapshot())
            .map_err(|e| IpcError::new("internal", format!("serializing live preview: {e}"))),
        None => Err(IpcError::from(lifecycle::stale_session(session_id))),
    }
}

fn partial(person: &PersonId, error: &Error) -> Error {
    Error::Service {
        code: PARTIAL_OUTCOME,
        message: format!(
            "person {person} was created, but the live turn was not attributed ({})",
            error.code()
        ),
    }
}

/// `record.live_attribute`: one Name / Confirm / Clear on exactly one shown
/// turn. Under the library lock, `verify` runs first (it takes the session
/// mutex and returns a guard held until the end: the session cannot stop
/// meanwhile, and a stale one creates nothing); then the evidence
/// fingerprint is refreshed and the turn fenced by revision, epoch and
/// evidence, before any person is created. The decision goes to the
/// session sidecar with the turn's journal anchors; the worker gets the
/// range as a transient session-local sample (or its revocation).
pub fn attribute<G>(
    lib: &Library,
    journal_root: &Path,
    preview: &LivePreview,
    params: LiveAttributeParams,
    verify: impl FnOnce() -> broscribe_core::Result<G>,
) -> Result<Value, IpcError> {
    let mut published: Vec<LiveTurn> = Vec::new();
    let mut message: Option<SpeakersInput> = None;
    let out = ops::with_index(lib, |tx, index| {
        let _session = verify()?;
        if params.session_id != preview.session_id
            || lifecycle::has_discard_intent(journal_root, &params.session_id)
        {
            return Err(lifecycle::stale_session(&params.session_id));
        }
        let names_lib = tx.library().clone();
        let names = |id: &str| name_of(Some(&names_lib), id);
        let fingerprint = evidence_fingerprint(tx.library())?;
        let mut board = lock(&preview.board);
        published.extend(board.refresh_evidence(&fingerprint, &names));
        let turn = board.check(&params)?;
        let person_ref = params
            .person_ref
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        let (person, created) = match params.action {
            LiveAction::Name => {
                let reference = person_ref
                    .ok_or_else(|| Error::InvalidInput("name the person for this turn".into()))?;
                let (person, created) = ops::resolve_or_new_person(tx.library(), reference)?;
                if created {
                    // Persisted before any decision can name it.
                    tx.save_person(&person)?;
                    if let Err(e) = index.upsert_person(tx, &person) {
                        return Err(partial(&person.id, &e));
                    }
                }
                (Some(person), created)
            }
            LiveAction::Confirm => {
                let suggested = turn
                    .suggestion
                    .as_ref()
                    .map(|s| s.person.person_id.as_str());
                if person_ref.is_none() || person_ref != suggested {
                    return Err(Error::InvalidInput(
                        "confirm the person this turn suggests".into(),
                    ));
                }
                let id = PersonId::parse(person_ref.unwrap_or_default())?;
                (Some(tx.library().load_person(&id)?), false)
            }
            LiveAction::Clear => {
                if person_ref.is_some() {
                    return Err(Error::InvalidInput("a clear names no person".into()));
                }
                if turn.named.is_none() {
                    return Err(Error::InvalidInput(
                        "this turn has no name of yours to clear".into(),
                    ));
                }
                (None, false)
            }
        };
        let fail = |e: Error| match (&person, created) {
            (Some(p), true) => partial(&p.id, &e),
            _ => e,
        };
        if created {
            // The new person changes the evidence: stamp the result with it
            // (other open pickers reload).
            let after = evidence_fingerprint(tx.library()).map_err(fail)?;
            published.extend(board.refresh_evidence(&after, &names));
        }
        let manifest = journal::load_manifest(&preview.journal_dir).map_err(fail)?;
        let placed = placements(&manifest, capture_track(turn.track));
        let decision = record_decision(
            tx,
            &params.session_id,
            NewDecision {
                turn_id: turn.turn_id.clone(),
                epoch: turn.epoch,
                turn_revision: turn.revision,
                action: params.action,
                person_id: person.as_ref().map(|p| p.id.clone()),
                range: LiveRange {
                    track: turn.track.as_str().to_owned(),
                    from_sample: turn.from_sample,
                    to_sample: turn.to_sample,
                    from: journal_point(&placed, turn.from_sample),
                    to: journal_point(&placed, turn.to_sample),
                },
            },
        )
        .map_err(fail)?;
        // The person's forget fence, read in this same locked commit: the
        // worker keeps the transient sample only while it is still current,
        // so any later Forget invalidates it whatever the clocks say. An
        // unreadable fence sends none (the name stands, no sample).
        let forget_fence = person
            .as_ref()
            .and_then(|p| live_forget_fence(tx.library(), p.id.as_str()).ok());
        let named = person.as_ref().map(|p| LivePerson {
            person_id: p.id.as_str().to_owned(),
            name: p.name.clone(),
        });
        let view = board
            .apply(&turn.turn_id, named, &names)
            .ok_or_else(|| Error::Internal("live turn vanished under its lock".into()))?;
        published.push(view.clone());
        message = Some(match (&person, params.action) {
            (Some(p), LiveAction::Name | LiveAction::Confirm) => SpeakersInput::SessionSample {
                turn_id: turn.turn_id.clone(),
                decision: decision.decision,
                forget_fence,
                track: turn.track,
                from_sample: turn.from_sample,
                to_sample: turn.to_sample,
                person_id: p.id.as_str().to_owned(),
            },
            _ => SpeakersInput::RevokeSessionSample {
                turn_id: turn.turn_id.clone(),
                decision: decision.decision,
            },
        });
        Ok(LiveAttributeOutcome {
            decision: decision.decision,
            turn: view,
            created_person_id: created
                .then(|| person.as_ref().map(|p| p.id.as_str().to_owned()))
                .flatten(),
        })
    });
    preview.publish_turns(published);
    let outcome = out.map_err(IpcError::from)?;
    if let Some(message) = message {
        preview.send(message);
    }
    serde_json::to_value(outcome)
        .map_err(|e| IpcError::new("internal", format!("serializing live outcome: {e}")))
}
