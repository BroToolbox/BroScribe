//! Recorder adapter for the daemon's capture boundary. All journal mutations
//! belong to one thread. Opening devices (a session's first starts, and
//! restarts of a failed device: stopping the old session, enumerating and
//! opening the device again) runs on a second thread, so a slow device never
//! holds up journaling the audio of one already open. Preview delivery runs
//! separately on durable chunks.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use broscribe_core::lifecycle::STALE_SESSION;
use broscribe_ipc::{
    CaptureEngine, CaptureEvents, CaptureHealth, CaptureNotice, CaptureOutputs, CaptureWarning,
    ChunkCutoff, DiskHealth, DiskLevel, LiveFrame, OrphanFailure, OrphanRecovery, RecordingStatus,
    SessionConfig, SessionHandle, SessionStatus, StoppedSession, TrackHealth, TrackState,
};
use broscribe_platform::{
    AudioDevice, CaptureSession, MicCapture, MicChunk, MicSink, PriorityClass, ProcessPriority,
    SystemAudioCapture, SystemInfo,
};

use crate::disk::{DiskGuard, journal_bytes_per_second};
use crate::journal::{self, CHUNK_SECONDS, JournalWriter, SessionMeta, Track};
use crate::live::{LiveAudio, LiveFeed, LiveItem};
use crate::resample::Resampler;
use crate::{Error, Result};

const POLL: Duration = Duration::from_millis(20);
const AUDIO_QUEUE: usize = 256;
/// How often free disk space is checked.
const DISK_CHECK: Duration = Duration::from_secs(1);
/// Wait before the second restart attempt of a failed track; each further
/// attempt waits twice as long, up to [`LONGEST_RETRY`]. The first attempt
/// is immediate (a device or route switch).
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LONGEST_RETRY: Duration = Duration::from_secs(30);
/// A capture that runs this long before failing starts its backoff afresh.
const HEALTHY_AFTER: Duration = Duration::from_secs(30);
/// Capture stops on its own once neither track has captured for this long.
const ALL_DOWN_LIMIT: Duration = Duration::from_secs(60);
/// How long a closed chunk's manifest update may wait for the next chunk's
/// opening (one write for both).
const MANIFEST_DELAY: Duration = Duration::from_millis(250);
/// The live preview's rate.
const PREVIEW_RATE: f64 = 16_000.0;
/// Conservative device rate for the disk estimate of a track whose device
/// has not reported its own.
const ESTIMATED_RATE: u32 = 48_000;
/// At most one changed-health notice per interval, unless a track's state
/// or the disk level changed.
const HEALTH_EVERY: Duration = Duration::from_secs(1);

/// Platform-independent recorder; native adapters are supplied by the daemon.
pub struct Recorder {
    mic: Arc<dyn MicCapture>,
    system: Arc<dyn SystemAudioCapture>,
    info: Arc<dyn SystemInfo>,
    priority: Arc<dyn ProcessPriority>,
}

/// Health as the journal thread keeps it, mic first.
struct Health {
    tracks: [TrackHealth; 2],
    retry_at: [Option<Instant>; 2],
    disk: Option<DiskHealth>,
}

impl Health {
    fn new() -> Self {
        Self {
            tracks: Track::ALL.map(|track| TrackHealth::capturing(ipc_track(track))),
            retry_at: [None, None],
            disk: None,
        }
    }

    fn snapshot(&self) -> CaptureHealth {
        let now = Instant::now();
        let tracks = self
            .tracks
            .iter()
            .zip(self.retry_at)
            .map(|(track, at)| TrackHealth {
                retry_in_s: at.map(|at| at.saturating_duration_since(now).as_secs_f64()),
                ..track.clone()
            })
            .collect();
        CaptureHealth {
            tracks,
            disk: self.disk,
        }
    }
}

/// The pinned microphone when the platform lists it (connected); `None`
/// means the default input. The one rule for every path that opens or
/// probes the mic, so an unplugged pin never blocks capture.
pub fn pinned_input(mic: &dyn MicCapture, pinned: Option<&str>) -> Option<AudioDevice> {
    let pinned = pinned?;
    mic.devices().ok()?.into_iter().find(|d| d.id == pinned)
}

impl Recorder {
    pub fn new(
        mic: Arc<dyn MicCapture>,
        system: Arc<dyn SystemAudioCapture>,
        info: Arc<dyn SystemInfo>,
        priority: Arc<dyn ProcessPriority>,
    ) -> Self {
        Self {
            mic,
            system,
            info,
            priority,
        }
    }

    /// Starts a session: returns once both devices capture into a new
    /// journal. [`CaptureEngine::start`] is this, boxed.
    pub fn start_session(
        &self,
        cfg: SessionConfig,
        out: CaptureOutputs,
    ) -> Result<RecorderSession> {
        // Use a conservative device-rate estimate until the first callbacks.
        let free = self.info.free_disk_bytes(&cfg.journal_root)?;
        if DiskGuard::default()
            .check(
                free,
                journal_bytes_per_second(&[ESTIMATED_RATE, ESTIMATED_RATE]),
            )
            .is_low()
        {
            return Err(Error::Service {
                code: broscribe_ipc::IpcError::DISK_LOW,
                message: "insufficient disk headroom for recording".into(),
            });
        }
        let mut journal = JournalWriter::create(
            &cfg.journal_root,
            &cfg.session_id,
            SessionMeta {
                library_root: Some(cfg.library_root.clone()),
                app: cfg.app.clone(),
                title: cfg.title.clone(),
                setup: cfg.setup.clone(),
                language: cfg.language.clone(),
                tags: cfg.tags.clone(),
                title_detected: cfg.title_detected,
                echo_handling: cfg.echo_handling.clone(),
                in_person_setup: cfg.in_person_setup.clone(),
                meeting_app_seen: false,
                pinned_mic: cfg.mic_device.clone(),
            },
            CHUNK_SECONDS,
            std::process::id(),
        )?;
        journal.set_started_at(cfg.started_at.to_utc())?;
        journal.defer_close_writes(MANIFEST_DELAY);
        journal.keep_closed_audio();
        let status = Arc::new(Mutex::new(SessionStatus {
            status: RecordingStatus::Recording,
            elapsed_s: 0.0,
            language: cfg.language,
            setup: cfg.setup,
            participants: journal
                .manifest()
                .participants
                .iter()
                .filter(|participant| !participant.removed)
                .map(|participant| participant.person_ref.clone())
                .collect(),
            tags: journal.manifest().meta.tags.clone(),
        }));
        let health = Arc::new(Mutex::new(Health::new()));
        let ended = Arc::new(Mutex::new(None));
        let (commands, command_rx) = mpsc::channel::<Command>();
        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_QUEUE);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let job = Job {
            journal,
            session_id: cfg.session_id.clone(),
            devices: Devices {
                mic: self.mic.clone(),
                system: self.system.clone(),
                pinned: cfg.mic_device,
                audio: audio_tx,
                dropped: Arc::new([AtomicU64::new(0), AtomicU64::new(0)]),
            },
            audio: audio_rx,
            commands: command_rx,
            out,
            info: self.info.clone(),
            priority: self.priority.clone(),
            status: status.clone(),
            health: health.clone(),
            ended: ended.clone(),
        };
        let writer = thread::Builder::new()
            .name("capture-journal".into())
            .spawn(move || job.run(ready_tx))
            .map_err(|e| Error::io("starting journal writer", e))?;
        ready_rx
            .recv()
            .map_err(|_| Error::Internal("capture startup exited".into()))??;
        Ok(RecorderSession {
            id: cfg.session_id,
            commands,
            status,
            health,
            ended,
            writer: Some(writer),
        })
    }
}

struct Audio {
    track: Track,
    generation: u32,
    chunk: MicChunk,
    at: Instant,
}

enum Op {
    Pause,
    Resume,
    Language(String),
    Bookmark(Option<String>),
    Note(String),
    Tags(Vec<String>),
    RemoveTags(Vec<String>),
    MeetingAppSeen,
    Participant(String, String, bool),
    Cutoff,
    Stop,
}

enum Reply {
    Done,
    At(f64),
    Cutoff(ChunkCutoff),
    Stopped(StoppedSession),
}
struct Command {
    op: Op,
    reply: mpsc::SyncSender<Result<Reply>>,
}

/// How the journal thread ended. Recorded with the reason as soon as it
/// decides to stop, before it closes the journal.
struct Ended {
    reason: String,
    /// The closed journal; `None` while the journal thread is closing it,
    /// or when closing it failed (a later recovery sweep closes it).
    stopped: Option<StoppedSession>,
}

/// A running recording. [`CaptureEngine::start`] returns it boxed;
/// [`Recorder::start_session`] also gives access to [`Self::health`].
pub struct RecorderSession {
    id: String,
    commands: mpsc::Sender<Command>,
    status: Arc<Mutex<SessionStatus>>,
    health: Arc<Mutex<Health>>,
    ended: Arc<Mutex<Option<Ended>>>,
    writer: Option<JoinHandle<()>>,
}

impl RecorderSession {
    fn call(&self, op: Op) -> Result<Reply> {
        let (tx, rx) = mpsc::sync_channel(1);
        if self.commands.send(Command { op, reply: tx }).is_err() {
            return Err(self.gone());
        }
        rx.recv().map_err(|_| self.gone())?
    }

    /// Why a request found the journal thread gone.
    fn gone(&self) -> Error {
        match lock(&self.ended).as_ref() {
            Some(ended) => not_applied(&self.id, &ended.reason),
            None => Error::Internal("capture writer exited".into()),
        }
    }
}

/// A request that reached a session after it stopped on its own (disk
/// guard, journal error, devices gone): nothing was applied.
fn not_applied(session_id: &str, reason: &str) -> Error {
    Error::Service {
        code: STALE_SESSION,
        message: format!(
            "recording session {session_id} stopped on its own ({reason}); \
             the request was not applied"
        ),
    }
}

impl SessionHandle for RecorderSession {
    fn session_id(&self) -> &str {
        &self.id
    }
    fn status(&self) -> SessionStatus {
        lock(&self.status).clone()
    }
    /// Per-track capture state, dropped audio and disk headroom, as the
    /// journal thread last measured them.
    fn health(&self) -> CaptureHealth {
        lock(&self.health).snapshot()
    }
    fn pause(&mut self) -> Result<()> {
        self.call(Op::Pause).map(|_| ())
    }
    fn resume(&mut self) -> Result<()> {
        self.call(Op::Resume).map(|_| ())
    }
    fn set_language(&mut self, language: &str) -> Result<f64> {
        match self.call(Op::Language(language.into()))? {
            Reply::At(at) => Ok(at),
            _ => Err(Error::Internal("invalid capture reply".into())),
        }
    }
    fn bookmark(&mut self, label: Option<&str>) -> Result<f64> {
        match self.call(Op::Bookmark(label.map(str::to_owned)))? {
            Reply::At(at) => Ok(at),
            _ => Err(Error::Internal("invalid capture reply".into())),
        }
    }
    fn note(&mut self, text: &str) -> Result<f64> {
        match self.call(Op::Note(text.into()))? {
            Reply::At(at) => Ok(at),
            _ => Err(Error::Internal("invalid capture reply".into())),
        }
    }
    fn add_tags(&mut self, tags: &[String]) -> Result<()> {
        self.call(Op::Tags(tags.into())).map(|_| ())
    }
    fn remove_tags(&mut self, tags: &[String]) -> Result<()> {
        self.call(Op::RemoveTags(tags.into())).map(|_| ())
    }
    fn mark_meeting_app_seen(&mut self) -> Result<()> {
        self.call(Op::MeetingAppSeen).map(|_| ())
    }
    fn participant(&mut self, person_id: &str, name: &str, confirm: bool) -> Result<()> {
        self.call(Op::Participant(person_id.into(), name.into(), confirm))
            .map(|_| ())
    }
    fn snapshot_cutoff(&mut self) -> Result<ChunkCutoff> {
        match self.call(Op::Cutoff)? {
            Reply::Cutoff(c) => Ok(c),
            _ => Err(Error::Internal("invalid capture reply".into())),
        }
    }
    fn stop(mut self: Box<Self>) -> Result<StoppedSession> {
        // `None`: no reply, as the session had stopped on its own (admission
        // closed). Its journal thread may still be closing the journal, so
        // `ended` is read only after the join.
        let (tx, rx) = mpsc::sync_channel(1);
        let reply = self
            .commands
            .send(Command {
                op: Op::Stop,
                reply: tx,
            })
            .ok()
            .and_then(|()| rx.recv().ok());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        match reply {
            Some(Ok(Reply::Stopped(stopped))) => Ok(stopped),
            Some(Ok(_)) => Err(Error::Internal("invalid capture reply".into())),
            Some(Err(error)) => Err(error),
            None => match lock(&self.ended).as_ref() {
                Some(Ended {
                    stopped: Some(stopped),
                    ..
                }) => Ok(stopped.clone()),
                // Stopped on its own, but the journal did not close.
                Some(ended) => Err(Error::Internal(format!(
                    "capture stopped ({}) but its journal could not be closed",
                    ended.reason
                ))),
                None => Err(Error::Internal("capture writer exited".into())),
            },
        }
    }
}

impl Drop for RecorderSession {
    fn drop(&mut self) {
        if self.writer.is_some() {
            let _ = self.call(Op::Stop);
            if let Some(writer) = self.writer.take() {
                let _ = writer.join();
            }
        }
    }
}

impl CaptureEngine for Recorder {
    fn start(&self, cfg: SessionConfig, out: CaptureOutputs) -> Result<Box<dyn SessionHandle>> {
        Ok(Box::new(self.start_session(cfg, out)?))
    }

    /// Recovers every journal it can; one that cannot be recovered is logged
    /// (folder name and code only), reported and left untouched without
    /// hiding the others. Only an unlistable journal root fails.
    fn recover_orphans(&self, root: &Path, library_root: &Path) -> Result<OrphanRecovery> {
        let recovery = journal::recover_orphans(root, library_root)?;
        let failed = recovery
            .failed
            .iter()
            .map(|failure| {
                let session_id = failure
                    .dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let code = failure.error.code();
                let corrupt = failure.corrupt();
                tracing::warn!(
                    dir = %session_id,
                    code,
                    corrupt,
                    "recording journal not recovered"
                );
                OrphanFailure {
                    session_id,
                    journal_dir: failure.dir.clone(),
                    code: code.to_owned(),
                    corrupt,
                }
            })
            .collect();
        let recovered = recovery
            .recovered
            .into_iter()
            .map(|session| StoppedSession {
                session_id: session.manifest.session_id,
                journal_dir: session.dir,
            })
            .collect();
        Ok(OrphanRecovery { recovered, failed })
    }
}

/// The device a capture opened.
struct DeviceInfo {
    /// The device asked for by ID (a connected pinned mic); `None`: default.
    id: Option<String>,
    name: Option<String>,
}

type Opened = (Box<dyn CaptureSession>, DeviceInfo);

/// Opens capture sessions whose audio goes to the journal thread.
struct Devices {
    mic: Arc<dyn MicCapture>,
    system: Arc<dyn SystemAudioCapture>,
    /// Pinned microphone: used while connected, the default input otherwise,
    /// so an unplugged pinned mic never stops a recording from starting.
    pinned: Option<String>,
    audio: mpsc::SyncSender<Audio>,
    dropped: Arc<[AtomicU64; 2]>,
}

impl Devices {
    fn start(&self, track: Track, generation: u32) -> Result<Opened> {
        let sink = sink(track, generation, &self.audio, &self.dropped);
        match track {
            Track::Mic => {
                let listed = pinned_input(self.mic.as_ref(), self.pinned.as_deref());
                let id = listed.as_ref().map(|d| d.id.clone());
                let session = self.mic.start(id.as_deref(), sink)?;
                let name = session.device_name().or(listed.map(|d| d.name));
                Ok((session, DeviceInfo { id, name }))
            }
            Track::System => {
                let session = self.system.start(sink)?;
                let name = session.device_name();
                Ok((session, DeviceInfo { id: None, name }))
            }
        }
    }
}

enum DeviceOp {
    Stop(Box<dyn CaptureSession>),
    Start { track: Track, generation: u32 },
}

struct Started {
    track: Track,
    generation: u32,
    result: Result<Opened>,
}

/// Runs device starts and stops in order, off the journal thread. A
/// session started after the journal thread left is stopped at once.
fn spawn_device_thread(
    devices: Devices,
    ops: mpsc::Receiver<DeviceOp>,
    done: mpsc::Sender<Started>,
    priority: Arc<dyn ProcessPriority>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("capture-devices".into())
        .spawn(move || {
            let _ = priority.set_current_thread(PriorityClass::Capture);
            for op in ops {
                match op {
                    DeviceOp::Stop(capture) => {
                        let _ = capture.stop();
                    }
                    DeviceOp::Start { track, generation } => {
                        let started = Started {
                            track,
                            generation,
                            result: devices.start(track, generation),
                        };
                        if let Err(mpsc::SendError(started)) = done.send(started)
                            && let Ok((capture, _)) = started.result
                        {
                            let _ = capture.stop();
                        }
                    }
                }
            }
        })
}

/// One track's capture on the journal thread.
struct TrackRun {
    capture: Option<Box<dyn CaptureSession>>,
    /// The rate of the latest capture opened on the track, kept after it
    /// failed (also before its first audio).
    rate: u32,
    /// The capture session counter; audio carries it.
    generation: u32,
    /// The device of a generation, once its start returned.
    device: Option<(u32, DeviceInfo)>,
    /// When the current capture started, or when the track went down.
    since: Instant,
    failures: u32,
    retry_at: Option<Instant>,
    /// A start of `generation` is on the device thread.
    starting: bool,
}

/// Changed-health notices of the journal thread.
struct HealthNotice {
    /// Health changed since the last notice.
    pending: bool,
    /// A track's state or the disk level changed: sent without waiting.
    urgent: bool,
    sent: Instant,
}

impl HealthNotice {
    fn changed(&mut self, urgent: bool) {
        self.pending = true;
        self.urgent |= urgent;
    }
}

/// Restart delay after `failures` consecutive failures.
fn backoff(failures: u32) -> Duration {
    match failures {
        0 | 1 => Duration::ZERO,
        n => FIRST_RETRY
            .saturating_mul(1 << (n - 2).min(5))
            .min(LONGEST_RETRY),
    }
}

/// Both tracks' captures, supervised by the journal thread: a failed track
/// is restarted with backoff while the other keeps recording.
struct Tracks {
    runs: [TrackRun; 2],
    ops: mpsc::Sender<DeviceOp>,
    health: Arc<Mutex<Health>>,
    health_notice: HealthNotice,
    events: Arc<dyn CaptureEvents>,
}

impl Tracks {
    fn warn(&self, warning: CaptureWarning) {
        self.events.notice(CaptureNotice::Warning(warning));
    }

    /// Sends a due [`CaptureNotice::HealthChanged`]. Called with no lock
    /// held: the receiver reads the health back.
    fn send_health(&mut self) {
        let notice = &mut self.health_notice;
        if notice.pending && (notice.urgent || notice.sent.elapsed() >= HEALTH_EVERY) {
            notice.pending = false;
            notice.urgent = false;
            notice.sent = Instant::now();
            self.events.notice(CaptureNotice::HealthChanged);
        }
    }

    /// Asks the device thread for the first capture of `track`.
    fn open(&mut self, track: Track) -> Result<()> {
        self.runs[track.index()].starting = true;
        self.ops
            .send(DeviceOp::Start {
                track,
                generation: 0,
            })
            .map_err(|_| Error::Internal("capture device thread exited".into()))
    }

    /// A first start returned, before the session is admitted: the track
    /// captures, announced to nobody (it resumed nothing, and its health is
    /// already capturing). The microphone opening is what opens the system
    /// track, so a refused microphone never opens it. An error fails the
    /// start: it is never retried.
    fn opened(&mut self, journal: &mut JournalWriter, started: Started) -> Result<()> {
        let run = &mut self.runs[started.track.index()];
        run.starting = false;
        let (capture, device) = started.result?;
        run.rate = capture.sample_rate();
        run.capture = Some(capture);
        run.since = Instant::now();
        let (id, name) = (device.id.clone(), device.name.clone());
        run.device = Some((started.generation, device));
        // Its audio may have begun the segment before the start returned.
        journal.set_device(
            started.track,
            started.generation,
            name.as_deref(),
            id.as_deref(),
        )?;
        if started.track == Track::Mic {
            self.open(Track::System)?;
        }
        Ok(())
    }

    /// A restart finished: the track captures again, or waits for its next
    /// attempt.
    fn started(&mut self, journal: &mut JournalWriter, started: Started) -> Result<()> {
        let t = started.track.index();
        let run = &mut self.runs[t];
        if !run.starting || started.generation != run.generation {
            if let Ok((capture, _)) = started.result {
                self.stop(capture);
            }
            return Ok(());
        }
        run.starting = false;
        match started.result {
            Ok((capture, device)) => {
                let gap = run.since.elapsed().as_secs_f64();
                run.rate = capture.sample_rate();
                run.capture = Some(capture);
                run.since = Instant::now();
                let generation = run.generation;
                let (id, device_name) = (device.id.clone(), device.name.clone());
                run.device = Some((generation, device));
                {
                    let mut health = lock(&self.health);
                    health.tracks[t].state = TrackState::Capturing;
                    health.retry_at[t] = None;
                }
                self.health_notice.changed(true);
                self.warn(CaptureWarning::TrackResumed {
                    track: ipc_track(started.track),
                    gap_s: gap,
                });
                // Audio of this generation may have begun its segment first.
                journal.set_device(
                    started.track,
                    generation,
                    device_name.as_deref(),
                    id.as_deref(),
                )
            }
            Err(error) => {
                run.failures += 1;
                let wait = backoff(run.failures);
                let at = Instant::now() + wait;
                run.retry_at = Some(at);
                {
                    let mut health = lock(&self.health);
                    let track = &mut health.tracks[t];
                    track.state = TrackState::Waiting;
                    track.failures = run.failures;
                    track.last_error = Some(error.to_string());
                    health.retry_at[t] = Some(at);
                }
                self.health_notice.changed(true);
                self.warn(CaptureWarning::TrackDown {
                    track: ipc_track(started.track),
                    error: error.to_string(),
                    retry_in_s: wait.as_secs_f64(),
                });
                Ok(())
            }
        }
    }

    /// Takes failed captures off their tracks and schedules their restarts.
    fn check_failures(&mut self) {
        for track in Track::ALL {
            let t = track.index();
            let run = &mut self.runs[t];
            let Some(reason) = run.capture.as_ref().and_then(|c| c.failure()) else {
                continue;
            };
            let Some(capture) = run.capture.take() else {
                continue;
            };
            if run.since.elapsed() >= HEALTHY_AFTER {
                run.failures = 0;
            }
            run.failures += 1;
            let wait = backoff(run.failures);
            run.since = Instant::now();
            run.retry_at = Some(run.since + wait);
            {
                let mut health = lock(&self.health);
                let state = &mut health.tracks[t];
                state.state = TrackState::Waiting;
                state.failures = run.failures;
                state.last_error = Some(reason.clone());
                health.retry_at[t] = run.retry_at;
            }
            self.health_notice.changed(true);
            self.stop(capture);
            self.warn(CaptureWarning::TrackDown {
                track: ipc_track(track),
                error: reason,
                retry_in_s: wait.as_secs_f64(),
            });
        }
    }

    /// Starts the restarts whose backoff is over; `false` when the device
    /// thread is gone.
    fn retry_due(&mut self) -> bool {
        let now = Instant::now();
        for track in Track::ALL {
            let t = track.index();
            let run = &mut self.runs[t];
            if run.capture.is_some() || run.starting || run.retry_at.is_none_or(|at| at > now) {
                continue;
            }
            run.generation += 1;
            run.starting = true;
            run.retry_at = None;
            let generation = run.generation;
            {
                let mut health = lock(&self.health);
                health.tracks[t].state = TrackState::Restarting;
                health.retry_at[t] = None;
            }
            self.health_notice.changed(true);
            if self
                .ops
                .send(DeviceOp::Start { track, generation })
                .is_err()
            {
                return false;
            }
        }
        true
    }

    /// Neither track has captured for [`ALL_DOWN_LIMIT`].
    fn all_down_too_long(&self) -> bool {
        self.runs.iter().all(|r| r.capture.is_none())
            && self
                .runs
                .iter()
                .map(|r| r.since)
                .max()
                .is_some_and(|since| since.elapsed() >= ALL_DOWN_LIMIT)
    }

    /// Device rates for the disk estimate: the running capture's, else the
    /// latest one opened on the track, so a track down before or after its
    /// first audio still counts. A device reporting no rate counts at
    /// [`ESTIMATED_RATE`]: the headroom is always finite.
    fn rates(&self) -> [u32; 2] {
        self.runs.each_ref().map(|run| {
            match run.capture.as_ref().map_or(run.rate, |c| c.sample_rate()) {
                0 => ESTIMATED_RATE,
                rate => rate,
            }
        })
    }

    /// Stops `capture` on the device thread (here when it is gone).
    fn stop(&self, capture: Box<dyn CaptureSession>) {
        if let Err(mpsc::SendError(DeviceOp::Stop(capture))) =
            self.ops.send(DeviceOp::Stop(capture))
        {
            let _ = capture.stop();
        }
    }
}

/// Everything the journal thread owns.
struct Job {
    journal: JournalWriter,
    session_id: String,
    devices: Devices,
    audio: mpsc::Receiver<Audio>,
    commands: mpsc::Receiver<Command>,
    out: CaptureOutputs,
    info: Arc<dyn SystemInfo>,
    priority: Arc<dyn ProcessPriority>,
    status: Arc<Mutex<SessionStatus>>,
    health: Arc<Mutex<Health>>,
    ended: Arc<Mutex<Option<Ended>>>,
}

/// Closes and deletes the journal of a start that failed.
fn abandon(journal: JournalWriter, now: u64, captures: Vec<Box<dyn CaptureSession>>) {
    for capture in captures {
        let _ = capture.stop();
    }
    let dir = journal.dir().to_owned();
    let _ = journal.finish(now, "start_failed");
    let _ = journal::remove(&dir);
}

impl Job {
    fn run(self, ready: mpsc::SyncSender<Result<()>>) {
        let Job {
            mut journal,
            session_id,
            devices,
            audio: audio_rx,
            commands: command_rx,
            out,
            info,
            priority,
            status,
            health,
            ended,
        } = self;
        let anchor = Instant::now();
        let _ = priority.set_current_thread(PriorityClass::Capture);
        let dropped = devices.dropped.clone();
        let (ops, ops_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let device_thread = match spawn_device_thread(devices, ops_rx, started_tx, priority.clone())
        {
            Ok(thread) => thread,
            Err(e) => {
                abandon(journal, ns(anchor), Vec::new());
                let _ = ready.send(Err(Error::io("starting capture device thread", e)));
                return;
            }
        };
        let feed = Arc::new(LiveFeed::new(10.0));
        let preview_control = Arc::new(PreviewControl::default());
        let sender = spawn_sender(
            feed.clone(),
            preview_control.clone(),
            out.clone(),
            priority.clone(),
        )
        .ok();
        let now = Instant::now();
        let pending = || TrackRun {
            capture: None,
            rate: 0,
            generation: 0,
            device: None,
            since: now,
            failures: 0,
            retry_at: None,
            starting: false,
        };
        let mut tracks = Tracks {
            runs: [pending(), pending()],
            ops,
            health: health.clone(),
            health_notice: HealthNotice {
                pending: false,
                urgent: false,
                sent: now,
            },
            events: out.events.clone(),
        };
        // The devices open on the device thread, the microphone first, while
        // this thread already journals and publishes the audio of those open.
        // Until both are, the session is not admitted: the start waits for
        // them as long as they take, nothing is supervised or announced, and
        // any failure fails the start.
        let mut ready = Some(ready);
        let mut start_error = None;
        let mut stopped_reason: Option<String> = None;
        if let Err(e) = tracks.open(Track::Mic) {
            stopped_reason = Some("start_failed".into());
            start_error = Some(e);
        }
        let mut emitted = [None; 2];
        let mut disk_checked = Instant::now();
        let mut disk_warned = false;
        let mut meters_at = Instant::now();
        let mut levels = [0.0f32; 2];
        loop {
            loop {
                let started = match started_rx.try_recv() {
                    Ok(started) => started,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        // Before admission the device thread ends only by
                        // failing in a start; afterwards a restart request
                        // finds it gone.
                        if ready.is_some() && stopped_reason.is_none() {
                            stopped_reason = Some("start_failed".into());
                            start_error =
                                Some(Error::Internal("capture device thread exited".into()));
                        }
                        break;
                    }
                };
                if ready.is_some() {
                    if let Err(e) = tracks.opened(&mut journal, started)
                        && stopped_reason.is_none()
                    {
                        stopped_reason = Some("start_failed".into());
                        start_error = Some(e);
                    }
                } else if let Err(e) = tracks.started(&mut journal, started)
                    && stopped_reason.is_none()
                {
                    stopped_reason = Some(format!("journal_error: {e}"));
                }
            }
            if ready.is_some()
                && stopped_reason.is_none()
                && tracks.runs.iter().all(|run| run.capture.is_some())
                && let Some(ready) = ready.take()
                && ready.send(Ok(())).is_ok()
            {
                preview_control.admitted.store(true, Ordering::Release);
                if sender.is_none() {
                    out.events
                        .notice(CaptureNotice::Warning(CaptureWarning::PreviewUnavailable));
                }
            }
            let batch = (|| -> Result<()> {
                // Bound each drain so commands cannot starve under continuous input.
                for _ in 0..AUDIO_QUEUE {
                    let Ok(audio) = audio_rx.try_recv() else {
                        break;
                    };
                    levels[audio.track.index()] =
                        levels[audio.track.index()].max(audio.chunk.level_peak);
                    append(&mut journal, anchor, audio, &tracks.runs)?;
                }
                for track in Track::ALL {
                    let lost = dropped[track.index()].swap(0, Ordering::Relaxed);
                    if lost > 0 {
                        journal.add_dropped(track, lost);
                        lock(&health).tracks[track.index()].dropped_frames =
                            journal.manifest().track(track).dropped_frames;
                        tracks.health_notice.changed(false);
                    }
                }
                journal.flush_due()?;
                publish(&mut journal, &feed, &mut emitted)?;
                Ok(())
            })();
            if let Err(e) = batch
                && stopped_reason.is_none()
            {
                stopped_reason = Some(format!("journal_error: {e}"));
                start_error = ready.is_some().then_some(e);
            }
            // Supervision begins with admission: a first start is neither
            // restarted nor limited in time.
            if stopped_reason.is_none() && ready.is_none() {
                tracks.check_failures();
                if !tracks.retry_due() || tracks.all_down_too_long() {
                    stopped_reason = Some("capture_device_unavailable".into());
                }
            }
            if stopped_reason.is_none() && disk_checked.elapsed() >= DISK_CHECK {
                disk_checked = Instant::now();
                let rates = tracks.rates();
                match info.free_disk_bytes(journal.dir()) {
                    Ok(free) => {
                        let guard = DiskGuard::default();
                        let disk = guard.check(free, journal_bytes_per_second(&rates));
                        let measured = DiskHealth {
                            free_bytes: disk.free_bytes,
                            headroom_s: disk.headroom_s,
                            level: disk.level,
                        };
                        let previous = lock(&health).disk.replace(measured);
                        if previous != Some(measured) {
                            tracks
                                .health_notice
                                .changed(previous.map(|d| d.level) != Some(disk.level));
                        }
                        match disk.level {
                            DiskLevel::Low => stopped_reason = Some("disk_low".into()),
                            // Warned once the session is admitted.
                            DiskLevel::Warning if !disk_warned && ready.is_none() => {
                                disk_warned = true;
                                out.events.notice(CaptureNotice::Warning(
                                    CaptureWarning::DiskLow {
                                        free_bytes: disk.free_bytes,
                                        headroom_s: disk.headroom_s,
                                        stop_below_bytes: guard.min_free_bytes,
                                        stop_below_s: guard.min_headroom_s,
                                    },
                                ));
                            }
                            DiskLevel::Warning => {}
                            DiskLevel::Ok => disk_warned = false,
                        }
                    }
                    Err(_) => stopped_reason = Some("disk_check_failed".into()),
                }
            }
            if stopped_reason.is_none() && ready.is_none() {
                tracks.send_health();
            }
            if meters_at.elapsed() >= Duration::from_millis(100) {
                if ready.is_none() {
                    out.events.notice(CaptureNotice::Meters {
                        me_dbfs: dbfs(levels[0]),
                        others_dbfs: dbfs(levels[1]),
                    });
                }
                levels = [0.0; 2];
                meters_at = Instant::now();
            }
            report(&status, &journal, ns(anchor));
            let command = match command_rx.recv_timeout(POLL) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    stopped_reason = Some("owner_gone".into());
                    None
                }
            };
            if command.is_some() && stopped_reason.is_none() {
                for _ in 0..AUDIO_QUEUE {
                    let Ok(audio) = audio_rx.try_recv() else {
                        break;
                    };
                    if let Err(error) = append(&mut journal, anchor, audio, &tracks.runs) {
                        stopped_reason = Some(format!("journal_error: {error}"));
                        break;
                    }
                }
            }
            let explicit_stop = command.as_ref().is_some_and(|c| matches!(c.op, Op::Stop));
            if stopped_reason.is_some() || explicit_stop {
                let reason = stopped_reason.clone().unwrap_or_else(|| "user".into());
                if let Some(ready) = ready.take() {
                    // The start failed before the session was admitted, so
                    // nothing of it is kept: the captures already open stop
                    // and the journal is gone before the start returns. The
                    // start never waits for a device still opening: once
                    // this thread lets go of the device thread, that start
                    // finds nobody to take its capture and stops it.
                    let error = start_error.take().unwrap_or_else(|| {
                        Error::Internal(format!("capture start failed ({reason})"))
                    });
                    let Tracks { runs, ops, .. } = tracks;
                    abandon(
                        journal,
                        ns(anchor),
                        runs.into_iter().filter_map(|run| run.capture).collect(),
                    );
                    preview_control.stop.store(true, Ordering::Release);
                    if let Some(sender) = sender {
                        let _ = sender.join();
                    }
                    let _ = ready.send(Err(error));
                    drop(ops);
                    drop(started_rx);
                    let _ = device_thread.join();
                    return;
                }
                // Recorded and answered before any device is stopped or the
                // device thread joined: a restart can be blocked in device
                // initialisation for long, and a request (sent under the
                // daemon's session mutex) must not wait for it. A request
                // arriving later learns why the session is gone.
                *lock(&ended) = Some(Ended {
                    reason: reason.clone(),
                    stopped: None,
                });
                let stop_reply = command.and_then(|c| match c.op {
                    Op::Stop => Some(c.reply),
                    // Raced a stop of its own: never acknowledged.
                    _ => {
                        let _ = c.reply.send(Err(not_applied(&session_id, &reason)));
                        None
                    }
                });
                // Admission closes: queued requests are refused and later
                // sends fail at once. A queued stop gets no reply: it reads
                // the closed journal from `ended` once this thread exits.
                for queued in command_rx.try_iter() {
                    if !matches!(queued.op, Op::Stop) {
                        let _ = queued.reply.send(Err(not_applied(&session_id, &reason)));
                    }
                }
                drop(command_rx);
                let Tracks { mut runs, ops, .. } = tracks;
                for run in &mut runs {
                    if let Some(capture) = run.capture.take() {
                        let _ = capture.stop();
                    }
                }
                // No restart outlives the session.
                drop(ops);
                let _ = device_thread.join();
                for started in started_rx.try_iter() {
                    if let Ok((capture, _)) = started.result {
                        let _ = capture.stop();
                    }
                }
                let result = (|| -> Result<StoppedSession> {
                    while let Ok(audio) = audio_rx.try_recv() {
                        append(&mut journal, anchor, audio, &runs)?;
                    }
                    journal.rotate()?;
                    publish(&mut journal, &feed, &mut emitted)?;
                    let now = ns(anchor);
                    report(&status, &journal, now);
                    let dir = journal.dir().to_path_buf();
                    let id = journal.manifest().session_id.clone();
                    journal.finish(now, &reason)?;
                    Ok(StoppedSession {
                        session_id: id,
                        journal_dir: dir,
                    })
                })();
                *lock(&ended) = Some(Ended {
                    reason,
                    stopped: result.as_ref().ok().cloned(),
                });
                if let Some(reply) = stop_reply {
                    let _ = reply.send(result.map(Reply::Stopped));
                }
                if let Some(reason) = stopped_reason {
                    out.events.notice(CaptureNotice::Stopped { reason });
                }
                preview_control.stop.store(true, Ordering::Release);
                if let Some(sender) = sender {
                    let _ = sender.join();
                }
                break;
            }
            if let Some(c) = command {
                let result = (|| -> Result<Reply> {
                    let now = ns(anchor);
                    let at = journal.recording_s(now);
                    match c.op {
                        // Reported before the reply: the daemon reads the status
                        // right after it to start or stop the meeting readers.
                        Op::Pause => {
                            journal.pause(now)?;
                            report(&status, &journal, now);
                        }
                        Op::Resume => {
                            journal.resume(now)?;
                            report(&status, &journal, now);
                        }
                        Op::Language(language) => {
                            journal.set_language(now, &language)?;
                            lock(&status).language = Some(language);
                            publish(&mut journal, &feed, &mut emitted)?;
                            return Ok(Reply::At(at));
                        }
                        Op::Bookmark(label) => {
                            journal.bookmark(now, label)?;
                            return Ok(Reply::At(at));
                        }
                        Op::Note(text) => {
                            journal.note(now, &text)?;
                            return Ok(Reply::At(at));
                        }
                        Op::Tags(tags) => {
                            let written = tags.iter().try_for_each(|tag| journal.add_tag(tag));
                            // The status shows what the journal holds, also
                            // after a write failed partway.
                            lock(&status).tags.clone_from(&journal.manifest().meta.tags);
                            written?;
                        }
                        Op::RemoveTags(tags) => {
                            let written = tags.iter().try_for_each(|tag| journal.remove_tag(tag));
                            lock(&status).tags.clone_from(&journal.manifest().meta.tags);
                            written?;
                        }
                        Op::MeetingAppSeen => journal.mark_meeting_app_seen()?,
                        Op::Participant(person_ref, name, confirm) => {
                            journal.set_participant(&person_ref, &name, confirm)?;
                            lock(&status).participants = journal
                                .manifest()
                                .participants
                                .iter()
                                .filter(|participant| !participant.removed)
                                .map(|participant| participant.person_ref.clone())
                                .collect();
                        }
                        Op::Cutoff => {
                            let cutoff = journal.rotate()?;
                            publish(&mut journal, &feed, &mut emitted)?;
                            let mut chunks = std::collections::BTreeMap::new();
                            for track in Track::ALL {
                                if let Some(seq) = cutoff.get(track) {
                                    chunks.insert(ipc_track(track), seq);
                                }
                            }
                            return Ok(Reply::Cutoff(ChunkCutoff {
                                chunks,
                                recording_s: at,
                            }));
                        }
                        Op::Stop => unreachable!("handled above"),
                    }
                    publish(&mut journal, &feed, &mut emitted)?;
                    Ok(Reply::Done)
                })();
                let _ = c.reply.send(result);
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Publishes the journal's recording state and time to the session status.
fn report(status: &Mutex<SessionStatus>, journal: &JournalWriter, now: u64) {
    let mut s = lock(status);
    s.elapsed_s = journal.recording_s(now);
    s.status = if journal.is_paused() {
        RecordingStatus::Paused
    } else {
        RecordingStatus::Recording
    };
}

fn ns(anchor: Instant) -> u64 {
    anchor.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}
fn ipc_track(track: Track) -> broscribe_ipc::Track {
    match track {
        Track::Mic => broscribe_ipc::Track::Mic,
        Track::System => broscribe_ipc::Track::System,
    }
}
fn dbfs(level: f32) -> f32 {
    if level > 0.0 {
        (20.0 * level.log10()).max(-120.0)
    } else {
        -120.0
    }
}

fn sink(
    track: Track,
    generation: u32,
    tx: &mpsc::SyncSender<Audio>,
    dropped: &Arc<[AtomicU64; 2]>,
) -> MicSink {
    let tx = tx.clone();
    let dropped = dropped.clone();
    Box::new(move |chunk| {
        let n = chunk.samples_mono_f32.len();
        if tx
            .try_send(Audio {
                track,
                generation,
                chunk,
                at: Instant::now(),
            })
            .is_err()
        {
            dropped[track.index()].fetch_add(n as u64, Ordering::Relaxed);
        }
    })
}

fn append(
    writer: &mut JournalWriter,
    anchor: Instant,
    audio: Audio,
    runs: &[TrackRun],
) -> Result<()> {
    let at = audio.chunk.host_time.unwrap_or_else(|| {
        audio
            .at
            .checked_sub(Duration::from_secs_f64(
                audio.chunk.samples_mono_f32.len() as f64
                    / f64::from(audio.chunk.sample_rate.max(1)),
            ))
            .unwrap_or(anchor)
    });
    let host = at
        .saturating_duration_since(anchor)
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64;
    let changed = writer.segment(audio.track).is_none_or(|s| {
        s.generation != audio.generation || s.sample_rate != audio.chunk.sample_rate
    });
    if changed {
        let device = runs
            .get(audio.track.index())
            .and_then(|run| run.device.as_ref())
            .filter(|(generation, _)| *generation == audio.generation)
            .map(|(_, device)| device);
        writer.begin_segment(
            audio.track,
            audio.generation,
            device.and_then(|d| d.name.clone()),
            audio.chunk.sample_rate,
            host,
        )?;
        if let Some(id) = device.and_then(|d| d.id.as_deref()) {
            writer.set_device(audio.track, audio.generation, None, Some(id))?;
        }
    }
    // A callback can arrive after resume even though its samples were captured
    // during the pause. Use capture time, not worker arrival time, to exclude it.
    let samples = &audio.chunk.samples_mono_f32;
    let rate = audio.chunk.sample_rate;
    let frame_at = |ns: u64| {
        ((u128::from(ns.saturating_sub(host)) * u128::from(rate)).div_ceil(1_000_000_000))
            .min(samples.len() as u128) as usize
    };
    let mut offset = 0;
    let start = writer
        .manifest()
        .pauses
        .partition_point(|pause| pause.to_ns.is_some_and(|end| end <= host));
    for index in start..writer.manifest().pauses.len() {
        let pause = writer.manifest().pauses[index];
        let before = frame_at(pause.from_ns);
        if before >= samples.len() {
            break;
        }
        if before > offset {
            writer.append(
                audio.track,
                host + journal::frames_to_ns(offset as u64, rate),
                &samples[offset..before],
            )?;
        }
        offset = pause.to_ns.map_or(samples.len(), frame_at).max(offset);
        if offset == samples.len() {
            break;
        }
    }
    if offset < samples.len() {
        writer.append(
            audio.track,
            host + journal::frames_to_ns(offset as u64, rate),
            &samples[offset..],
        )?;
    }
    Ok(())
}

/// Hands newly closed chunks to the preview, from the samples the writer
/// kept when it can, else read back from their files.
fn publish(
    writer: &mut JournalWriter,
    feed: &LiveFeed,
    emitted: &mut [Option<u64>; 2],
) -> Result<()> {
    for track in Track::ALL {
        let t = track.index();
        loop {
            let journal = writer.manifest().track(track);
            let next = journal
                .chunks
                .partition_point(|chunk| emitted[t].is_some_and(|seq| chunk.seq <= seq));
            let Some(chunk) = journal
                .chunks
                .get(next)
                .filter(|chunk| chunk.frames.is_some())
                .cloned()
            else {
                break;
            };
            let sample_rate = journal
                .segments
                .iter()
                .find(|s| s.index == chunk.segment)
                .map(|s| s.sample_rate)
                .ok_or_else(|| Error::Internal("chunk has no segment".into()))?;
            let start_s = writer.recording_s(chunk.host_start_ns);
            let samples = match writer.take_closed_audio(track, chunk.seq) {
                Some(samples) => samples,
                None => journal::read_chunk(writer.dir(), track, &chunk)?,
            };
            feed.push(LiveAudio {
                track,
                start_s,
                sample_rate,
                samples,
            });
            emitted[t] = Some(chunk.seq);
        }
    }
    Ok(())
}

/// One shared allocation controls shutdown and the startup notice fence.
#[derive(Default)]
struct PreviewControl {
    stop: AtomicBool,
    admitted: AtomicBool,
}

fn spawn_sender(
    feed: Arc<LiveFeed>,
    control: Arc<PreviewControl>,
    out: CaptureOutputs,
    priority: Arc<dyn ProcessPriority>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("capture-preview".into())
        .spawn(move || {
            let _ = priority.set_current_thread(PriorityClass::Utility);
            // One resampler per track, rebuilt only when its device rate changes.
            let mut resamplers: [Option<(u32, Resampler)>; 2] = [None, None];
            // Same drop-range coalescing as LiveFeed, at most one per track.
            // Failed starts discard these; successful starts disclose them.
            let mut pending_gaps: [Option<(f64, f64)>; 2] = [None; 2];
            loop {
                if control.admitted.load(Ordering::Acquire) {
                    for track in Track::ALL {
                        if let Some((from_s, to_s)) = pending_gaps[track.index()].take() {
                            out.events.notice(CaptureNotice::PreviewGap {
                                track: ipc_track(track),
                                from_s,
                                to_s,
                            });
                        }
                    }
                }
                match feed.pop_timeout(POLL) {
                    Some(LiveItem::Audio(audio)) => {
                        let slot = &mut resamplers[audio.track.index()];
                        if slot
                            .as_ref()
                            .is_none_or(|(rate, _)| *rate != audio.sample_rate)
                        {
                            *slot = Some((
                                audio.sample_rate,
                                Resampler::new(f64::from(audio.sample_rate), PREVIEW_RATE),
                            ));
                        }
                        let Some((_, resampler)) = slot.as_mut() else {
                            continue;
                        };
                        // Each chunk resampled whole, as the live clock assumes.
                        let mut samples = Vec::new();
                        resampler.process(&audio.samples, &mut samples);
                        out.live.push(LiveFrame {
                            track: ipc_track(audio.track),
                            start_sample: crate::anchors::live_start_sample(audio.start_s),
                            samples,
                        });
                    }
                    Some(LiveItem::Gap {
                        track,
                        from_s,
                        to_s,
                    }) => {
                        let gap = &mut pending_gaps[track.index()];
                        *gap = Some(match *gap {
                            Some((from, to)) => (from.min(from_s), to.max(to_s)),
                            None => (from_s, to_s),
                        });
                    }
                    None if control.stop.load(Ordering::Acquire) => break,
                    None => {}
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_ipc::{
        DaemonState, Event, IndexState, JobsState, LiveSink, PROTOCOL_VERSION, PreviewState,
        RecordingState, RequestId, Response, ServerMessage,
    };
    use broscribe_platform::AudioDevice;
    use broscribe_platform::fakes::{FakeCapture, FakeMicCapture, FakeSystemInfo};
    use broscribe_platform::priority::FakeProcessPriority;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, OnceLock};

    /// Disk checks after the start-up one take just over the check interval,
    /// so the writer checks the disk on every loop, before refreshing status.
    #[derive(Default)]
    struct SlowDisk(AtomicUsize);
    impl SystemInfo for SlowDisk {
        fn chip(&self) -> String {
            String::new()
        }
        fn memory_bytes(&self) -> u64 {
            0
        }
        fn free_disk_bytes(&self, path: &Path) -> broscribe_platform::Result<u64> {
            if self.0.fetch_add(1, Ordering::AcqRel) > 0 {
                thread::sleep(Duration::from_millis(1_050));
            }
            FakeSystemInfo::default().free_disk_bytes(path)
        }
        fn cpu_cores(&self) -> usize {
            1
        }
    }

    #[derive(Default)]
    struct Quiet(Mutex<Vec<CaptureNotice>>);
    impl LiveSink for Quiet {
        fn push(&self, _: LiveFrame) {}
    }
    impl CaptureEvents for Quiet {
        fn notice(&self, notice: CaptureNotice) {
            if !matches!(notice, CaptureNotice::Meters { .. }) {
                lock(&self.0).push(notice);
            }
        }
    }

    fn config(root: &Path, mic_device: Option<&str>) -> SessionConfig {
        SessionConfig {
            session_id: "synthetic-session".into(),
            journal_root: root.into(),
            library_root: root.canonicalize().unwrap(),
            started_at: chrono::Utc::now().fixed_offset(),
            title: None,
            app: None,
            language: None,
            setup: None,
            tags: Vec::new(),
            title_detected: false,
            echo_handling: None,
            in_person_setup: None,
            mic_device: mic_device.map(str::to_owned),
        }
    }

    fn start(
        root: &Path,
        mic: Arc<FakeMicCapture>,
        info: Arc<dyn SystemInfo>,
        mic_device: Option<&str>,
    ) -> Box<dyn SessionHandle> {
        let quiet = Arc::new(Quiet::default());
        Recorder::new(
            mic,
            Arc::new(FakeCapture::new(16_000, vec![0.0; 160])),
            info,
            Arc::new(FakeProcessPriority::default()),
        )
        .start(
            config(root, mic_device),
            CaptureOutputs {
                live: quiet.clone(),
                events: quiet,
            },
        )
        .unwrap()
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn pause_and_resume_report_status_before_replying() {
        let root = tempfile::tempdir().unwrap();
        let disk = Arc::new(SlowDisk::default());
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let mut session = start(root.path(), mic, disk.clone(), None);
        // Once the writer is in a slow disk check, every reply is followed by
        // another one before the loop would refresh the status.
        wait_until("a disk check", || disk.0.load(Ordering::Acquire) >= 2);
        session.pause().unwrap();
        let paused = session.status();
        assert_eq!(paused.status, RecordingStatus::Paused);
        assert!(paused.elapsed_s >= 1.0, "{}", paused.elapsed_s);
        session.resume().unwrap();
        assert_eq!(session.status().status, RecordingStatus::Recording);
        session.stop().unwrap();
    }

    #[test]
    fn record_tags_are_removed_case_insensitively() {
        let root = tempfile::tempdir().unwrap();
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let info = Arc::new(FakeSystemInfo::default());
        let mut session = start(root.path(), mic, info, None);
        session
            .add_tags(&["Alpha".to_owned(), "beta".to_owned()])
            .unwrap();
        assert_eq!(session.status().tags, ["Alpha", "beta"]);
        session.remove_tags(&[" ALPHA ".to_owned()]).unwrap();
        assert_eq!(
            session.status().tags,
            ["beta"],
            "snapshots carry current tags"
        );
        let stopped = session.stop().unwrap();
        let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
        assert_eq!(manifest.meta.tags, ["beta"]);
    }

    /// The journal names the microphone actually opened, and keeps the pin
    /// even when it was not connected.
    #[test]
    fn record_uses_the_pinned_mic_only_while_connected() {
        for (pinned, opened, name) in [
            (Some("fake-mic"), Some("fake-mic"), Some("Fake microphone")),
            (Some("unplugged-mic"), None, None),
            (None, None, None),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mic = Arc::new(FakeMicCapture::new(16_000));
            let info = Arc::new(FakeSystemInfo::default());
            let session = start(root.path(), mic.clone(), info, pinned);
            let stopped = session.stop().unwrap();
            assert_eq!(
                *mic.started.lock().unwrap(),
                [opened.map(str::to_owned)],
                "pinned {pinned:?}"
            );
            let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
            assert_eq!(manifest.meta.pinned_mic.as_deref(), pinned);
            let segment = &manifest.mic.segments[0];
            assert_eq!(segment.device_id.as_deref(), opened, "pinned {pinned:?}");
            assert_eq!(segment.device.as_deref(), name, "pinned {pinned:?}");
        }
    }

    #[test]
    fn pinned_input_is_the_pin_only_while_listed() {
        let mic = FakeMicCapture::new(16_000);
        let listed = pinned_input(&mic, Some("fake-mic")).unwrap();
        assert_eq!(listed.id, "fake-mic");
        assert!(pinned_input(&mic, Some("unplugged-mic")).is_none());
        assert!(pinned_input(&mic, None).is_none());
    }

    struct PartlyListedMic {
        inner: Arc<FakeMicCapture>,
        all_bad: bool,
    }
    impl MicCapture for PartlyListedMic {
        fn devices(&self) -> Result<Vec<AudioDevice>> {
            broscribe_platform::devices::collect_devices(
                "microphones",
                std::iter::once(Err(Error::Internal("unreadable synthetic device".into()))).chain(
                    self.inner
                        .devices()?
                        .into_iter()
                        .filter(|_| !self.all_bad)
                        .map(|device| Ok(Some(device))),
                ),
                |result| result,
            )
        }

        fn start(&self, device: Option<&str>, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
            self.inner.start(device, sink)
        }
    }

    #[test]
    fn an_unreadable_neighbor_does_not_make_recording_open_the_default_instead_of_the_pin() {
        for all_bad in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let inner = Arc::new(FakeMicCapture::new(16_000));
            let mic = Arc::new(PartlyListedMic {
                inner: inner.clone(),
                all_bad,
            });
            let opened = (!all_bad).then_some("fake-mic");
            assert_eq!(
                pinned_input(mic.as_ref(), Some("fake-mic"))
                    .as_ref()
                    .map(|device| device.id.as_str()),
                opened
            );
            let quiet = Arc::new(Quiet::default());
            let session = Recorder::new(
                mic,
                Arc::new(FakeCapture::new(16_000, vec![0.0; 160])),
                Arc::new(FakeSystemInfo::default()),
                Arc::new(FakeProcessPriority::default()),
            )
            .start(
                config(root.path(), Some("fake-mic")),
                CaptureOutputs {
                    live: quiet.clone(),
                    events: quiet,
                },
            )
            .unwrap();
            let stopped = session.stop().unwrap();
            assert_eq!(*inner.started.lock().unwrap(), [opened.map(str::to_owned)]);
            let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
            assert_eq!(manifest.meta.pinned_mic.as_deref(), Some("fake-mic"));
            assert_eq!(manifest.mic.segments[0].device_id.as_deref(), opened);
        }
    }

    /// A source whose current session can be failed and whose next starts
    /// can be refused, like a device lost mid-meeting.
    #[derive(Default)]
    struct Flaky {
        sink: Mutex<Option<MicSink>>,
        /// Counter of the current session.
        epoch: AtomicUsize,
        failed: AtomicBool,
        refuse: AtomicUsize,
        restart_held: Mutex<bool>,
        restart_ready: Condvar,
    }
    struct FlakySession(Arc<Flaky>, usize);
    struct RestartHold(Arc<Flaky>);
    impl Drop for RestartHold {
        fn drop(&mut self) {
            *lock(&self.0.restart_held) = false;
            self.0.restart_ready.notify_all();
        }
    }
    impl Flaky {
        fn hold_restarts(self: &Arc<Self>) -> RestartHold {
            *lock(&self.restart_held) = true;
            RestartHold(self.clone())
        }
        fn push(&self, samples: Vec<f32>) {
            if let Some(sink) = lock(&self.sink).as_mut() {
                let mut chunk = MicChunk::new(samples, 16_000);
                chunk.host_time = Some(Instant::now());
                sink(chunk);
            }
        }
        fn fail(&self) {
            self.failed.store(true, Ordering::SeqCst);
            lock(&self.sink).take();
        }
        fn open(self: &Arc<Self>, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
            if self
                .refuse
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(Error::NotFound("synthetic device is gone".into()));
            }
            let held = self
                .restart_ready
                .wait_while(lock(&self.restart_held), |held| *held)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            drop(held);
            let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
            self.failed.store(false, Ordering::SeqCst);
            *lock(&self.sink) = Some(sink);
            Ok(Box::new(FlakySession(self.clone(), epoch)))
        }
    }
    impl CaptureSession for FlakySession {
        fn sample_rate(&self) -> u32 {
            16_000
        }
        fn failure(&self) -> Option<String> {
            (self.0.epoch.load(Ordering::SeqCst) == self.1 && self.0.failed.load(Ordering::SeqCst))
                .then(|| "synthetic late audio".to_owned())
        }
        fn stop(self: Box<Self>) -> Result<()> {
            if self.0.epoch.load(Ordering::SeqCst) == self.1 {
                lock(&self.0.sink).take();
            }
            Ok(())
        }
    }
    struct FlakySource(Arc<Flaky>);
    impl MicCapture for FlakySource {
        fn devices(&self) -> Result<Vec<AudioDevice>> {
            Ok(Vec::new())
        }
        fn start(&self, _: Option<&str>, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
            self.0.open(sink)
        }
    }
    impl SystemAudioCapture for FlakySource {
        fn set_excluded_pids(&self, _: &[u32]) -> Result<()> {
            Ok(())
        }
        fn start(&self, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
            self.0.open(sink)
        }
    }

    /// The system track failing and refusing its first restart never stops
    /// the microphone: it keeps journaling while the system track waits and
    /// comes back as a new segment after the gap.
    #[test]
    fn a_failed_track_restarts_while_the_healthy_one_records() {
        let root = tempfile::tempdir().unwrap();
        let mic = Arc::new(Flaky::default());
        let system = Arc::new(Flaky::default());
        let events = Arc::new(Quiet::default());
        let mut session = Recorder::new(
            Arc::new(FlakySource(mic.clone())),
            Arc::new(FlakySource(system.clone())),
            Arc::new(FakeSystemInfo::default()),
            Arc::new(FakeProcessPriority::default()),
        )
        .start_session(
            config(root.path(), None),
            CaptureOutputs {
                live: events.clone(),
                events: events.clone(),
            },
        )
        .unwrap();
        let mut mic_frames = 0;
        let mut push_mic = || {
            mic.push(vec![0.25; 160]);
            mic_frames += 160;
        };
        push_mic();
        system.push(vec![0.5; 160]);
        // Keep the successful reconnect pending until its failed attempt
        // and the healthy track have been observed through public health.
        let restart = system.hold_restarts();
        system.refuse.store(1, Ordering::SeqCst);
        system.fail();
        let mut waiting = None;
        wait_until("the refused restart", || {
            {
                let notices = lock(&events.0);
                assert!(
                    notices
                        .iter()
                        .all(|n| !matches!(n, CaptureNotice::Stopped { .. })),
                    "capture stopped before reconnect: {notices:?}"
                );
            }
            let health = session.health();
            let system = &health.tracks[1];
            let refused = matches!(system.state, TrackState::Waiting | TrackState::Restarting)
                && system.failures == 2;
            if refused {
                waiting = Some(health);
            }
            refused
        });
        let health = waiting.unwrap();
        assert_eq!(health.tracks[0].state, TrackState::Capturing);
        assert_eq!(
            health.tracks[1].retry_in_s.is_some(),
            health.tracks[1].state == TrackState::Waiting,
            "a scheduled retry has a deadline; an in-flight restart does not"
        );
        assert_eq!(
            health.tracks[1].last_error.as_deref(),
            Some("not found: synthetic device is gone")
        );
        // Feed a bounded block while the peer is down, and require its
        // durable cutoff before permitting reconnect. Polling must not
        // flood the callback's deliberately bounded audio queue.
        push_mic();
        let cutoff = session.snapshot_cutoff().unwrap();
        let mic_cutoff = cutoff.chunks[&broscribe_ipc::Track::Mic];
        drop(restart);
        wait_until("the system restart", || {
            {
                let notices = lock(&events.0);
                assert!(
                    notices
                        .iter()
                        .all(|n| !matches!(n, CaptureNotice::Stopped { .. })),
                    "capture stopped during reconnect: {notices:?}"
                );
            }
            session.health().tracks[1].state == TrackState::Capturing
        });
        system.push(vec![0.75; 160]);
        push_mic();
        let stopped = Box::new(session).stop().unwrap();
        assert!(
            lock(&events.0)
                .iter()
                .all(|n| !matches!(n, CaptureNotice::Stopped { .. })),
            "a single track failure never stops capture"
        );
        let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
        assert_eq!(manifest.stop_reason.as_deref(), Some("user"));
        let frames =
            |t: &journal::TrackJournal| t.chunks.iter().filter_map(|c| c.frames).sum::<u64>();
        assert_eq!(frames(&manifest.mic), mic_frames);
        let before_reconnect: u64 = manifest
            .mic
            .chunks
            .iter()
            .filter(|c| c.seq <= mic_cutoff)
            .filter_map(|c| c.frames)
            .sum();
        assert_eq!(
            before_reconnect, 320,
            "healthy audio was durable before reconnect"
        );
        assert_eq!(manifest.mic.dropped_frames, 0);
        assert_eq!(manifest.mic.segments.len(), 1);
        assert_eq!(frames(&manifest.system), 320);
        let generations: Vec<u32> = manifest
            .system
            .segments
            .iter()
            .map(|s| s.generation)
            .collect();
        assert_eq!(generations, [0, 2], "the refused attempt journaled nothing");
    }

    const GIB: u64 = 1 << 30;

    /// Free space the test sets. While held, the journal thread stays in its
    /// disk check, so nothing drains the audio queue.
    struct Disk {
        free: AtomicU64,
        checks: AtomicUsize,
        held: Mutex<bool>,
        released: Condvar,
    }
    impl Disk {
        fn new(free: u64) -> Arc<Self> {
            Arc::new(Self {
                free: AtomicU64::new(free),
                checks: AtomicUsize::new(0),
                held: Mutex::new(false),
                released: Condvar::new(),
            })
        }
        fn set(&self, free: u64) {
            self.free.store(free, Ordering::SeqCst);
        }
        fn checks(&self) -> usize {
            self.checks.load(Ordering::SeqCst)
        }
        /// Returns once a check that began after this call has finished
        /// with everything the writer does after it (one check per loop).
        fn checked(&self) {
            let begun = self.checks();
            wait_until("a full disk check", || self.checks() >= begun + 2);
        }
        /// Returns with the journal thread inside a held check.
        fn hold(&self) {
            *lock(&self.held) = true;
            let begun = self.checks();
            wait_until("the writer inside a held disk check", || {
                self.checks() > begun
            });
        }
        fn release(&self) {
            *lock(&self.held) = false;
            self.released.notify_all();
        }
    }
    impl SystemInfo for Disk {
        fn chip(&self) -> String {
            String::new()
        }
        fn memory_bytes(&self) -> u64 {
            0
        }
        fn free_disk_bytes(&self, _: &Path) -> broscribe_platform::Result<u64> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            let mut held = lock(&self.held);
            while *held {
                held = self.released.wait(held).unwrap_or_else(|e| e.into_inner());
            }
            drop(held);
            Ok(self.free.load(Ordering::SeqCst))
        }
        fn cpu_cores(&self) -> usize {
            1
        }
    }

    /// Reads the health back on every `HealthChanged`, as the daemon does
    /// (which would deadlock if the notice came under the health lock).
    #[derive(Default)]
    struct Watched {
        notices: Mutex<Vec<CaptureNotice>>,
        health: OnceLock<Arc<Mutex<Health>>>,
        seen: Mutex<Vec<CaptureHealth>>,
    }
    impl Watched {
        fn seen(&self) -> Vec<CaptureHealth> {
            lock(&self.seen).clone()
        }
        fn last_seen(&self, check: impl Fn(&CaptureHealth) -> bool) -> bool {
            lock(&self.seen).last().is_some_and(check)
        }
    }
    impl LiveSink for Watched {
        fn push(&self, _: LiveFrame) {}
    }
    impl CaptureEvents for Watched {
        fn notice(&self, notice: CaptureNotice) {
            match notice {
                CaptureNotice::Meters { .. } => {}
                CaptureNotice::HealthChanged => {
                    if let Some(health) = self.health.get() {
                        let health = lock(health).snapshot();
                        lock(&self.seen).push(health);
                    }
                }
                notice => lock(&self.notices).push(notice),
            }
        }
    }

    fn watch(
        root: &Path,
        mic: &Arc<Flaky>,
        system: &Arc<Flaky>,
        disk: &Arc<Disk>,
    ) -> (RecorderSession, Arc<Watched>) {
        let watched = Arc::new(Watched::default());
        let session = Recorder::new(
            Arc::new(FlakySource(mic.clone())),
            Arc::new(FlakySource(system.clone())),
            disk.clone(),
            Arc::new(FakeProcessPriority::default()),
        )
        .start_session(
            config(root, None),
            CaptureOutputs {
                live: watched.clone(),
                events: watched.clone(),
            },
        )
        .unwrap();
        assert!(watched.health.set(session.health.clone()).is_ok());
        (session, watched)
    }

    fn disk_is(level: DiskLevel, free: u64) -> impl Fn(&CaptureHealth) -> bool {
        move |health| {
            health
                .disk
                .is_some_and(|disk| disk.level == level && disk.free_bytes == free)
        }
    }

    /// A healthy recording that receives no command still announces new
    /// free space, lost audio and the disk going back to OK, each read back
    /// by the receiver; unchanged health announces nothing.
    #[test]
    fn changed_health_is_announced_without_commands() {
        let root = tempfile::tempdir().unwrap();
        let (mic, system) = (Arc::new(Flaky::default()), Arc::new(Flaky::default()));
        let disk = Disk::new(100 * GIB);
        let (session, watched) = watch(root.path(), &mic, &system, &disk);
        wait_until("the first disk measurement", || {
            watched.last_seen(disk_is(DiskLevel::Ok, 100 * GIB))
        });
        let announced = watched.seen().len();
        disk.checked();
        assert_eq!(watched.seen().len(), announced, "nothing changed");

        // The full queue loses exactly the chunks past its capacity.
        disk.hold();
        for _ in 0..AUDIO_QUEUE + 10 {
            mic.push(vec![0.25; 160]);
        }
        disk.set(900 << 20);
        disk.release();
        wait_until("the losses and the low disk read back", || {
            watched.last_seen(|health| {
                health.tracks[0].dropped_frames == 1_600
                    && health.tracks[1].dropped_frames == 0
                    && disk_is(DiskLevel::Warning, 900 << 20)(health)
            })
        });
        disk.set(800 << 20);
        wait_until("new free space at the same level", || {
            watched.last_seen(disk_is(DiskLevel::Warning, 800 << 20))
        });
        disk.set(100 * GIB);
        wait_until("the disk back to OK", || {
            watched.last_seen(disk_is(DiskLevel::Ok, 100 * GIB))
        });
        let announced = watched.seen().len();
        disk.checked();
        assert_eq!(watched.seen().len(), announced, "nothing changed");

        let notices = lock(&watched.notices).clone();
        let low = notices
            .iter()
            .filter(|n| matches!(n, CaptureNotice::Warning(CaptureWarning::DiskLow { .. })))
            .count();
        assert_eq!(low, 1, "one warning per low stretch: {notices:?}");
        assert!(
            watched
                .seen()
                .iter()
                .all(|h| h.tracks.iter().all(|t| t.state == TrackState::Capturing)),
            "both tracks kept capturing"
        );
        let stopped = Box::new(session).stop().unwrap();
        assert!(
            lock(&watched.notices)
                .iter()
                .all(|n| !matches!(n, CaptureNotice::Stopped { .. }))
        );
        let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
        assert_eq!(manifest.mic.dropped_frames, 1_600);
    }

    /// What JSON carries exactly: its float parsing may differ in the last
    /// bit, so seconds are compared to the microsecond.
    fn to_the_microsecond(mut health: CaptureHealth) -> CaptureHealth {
        let us = |s: f64| (s * 1e6).round();
        for track in &mut health.tracks {
            track.retry_in_s = track.retry_in_s.map(us);
        }
        if let Some(disk) = &mut health.disk {
            disk.headroom_s = us(disk.headroom_s);
        }
        health
    }

    /// Both devices fail before their first audio and refuse every restart.
    /// The disk estimate keeps the rates they opened with, so the health
    /// stays finite through the daemon's status reply and state event, and
    /// each retry after its backoff is announced.
    #[test]
    fn tracks_down_before_audio_keep_health_on_the_wire() {
        let root = tempfile::tempdir().unwrap();
        let (mic, system) = (Arc::new(Flaky::default()), Arc::new(Flaky::default()));
        let disk = Disk::new(100 * GIB);
        let (session, watched) = watch(root.path(), &mic, &system, &disk);
        for flaky in [&mic, &system] {
            flaky.refuse.store(usize::MAX, Ordering::SeqCst);
            flaky.fail();
        }
        wait_until("both tracks refusing restarts", || {
            session
                .health()
                .tracks
                .iter()
                .all(|t| t.state != TrackState::Capturing && t.failures >= 2)
        });
        wait_until("a restart after its backoff", || {
            watched.seen().iter().any(|health| {
                health
                    .tracks
                    .iter()
                    .any(|t| t.state == TrackState::Restarting && t.failures >= 2)
            })
        });
        disk.set(50 * GIB);
        wait_until("a disk check with both tracks down", || {
            session
                .health()
                .disk
                .is_some_and(|disk| disk.free_bytes == 50 * GIB)
        });
        let health = session.health();
        assert!(
            health
                .tracks
                .iter()
                .all(|t| t.state != TrackState::Capturing && t.failures >= 2),
            "{health:?}"
        );
        let disk_health = health.disk.unwrap();
        assert_eq!(
            disk_health.headroom_s,
            (50 * GIB) as f64 / f64::from(2 * 16_000 * 4),
            "the opened devices' rates"
        );

        let state = DaemonState {
            pid: 1,
            version: "synthetic".into(),
            protocol: PROTOCOL_VERSION,
            library: "synthetic".into(),
            detection_enabled: false,
            access: Default::default(),
            recording: Some(RecordingState {
                session_id: "synthetic-session".into(),
                status: session.status().status,
                started_at: chrono::Utc::now().fixed_offset(),
                elapsed_s: 0.0,
                language: None,
                app: None,
                title: None,
                setup: None,
                participants: Vec::new(),
                observed_roster: Vec::new(),
                tags: Vec::new(),
                auto_stop: None,
                health: health.clone(),
                reader: None,
                reader_health: None,
            }),
            jobs: JobsState::default(),
            preview: PreviewState::default(),
            pending_notifications: Vec::new(),
            stopped: Vec::new(),
            saved: Vec::new(),
            journal_issues: Vec::new(),
            index: IndexState::default(),
            install: None,
            build: None,
        };
        let received =
            |state: DaemonState| to_the_microsecond(state.recording.expect("recording").health);
        let expected = to_the_microsecond(health);

        let reply = Response::ok(RequestId::random(), serde_json::to_value(&state).unwrap())
            .to_line()
            .unwrap();
        let ServerMessage::Response(Response {
            outcome: Ok(result),
            ..
        }) = ServerMessage::parse(&reply).unwrap()
        else {
            panic!("not a successful reply: {reply}");
        };
        let status: DaemonState = serde_json::from_value(result).unwrap();
        assert_eq!(received(status), expected);

        let line = Event::State(Box::new(state)).to_line().unwrap();
        let ServerMessage::Event(Event::State(event)) = ServerMessage::parse(&line).unwrap() else {
            panic!("not a state event: {line}");
        };
        assert_eq!(received(*event), expected);

        let stopped = Box::new(session).stop().unwrap();
        assert!(
            lock(&watched.notices)
                .iter()
                .all(|n| !matches!(n, CaptureNotice::Stopped { .. })),
            "both tracks down within the grace never stop capture on its own"
        );
        let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
        assert_eq!(manifest.stop_reason.as_deref(), Some("user"));
    }

    /// Disk space runs out while the writer checks it; a pause sent during
    /// that check is refused, never acknowledged as applied.
    #[test]
    fn a_command_racing_a_protective_stop_is_refused() {
        #[derive(Default)]
        struct Filling {
            armed: AtomicBool,
            checking: AtomicBool,
        }
        impl SystemInfo for Filling {
            fn chip(&self) -> String {
                String::new()
            }
            fn memory_bytes(&self) -> u64 {
                0
            }
            fn free_disk_bytes(&self, _: &Path) -> broscribe_platform::Result<u64> {
                if !self.armed.load(Ordering::Acquire) {
                    return Ok(100 << 30);
                }
                self.checking.store(true, Ordering::Release);
                thread::sleep(Duration::from_millis(300));
                Ok(1)
            }
            fn cpu_cores(&self) -> usize {
                1
            }
        }
        let root = tempfile::tempdir().unwrap();
        let disk = Arc::new(Filling::default());
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let mut session = start(root.path(), mic, disk.clone(), None);
        disk.armed.store(true, Ordering::Release);
        wait_until("the failing disk check", || {
            disk.checking.load(Ordering::Acquire)
        });
        let error = session.pause().unwrap_err();
        assert_eq!(error.code(), STALE_SESSION);
        assert_eq!(session.bookmark(None).unwrap_err().code(), STALE_SESSION);
        let stopped = session.stop().unwrap();
        let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
        assert_eq!(manifest.stop_reason.as_deref(), Some("disk_low"));
        assert!(
            manifest.pauses.is_empty(),
            "the refused pause was not applied"
        );
        assert!(manifest.bookmarks.is_empty());
    }

    /// The disk runs out while a track's restart is stuck in device
    /// initialisation. A request made under the owner's session mutex, as
    /// the daemon makes it, is refused before the restart returns; later
    /// requests and the status do not wait for it either. Once the restart
    /// returns, the stop closes the journal with what was recorded and
    /// stops the capture that opened late.
    #[test]
    fn a_protective_stop_refuses_requests_before_a_blocked_restart_returns() {
        let root = tempfile::tempdir().unwrap();
        let (mic, system) = (Arc::new(Flaky::default()), Arc::new(Flaky::default()));
        let disk = Disk::new(100 * GIB);
        let events = Arc::new(Quiet::default());
        let session = Recorder::new(
            Arc::new(FlakySource(mic.clone())),
            Arc::new(FlakySource(system.clone())),
            disk.clone(),
            Arc::new(FakeProcessPriority::default()),
        )
        .start(
            config(root.path(), None),
            CaptureOutputs {
                live: events.clone(),
                events: events.clone(),
            },
        )
        .unwrap();
        let session = Arc::new(Mutex::new(session));
        mic.push(vec![0.25; 160]);
        let restart = system.hold_restarts();
        system.fail();
        wait_until("the held system restart", || {
            lock(&session).health().tracks[1].state == TrackState::Restarting
        });
        disk.hold();
        disk.set(1);
        let (reply_tx, reply) = mpsc::channel();
        let pausing = thread::spawn({
            let session = session.clone();
            move || reply_tx.send(lock(&session).pause()).unwrap()
        });
        disk.release();
        let mut paused = None;
        wait_until("the refused pause", || {
            paused = reply.try_recv().ok();
            paused.is_some()
        });
        assert_eq!(paused.unwrap().unwrap_err().code(), STALE_SESSION);
        pausing.join().unwrap();
        {
            let mut held = lock(&session);
            assert_eq!(held.bookmark(None).unwrap_err().code(), STALE_SESSION);
            assert_eq!(held.status().status, RecordingStatus::Recording);
        }
        assert!(
            lock(&events.0)
                .iter()
                .all(|n| !matches!(n, CaptureNotice::Stopped { .. })),
            "the journal closes only after the restart returns"
        );
        drop(restart);
        let session = Arc::into_inner(session)
            .unwrap()
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stopped = session.stop().unwrap();
        let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
        assert_eq!(manifest.stop_reason.as_deref(), Some("disk_low"));
        assert!(manifest.pauses.is_empty(), "the refused pause");
        assert!(manifest.bookmarks.is_empty(), "the refused bookmark");
        let frames: u64 = manifest.mic.chunks.iter().filter_map(|c| c.frames).sum();
        assert_eq!(frames, 160, "recorded audio is in the closed journal");
        assert!(
            lock(&events.0)
                .iter()
                .any(|n| matches!(n, CaptureNotice::Stopped { reason } if reason == "disk_low"))
        );
        assert!(
            lock(&system.sink).is_none() && lock(&mic.sink).is_none(),
            "no capture outlives the session, including the restart that opened late"
        );
    }
}
