//! The capture-engine boundary (plan M4 §3, §12.5–§12.6, §14.1–§14.2).
//!
//! `broscribed` drives capture through [`CaptureEngine`]; the macOS engine
//! (process tap + microphone + chunk journal) implements it in its own
//! crate, and the daemon's tests use a fake. The engine never depends on ML
//! or UI code, and nothing here may stall capture:
//! - [`LiveSink::push`] and [`CaptureEvents::notice`] must not block: the
//!   daemon's implementations drop the oldest data when full.
//! - Live frames are pushed only after their audio is journaled (the journal
//!   writer is the only durable path), never from the capture callbacks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use broscribe_core::Result;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::live::{LiveFrame, Track};
use crate::protocol::RecordingStatus;

/// What a new session starts with.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfig {
    /// Chosen by the daemon; also the journal folder name.
    pub session_id: String,
    /// `<data_dir>/journal`: the session journals into
    /// `<journal_root>/<session_id>/`.
    pub journal_root: PathBuf,
    /// Canonical path of the library that owns this recording.
    pub library_root: PathBuf,
    pub started_at: DateTime<FixedOffset>,
    pub title: Option<String>,
    /// Meeting-app detection rule ID, or bundle ID for a manual session.
    pub app: Option<String>,
    /// First language span (BCP 47).
    pub language: Option<String>,
    /// Setup override; the engine detects it when `None`.
    pub setup: Option<String>,
    pub tags: Vec<String>,
    /// `title` is a detected window title rather than one the user entered.
    pub title_detected: bool,
    /// Echo handling chosen by the audio check for this device pair, as the
    /// snake_case name of `broscribe_dsp::EchoHandling`; the setup default when absent.
    pub echo_handling: Option<String>,
    /// The rules' setup if this start turns out not to be a remote meeting.
    /// Set only for an auto-detected manual start the daemon can observe; the
    /// final job uses it when no meeting app held the mic and the system
    /// track stayed silent (design §5).
    pub in_person_setup: Option<String>,
    /// Pinned microphone UID: used while that device is connected, the
    /// default input otherwise.
    pub mic_device: Option<String>,
}

/// Receives 16 kHz PCM per track for the live preview. Must never block.
pub trait LiveSink: Send + Sync {
    fn push(&self, frame: LiveFrame);
}

/// Something the engine reports while capturing.
#[derive(Clone, Debug, PartialEq)]
pub enum CaptureNotice {
    /// Peak levels since the last notice, dBFS.
    Meters { me_dbfs: f32, others_dbfs: f32 },
    /// A problem capture survived; the session goes on.
    Warning(CaptureWarning),
    /// Preview audio dropped after journaling; never a recording gap.
    PreviewGap {
        track: Track,
        from_s: f64,
        to_s: f64,
    },
    /// [`SessionHandle::health`] changed: sent at once for a track's state
    /// change or a new disk level, otherwise at most once a second for new
    /// losses or free space, and never for unchanged health. Carries no
    /// copy: the receiver reads the current health. Diagnostics only, never
    /// call evidence.
    HealthChanged,
    /// Capture ended on its own (disk guard, device gone): the daemon stops
    /// the session the usual way, so the journal is closed and queued.
    Stopped { reason: String },
}

/// A problem a recording's capture survived. Diagnostics only: never call
/// evidence and never a reason to end the recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureWarning {
    /// A track's device failed; the other track keeps recording while this
    /// one restarts after `retry_in_s`.
    TrackDown {
        track: Track,
        error: String,
        retry_in_s: f64,
    },
    /// The track captures again; its gap is kept as silence.
    TrackResumed { track: Track, gap_s: f64 },
    /// Free space is low: capture stops and saves once it falls below
    /// `stop_below_bytes` or `stop_below_s` of recording.
    DiskLow {
        free_bytes: u64,
        headroom_s: f64,
        stop_below_bytes: u64,
        stop_below_s: f64,
    },
    /// The live preview could not be fed for this session; the recording
    /// is unaffected.
    PreviewUnavailable,
}

/// What a track's capture is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackState {
    /// The device delivers audio to the journal.
    Capturing,
    /// The device failed; a restart is being attempted now.
    Restarting,
    /// The device failed; the next restart waits for its backoff.
    Waiting,
}

/// One track's capture health.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackHealth {
    pub track: Track,
    pub state: TrackState,
    /// Frames lost before reaching the journal (full capture queue) over
    /// the whole session; the journal records the same count.
    pub dropped_frames: u64,
    /// Device failures and failed restarts since the track last captured
    /// for a while without failing.
    pub failures: u32,
    /// The latest failure, as the platform or the restart reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// While [`TrackState::Waiting`]: seconds until the next restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_in_s: Option<f64>,
}

impl TrackHealth {
    /// A track capturing without any failure or loss so far.
    pub fn capturing(track: Track) -> Self {
        Self {
            track,
            state: TrackState::Capturing,
            dropped_frames: 0,
            failures: 0,
            last_error: None,
            retry_in_s: None,
        }
    }
}

/// How close the journal volume is to the protective stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskLevel {
    Ok,
    /// Low: warned before the protective stop.
    Warning,
    /// Below the protective limit: capture stops and saves.
    Low,
}

/// The journal volume as last measured.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskHealth {
    pub free_bytes: u64,
    /// Recording time the free space holds at the current journal rate.
    /// Always finite: a track down before or after its first audio counts at
    /// its last opened device's rate.
    pub headroom_s: f64,
    pub level: DiskLevel,
}

/// A recording's capture health (`RecordingState.health`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureHealth {
    /// Mic first, then system.
    pub tracks: Vec<TrackHealth>,
    /// `None` until the first check after the start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskHealth>,
}

impl CaptureHealth {
    /// Both tracks capturing, disk not measured yet.
    pub fn starting() -> Self {
        Self {
            tracks: vec![
                TrackHealth::capturing(Track::Mic),
                TrackHealth::capturing(Track::System),
            ],
            disk: None,
        }
    }
}

/// Receives [`CaptureNotice`]s. Must never block.
pub trait CaptureEvents: Send + Sync {
    fn notice(&self, notice: CaptureNotice);
}

/// Where a session sends live audio and notices.
#[derive(Clone)]
pub struct CaptureOutputs {
    pub live: Arc<dyn LiveSink>,
    pub events: Arc<dyn CaptureEvents>,
}

/// What [`SessionHandle::status`] reports.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionStatus {
    pub status: RecordingStatus,
    /// Recording time so far, pauses excluded.
    pub elapsed_s: f64,
    /// Current language span.
    pub language: Option<String>,
    /// Detected or chosen setup.
    pub setup: Option<String>,
    /// User-confirmed library person IDs currently in the session roster.
    pub participants: Vec<String>,
    /// The session's current tags, as journaled.
    pub tags: Vec<String>,
}

/// The durable chunk boundary of a "process now" request (plan §14.2): the
/// last fsynced chunk per track. The provisional job reads only chunks up to
/// these indices and never deletes journal data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkCutoff {
    /// Last durable chunk index per track (inclusive).
    pub chunks: BTreeMap<Track, u64>,
    /// Recording time the cutoff corresponds to.
    pub recording_s: f64,
}

/// A closed session whose journal is ready for the final job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoppedSession {
    pub session_id: String,
    pub journal_dir: PathBuf,
}

/// A journal [`CaptureEngine::recover_orphans`] could not bring to the
/// stopped state. It stays where it is, untouched.
#[derive(Clone, Debug, PartialEq)]
pub struct OrphanFailure {
    /// The journal folder's name.
    pub session_id: String,
    pub journal_dir: PathBuf,
    /// The stable error code.
    pub code: String,
    /// The manifest itself is unreadable: retrying cannot help. Other
    /// failures are I/O on that one journal and may pass on a retry.
    pub corrupt: bool,
}

/// What one recovery sweep found: one journal failing never hides others.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OrphanRecovery {
    /// Journals now stopped and ready for their final jobs, oldest first.
    pub recovered: Vec<StoppedSession>,
    pub failed: Vec<OrphanFailure>,
}

/// A running capture session. Every call is short: no method waits on the
/// live worker, the IPC subscribers or the library lock.
pub trait SessionHandle: Send {
    fn session_id(&self) -> &str;
    fn status(&self) -> SessionStatus;
    /// Per-track state, dropped frames and disk headroom, as last measured;
    /// [`CaptureNotice::HealthChanged`] tells when it changed.
    fn health(&self) -> CaptureHealth;
    fn pause(&mut self) -> Result<()>;
    fn resume(&mut self) -> Result<()>;
    /// Starts a new language span now; returns its recording time in seconds.
    fn set_language(&mut self, language: &str) -> Result<f64>;
    /// Adds a bookmark now; returns its recording time in seconds.
    fn bookmark(&mut self, label: Option<&str>) -> Result<f64>;
    /// Adds a note now; returns its recording time in seconds.
    fn note(&mut self, text: &str) -> Result<f64>;
    fn add_tags(&mut self, tags: &[String]) -> Result<()>;
    /// Removes tags (compared trimmed, case-insensitively).
    fn remove_tags(&mut self, tags: &[String]) -> Result<()>;
    /// A meeting app held the mic during the session (remote evidence for
    /// the final job's setup). Idempotent.
    fn mark_meeting_app_seen(&mut self) -> Result<()>;
    /// User-confirmed session roster prior, keyed by a stable library person ID.
    fn participant(&mut self, person_id: &str, name: &str, confirm: bool) -> Result<()>;
    /// The durable cutoff for "process now"; capture continues.
    fn snapshot_cutoff(&mut self) -> Result<ChunkCutoff>;
    /// Stops capture, flushes and closes the journal.
    fn stop(self: Box<Self>) -> Result<StoppedSession>;
}

/// Starts capture sessions and recovers journals of interrupted ones.
pub trait CaptureEngine: Send + Sync {
    fn start(&self, cfg: SessionConfig, out: CaptureOutputs) -> Result<Box<dyn SessionHandle>>;

    /// Journals under `journal_root` left open by a crash (the daemon was
    /// killed mid-session), made consistent (a truncated last chunk stays
    /// usable) and returned so the daemon queues their final jobs, with
    /// every journal that could not be recovered. Only journals bound to
    /// the canonical `library_root` are recovered. Journals already queued
    /// may be returned again: queueing is idempotent. Only a failure to
    /// list `journal_root` itself is an error.
    fn recover_orphans(&self, journal_root: &Path, library_root: &Path) -> Result<OrphanRecovery>;
}
