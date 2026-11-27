//! `diagnostics`: a counts-only technical report of this installation, its
//! library, the local recording journals, BroScribe Agent and what this
//! platform can do (PL39, SH25). MCP `get_diagnostics` and the app's About
//! "Copy Diagnostics" (which runs this command) use this same collector.
//!
//! Read only and bounded to metadata: `meeting.json` files, queue job
//! records, journal `session.json` manifests, the index's pending-repair
//! list, model install markers, the About facts and a running agent's state
//! and capture grants. Nothing is created (no library, data folder, queue,
//! index or journal folder); no model is loaded, downloaded or run; no
//! microphone, player or capture is opened; no permission is requested and
//! nothing goes to the network.
//!
//! The report is an explicit whitelist of typed fields: versions, counts
//! and technical states. It never carries names, titles, e-mails, aliases,
//! transcript or note text, tags, meeting times, identifiers of meetings,
//! people, sessions, jobs or accounts, paths, model IDs, error messages,
//! endpoints or provider output: a part that cannot be read reports its
//! stable error code, and a file that cannot be read is counted by code,
//! never skipped. A measure the sources do not keep is reported as not
//! recorded, and one this report cannot observe as unknown, never as 0 or
//! as passing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use broscribe_capture::journal::{self, SessionManifest, SessionState, TrackJournal};
use broscribe_core::config::{Config, Profile};
use broscribe_core::index::Index;
use broscribe_core::lifecycle;
use broscribe_core::queue::{JobState, MAX_ATTEMPTS, Queue};
use broscribe_core::schema::{self, Handling, Meeting, ProcessingStatus, Source, TrackRole};
use broscribe_core::{Error, Library, Result};
use broscribe_dsp::{EchoHandling, EchoReport};
use broscribe_ipc::{
    Call, DaemonState, DiskLevel, PROTOCOL_VERSION, PreviewState, ReaderStatus, RecordingStatus,
    StoppedRecordingState, TrackState, WorkerProblem,
};
use broscribe_models::{Health, ModelStore, Registry, Role};
use broscribe_pipeline::worker::WORKER_KINDS;
use broscribe_platform::about::{BuildInfoState, DiagnosticPolicy, Packaging, SealState};
use broscribe_platform::ffmpeg_encoder::FfmpegEncoder;
use broscribe_platform::ffmpeg_player::FfplayPlayer;
use broscribe_platform_native::NativePlatform;
use broscribe_readers::{IncompleteReason, ManualAccessibility, ReaderHealth};
use serde::{Deserialize as _, Serialize};

use crate::Ctx;

/// Version of the report's shape.
pub(crate) const SCHEMA: u32 = 1;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Error codes and how many files or parts failed with each.
type CodeCounts = BTreeMap<&'static str, usize>;

/// The whole report: the same value for CLI `--json`, MCP and the app.
#[derive(Serialize)]
pub(crate) struct DiagnosticsReport {
    pub schema: u32,
    pub software: Software,
    pub config: ConfigState,
    pub library: Part<LibraryCounts>,
    pub journals: Part<JournalCounts>,
    pub models: Part<ProfileModels>,
    pub agent: AgentCounts,
    pub capabilities: Vec<Capability>,
}

/// A part of the report and whether it could be read.
#[derive(Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub(crate) enum Part<T> {
    Available(T),
    /// Not there: nothing was ever written (and this read created nothing).
    Absent,
    /// Where it is cannot be known.
    Unknown {
        reason: UnknownReason,
    },
    /// There, but unreadable: its stable error code.
    Unreadable {
        code: &'static str,
    },
}

impl<T> Part<T> {
    fn of(result: Result<T>) -> Self {
        match result {
            Ok(value) => Self::Available(value),
            Err(e) => Self::Unreadable { code: e.code() },
        }
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnknownReason {
    /// `config.toml` names the library and the profile, and it is unreadable.
    ConfigUnreadable,
}

/// A measure saved meetings do not keep.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Metric {
    NotRecorded,
}

// ---- software ----

#[derive(Serialize)]
pub(crate) struct Software {
    pub version: &'static str,
    pub protocol: u32,
    pub os: &'static str,
    pub arch: &'static str,
    /// Running translated for another CPU; `None` when not observable.
    pub translated: Option<bool>,
    pub packaging: Option<Packaging>,
    pub build: Build,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_seal_status: Option<i64>,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Build {
    Present {
        build: u64,
        target: String,
    },
    Missing,
    Invalid,
    /// The About reader failed: its error code.
    Unknown {
        code: &'static str,
    },
}

fn software() -> Software {
    let mut software = Software {
        version: VERSION,
        protocol: PROTOCOL_VERSION,
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        translated: None,
        packaging: None,
        build: Build::Missing,
        signature_seal_status: None,
    };
    match NativePlatform::about_reader(VERSION).read() {
        Ok(facts) => {
            software.translated = facts.architecture.translated;
            software.packaging = Some(facts.packaging);
            software.signature_seal_status = match facts.signature.seal {
                SealState::Broken { status } => Some(status),
                SealState::Intact | SealState::NotChecked => None,
            };
            software.build = match facts.build {
                BuildInfoState::Present(info) if is_target(&info.target) => Build::Present {
                    build: info.build,
                    target: info.target,
                },
                BuildInfoState::Present(_) | BuildInfoState::Invalid { .. } => Build::Invalid,
                BuildInfoState::Missing => Build::Missing,
            };
        }
        Err(e) => software.build = Build::Unknown { code: e.code() },
    }
    software
}

/// A Rust target triple, and nothing else that build information might
/// carry.
fn is_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 64
        && target
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

// ---- configuration ----

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ConfigState {
    Written,
    /// No `config.toml` yet: the defaults apply.
    Defaults,
    Unreadable {
        code: &'static str,
    },
}

// ---- library ----

#[derive(Serialize)]
pub(crate) struct LibraryCounts {
    pub meetings: Part<MeetingCounts>,
    pub index: Part<IndexCounts>,
    pub queue: Part<QueueCounts>,
}

#[derive(Default, Serialize)]
pub(crate) struct MeetingCounts {
    /// Meeting folders holding a `meeting.json`.
    pub folders: usize,
    pub readable: usize,
    /// Unreadable `meeting.json` files by error code.
    pub unreadable: CodeCounts,
    pub status: StatusCounts,
    pub source: SourceCounts,
    pub recording: RecordingCounts,
    pub tracks: TrackCounts,
    pub processing: ProcessingCounts,
    pub models: ModelUseCounts,
    pub echo: EchoCounts,
    pub history: MeetingHistory,
}

#[derive(Default, Serialize)]
pub(crate) struct StatusCounts {
    pub queued: usize,
    pub analyzed: usize,
    pub provisional: usize,
    pub done: usize,
    pub failed: usize,
}

#[derive(Default, Serialize)]
pub(crate) struct SourceCounts {
    pub import: usize,
    pub recording: usize,
}

/// The meetings' audio: whether they keep it and how it was imported.
#[derive(Default, Serialize)]
pub(crate) struct RecordingCounts {
    pub none: usize,
    pub available: usize,
    pub missing: usize,
    pub deleted: usize,
    pub copied: usize,
    pub moved: usize,
    pub referenced: usize,
}

#[derive(Default, Serialize)]
pub(crate) struct TrackCounts {
    pub mixed: usize,
    pub mic: usize,
    pub system: usize,
    pub participant: usize,
    /// Meetings with more than one track.
    pub multitrack_meetings: usize,
}

#[derive(Default, Serialize)]
pub(crate) struct ProcessingCounts {
    pub duration_recorded: usize,
    pub duration_not_recorded: usize,
    /// Meetings whose processing recorded an error (never its text).
    pub with_error: usize,
}

/// The models meetings were processed with, per role: never their IDs.
#[derive(Default, Serialize)]
pub(crate) struct ModelUseCounts {
    pub vad: ModelUse,
    pub asr: ModelUse,
    pub diarization: ModelUse,
    pub speaker_embedding: ModelUse,
}

#[derive(Default, Serialize)]
pub(crate) struct ModelUse {
    pub recorded: usize,
    pub not_recorded: usize,
    /// Different models among the recorded ones.
    pub distinct: usize,
    /// Recorded with the active profile's model; `None` when the profile
    /// is unknown (unreadable settings).
    pub current_profile: Option<usize>,
}

/// One role's model IDs while counting; only counts leave it.
#[derive(Default)]
struct ModelTally {
    seen: BTreeSet<String>,
    use_: ModelUse,
}

impl ModelTally {
    fn add(&mut self, used: Option<&str>, current: Option<&str>) {
        let Some(id) = used else {
            self.use_.not_recorded += 1;
            return;
        };
        self.use_.recorded += 1;
        if current == Some(id) {
            self.use_.current_profile = Some(self.use_.current_profile.unwrap_or(0) + 1);
        }
        if !self.seen.contains(id) {
            self.seen.insert(id.to_owned());
        }
    }

    fn finish(mut self, profile_known: bool) -> ModelUse {
        self.use_.distinct = self.seen.len();
        self.use_.current_profile = profile_known.then_some(self.use_.current_profile.unwrap_or(0));
        self.use_
    }
}

#[derive(Default, Serialize)]
pub(crate) struct EchoCounts {
    /// Meetings without an echo report (single-track imports have none).
    pub not_recorded: usize,
    pub reports: usize,
    /// Reports that are not a valid echo report.
    pub invalid: usize,
    pub requested: HandlingCounts,
    pub applied: HandlingCounts,
    /// Full echo cancellation was requested and not achieved.
    pub degraded: usize,
    pub measured_alignment: usize,
}

#[derive(Default, Serialize)]
pub(crate) struct HandlingCounts {
    pub none: usize,
    pub residue_gating: usize,
    pub full_aec: usize,
}

impl HandlingCounts {
    fn add(&mut self, handling: EchoHandling) {
        *match handling {
            EchoHandling::None => &mut self.none,
            EchoHandling::ResidueGating => &mut self.residue_gating,
            EchoHandling::FullAec => &mut self.full_aec,
        } += 1;
    }
}

/// Capture gaps and dropped frames are kept by the journal while a
/// recording is unsaved (see [`JournalCounts`]), not by saved meetings.
#[derive(Serialize)]
pub(crate) struct MeetingHistory {
    pub capture_gaps: Metric,
    pub dropped_frames: Metric,
}

impl Default for MeetingHistory {
    fn default() -> Self {
        Self {
            capture_gaps: Metric::NotRecorded,
            dropped_frames: Metric::NotRecorded,
        }
    }
}

impl MeetingCounts {
    fn add(&mut self, m: &Meeting, tallies: &mut [ModelTally; 4], profile: Option<&Profile>) {
        self.readable += 1;
        *match m.processing.status {
            ProcessingStatus::Queued => &mut self.status.queued,
            ProcessingStatus::Analyzed => &mut self.status.analyzed,
            ProcessingStatus::Provisional => &mut self.status.provisional,
            ProcessingStatus::Done => &mut self.status.done,
            ProcessingStatus::Failed => &mut self.status.failed,
        } += 1;
        *match m.source {
            Source::Import => &mut self.source.import,
            Source::Recording => &mut self.source.recording,
        } += 1;
        match &m.recording {
            None => self.recording.none += 1,
            Some(r) => {
                *match r.state {
                    schema::RecordingState::Available => &mut self.recording.available,
                    schema::RecordingState::Missing => &mut self.recording.missing,
                    schema::RecordingState::Deleted => &mut self.recording.deleted,
                } += 1;
                *match r.handling {
                    Handling::Copy => &mut self.recording.copied,
                    Handling::Move => &mut self.recording.moved,
                    Handling::Reference => &mut self.recording.referenced,
                } += 1;
            }
        }
        for track in &m.tracks {
            *match track.role {
                TrackRole::Mixed => &mut self.tracks.mixed,
                TrackRole::Mic => &mut self.tracks.mic,
                TrackRole::System => &mut self.tracks.system,
                TrackRole::Participant => &mut self.tracks.participant,
            } += 1;
        }
        if m.tracks.len() > 1 {
            self.tracks.multitrack_meetings += 1;
        }
        if m.processing.duration_ms.is_some() {
            self.processing.duration_recorded += 1;
        } else {
            self.processing.duration_not_recorded += 1;
        }
        if m.processing.error.is_some() {
            self.processing.with_error += 1;
        }
        let used = &m.models;
        let [vad, asr, diarization, speaker] = tallies;
        vad.add(used.vad.as_deref(), profile.map(|p| p.vad.as_str()));
        asr.add(used.asr.as_deref(), profile.map(|p| p.asr.as_str()));
        diarization.add(
            used.diarization.as_deref(),
            profile.map(|p| p.diarization.as_str()),
        );
        speaker.add(
            used.speaker_embedding.as_deref(),
            profile.map(|p| p.speaker_embedding.as_str()),
        );
        match &m.processing.echo_report {
            None => self.echo.not_recorded += 1,
            Some(value) => {
                self.echo.reports += 1;
                match EchoReport::deserialize(value) {
                    Ok(report) => {
                        self.echo.requested.add(report.requested);
                        self.echo.applied.add(report.applied);
                        if report.degraded {
                            self.echo.degraded += 1;
                        }
                        if report.alignment.is_measured() {
                            self.echo.measured_alignment += 1;
                        }
                    }
                    Err(_) => self.echo.invalid += 1,
                }
            }
        }
    }
}

/// Every meeting folder's `meeting.json`, read one at a time.
fn meetings(lib: &Library, profile: Option<&Profile>) -> Result<MeetingCounts> {
    let dirs = lib.list_meeting_dirs()?;
    let mut counts = MeetingCounts {
        folders: dirs.len(),
        ..MeetingCounts::default()
    };
    let mut tallies: [ModelTally; 4] = Default::default();
    for dir in &dirs {
        match lib.load_meeting_at(dir) {
            Ok(meeting) => counts.add(&meeting, &mut tallies, profile),
            Err(e) => *counts.unreadable.entry(e.code()).or_insert(0) += 1,
        }
    }
    let known = profile.is_some();
    let [vad, asr, diarization, speaker] = tallies;
    counts.models = ModelUseCounts {
        vad: vad.finish(known),
        asr: asr.finish(known),
        diarization: diarization.finish(known),
        speaker_embedding: speaker.finish(known),
    };
    Ok(counts)
}

#[derive(Serialize)]
pub(crate) struct IndexCounts {
    /// The search database exists.
    pub database: bool,
    pub rebuild_needed: bool,
    pub pending_meetings: usize,
    pub pending_people: usize,
}

fn index(root: &Path) -> Result<IndexCounts> {
    let health = Index::health(root)?;
    Ok(IndexCounts {
        database: Index::path(root).is_file(),
        rebuild_needed: health.rebuild,
        pending_meetings: health.meetings.len(),
        pending_people: health.people.len(),
    })
}

#[derive(Default, Serialize)]
pub(crate) struct QueueCounts {
    pub jobs: usize,
    /// Job folders whose record is unreadable.
    pub invalid: usize,
    pub state: JobStateCounts,
    /// A worker would take them now.
    pub waiting: usize,
    /// A worker's lease on them is fresh.
    pub leased: usize,
    /// Held back until a later time.
    pub deferred: usize,
    /// Failed with attempts left: run again after their backoff.
    pub retry_pending: usize,
    /// Failed for good: only a new request runs them again.
    pub failed_final: usize,
    /// Jobs that spent at least one attempt.
    pub retried: usize,
    /// Jobs by kind; kinds this version does not run count as `other`.
    pub kinds: BTreeMap<&'static str, usize>,
}

#[derive(Default, Serialize)]
pub(crate) struct JobStateCounts {
    pub queued: usize,
    pub running: usize,
    pub analyzed: usize,
    pub done: usize,
    pub failed: usize,
}

fn queue(root: &Path) -> Part<QueueCounts> {
    let queue = match Queue::open_existing(root) {
        Ok(Some(queue)) => queue,
        Ok(None) => return Part::Absent,
        Err(e) => return Part::Unreadable { code: e.code() },
    };
    Part::of(queue.snapshot().map(|snapshot| {
        let at = snapshot.at();
        let stale_after =
            chrono::TimeDelta::from_std(queue.stale_after()).unwrap_or(chrono::TimeDelta::MAX);
        let mut counts = QueueCounts {
            jobs: snapshot.jobs.len(),
            invalid: snapshot.invalid.len(),
            ..QueueCounts::default()
        };
        for job in &snapshot.jobs {
            *match job.state {
                JobState::Queued => &mut counts.state.queued,
                JobState::Running => &mut counts.state.running,
                JobState::Analyzed => &mut counts.state.analyzed,
                JobState::Done => &mut counts.state.done,
                JobState::Failed => &mut counts.state.failed,
            } += 1;
            if snapshot.is_waiting(job) {
                counts.waiting += 1;
            }
            if job
                .lease
                .as_ref()
                .is_some_and(|lease| lease.is_fresh(at, stale_after))
            {
                counts.leased += 1;
            }
            if job.not_before.is_some_and(|t| t > at) {
                counts.deferred += 1;
            }
            if job.state == JobState::Failed {
                if job.attempts >= MAX_ATTEMPTS || job.is_interactive() {
                    counts.failed_final += 1;
                } else {
                    counts.retry_pending += 1;
                }
            }
            if job.attempts > 0 {
                counts.retried += 1;
            }
            *counts.kinds.entry(known_kind(&job.kind)).or_insert(0) += 1;
        }
        counts
    }))
}

fn known_kind(kind: &str) -> &'static str {
    WORKER_KINDS
        .iter()
        .copied()
        .find(|known| *known == kind)
        .unwrap_or("other")
}

fn library(path: Option<&Path>, profile: Option<&Profile>) -> Part<LibraryCounts> {
    let Some(path) = path else {
        return Part::Unknown {
            reason: UnknownReason::ConfigUnreadable,
        };
    };
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Part::Unreadable {
                code: Error::InvalidInput(String::new()).code(),
            };
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Part::Absent,
        Err(e) => {
            return Part::Unreadable {
                code: Error::io("reading the library", e).code(),
            };
        }
    }
    let lib = match Library::open_existing(path) {
        Ok(lib) => lib,
        Err(Error::NotFound(_)) => return Part::Absent,
        Err(e) => return Part::Unreadable { code: e.code() },
    };
    let root = lib.root();
    Part::Available(LibraryCounts {
        meetings: Part::of(meetings(&lib, profile)),
        index: Part::of(index(root)),
        queue: queue(root),
    })
}

// ---- journals ----

/// The recording journals in this data folder: recordings not saved yet.
#[derive(Default, Serialize)]
pub(crate) struct JournalCounts {
    pub sessions: usize,
    /// Unreadable manifests by error code.
    pub unreadable: CodeCounts,
    pub recording: usize,
    pub stopped: usize,
    pub mic: JournalTrackCounts,
    pub system: JournalTrackCounts,
}

#[derive(Default, Serialize)]
pub(crate) struct JournalTrackCounts {
    pub chunks: usize,
    /// Still being written (or cut short by a crash).
    pub open_chunks: usize,
    /// Chunks that do not continue the one before: capture gaps, dropped
    /// blocks or pauses.
    pub gaps: usize,
    /// Sized from their files after a crash.
    pub recovered_chunks: usize,
    /// Device or rate changes.
    pub device_segments: usize,
    pub dropped_frames: u64,
    pub sessions_with_gaps: usize,
    pub sessions_with_dropped_frames: usize,
}

impl JournalTrackCounts {
    fn add(&mut self, track: &TrackJournal) {
        let gaps = track.chunks.iter().filter(|c| c.after_gap).count();
        self.chunks += track.chunks.len();
        self.open_chunks += track.chunks.iter().filter(|c| c.frames.is_none()).count();
        self.gaps += gaps;
        self.recovered_chunks += track.chunks.iter().filter(|c| c.recovered).count();
        self.device_segments += track.segments.len();
        self.dropped_frames = self.dropped_frames.saturating_add(track.dropped_frames);
        if gaps > 0 {
            self.sessions_with_gaps += 1;
        }
        if track.dropped_frames > 0 {
            self.sessions_with_dropped_frames += 1;
        }
    }
}

impl JournalCounts {
    fn add(&mut self, manifest: &SessionManifest) {
        self.sessions += 1;
        match manifest.state {
            SessionState::Recording => self.recording += 1,
            SessionState::Stopped => self.stopped += 1,
        }
        self.mic.add(&manifest.mic);
        self.system.add(&manifest.system);
    }
}

/// The manifest of every journal folder, never its audio.
fn journals(data_dir: &Path) -> Part<JournalCounts> {
    let root = lifecycle::journal_root(data_dir);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Part::Absent,
        Err(e) => {
            return Part::Unreadable {
                code: Error::io("reading the journal folder", e).code(),
            };
        }
    };
    let mut counts = JournalCounts::default();
    for entry in entries {
        let dir = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                *counts
                    .unreadable
                    .entry(Error::io("reading the journal folder", e).code())
                    .or_insert(0) += 1;
                continue;
            }
        };
        if !dir.join(journal::MANIFEST_FILE).is_file() {
            continue;
        }
        match journal::load_manifest(&dir) {
            Ok(manifest) => counts.add(&manifest),
            Err(e) => *counts.unreadable.entry(e.code()).or_insert(0) += 1,
        }
    }
    Part::Available(counts)
}

// ---- models ----

/// Whether the active profile's models are installed, per role (never
/// their IDs). Reads install markers and file sizes only.
#[derive(Serialize)]
pub(crate) struct ProfileModels {
    pub roles: Vec<ModelRole>,
}

#[derive(Serialize)]
pub(crate) struct ModelRole {
    pub role: Role,
    #[serde(flatten)]
    pub state: ModelState,
}

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ModelState {
    Installed,
    Missing,
    Corrupt,
    Unreadable { code: &'static str },
}

fn profile_models(data_dir: &Path, profile: &Profile) -> Result<ProfileModels> {
    let registry = Registry::load(data_dir)?;
    let store = ModelStore::new(data_dir);
    let mut roles = Vec::new();
    for (entry, entry_roles) in super::models::profile_models(&registry, profile)? {
        let state = match store.doctor(entry, false) {
            Ok(Health::Installed) => ModelState::Installed,
            Ok(Health::Missing) => ModelState::Missing,
            Ok(Health::Corrupt { .. }) => ModelState::Corrupt,
            Err(e) => ModelState::Unreadable { code: e.code() },
        };
        roles.extend(
            entry_roles
                .into_iter()
                .map(|role| ModelRole { role, state }),
        );
    }
    Ok(ProfileModels { roles })
}

// ---- the local part ----

/// What [`collect_local`] reads from files: everything but the agent.
pub(crate) struct Local {
    software: Software,
    config: ConfigState,
    library: Part<LibraryCounts>,
    journals: Part<JournalCounts>,
    models: Part<ProfileModels>,
}

/// Reads the software, the library at `library` (`None`: unknown, the
/// settings naming it are unreadable), the journals and the profile's
/// models of `data_dir`. `config` is the configuration as loaded.
pub(crate) fn collect_local(
    library_path: Option<&Path>,
    config: std::result::Result<&Config, &Error>,
    data_dir: &Path,
) -> Local {
    let profile = config.ok().map(|c| &c.profile);
    Local {
        software: software(),
        config: match config {
            Ok(_) if Config::path(data_dir).is_file() => ConfigState::Written,
            Ok(_) => ConfigState::Defaults,
            Err(e) => ConfigState::Unreadable { code: e.code() },
        },
        library: library(library_path, profile),
        journals: journals(data_dir),
        models: match profile {
            Some(profile) => Part::of(profile_models(data_dir, profile)),
            None => Part::Unknown {
                reason: UnknownReason::ConfigUnreadable,
            },
        },
    }
}

// ---- the agent ----

/// The recording agent, as asked now. `doctor` words the same answer.
pub(crate) enum Agent {
    NotRunning,
    /// It could not be asked (an older protocol, an unusable transport).
    Unreachable(Error),
    Running {
        /// As the agent sent it, also when the rest is unreadable.
        protocol: Option<u64>,
        state: Result<Box<DaemonState>>,
        permissions: Permissions,
    },
}

/// The agent's capture grants.
pub(crate) enum Permissions {
    Answered {
        microphone: Grant,
        system_audio: Grant,
    },
    /// The agent stopped before it answered.
    Stopped,
    Failed(Error),
}

/// A capture grant as the agent's platform reports it, never requested.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Grant {
    Granted,
    Denied,
    NotDetermined,
    /// The system exposes no grant state: only a recording proves capture.
    NotExposed,
    Unknown,
}

impl Grant {
    fn of(status: Option<&str>) -> Self {
        match status {
            Some("granted") => Self::Granted,
            Some("denied" | "restricted") => Self::Denied,
            Some("not_determined") => Self::NotDetermined,
            Some("unsupported") => Self::NotExposed,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::NotDetermined => "not_determined",
            Self::NotExposed => "not_exposed",
            Self::Unknown => "unknown",
        }
    }
}

impl Agent {
    pub(crate) fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    pub(crate) fn protocol(&self) -> Option<u64> {
        match self {
            Self::Running { protocol, .. } => *protocol,
            _ => None,
        }
    }

    pub(crate) fn error(&self) -> Option<&Error> {
        match self {
            Self::Unreachable(e) => Some(e),
            _ => None,
        }
    }

    pub(crate) fn state(&self) -> Option<&DaemonState> {
        match self {
            Self::Running {
                state: Ok(state), ..
            } => Some(state.as_ref()),
            _ => None,
        }
    }
}

/// Asks a running agent for its state and capture grants (read-only
/// calls: nothing is started, requested or changed). Without an agent
/// nothing is started either.
pub(crate) fn ask_agent(data_dir: &Path) -> Agent {
    let ping = match super::daemon::optional_call(data_dir, Call::Ping) {
        Ok(Some(value)) => match broscribe_ipc::AgentPing::from_value(value) {
            Ok(ping) => ping,
            Err(error) => return Agent::Unreachable(error.into_core()),
        },
        Ok(None) => return Agent::NotRunning,
        Err(error) => return Agent::Unreachable(error),
    };
    if ping.mode == broscribe_core::maintenance::Mode::Maintenance {
        return Agent::Unreachable(super::maintenance::denied());
    }
    let value = match super::daemon::optional_call(data_dir, Call::Status) {
        Ok(Some(value)) => value,
        Ok(None) => return Agent::NotRunning,
        Err(e) => return Agent::Unreachable(e),
    };
    let protocol = value["protocol"].as_u64();
    let state = serde_json::from_value::<DaemonState>(value)
        .map(Box::new)
        .map_err(|e| Error::Schema(format!("BroScribe Agent’s state: {e}")));
    let permissions = match super::daemon::optional_call(data_dir, Call::PermissionsStatus) {
        Ok(Some(answer)) => Permissions::Answered {
            microphone: Grant::of(answer["microphone"].as_str()),
            system_audio: Grant::of(answer["system_audio"].as_str()),
        },
        Ok(None) => Permissions::Stopped,
        Err(e) => Permissions::Failed(e),
    };
    Agent::Running {
        protocol,
        state,
        permissions,
    }
}

/// The agent part of the report.
#[derive(Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub(crate) enum AgentCounts {
    NotRunning,
    Unreachable {
        code: &'static str,
    },
    /// It answered with a state this version cannot read.
    Unreadable {
        code: &'static str,
        protocol: Option<u64>,
    },
    Running(Box<AgentHealth>),
}

#[derive(Serialize)]
pub(crate) struct AgentHealth {
    pub protocol: u32,
    pub protocol_matches: bool,
    pub version_matches: bool,
    /// The agent's build number; `None` when it does not know it.
    pub build: Option<u64>,
    pub recording: Option<RecordingHealth>,
    pub preview: PreviewHealth,
    pub jobs: AgentJobs,
    pub stopped: StoppedCounts,
    pub saved_this_run: usize,
    pub journal_issues: usize,
    pub corrupt_journals: usize,
    pub index: AgentIndex,
    pub install_hold: bool,
    pub pending_notifications: usize,
}

#[derive(Serialize)]
pub(crate) struct RecordingHealth {
    pub status: RecordingStatus,
    pub mic: Option<TrackHealthCounts>,
    pub system: Option<TrackHealthCounts>,
    pub disk: Option<DiskLevel>,
    /// `None`: the recording has no meeting app to read.
    pub reader: Option<ReaderStatus>,
    /// The reader's latest diagnostics as counts and closed states;
    /// `None` before its first batch.
    pub reader_health: Option<ReaderHealthCounts>,
}

/// [`ReaderHealth`] field by field, so a field added to it never leaves
/// here unreviewed: closed reasons and counts, never text, titles or IDs.
#[derive(Serialize)]
pub(crate) struct ReaderHealthCounts {
    pub incomplete: Option<IncompleteReason>,
    pub consecutive_failures: u32,
    /// A retry is scheduled (its recording time is left out).
    pub retry_scheduled: bool,
    pub manual_accessibility: ManualAccessibility,
    pub scoped_documents: u32,
    pub skipped_documents: u32,
}

impl ReaderHealthCounts {
    fn of(health: &ReaderHealth) -> Self {
        let ReaderHealth {
            incomplete,
            consecutive_failures,
            retry_after_s,
            manual_accessibility,
            scoped_documents,
            skipped_documents,
        } = health;
        Self {
            incomplete: *incomplete,
            consecutive_failures: *consecutive_failures,
            retry_scheduled: retry_after_s.is_some(),
            manual_accessibility: *manual_accessibility,
            scoped_documents: *scoped_documents,
            skipped_documents: *skipped_documents,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct TrackHealthCounts {
    pub state: TrackState,
    pub dropped_frames: u64,
    pub failures: u32,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum PreviewHealth {
    Off,
    Loading,
    Ready,
    Restarting { reason: WorkerProblem },
    Unavailable { reason: WorkerProblem },
}

#[derive(Serialize)]
pub(crate) struct AgentJobs {
    pub running: Option<RunningJob>,
    pub waiting: usize,
    pub deferred: usize,
}

#[derive(Serialize)]
pub(crate) struct RunningJob {
    pub kind: &'static str,
    pub phase: &'static str,
}

#[derive(Default, Serialize)]
pub(crate) struct StoppedCounts {
    pub queued: usize,
    pub running: usize,
    pub retrying: usize,
    pub failed: usize,
    pub discarding: usize,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum AgentIndex {
    Checking,
    Current,
    Pending { skipped: usize },
    Failed,
}

/// The progress phases a worker reports; anything else is `other`.
const JOB_PHASES: [&str; 7] = [
    "started", "progress", "done", "failed", "crashed", "gave_up", "waiting",
];

fn agent_counts(agent: &Agent) -> AgentCounts {
    let (protocol, state) = match agent {
        Agent::NotRunning => return AgentCounts::NotRunning,
        Agent::Unreachable(e) => return AgentCounts::Unreachable { code: e.code() },
        Agent::Running {
            protocol,
            state: Err(e),
            ..
        } => {
            return AgentCounts::Unreadable {
                code: e.code(),
                protocol: *protocol,
            };
        }
        Agent::Running {
            protocol,
            state: Ok(state),
            ..
        } => (*protocol, state),
    };
    let track = |track: broscribe_ipc::Track| {
        state.recording.as_ref().and_then(|r| {
            r.health
                .tracks
                .iter()
                .find(|t| t.track == track)
                .map(|t| TrackHealthCounts {
                    state: t.state,
                    dropped_frames: t.dropped_frames,
                    failures: t.failures,
                })
        })
    };
    let mut stopped = StoppedCounts::default();
    for s in &state.stopped {
        *match s.state {
            StoppedRecordingState::Queued { .. } => &mut stopped.queued,
            StoppedRecordingState::Running { .. } => &mut stopped.running,
            StoppedRecordingState::Retrying { .. } => &mut stopped.retrying,
            StoppedRecordingState::Failed { .. } => &mut stopped.failed,
            StoppedRecordingState::Discarding => &mut stopped.discarding,
        } += 1;
    }
    AgentCounts::Running(Box::new(AgentHealth {
        protocol: state.protocol,
        protocol_matches: protocol == Some(u64::from(PROTOCOL_VERSION)),
        version_matches: state.version == VERSION,
        build: state.build.as_ref().map(|b| b.build),
        recording: state.recording.as_ref().map(|r| RecordingHealth {
            status: r.status,
            mic: track(broscribe_ipc::Track::Mic),
            system: track(broscribe_ipc::Track::System),
            disk: r.health.disk.map(|d| d.level),
            reader: r.reader.clone(),
            reader_health: r.reader_health.as_ref().map(ReaderHealthCounts::of),
        }),
        preview: match &state.preview {
            PreviewState::Off => PreviewHealth::Off,
            PreviewState::Loading { .. } => PreviewHealth::Loading,
            PreviewState::Ready { .. } => PreviewHealth::Ready,
            PreviewState::Restarting { reason, .. } => {
                PreviewHealth::Restarting { reason: *reason }
            }
            PreviewState::Unavailable { reason } => PreviewHealth::Unavailable { reason: *reason },
        },
        jobs: AgentJobs {
            running: state.jobs.running.as_ref().map(|job| RunningJob {
                kind: known_kind(&job.kind),
                phase: JOB_PHASES
                    .iter()
                    .copied()
                    .find(|p| *p == job.phase)
                    .unwrap_or("other"),
            }),
            waiting: state.jobs.waiting,
            deferred: state.jobs.deferred,
        },
        stopped,
        saved_this_run: state.saved.len(),
        journal_issues: state.journal_issues.len(),
        corrupt_journals: state.journal_issues.iter().filter(|i| i.corrupt).count(),
        index: match &state.index {
            broscribe_ipc::IndexState::Checking => AgentIndex::Checking,
            broscribe_ipc::IndexState::Current => AgentIndex::Current,
            broscribe_ipc::IndexState::Pending { skipped } => {
                AgentIndex::Pending { skipped: *skipped }
            }
            broscribe_ipc::IndexState::Failed { .. } => AgentIndex::Failed,
        },
        install_hold: state.install.is_some(),
        pending_notifications: state.pending_notifications.len(),
    }))
}

// ---- capabilities (SH25) ----

/// What this platform can do, kept apart: whether the facility is there,
/// whether its permission is granted, and whether it was seen working.
/// Nothing is opened or requested to find out.
#[derive(Clone, Copy, Serialize)]
pub(crate) struct Capability {
    pub name: CapabilityName,
    pub availability: Availability,
    pub permission: Permission,
    pub runtime: Runtime,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityName {
    Encoder,
    Playback,
    Microphone,
    SystemAudio,
    MeetingReaders,
    AgentIpc,
    WindowTitles,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Availability {
    /// Part of the operating system (not a claim about devices).
    BuiltIn,
    /// The external program it needs was found.
    Found,
    NotFound,
    /// The system service answered.
    Reachable,
    Unreachable,
    /// The BroScribe Agent isn't running.
    NotRunning,
    /// Not observable from here.
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Permission {
    Granted,
    Denied,
    NotDetermined,
    /// The system exposes no permission state for it.
    NotExposed,
    NotApplicable,
    /// It belongs to another process (the app, the agent) that did not
    /// report it.
    Unknown,
}

impl From<Grant> for Permission {
    fn from(grant: Grant) -> Self {
        match grant {
            Grant::Granted => Self::Granted,
            Grant::Denied => Self::Denied,
            Grant::NotDetermined => Self::NotDetermined,
            Grant::NotExposed => Self::NotExposed,
            Grant::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Runtime {
    /// Working now, as the agent reports it.
    Observed,
    /// Running now with failures.
    Degraded,
    /// Not exercised: no claim either way.
    NotChecked,
}

/// Probes every capability without side effects: program lookups on
/// `PATH`, the accessibility service's answer where it is a system
/// service (AT-SPI, UI Automation), window-title availability, and what
/// the running agent reports about capture, readers and itself.
pub(crate) fn capabilities(agent: &Agent) -> Vec<Capability> {
    capabilities_with(agent, NativePlatform::diagnostic_policy())
}

/// What not finding a program on `PATH` means under `policy`: a platform
/// that also looks beside BroScribe can't call it absent.
fn tool_availability(found: bool, policy: DiagnosticPolicy) -> Availability {
    match (found, policy.external_tools_beside_executable) {
        (true, _) => Availability::Found,
        (false, true) => Availability::Unknown,
        (false, false) => Availability::NotFound,
    }
}

/// [`capabilities`] under the platform's lookup and permission `policy`;
/// the probes themselves stay real.
fn capabilities_with(agent: &Agent, policy: DiagnosticPolicy) -> Vec<Capability> {
    let not_checked = |name, availability, permission| Capability {
        name,
        availability,
        permission,
        runtime: Runtime::NotChecked,
    };
    let encoder = if NativePlatform::audio_encoder().name() == FfmpegEncoder::NAME {
        tool_availability(FfmpegEncoder::locate().is_available(), policy)
    } else {
        Availability::BuiltIn
    };
    // A native player is part of the system; elsewhere the player is ffplay.
    let playback = if policy.native_playback {
        Availability::BuiltIn
    } else {
        tool_availability(FfplayPlayer::locate().is_available(), policy)
    };
    let (microphone, system_audio) = match agent {
        Agent::Running {
            permissions:
                Permissions::Answered {
                    microphone,
                    system_audio,
                },
            ..
        } => ((*microphone).into(), (*system_audio).into()),
        _ => (Permission::Unknown, Permission::Unknown),
    };
    let recording = agent.state().and_then(|s| s.recording.as_ref());
    let capture = |name, track: broscribe_ipc::Track, permission| {
        let runtime = recording
            .and_then(|r| r.health.tracks.iter().find(|t| t.track == track))
            .map_or(Runtime::NotChecked, |t| match t.state {
                TrackState::Capturing => Runtime::Observed,
                TrackState::Restarting | TrackState::Waiting => Runtime::Degraded,
            });
        Capability {
            name,
            availability: if runtime == Runtime::Observed {
                Availability::Reachable
            } else {
                Availability::Unknown
            },
            permission,
            runtime,
        }
    };
    let (readers, reader_permission) =
        reader_capability(policy, || NativePlatform::app_ui_reader().is_trusted());
    let reader_runtime = match recording.and_then(|r| r.reader.as_ref()) {
        Some(ReaderStatus::Reading) => Runtime::Observed,
        Some(ReaderStatus::Degraded { .. } | ReaderStatus::Refused { .. }) => Runtime::Degraded,
        _ => Runtime::NotChecked,
    };
    let (ipc, ipc_runtime) = match agent {
        Agent::NotRunning => (Availability::NotRunning, Runtime::NotChecked),
        Agent::Unreachable(_) => (Availability::Unreachable, Runtime::NotChecked),
        Agent::Running {
            protocol, state, ..
        } => (
            Availability::Reachable,
            if state.is_ok() && *protocol == Some(u64::from(PROTOCOL_VERSION)) {
                Runtime::Observed
            } else {
                Runtime::Degraded
            },
        ),
    };
    vec![
        not_checked(CapabilityName::Encoder, encoder, Permission::NotApplicable),
        not_checked(
            CapabilityName::Playback,
            playback,
            Permission::NotApplicable,
        ),
        capture(
            CapabilityName::Microphone,
            broscribe_ipc::Track::Mic,
            microphone,
        ),
        capture(
            CapabilityName::SystemAudio,
            broscribe_ipc::Track::System,
            system_audio,
        ),
        Capability {
            name: CapabilityName::MeetingReaders,
            availability: readers,
            permission: reader_permission,
            runtime: reader_runtime,
        },
        Capability {
            name: CapabilityName::AgentIpc,
            availability: ipc,
            permission: Permission::NotApplicable,
            runtime: ipc_runtime,
        },
        not_checked(
            CapabilityName::WindowTitles,
            if NativePlatform::window_titles_available() {
                Availability::Reachable
            } else {
                Availability::Unreachable
            },
            Permission::Unknown,
        ),
    ]
}

/// The meeting readers' availability and permission under `policy`. A
/// grant that belongs to the app itself (the macOS Accessibility grant)
/// can't be read by another process, so `trusted` is not asked then;
/// session services (AT-SPI, UI Automation) answer whoever asks.
fn reader_capability(
    policy: DiagnosticPolicy,
    trusted: impl FnOnce() -> bool,
) -> (Availability, Permission) {
    if policy.reader_permission_is_app_specific {
        (Availability::BuiltIn, Permission::Unknown)
    } else if trusted() {
        (Availability::Reachable, Permission::NotApplicable)
    } else {
        (Availability::Unreachable, Permission::NotApplicable)
    }
}

// ---- the report ----

/// The report from the local part and the agent's answer; probes the
/// capabilities (blocking: call it off async threads).
pub(crate) fn finish(local: Local, agent: &Agent) -> DiagnosticsReport {
    DiagnosticsReport {
        schema: SCHEMA,
        software: local.software,
        config: local.config,
        library: local.library,
        journals: local.journals,
        models: local.models,
        agent: agent_counts(agent),
        capabilities: capabilities(agent),
    }
}

/// `diagnostics`: the report for the library this command resolves
/// (`--library`, `BROSCRIBE_LIBRARY`, settings, default) and its data
/// folder. With unreadable settings only an explicit library is read.
pub(crate) fn run(ctx: &mut Ctx<'_>) -> Result<()> {
    let agent = ask_agent(ctx.data_dir());
    if agent
        .error()
        .is_some_and(|error| error.code() == broscribe_core::maintenance::ERROR_CODE)
    {
        return Err(super::maintenance::denied());
    }
    let config = ctx.config();
    let library_path = match &config {
        Ok(config) => Some(ctx.library_path(config).0),
        Err(_) => ctx
            .library_flag
            .clone()
            .or_else(|| ctx.paths.library_from_env.clone()),
    };
    let local = collect_local(library_path.as_deref(), config.as_ref(), ctx.data_dir());
    let report = finish(local, &agent);
    let value = crate::to_value(&report)?;
    ctx.emit(value, |out| human(out, &report))
}

/// A serialized unit enum's name.
fn name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

/// A part's state when it is not available.
fn unavailable<T>(part: &Part<T>) -> Option<String> {
    match part {
        Part::Available(_) => None,
        Part::Absent => Some("absent".into()),
        Part::Unknown { reason } => Some(format!("unknown ({})", name(reason))),
        Part::Unreadable { code } => Some(format!("unreadable ({code})")),
    }
}

fn human(out: &mut dyn Write, r: &DiagnosticsReport) -> std::io::Result<()> {
    let s = &r.software;
    let build = match &s.build {
        Build::Present { build, target } => format!("build {build} ({target})"),
        Build::Missing => "no build information".into(),
        Build::Invalid => "invalid build information".into(),
        Build::Unknown { code } => format!("build unknown ({code})"),
    };
    writeln!(
        out,
        "BroScribe {} on {}/{}, protocol {}, {build}",
        s.version, s.os, s.arch, s.protocol
    )?;
    writeln!(
        out,
        "Settings: {}",
        match &r.config {
            ConfigState::Written => "written".to_owned(),
            ConfigState::Defaults => "defaults (not written yet)".to_owned(),
            ConfigState::Unreadable { code } => format!("unreadable ({code})"),
        }
    )?;
    match &r.library {
        Part::Available(lib) => {
            match &lib.meetings {
                Part::Available(m) => writeln!(
                    out,
                    "Meetings: {} ({} readable, {} unreadable); done {}, failed {}, queued {}; \
                     echo reports {} ({} degraded, {} invalid)",
                    m.folders,
                    m.readable,
                    m.unreadable.values().sum::<usize>(),
                    m.status.done,
                    m.status.failed,
                    m.status.queued,
                    m.echo.reports,
                    m.echo.degraded,
                    m.echo.invalid,
                )?,
                part => writeln!(out, "Meetings: {}", unavailable(part).unwrap_or_default())?,
            }
            match &lib.index {
                Part::Available(i) => writeln!(
                    out,
                    "Index: database {}, rebuild needed {}, pending {} meetings and {} people",
                    i.database, i.rebuild_needed, i.pending_meetings, i.pending_people
                )?,
                part => writeln!(out, "Index: {}", unavailable(part).unwrap_or_default())?,
            }
            match &lib.queue {
                Part::Available(q) => writeln!(
                    out,
                    "Queue: {} jobs ({} unreadable), {} waiting, {} leased, {} to retry, {} failed for good",
                    q.jobs, q.invalid, q.waiting, q.leased, q.retry_pending, q.failed_final
                )?,
                part => writeln!(out, "Queue: {}", unavailable(part).unwrap_or_default())?,
            }
        }
        part => writeln!(out, "Library: {}", unavailable(part).unwrap_or_default())?,
    }
    match &r.journals {
        Part::Available(j) => writeln!(
            out,
            "Journals: {} ({} unreadable); gaps mic {} / system {}; dropped frames mic {} / system {}",
            j.sessions,
            j.unreadable.values().sum::<usize>(),
            j.mic.gaps,
            j.system.gaps,
            j.mic.dropped_frames,
            j.system.dropped_frames
        )?,
        part => writeln!(out, "Journals: {}", unavailable(part).unwrap_or_default())?,
    }
    match &r.models {
        Part::Available(m) => {
            let roles: Vec<String> = m
                .roles
                .iter()
                .map(|r| {
                    let state = match r.state {
                        ModelState::Unreadable { code } => format!("unreadable ({code})"),
                        state => name(&state),
                    };
                    format!("{} {state}", name(&r.role))
                })
                .collect();
            writeln!(out, "Models: {}", roles.join(", "))?;
        }
        part => writeln!(out, "Models: {}", unavailable(part).unwrap_or_default())?,
    }
    match &r.agent {
        AgentCounts::NotRunning => writeln!(out, "Agent: not running")?,
        AgentCounts::Unreachable { code } => writeln!(out, "Agent: unreachable ({code})")?,
        AgentCounts::Unreadable { code, .. } => writeln!(out, "Agent: unreadable state ({code})")?,
        AgentCounts::Running(a) => {
            writeln!(
                out,
                "Agent: running, protocol {} ({}), recording {}, {} jobs waiting",
                a.protocol,
                if a.protocol_matches {
                    "matches"
                } else {
                    "differs"
                },
                a.recording.is_some(),
                a.jobs.waiting
            )?;
            if let Some(reader) = a.recording.as_ref().and_then(|r| r.reader.as_ref()) {
                let state = match reader {
                    ReaderStatus::Unavailable => "unavailable",
                    ReaderStatus::Starting => "starting",
                    ReaderStatus::Reading => "reading",
                    ReaderStatus::Degraded { .. } => "degraded",
                    ReaderStatus::Refused { .. } => "refused",
                    ReaderStatus::Paused => "paused",
                };
                write!(out, "Meeting reader state: {state}")?;
                match reader {
                    ReaderStatus::Degraded { reason } => write!(out, " ({})", name(reason))?,
                    ReaderStatus::Refused { refusal } => write!(out, " ({})", name(refusal))?,
                    _ => {}
                }
                writeln!(out)?;
            }
            if let Some(h) = a.recording.as_ref().and_then(|r| r.reader_health.as_ref()) {
                writeln!(
                    out,
                    "Meeting reader: incomplete {}, {} failures in a row, retry scheduled {}, \
                     manual accessibility {}, {} documents read, {} skipped",
                    h.incomplete.as_ref().map_or_else(|| "no".to_owned(), name),
                    h.consecutive_failures,
                    h.retry_scheduled,
                    name(&h.manual_accessibility),
                    h.scoped_documents,
                    h.skipped_documents
                )?;
            }
        }
    }
    for c in &r.capabilities {
        writeln!(
            out,
            "{}: {}; permission {}; runtime {}",
            name(&c.name),
            name(&c.availability),
            name(&c.permission),
            name(&c.runtime)
        )?;
    }
    writeln!(out, "Every count: `broscribe --json diagnostics`.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_leave_only_as_counts() {
        let mut tally = ModelTally::default();
        tally.add(Some("synthetic-a"), Some("synthetic-a"));
        tally.add(Some("synthetic-b"), Some("synthetic-a"));
        tally.add(Some("synthetic-a"), Some("synthetic-a"));
        tally.add(None, Some("synthetic-a"));
        let used = tally.finish(true);
        assert_eq!((used.recorded, used.not_recorded, used.distinct), (3, 1, 2));
        assert_eq!(used.current_profile, Some(2));
        let mut unknown = ModelTally::default();
        unknown.add(Some("synthetic-a"), None);
        assert_eq!(unknown.finish(false).current_profile, None);
    }

    #[test]
    fn unknown_kinds_and_build_targets_are_not_echoed() {
        assert_eq!(known_kind("import"), "import");
        assert_eq!(known_kind("secret-kind-marker"), "other");
        assert!(is_target("aarch64-apple-darwin"));
        assert!(!is_target("/Users/someone/secret"));
        assert!(!is_target(""));
    }

    #[test]
    fn agent_grants_keep_unexposed_apart_from_denied() {
        assert!(matches!(Grant::of(Some("unsupported")), Grant::NotExposed));
        assert!(matches!(Grant::of(Some("restricted")), Grant::Denied));
        assert!(matches!(Grant::of(None), Grant::Unknown));
        assert!(matches!(Grant::of(Some("granted")), Grant::Granted));
    }

    const MAC_LIKE: DiagnosticPolicy = DiagnosticPolicy {
        external_tools_beside_executable: false,
        native_playback: true,
        reader_permission_is_app_specific: true,
    };
    const WINDOWS_LIKE: DiagnosticPolicy = DiagnosticPolicy {
        external_tools_beside_executable: true,
        native_playback: false,
        reader_permission_is_app_specific: false,
    };
    const LINUX_LIKE: DiagnosticPolicy = DiagnosticPolicy {
        external_tools_beside_executable: false,
        native_playback: false,
        reader_permission_is_app_specific: false,
    };

    /// The platform's policy, not the compiled OS, decides what a probe
    /// means: an app-specific reader grant is never asked from here, a
    /// session service is, and a tool missing from `PATH` is absent only
    /// where nothing else is searched.
    #[test]
    fn capability_rows_follow_the_platform_policy() {
        let (readers, permission) =
            reader_capability(MAC_LIKE, || panic!("an app-specific grant was probed"));
        assert!(matches!(
            (readers, permission),
            (Availability::BuiltIn, Permission::Unknown)
        ));
        for policy in [WINDOWS_LIKE, LINUX_LIKE] {
            assert!(matches!(
                reader_capability(policy, || true),
                (Availability::Reachable, Permission::NotApplicable)
            ));
            assert!(matches!(
                reader_capability(policy, || false),
                (Availability::Unreachable, Permission::NotApplicable)
            ));
        }
        assert!(matches!(
            tool_availability(false, WINDOWS_LIKE),
            Availability::Unknown
        ));
        assert!(matches!(
            tool_availability(false, LINUX_LIKE),
            Availability::NotFound
        ));
        assert!(matches!(
            tool_availability(true, WINDOWS_LIKE),
            Availability::Found
        ));
        let rows = capabilities_with(&Agent::NotRunning, MAC_LIKE);
        let playback = rows
            .iter()
            .find(|c| c.name == CapabilityName::Playback)
            .unwrap();
        assert!(matches!(playback.availability, Availability::BuiltIn));
        let readers = rows
            .iter()
            .find(|c| c.name == CapabilityName::MeetingReaders)
            .unwrap();
        assert!(matches!(
            (readers.availability, readers.permission, readers.runtime),
            (
                Availability::BuiltIn,
                Permission::Unknown,
                Runtime::NotChecked
            )
        ));
    }

    /// The agent's reader diagnostics reach the report as closed reasons
    /// and counts only; the recording's title, app, library and session
    /// stay out.
    #[test]
    fn reader_health_leaves_only_as_counts() {
        let mut state: DaemonState = serde_json::from_value(serde_json::json!({
            "pid": 7, "version": VERSION, "protocol": PROTOCOL_VERSION,
            "library": "/synthetic/private-library",
            "recording": {
                "session_id": "synthetic-private-session",
                "status": "recording",
                "started_at": "2026-01-01T09:00:00+00:00",
                "elapsed_s": 12.0,
                "app": "Synthetic Private App",
                "title": "Synthetic private title",
                "health": { "tracks": [] },
                "reader": { "state": "degraded", "reason": "unusable_call_state" },
            },
            "jobs": { "running": null, "waiting": 0, "deferred": 0 },
            "preview": { "state": "off" },
        }))
        .unwrap();
        state.recording.as_mut().unwrap().reader_health = Some(ReaderHealth {
            incomplete: Some(IncompleteReason::NodeCap),
            consecutive_failures: 3,
            retry_after_s: Some(41.5),
            manual_accessibility: ManualAccessibility::Failed,
            scoped_documents: 2,
            skipped_documents: 1,
        });
        let agent = Agent::Running {
            protocol: Some(u64::from(PROTOCOL_VERSION)),
            state: Ok(Box::new(state)),
            permissions: Permissions::Stopped,
        };
        let report = serde_json::to_value(agent_counts(&agent)).unwrap();
        let text = report.to_string();
        assert_eq!(report["availability"], "running");
        assert_eq!(
            report["recording"]["reader_health"],
            serde_json::json!({
                "incomplete": "node_cap",
                "consecutive_failures": 3,
                "retry_scheduled": true,
                "manual_accessibility": "failed",
                "scoped_documents": 2,
                "skipped_documents": 1,
            })
        );
        for private in [
            "private-library",
            "private-session",
            "Private App",
            "private title",
        ] {
            assert!(!text.contains(private), "{private} leaked: {text}");
        }
        assert!(!text.contains("41.5"), "the retry time leaked: {text}");
    }

    #[test]
    fn a_refused_reader_is_degraded_and_visible_without_private_context() {
        let state: DaemonState = serde_json::from_value(serde_json::json!({
            "pid": 7, "version": VERSION, "protocol": PROTOCOL_VERSION,
            "library": "/synthetic/private-library",
            "recording": {
                "session_id": "synthetic-private-session",
                "status": "recording",
                "started_at": "2026-01-01T09:00:00+00:00",
                "elapsed_s": 12.0,
                "app": "Synthetic Private App",
                "title": "Synthetic private title",
                "health": { "tracks": [] },
                "reader": { "state": "refused", "refusal": "untrusted" }
            },
            "jobs": { "running": null, "waiting": 0, "deferred": 0 },
            "preview": { "state": "off" }
        }))
        .unwrap();
        let agent = Agent::Running {
            protocol: Some(u64::from(PROTOCOL_VERSION)),
            state: Ok(Box::new(state)),
            permissions: Permissions::Stopped,
        };
        let report = DiagnosticsReport {
            schema: SCHEMA,
            software: Software {
                version: VERSION,
                protocol: PROTOCOL_VERSION,
                os: "synthetic",
                arch: "synthetic",
                translated: None,
                packaging: None,
                build: Build::Missing,
                signature_seal_status: None,
            },
            config: ConfigState::Defaults,
            library: Part::Absent,
            journals: Part::Absent,
            models: Part::Absent,
            agent: agent_counts(&agent),
            capabilities: capabilities_with(&agent, MAC_LIKE),
        };
        let readers = report
            .capabilities
            .iter()
            .find(|c| c.name == CapabilityName::MeetingReaders)
            .unwrap();
        assert!(matches!(readers.runtime, Runtime::Degraded));
        let mut out = Vec::new();
        human(&mut out, &report).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("refused") && text.contains("untrusted"),
            "{text}"
        );
        for private in [
            "private-library",
            "private-session",
            "Private App",
            "private title",
        ] {
            assert!(!text.contains(private), "{private} leaked: {text}");
        }
    }
}
