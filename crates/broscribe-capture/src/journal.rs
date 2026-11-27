//! The chunk journal (plan M4 §12.5, §14.1): the only durable path for
//! captured audio.
//!
//! Layout of one session:
//!
//! ```text
//! <journal_root>/<session-id>/session.json     manifest (atomic rewrite)
//! <journal_root>/<session-id>/mic/000000.f32   raw little-endian f32 mono
//! <journal_root>/<session-id>/system/000000.f32
//! ```
//!
//! Each track is a list of chunks of at most `chunk_seconds` at the
//! segment's device rate. A chunk is listed in `session.json` (with its host
//! start time) before its file is created, and gets its frame count once it
//! is closed and fsynced. A crash therefore loses at most the unflushed tail
//! of the open chunk; [`recover`] sizes that chunk from its file length.
//!
//! The manifest is rewritten whole, so each rewrite carries every change
//! since the last one: closing a full chunk and opening the next one, a
//! rotation of both tracks, or a pause with the chunks it closes are one
//! write. A writer may also let a closed chunk's frame count wait briefly
//! for the next write ([`JournalWriter::defer_close_writes`]); its file is
//! fsynced at close either way, so recovery sizes it exactly.
//!
//! Host times are nanoseconds on a monotonic clock since the session started
//! (the recorder's `Instant` anchor). One [`TimeMap`] turns them into
//! recording time by removing pause spans; chunks, language spans,
//! bookmarks, notes and live-preview timestamps all go through it.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The manifest file; core's lifecycle fences read the same name.
pub const MANIFEST_FILE: &str = broscribe_core::lifecycle::SESSION_MANIFEST;
pub const MANIFEST_VERSION: u32 = 1;
/// Default chunk length.
pub const CHUNK_SECONDS: f64 = 5.0;
/// A block that starts this much later than the open chunk's end starts a
/// new chunk flagged `after_gap`; finalization zero-fills the hole.
pub const GAP_NS: u64 = 2_000_000;

/// The two recorded tracks. Never mixed before analysis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Track {
    Mic,
    System,
}

impl Track {
    pub const ALL: [Track; 2] = [Track::Mic, Track::System];

    /// Directory name and encoded track title.
    pub fn as_str(self) -> &'static str {
        match self {
            Track::Mic => "mic",
            Track::System => "system",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Track::Mic => 0,
            Track::System => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Capture is running (or the process died; see [`recover`]).
    Recording,
    /// Capture ended; the journal is complete and waits for the final job.
    Stopped,
}

/// Descriptive fields stored with the session and carried into the meeting.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Canonical path of the library that owns this recording. Absent in
    /// older journals, whose destination cannot safely be inferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_root: Option<PathBuf>,
    /// Detected meeting app (bundle id) when auto-recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Audio setup (`headphones`, `speakers_mic`…), detected or overridden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    /// Language at the start; later changes are [`LanguageSpan`]s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// `title` came from meeting detection (a window title), so a matching
    /// calendar event may replace it; user-entered titles leave this false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub title_detected: bool,
    /// Echo handling chosen by the audio check (`none`, `residue_gating`,
    /// `full_aec`); the setup's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo_handling: Option<String>,
    /// The rules' setup without a remote meeting, offered for an
    /// auto-detected manual start (design §5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_person_setup: Option<String>,
    /// A meeting app held the mic at some point during the recording.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub meeting_app_seen: bool,
    /// The microphone UID the user pinned when the recording started, also
    /// when it was not connected; each mic [`Segment::device_id`] says which
    /// device was actually opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_mic: Option<String>,
}

/// A run of chunks from one device at one rate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub index: u32,
    /// The recorder's capture-session counter for this track; one capture
    /// session may span several segments if its rate changes.
    pub generation: u32,
    /// Name of the capturing device, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The device the recorder asked for by its stable ID (a pinned
    /// microphone); `None` when it opened the system default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub sample_rate: u32,
    pub started_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkEntry {
    pub seq: u64,
    pub segment: u32,
    /// Host time of the chunk's first sample.
    pub host_start_ns: u64,
    /// Frames in the file; `None` while the chunk is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<u64>,
    /// The chunk does not continue the previous one (capture gap, dropped
    /// blocks or a pause before it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub after_gap: bool,
    /// Sized from its file by [`recover`] after a crash.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recovered: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackJournal {
    pub segments: Vec<Segment>,
    pub chunks: Vec<ChunkEntry>,
    /// Frames lost before reaching the journal (full queues).
    #[serde(default)]
    pub dropped_frames: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseSpan {
    pub from_ns: u64,
    /// `None` while paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageSpan {
    pub at_ns: u64,
    pub language: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub at_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub at_ns: u64,
    pub text: String,
}

/// An explicit session roster decision; never inferred from live embeddings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub person_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub evidence: Vec<String>,
    pub removed: bool,
}

/// `session.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionManifest {
    pub version: u32,
    pub session_id: String,
    pub state: SessionState,
    /// Process that writes the journal; a `recording` session whose owner is
    /// gone is recovered at daemon startup.
    pub owner_pid: u32,
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<DateTime<Utc>>,
    /// Host time of the stop (the end of the data for a recovered journal).
    /// Forensic metadata only: it tells how far a stopped capture ran past
    /// its last chunk; timelines and jobs never read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_ns: Option<u64>,
    /// `user`, `disk_low`, `journal_error`, `recovered`…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub chunk_seconds: f64,
    #[serde(default)]
    pub meta: SessionMeta,
    pub mic: TrackJournal,
    pub system: TrackJournal,
    #[serde(default)]
    pub pauses: Vec<PauseSpan>,
    #[serde(default)]
    pub languages: Vec<LanguageSpan>,
    #[serde(default)]
    pub bookmarks: Vec<Bookmark>,
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(default)]
    pub participants: Vec<Participant>,
}

impl SessionManifest {
    pub fn track(&self, track: Track) -> &TrackJournal {
        match track {
            Track::Mic => &self.mic,
            Track::System => &self.system,
        }
    }

    pub fn track_mut(&mut self, track: Track) -> &mut TrackJournal {
        match track {
            Track::Mic => &mut self.mic,
            Track::System => &mut self.system,
        }
    }

    pub fn time_map(&self) -> TimeMap {
        TimeMap {
            pauses: self.pauses.clone(),
        }
    }

    /// The last closed (fsynced) chunk of each track.
    pub fn durable_cutoff(&self) -> Cutoff {
        let last = |t: Track| {
            self.track(t)
                .chunks
                .iter()
                .rev()
                .find(|c| c.frames.is_some())
                .map(|c| c.seq)
        };
        Cutoff {
            mic: last(Track::Mic),
            system: last(Track::System),
        }
    }
}

/// The process-now boundary (plan M4 §14.2): per track, the last chunk that
/// was durable when the snapshot was taken. A job reads only chunks up to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cutoff {
    pub mic: Option<u64>,
    pub system: Option<u64>,
}

impl Cutoff {
    pub fn get(&self, track: Track) -> Option<u64> {
        match track {
            Track::Mic => self.mic,
            Track::System => self.system,
        }
    }
}

/// Host time → recording time, with pause spans removed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimeMap {
    pub pauses: Vec<PauseSpan>,
}

impl TimeMap {
    /// Recording time (ns) of host time `host_ns`. Inside a pause it is the
    /// recording time at which the pause started.
    pub fn recording_ns(&self, host_ns: u64) -> u64 {
        recording_ns(&self.pauses, host_ns)
    }

    pub fn recording_s(&self, host_ns: u64) -> f64 {
        self.recording_ns(host_ns) as f64 / 1e9
    }
}

/// [`TimeMap::recording_ns`] over borrowed pause spans.
fn recording_ns(pauses: &[PauseSpan], host_ns: u64) -> u64 {
    let paused: u64 = pauses
        .iter()
        .filter(|p| p.from_ns < host_ns)
        .map(|p| {
            p.to_ns
                .unwrap_or(u64::MAX)
                .min(host_ns)
                .saturating_sub(p.from_ns)
        })
        .sum();
    host_ns - paused.min(host_ns)
}

pub fn frames_to_ns(frames: u64, rate: u32) -> u64 {
    (u128::from(frames) * 1_000_000_000 / u128::from(rate.max(1))) as u64
}

pub fn chunk_path(dir: &Path, track: Track, seq: u64) -> PathBuf {
    dir.join(track.as_str()).join(format!("{seq:06}.f32"))
}

/// `<root>/<id>`; ids are restricted to `[A-Za-z0-9_-]`.
pub fn session_dir(root: &Path, session_id: &str) -> Result<PathBuf> {
    let ok = !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !ok {
        return Err(Error::InvalidInput(format!(
            "invalid recording session id {session_id:?}"
        )));
    }
    Ok(root.join(session_id))
}

/// Creates a journal folder readable only by the user (0700 on Unix): it holds
/// raw meeting audio and the cached calendar prior. `recursive` also creates
/// parents and tightens an existing journal root left with wider permissions.
fn private_dir(path: &Path, recursive: bool) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        builder.mode(0o700);
        builder.create(path)?;
        if recursive && fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    builder.create(path)
}

pub fn load_manifest(dir: &Path) -> Result<SessionManifest> {
    let path = dir.join(MANIFEST_FILE);
    let bytes = fs::read(&path).map_err(|e| Error::io_at(&path, e))?;
    let manifest: SessionManifest = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Schema(format!("{}: {e}", path.display())))?;
    if manifest.version != MANIFEST_VERSION {
        return Err(Error::Schema(format!(
            "{}: unsupported journal version {}",
            path.display(),
            manifest.version
        )));
    }
    validate_sample_rates(&manifest)?;
    Ok(manifest)
}

pub(crate) fn validate_sample_rates(manifest: &SessionManifest) -> Result<()> {
    for track in Track::ALL {
        if let Some(segment) = manifest
            .track(track)
            .segments
            .iter()
            .find(|s| s.sample_rate == 0)
        {
            return Err(Error::Schema(format!(
                "{} segment {} has a zero sample rate",
                track.as_str(),
                segment.index
            )));
        }
    }
    Ok(())
}

pub use broscribe_ipc::recording_words::{
    JOURNAL_LIBRARY_MISMATCH as LIBRARY_MISMATCH, JOURNAL_LIBRARY_UNKNOWN as LIBRARY_UNKNOWN,
};

/// Refuse ambiguous or foreign journals before repairing or publishing them.
pub fn check_library(manifest: &SessionManifest, canonical_root: &Path) -> Result<()> {
    match manifest.meta.library_root.as_deref() {
        Some(root) if root == canonical_root => Ok(()),
        Some(_) => Err(Error::Service {
            code: LIBRARY_MISMATCH,
            message: "the recording journal belongs to another library".into(),
        }),
        None => Err(Error::Service {
            code: LIBRARY_UNKNOWN,
            message: "the recording journal has no library identity".into(),
        }),
    }
}

/// Whether repeating a sweep against the same library cannot recover this journal.
pub fn library_blocked(code: &str) -> bool {
    matches!(code, LIBRARY_UNKNOWN | LIBRARY_MISMATCH)
}

/// Writes `session.json` atomically: temp file, fsync, rename, fsync of the
/// directory. Compact JSON: the chunk lists grow all session long and the
/// whole manifest is rewritten on every change.
pub fn write_manifest(dir: &Path, manifest: &SessionManifest) -> Result<()> {
    write_manifest_bytes(dir, manifest).map(|_| ())
}

/// [`write_manifest`], returning the bytes written.
fn write_manifest_bytes(dir: &Path, manifest: &SessionManifest) -> Result<u64> {
    let path = dir.join(MANIFEST_FILE);
    let tmp = dir.join(format!("{MANIFEST_FILE}.tmp"));
    let bytes = serde_json::to_vec(manifest)
        .map_err(|e| Error::Internal(format!("serializing {}: {e}", path.display())))?;
    let mut file = File::create(&tmp).map_err(|e| Error::io_at(&tmp, e))?;
    file.write_all(&bytes).map_err(|e| Error::io_at(&tmp, e))?;
    file.sync_all().map_err(|e| Error::io_at(&tmp, e))?;
    drop(file);
    fs::rename(&tmp, &path).map_err(|e| Error::io_at(&path, e))?;
    broscribe_core::fsutil::sync_dir(dir)?;
    Ok(bytes.len() as u64)
}

/// A journal found on disk.
#[derive(Clone, Debug)]
pub struct JournalSession {
    pub dir: PathBuf,
    pub manifest: SessionManifest,
}

/// The session folders under `root` that hold a manifest. Only a failure to
/// list `root` itself is an error; a missing root holds no journals.
pub fn journal_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(root, e)),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let dir = entry.map_err(|e| Error::io_at(root, e))?.path();
        if dir.join(MANIFEST_FILE).is_file() {
            dirs.push(dir);
        }
    }
    Ok(dirs)
}

/// Every session under `root` with a readable manifest, oldest first.
/// Unreadable ones are logged and skipped.
pub fn list_sessions(root: &Path) -> Result<Vec<JournalSession>> {
    let mut out = Vec::new();
    for dir in journal_dirs(root)? {
        match load_manifest(&dir) {
            Ok(manifest) => out.push(JournalSession { dir, manifest }),
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "unreadable recording journal")
            }
        }
    }
    out.sort_by_key(|s| s.manifest.started_at);
    Ok(out)
}

/// A journal [`recover_orphans`] could not bring to the `stopped` state. It
/// is left untouched where it is.
#[derive(Debug)]
pub struct JournalFailure {
    pub dir: PathBuf,
    pub error: Error,
}

impl JournalFailure {
    /// The manifest itself is unreadable (not JSON, not a journal, another
    /// version): retrying cannot help. Other failures are I/O on that one
    /// journal and may pass.
    pub fn corrupt(&self) -> bool {
        matches!(self.error, Error::Schema(_))
    }
}

/// What one recovery sweep found.
#[derive(Debug, Default)]
pub struct Recovery {
    /// Every journal under the root, now `stopped`, oldest first.
    pub recovered: Vec<JournalSession>,
    /// Journals that could not be recovered; they never hide the others.
    pub failed: Vec<JournalFailure>,
}

/// Runs [`recover`] on every journal under `root`. One journal failing is
/// reported in [`Recovery::failed`] and the sweep continues; only a failure
/// to list `root` is an error. A journal removed during the sweep (its job
/// finished) is neither.
pub fn recover_orphans(root: &Path, canonical_library: &Path) -> Result<Recovery> {
    let mut out = Recovery::default();
    for dir in journal_dirs(root)? {
        let result = load_manifest(&dir).and_then(|manifest| {
            check_library(&manifest, canonical_library)?;
            recover_manifest(&dir, manifest)
        });
        match result {
            Ok(manifest) => out.recovered.push(JournalSession { dir, manifest }),
            Err(_) if removed(&dir) => {}
            Err(error) => out.failed.push(JournalFailure { dir, error }),
        }
    }
    out.recovered.sort_by_key(|s| s.manifest.started_at);
    Ok(out)
}

/// The journal's manifest is gone (not merely unreadable).
fn removed(dir: &Path) -> bool {
    matches!(
        fs::metadata(dir.join(MANIFEST_FILE)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// Brings the journal of a session whose writer died into the `stopped`
/// state: sizes open chunks from their files (dropping a torn trailing
/// partial sample), drops listed chunks whose file never appeared, closes an
/// open pause. Idempotent; a `stopped` session is returned unchanged.
pub fn recover(dir: &Path) -> Result<SessionManifest> {
    recover_manifest(dir, load_manifest(dir)?)
}

fn recover_manifest(dir: &Path, mut manifest: SessionManifest) -> Result<SessionManifest> {
    if manifest.state == SessionState::Stopped {
        return Ok(manifest);
    }
    let mut last_ns = 0u64;
    for track in Track::ALL {
        let rates: Vec<(u32, u32)> = manifest
            .track(track)
            .segments
            .iter()
            .map(|s| (s.index, s.sample_rate))
            .collect();
        let journal = manifest.track_mut(track);
        let mut kept = Vec::with_capacity(journal.chunks.len());
        for mut chunk in journal.chunks.drain(..) {
            if chunk.frames.is_none() {
                let path = chunk_path(dir, track, chunk.seq);
                let len = match fs::metadata(&path) {
                    Ok(m) => m.len(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(Error::io_at(&path, e)),
                };
                let whole = len - len % 4;
                let file = OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .map_err(|e| Error::io_at(&path, e))?;
                if whole != len {
                    file.set_len(whole).map_err(|e| Error::io_at(&path, e))?;
                }
                file.sync_all().map_err(|e| Error::io_at(&path, e))?;
                chunk.frames = Some(whole / 4);
                chunk.recovered = true;
            }
            let rate = rates
                .iter()
                .find(|(i, _)| *i == chunk.segment)
                .map_or(0, |(_, r)| *r);
            if let Some(frames) = chunk.frames
                && rate > 0
            {
                last_ns = last_ns.max(chunk.host_start_ns + frames_to_ns(frames, rate));
            }
            kept.push(chunk);
        }
        journal.chunks = kept;
    }
    for pause in &mut manifest.pauses {
        if pause.to_ns.is_none() {
            pause.to_ns = Some(last_ns.max(pause.from_ns));
        }
    }
    manifest.state = SessionState::Stopped;
    manifest.stopped_at = Some(Utc::now());
    if manifest.stopped_ns.is_none() {
        manifest.stopped_ns = Some(last_ns);
    }
    if manifest.stop_reason.is_none() {
        manifest.stop_reason = Some("recovered".into());
    }
    write_manifest(dir, &manifest)?;
    Ok(manifest)
}

/// Deletes a session's journal. Refuses while it is still recording.
pub fn remove(dir: &Path) -> Result<()> {
    let manifest = load_manifest(dir)?;
    if manifest.state == SessionState::Recording {
        return Err(Error::InvalidInput(format!(
            "recording session {} is still recording",
            manifest.session_id
        )));
    }
    fs::remove_dir_all(dir).map_err(|e| Error::io_at(dir, e))
}

/// Closed chunks per track whose samples [`JournalWriter::take_closed_audio`]
/// can still hand out; older ones are read back from their files.
pub const KEPT_CLOSED_CHUNKS: usize = 4;

/// Disk work a [`JournalWriter`] did, counted where it happens (successful
/// operations only). Each manifest write is a temp-file write, its fsync, a
/// rename and a directory fsync.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JournalStats {
    /// `session.json` rewrites.
    pub manifest_writes: u64,
    /// Bytes of those rewrites, summed.
    pub manifest_bytes: u64,
    /// Chunk files fsynced at close.
    pub chunk_fsyncs: u64,
    /// Audio bytes written to chunk files.
    pub audio_bytes: u64,
    /// Closed chunks whose samples [`JournalWriter::take_closed_audio`]
    /// handed out instead of a read back from disk.
    pub kept_chunks_taken: u64,
}

struct OpenChunk {
    file: File,
    seq: u64,
    frames: u64,
    max_frames: u64,
}

/// Single-threaded journal writer; the recorder runs it on its own thread.
pub struct JournalWriter {
    dir: PathBuf,
    manifest: SessionManifest,
    open: [Option<OpenChunk>; 2],
    next_seq: [u64; 2],
    /// The next chunk opened does not continue the previous one.
    next_after_gap: [bool; 2],
    /// Host time the next block of each track should start at; `None` at a
    /// segment start or after a pause.
    expected_ns: [Option<u64>; 2],
    paused: bool,
    bytes: Vec<u8>,
    /// Since when `session.json` lacks a chunk close; `None` when it holds
    /// every close in [`Self::manifest`].
    unwritten_since: Option<Instant>,
    /// How long a chunk close may wait for the next manifest write.
    close_delay: Duration,
    /// Keep each open chunk's samples ([`Self::keep_closed_audio`]).
    keep_audio: bool,
    open_audio: [Vec<f32>; 2],
    /// `(seq, samples)` of recently closed chunks, oldest first.
    closed_audio: [VecDeque<(u64, Vec<f32>)>; 2],
    stats: JournalStats,
}

impl JournalWriter {
    /// Creates `<root>/<session_id>` (which must not exist) and its manifest.
    pub fn create(
        root: &Path,
        session_id: &str,
        meta: SessionMeta,
        chunk_seconds: f64,
        owner_pid: u32,
    ) -> Result<Self> {
        if !(chunk_seconds > 0.0 && chunk_seconds <= 60.0) {
            return Err(Error::InvalidInput(format!(
                "chunk length {chunk_seconds} s is out of range"
            )));
        }
        let dir = session_dir(root, session_id)?;
        private_dir(root, true).map_err(|e| Error::io_at(root, e))?;
        match private_dir(&dir, false) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::AlreadyExists(format!(
                    "recording journal {}",
                    dir.display()
                )));
            }
            Err(e) => return Err(Error::io_at(&dir, e)),
        }
        for track in Track::ALL {
            let sub = dir.join(track.as_str());
            private_dir(&sub, false).map_err(|e| Error::io_at(&sub, e))?;
        }
        let manifest = SessionManifest {
            version: MANIFEST_VERSION,
            session_id: session_id.to_string(),
            state: SessionState::Recording,
            owner_pid,
            started_at: Utc::now(),
            stopped_at: None,
            stopped_ns: None,
            stop_reason: None,
            chunk_seconds,
            meta,
            mic: TrackJournal::default(),
            system: TrackJournal::default(),
            pauses: Vec::new(),
            languages: Vec::new(),
            bookmarks: Vec::new(),
            notes: Vec::new(),
            participants: Vec::new(),
        };
        let manifest_bytes = write_manifest_bytes(&dir, &manifest)?;
        Ok(Self {
            dir,
            manifest,
            open: [None, None],
            next_seq: [0, 0],
            next_after_gap: [false, false],
            expected_ns: [None, None],
            paused: false,
            bytes: Vec::new(),
            unwritten_since: None,
            close_delay: Duration::ZERO,
            keep_audio: false,
            open_audio: [Vec::new(), Vec::new()],
            closed_audio: [VecDeque::new(), VecDeque::new()],
            stats: JournalStats {
                manifest_writes: 1,
                manifest_bytes,
                ..JournalStats::default()
            },
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn manifest(&self) -> &SessionManifest {
        &self.manifest
    }

    /// What this writer has done to disk so far.
    pub fn stats(&self) -> JournalStats {
        self.stats
    }

    /// Lets the frame count of a chunk closed by [`Self::append`] wait up to
    /// `delay` for the next manifest write (normally the next chunk's
    /// opening, milliseconds later) instead of rewriting the manifest for it
    /// alone; [`Self::flush_due`] writes it once the delay is over. The chunk
    /// file is fsynced at close either way, so a crash meanwhile loses
    /// nothing: [`recover`] sizes the chunk from its file. Closes by
    /// rotation, pause, segment change and finish are always written at once.
    pub fn defer_close_writes(&mut self, delay: Duration) {
        self.close_delay = delay;
    }

    /// Writes a deferred chunk close whose delay is over.
    pub fn flush_due(&mut self) -> Result<()> {
        match self.unwritten_since {
            Some(since) if since.elapsed() >= self.close_delay => self.save(),
            _ => Ok(()),
        }
    }

    /// Keeps each chunk's samples in memory while it is open, so the audio of
    /// a chunk just closed can be taken with [`Self::take_closed_audio`]
    /// instead of read back from the file that was just fsynced.
    pub fn keep_closed_audio(&mut self) {
        self.keep_audio = true;
    }

    /// The samples of closed chunk `seq` of `track` (exactly what its file
    /// holds), if they are still kept; kept chunks before it are dropped.
    pub fn take_closed_audio(&mut self, track: Track, seq: u64) -> Option<Vec<f32>> {
        let kept = &mut self.closed_audio[track.index()];
        while kept.front().is_some_and(|(s, _)| *s < seq) {
            kept.pop_front();
        }
        if kept.front().is_some_and(|(s, _)| *s == seq) {
            self.stats.kept_chunks_taken += 1;
            kept.pop_front().map(|(_, samples)| samples)
        } else {
            None
        }
    }

    /// Uses the daemon's session start time rather than thread startup time.
    pub fn set_started_at(&mut self, started_at: DateTime<Utc>) -> Result<()> {
        self.manifest.started_at = started_at;
        self.save()
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Writes the manifest with every pending change.
    fn save(&mut self) -> Result<()> {
        let bytes = write_manifest_bytes(&self.dir, &self.manifest)?;
        self.unwritten_since = None;
        self.stats.manifest_writes += 1;
        self.stats.manifest_bytes += bytes;
        Ok(())
    }

    /// Writes chunk closes no later write of this call carried, unless
    /// [`Self::defer_close_writes`] lets them wait.
    fn settle(&mut self) -> Result<()> {
        if self.unwritten_since.is_some() && self.close_delay.is_zero() {
            self.save()
        } else {
            Ok(())
        }
    }

    /// Starts a new segment (device or rate change) at `host_ns`.
    pub fn begin_segment(
        &mut self,
        track: Track,
        generation: u32,
        device: Option<String>,
        sample_rate: u32,
        host_ns: u64,
    ) -> Result<()> {
        if sample_rate == 0 {
            return Err(Error::InvalidInput("segment sample rate is 0".into()));
        }
        self.close(track)?;
        let journal = self.manifest.track_mut(track);
        let index = journal.segments.len() as u32;
        journal.segments.push(Segment {
            index,
            generation,
            device,
            device_id: None,
            sample_rate,
            started_ns: host_ns,
        });
        // A new segment is always a new run on the timeline.
        self.next_after_gap[track.index()] = false;
        self.expected_ns[track.index()] = None;
        self.save()
    }

    /// The current segment of `track`.
    pub fn segment(&self, track: Track) -> Option<&Segment> {
        self.manifest.track(track).segments.last()
    }

    /// Records which device capture `generation` of `track` opened: `id` is
    /// the device it asked for (`None`: the system default) and `name` its
    /// name when known. The capture may deliver audio, and so begin its
    /// segments, before the recorder learns this.
    pub fn set_device(
        &mut self,
        track: Track,
        generation: u32,
        name: Option<&str>,
        id: Option<&str>,
    ) -> Result<()> {
        let mut changed = false;
        for segment in &mut self.manifest.track_mut(track).segments {
            if segment.generation != generation {
                continue;
            }
            if let Some(name) = name
                && segment.device.as_deref() != Some(name)
            {
                segment.device = Some(name.to_string());
                changed = true;
            }
            if segment.device_id.as_deref() != id {
                segment.device_id = id.map(str::to_string);
                changed = true;
            }
        }
        if changed { self.save() } else { Ok(()) }
    }

    /// Appends samples captured from `host_start_ns` to the current segment
    /// of `track`. Dropped while paused. A block that starts more than
    /// [`GAP_NS`] after the open chunk's end starts a new chunk flagged
    /// `after_gap`. Returns whether the samples were journaled.
    pub fn append(&mut self, track: Track, host_start_ns: u64, samples: &[f32]) -> Result<bool> {
        if self.paused {
            return Ok(false);
        }
        let Some(rate) = self.segment(track).map(|s| s.sample_rate) else {
            return Err(Error::Internal(format!(
                "{} audio before its first segment",
                track.as_str()
            )));
        };
        let t = track.index();
        if let Some(expected) = self.expected_ns[t]
            && host_start_ns > expected + GAP_NS
        {
            self.close(track)?;
            self.next_after_gap[t] = true;
        }
        let mut host = host_start_ns;
        let mut rest = samples;
        while !rest.is_empty() {
            if self.open[t].is_none() {
                // Its manifest write also carries the closes before it.
                self.open_chunk(track, host, rate)?;
            }
            let Some(open) = self.open[t].as_mut() else {
                return Err(Error::Internal("journal chunk did not open".into()));
            };
            let n = ((open.max_frames - open.frames) as usize).min(rest.len());
            self.bytes.clear();
            self.bytes
                .extend(rest[..n].iter().flat_map(|s| s.to_le_bytes()));
            let path = chunk_path(&self.dir, track, open.seq);
            open.file
                .write_all(&self.bytes)
                .map_err(|e| Error::io_at(&path, e))?;
            self.stats.audio_bytes += self.bytes.len() as u64;
            open.frames += n as u64;
            let full = open.frames >= open.max_frames;
            if self.keep_audio {
                self.open_audio[t].extend_from_slice(&rest[..n]);
            }
            host += frames_to_ns(n as u64, rate);
            rest = &rest[n..];
            if full {
                self.close(track)?;
            }
        }
        self.expected_ns[t] = Some(host);
        self.settle()?;
        Ok(true)
    }

    /// Frames lost before reaching the journal.
    pub fn add_dropped(&mut self, track: Track, frames: u64) {
        self.manifest.track_mut(track).dropped_frames += frames;
        self.next_after_gap[track.index()] = true;
    }

    fn open_chunk(&mut self, track: Track, host_start_ns: u64, rate: u32) -> Result<()> {
        let t = track.index();
        let seq = self.next_seq[t];
        let segment = self.segment(track).map_or(0, |s| s.index);
        let after_gap = std::mem::take(&mut self.next_after_gap[t]);
        self.manifest.track_mut(track).chunks.push(ChunkEntry {
            seq,
            segment,
            host_start_ns,
            frames: None,
            after_gap,
            recovered: false,
        });
        // Listed before the file exists, so recovery always knows its start.
        self.save()?;
        let path = chunk_path(&self.dir, track, seq);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| Error::io_at(&path, e))?;
        let max_frames = ((self.manifest.chunk_seconds * f64::from(rate)).round() as u64).max(1);
        if self.keep_audio {
            self.open_audio[t] = Vec::with_capacity(max_frames as usize);
        }
        self.open[t] = Some(OpenChunk {
            file,
            seq,
            frames: 0,
            max_frames,
        });
        self.next_seq[t] = seq + 1;
        Ok(())
    }

    /// Fsyncs and closes the open chunk of `track`, if any, in memory; the
    /// caller's next manifest write records it. Returns whether one was
    /// closed.
    fn close(&mut self, track: Track) -> Result<bool> {
        let t = track.index();
        let Some(open) = self.open[t].take() else {
            return Ok(false);
        };
        let audio = std::mem::take(&mut self.open_audio[t]);
        let path = chunk_path(&self.dir, track, open.seq);
        open.file.sync_all().map_err(|e| Error::io_at(&path, e))?;
        self.stats.chunk_fsyncs += 1;
        drop(open.file);
        let journal = self.manifest.track_mut(track);
        if open.frames == 0 {
            // Nothing was written: forget the chunk entirely.
            journal.chunks.retain(|c| c.seq != open.seq);
            let _ = fs::remove_file(&path);
            self.next_seq[t] = open.seq;
        } else {
            if let Some(entry) = journal.chunks.iter_mut().find(|c| c.seq == open.seq) {
                entry.frames = Some(open.frames);
            }
            if self.keep_audio {
                let kept = &mut self.closed_audio[t];
                kept.push_back((open.seq, audio));
                if kept.len() > KEPT_CLOSED_CHUNKS {
                    kept.pop_front();
                }
            }
        }
        self.unwritten_since.get_or_insert_with(Instant::now);
        Ok(open.frames > 0)
    }

    /// Closes every open chunk and writes the manifest, so the cutoff covers
    /// everything captured so far, on disk too; the next blocks continue in
    /// new chunks without a gap.
    pub fn rotate(&mut self) -> Result<Cutoff> {
        for track in Track::ALL {
            self.close(track)?;
        }
        if self.unwritten_since.is_some() {
            self.save()?;
        }
        Ok(self.manifest.durable_cutoff())
    }

    pub fn cutoff(&self) -> Cutoff {
        self.manifest.durable_cutoff()
    }

    /// Recording time of `host_ns` under the current pauses.
    pub fn recording_s(&self, host_ns: u64) -> f64 {
        recording_ns(&self.manifest.pauses, host_ns) as f64 / 1e9
    }

    pub fn pause(&mut self, host_ns: u64) -> Result<()> {
        if self.paused {
            return Ok(());
        }
        for track in Track::ALL {
            self.close(track)?;
            self.next_after_gap[track.index()] = true;
            self.expected_ns[track.index()] = None;
        }
        self.paused = true;
        self.manifest.pauses.push(PauseSpan {
            from_ns: host_ns,
            to_ns: None,
        });
        self.save()
    }

    pub fn resume(&mut self, host_ns: u64) -> Result<()> {
        if !self.paused {
            return Ok(());
        }
        self.paused = false;
        if let Some(span) = self.manifest.pauses.last_mut() {
            span.to_ns = Some(host_ns.max(span.from_ns));
        }
        self.save()
    }

    pub fn set_language(&mut self, host_ns: u64, language: &str) -> Result<()> {
        self.manifest.languages.push(LanguageSpan {
            at_ns: host_ns,
            language: language.to_string(),
        });
        self.save()
    }

    pub fn bookmark(&mut self, host_ns: u64, label: Option<String>) -> Result<()> {
        self.manifest.bookmarks.push(Bookmark {
            at_ns: host_ns,
            label,
        });
        self.save()
    }

    pub fn note(&mut self, host_ns: u64, text: &str) -> Result<()> {
        self.manifest.notes.push(Note {
            at_ns: host_ns,
            text: text.to_string(),
        });
        self.save()
    }

    pub fn add_tag(&mut self, tag: &str) -> Result<()> {
        if self.manifest.meta.tags.iter().any(|t| t == tag) {
            return Ok(());
        }
        let before = self.manifest.meta.tags.clone();
        self.manifest.meta.tags.push(tag.to_string());
        self.write_or_restore(|m| m.meta.tags = before)
    }

    /// Removes `tag`, compared trimmed and case-insensitively.
    pub fn remove_tag(&mut self, tag: &str) -> Result<()> {
        let fold = |t: &str| t.trim().to_lowercase();
        let tag = fold(tag);
        let before = self.manifest.meta.tags.clone();
        self.manifest.meta.tags.retain(|t| fold(t) != tag);
        if self.manifest.meta.tags.len() == before.len() {
            return Ok(());
        }
        self.write_or_restore(|m| m.meta.tags = before)
    }

    pub fn mark_meeting_app_seen(&mut self) -> Result<()> {
        if self.manifest.meta.meeting_app_seen {
            return Ok(());
        }
        self.manifest.meta.meeting_app_seen = true;
        self.save()
    }

    /// Replaces the current explicit decision without duplicating the person.
    /// A failed write leaves the decision unrecorded, in memory too, so no
    /// later write can persist an attendance reported as not recorded.
    pub fn set_participant(&mut self, person_ref: &str, name: &str, confirm: bool) -> Result<()> {
        if person_ref.is_empty() {
            return Err(Error::InvalidInput("participant reference is empty".into()));
        }
        let before = self.manifest.participants.clone();
        if let Some(participant) = self
            .manifest
            .participants
            .iter_mut()
            .find(|p| p.person_ref == person_ref)
        {
            participant.name = confirm.then(|| name.to_owned());
            participant.removed = !confirm;
            participant.evidence = vec!["user".into()];
        } else {
            self.manifest.participants.push(Participant {
                person_ref: person_ref.into(),
                name: confirm.then(|| name.to_owned()),
                email: None,
                evidence: vec!["user".into()],
                removed: !confirm,
            });
        }
        self.write_or_restore(|m| m.participants = before)
    }

    /// Writes the manifest; on failure `restore` puts back what the caller
    /// changed, so the in-memory manifest matches what is on disk.
    fn write_or_restore(&mut self, restore: impl FnOnce(&mut SessionManifest)) -> Result<()> {
        let written = self.save();
        if written.is_err() {
            restore(&mut self.manifest);
        }
        written
    }

    pub fn set_meta(&mut self, mut meta: SessionMeta) -> Result<()> {
        meta.library_root
            .clone_from(&self.manifest.meta.library_root);
        self.manifest.meta = meta;
        self.save()
    }

    /// Closes the journal: fsyncs open chunks, ends an open pause and marks
    /// the session `stopped`.
    pub fn finish(mut self, host_ns: u64, reason: &str) -> Result<SessionManifest> {
        for track in Track::ALL {
            self.close(track)?;
        }
        if let Some(span) = self.manifest.pauses.last_mut()
            && span.to_ns.is_none()
        {
            span.to_ns = Some(host_ns.max(span.from_ns));
        }
        self.manifest.state = SessionState::Stopped;
        self.manifest.stopped_at = Some(Utc::now());
        self.manifest.stopped_ns = Some(host_ns);
        self.manifest.stop_reason = Some(reason.to_string());
        self.save()?;
        Ok(self.manifest)
    }
}

/// Reads one chunk's samples (at most its listed frame count).
pub fn read_chunk(dir: &Path, track: Track, chunk: &ChunkEntry) -> Result<Vec<f32>> {
    let path = chunk_path(dir, track, chunk.seq);
    let bytes = fs::read(&path).map_err(|e| Error::io_at(&path, e))?;
    let frames = chunk.frames.unwrap_or((bytes.len() / 4) as u64) as usize;
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .take(frames)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(root: &Path) -> JournalWriter {
        JournalWriter::create(root, "s1", SessionMeta::default(), 1.0, 42).unwrap()
    }

    fn ramp(n: usize, start: f32) -> Vec<f32> {
        (0..n).map(|i| start + i as f32).collect()
    }

    /// A manifest write that fails leaves no unwritten attendance or tag in
    /// memory for a later, successful write to persist.
    #[cfg(unix)]
    #[test]
    fn failed_metadata_writes_are_never_persisted_later() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let mut w = writer(root.path());
        let dir = w.dir().to_owned();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        if File::create(dir.join("probe")).is_ok() {
            // Permissions do not bind this user (root): nothing to prove.
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        assert!(
            w.set_participant("p-0000aa", "Synthetic Example", true)
                .is_err()
        );
        assert!(w.add_tag("unsaved").is_err());
        assert!(w.manifest().participants.is_empty());
        assert!(w.manifest().meta.tags.is_empty());
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        // A later metadata write succeeds and carries only what was written.
        w.bookmark(0, None).unwrap();
        let on_disk = load_manifest(&dir).unwrap();
        assert!(on_disk.participants.is_empty());
        assert!(on_disk.meta.tags.is_empty());
        assert_eq!(on_disk.bookmarks.len(), 1);
    }

    #[test]
    fn chunks_roll_over_and_are_listed() {
        let root = tempfile::tempdir().unwrap();
        let mut w = writer(root.path());
        w.begin_segment(Track::Mic, 0, Some("Mic".into()), 100, 0)
            .unwrap();
        // 250 frames at 100 Hz with 1 s chunks: 100 + 100 + 50 (open).
        assert!(w.append(Track::Mic, 0, &ramp(250, 0.0)).unwrap());
        let m = w.manifest().clone();
        assert_eq!(m.mic.chunks.len(), 3);
        assert_eq!(m.mic.chunks[0].frames, Some(100));
        assert_eq!(m.mic.chunks[1].host_start_ns, 1_000_000_000);
        assert_eq!(m.mic.chunks[2].frames, None);
        assert_eq!(
            w.cutoff(),
            Cutoff {
                mic: Some(1),
                system: None
            }
        );
        let on_disk = load_manifest(w.dir()).unwrap();
        assert_eq!(on_disk.mic.chunks.len(), 3);

        let dir = w.dir().to_path_buf();
        let m = w.finish(2_500_000_000, "user").unwrap();
        assert_eq!(m.state, SessionState::Stopped);
        assert_eq!(m.mic.chunks[2].frames, Some(50));
        let samples: Vec<f32> = m
            .mic
            .chunks
            .iter()
            .flat_map(|c| read_chunk(&dir, Track::Mic, c).unwrap())
            .collect();
        assert_eq!(samples, ramp(250, 0.0));
    }

    #[test]
    fn late_block_starts_a_gap_chunk() {
        let root = tempfile::tempdir().unwrap();
        let mut w = writer(root.path());
        w.begin_segment(Track::System, 0, None, 100, 0).unwrap();
        w.append(Track::System, 0, &ramp(30, 0.0)).unwrap();
        // Jitter below the threshold continues the chunk.
        w.append(Track::System, 300_000_000 + 1_000_000, &ramp(10, 30.0))
            .unwrap();
        // 1 s later than expected: new chunk after a gap.
        w.append(Track::System, 2_000_000_000, &ramp(10, 40.0))
            .unwrap();
        let chunks = &w.manifest().system.chunks;
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].frames, Some(40));
        assert!(!chunks[0].after_gap);
        assert!(chunks[1].after_gap);
        assert_eq!(chunks[1].host_start_ns, 2_000_000_000);
    }

    #[test]
    fn a_dropped_callback_is_not_time_compressed() {
        let root = tempfile::tempdir().unwrap();
        let mut w = writer(root.path());
        w.begin_segment(Track::System, 0, None, 48_000, 0).unwrap();
        w.append(Track::System, 0, &[0.25; 960]).unwrap();
        // One missing 20 ms callback is a real timeline hole, not jitter.
        w.append(Track::System, 40_000_000, &[0.5; 960]).unwrap();
        let manifest = w.finish(60_000_000, "user").unwrap();
        let timeline = crate::timeline::Timeline::plan(
            &root.path().join("s1"),
            &manifest,
            &crate::timeline::TimelineOptions::default(),
        )
        .unwrap();
        let samples = timeline.read_track(Track::System).unwrap();
        assert_eq!(samples.len(), 2880);
        assert!(samples[..960].iter().all(|s| *s == 0.25));
        assert!(samples[960..1920].iter().all(|s| *s == 0.0));
        assert!(samples[1920..].iter().all(|s| *s == 0.5));
    }

    #[test]
    fn crash_recovery_sizes_truncated_chunk() {
        let root = tempfile::tempdir().unwrap();
        let mut w = writer(root.path());
        w.begin_segment(Track::Mic, 0, None, 100, 0).unwrap();
        w.append(Track::Mic, 0, &ramp(130, 0.0)).unwrap();
        w.pause(1_300_000_000).unwrap();
        w.resume(2_000_000_000).unwrap();
        w.append(Track::Mic, 2_000_000_000, &ramp(20, 130.0))
            .unwrap();
        w.pause(2_200_000_000).unwrap();
        let dir = w.dir().to_path_buf();
        // Simulate a crash: the writer vanishes with chunk 2 open and the
        // last sample torn in half.
        let open_seq = w.manifest().mic.chunks.last().unwrap().seq;
        std::mem::forget(w);
        let path = chunk_path(&dir, Track::Mic, open_seq);
        let len = fs::metadata(&path).unwrap().len();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(len - 2).unwrap();
        drop(file);
        let before = load_manifest(&dir).unwrap();
        // Chunk 2 was closed by the pause, so make it look open again.
        let mut crashed = before.clone();
        crashed.mic.chunks.last_mut().unwrap().frames = None;
        crashed.pauses.last_mut().unwrap().to_ns = None;
        // A chunk listed but never created is dropped by recovery.
        crashed.mic.chunks.push(ChunkEntry {
            seq: open_seq + 1,
            segment: 0,
            host_start_ns: 9_000_000_000,
            frames: None,
            after_gap: true,
            recovered: false,
        });
        write_manifest(&dir, &crashed).unwrap();

        let m = recover(&dir).unwrap();
        assert_eq!(m.state, SessionState::Stopped);
        assert_eq!(m.stop_reason.as_deref(), Some("recovered"));
        let last = m.mic.chunks.last().unwrap();
        assert_eq!(last.seq, open_seq);
        assert_eq!(last.frames, Some(19), "torn sample dropped");
        assert!(last.recovered);
        assert!(m.pauses.iter().all(|p| p.to_ns.is_some()));
        let samples: Vec<f32> = m
            .mic
            .chunks
            .iter()
            .flat_map(|c| read_chunk(&dir, Track::Mic, c).unwrap())
            .collect();
        assert_eq!(samples, ramp(149, 0.0));
        // Idempotent, and removable once stopped.
        assert_eq!(recover(&dir).unwrap(), m);
        remove(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn time_map_removes_pauses() {
        let map = TimeMap {
            pauses: vec![
                PauseSpan {
                    from_ns: 10,
                    to_ns: Some(20),
                },
                PauseSpan {
                    from_ns: 30,
                    to_ns: None,
                },
            ],
        };
        assert_eq!(map.recording_ns(5), 5);
        assert_eq!(map.recording_ns(15), 10, "inside a pause: its start");
        assert_eq!(map.recording_ns(25), 15);
        assert_eq!(map.recording_ns(40), 20, "open pause runs to now");
    }

    #[test]
    fn rejects_bad_ids_and_existing_sessions() {
        let root = tempfile::tempdir().unwrap();
        assert!(session_dir(root.path(), "../x").is_err());
        assert!(session_dir(root.path(), "").is_err());
        let _w = writer(root.path());
        assert_eq!(
            JournalWriter::create(root.path(), "s1", SessionMeta::default(), 1.0, 1)
                .err()
                .unwrap()
                .code(),
            "already_exists"
        );
        assert_eq!(list_sessions(root.path()).unwrap().len(), 1);
        assert!(remove(&root.path().join("s1")).is_err(), "still recording");
    }

    /// One unreadable journal never hides another one's recovery; only an
    /// unreadable root fails the sweep.
    #[test]
    fn recovery_reports_a_corrupt_journal_and_recovers_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let library = root.path().canonicalize().unwrap();
        let meta = SessionMeta {
            library_root: Some(library.clone()),
            ..Default::default()
        };
        let mut healthy = JournalWriter::create(root.path(), "healthy", meta, 1.0, 1).unwrap();
        healthy.begin_segment(Track::Mic, 0, None, 100, 0).unwrap();
        healthy.append(Track::Mic, 0, &ramp(150, 0.0)).unwrap();
        let healthy_dir = healthy.dir().to_owned();
        std::mem::forget(healthy);
        let corrupt =
            JournalWriter::create(root.path(), "corrupt", SessionMeta::default(), 1.0, 1).unwrap();
        let corrupt_dir = corrupt.dir().to_owned();
        drop(corrupt);
        fs::write(corrupt_dir.join(MANIFEST_FILE), b"{\"version\":1,").unwrap();

        let recovery = recover_orphans(root.path(), &library).unwrap();
        assert_eq!(recovery.recovered.len(), 1);
        assert_eq!(recovery.recovered[0].dir, healthy_dir);
        let manifest = &recovery.recovered[0].manifest;
        assert_eq!(manifest.state, SessionState::Stopped);
        let frames: u64 = manifest.mic.chunks.iter().filter_map(|c| c.frames).sum();
        assert_eq!(frames, 150);
        assert_eq!(recovery.failed.len(), 1);
        assert_eq!(recovery.failed[0].dir, corrupt_dir);
        assert!(recovery.failed[0].corrupt());
        assert_eq!(
            fs::read(corrupt_dir.join(MANIFEST_FILE)).unwrap(),
            b"{\"version\":1,",
            "a failed journal is left as it was"
        );

        let not_a_dir = root.path().join("file");
        fs::write(&not_a_dir, b"").unwrap();
        assert!(recover_orphans(&not_a_dir, &library).is_err());
        assert!(
            recover_orphans(&root.path().join("absent"), &library)
                .unwrap()
                .recovered
                .is_empty()
        );
    }

    /// A chunk close waiting for the next manifest write loses nothing in a
    /// crash, and the kept samples are exactly the file's.
    #[test]
    fn deferred_close_survives_a_crash_with_identical_audio() {
        let root = tempfile::tempdir().unwrap();
        let mut w = writer(root.path());
        w.defer_close_writes(Duration::from_secs(3600));
        w.keep_closed_audio();
        w.begin_segment(Track::Mic, 0, None, 100, 0).unwrap();
        // Exactly one full chunk: closed and fsynced, its frame count not yet
        // written.
        w.append(Track::Mic, 0, &ramp(100, 0.5)).unwrap();
        let dir = w.dir().to_path_buf();
        let chunk = w.manifest().mic.chunks[0].clone();
        assert_eq!(chunk.frames, Some(100));
        assert_eq!(load_manifest(&dir).unwrap().mic.chunks[0].frames, None);
        assert_eq!(
            w.take_closed_audio(Track::Mic, chunk.seq).unwrap(),
            read_chunk(&dir, Track::Mic, &chunk).unwrap()
        );
        assert!(w.take_closed_audio(Track::Mic, chunk.seq).is_none());
        std::mem::forget(w);

        let m = recover(&dir).unwrap();
        assert_eq!(m.mic.chunks.len(), 1);
        assert_eq!(m.mic.chunks[0].frames, Some(100));
        assert_eq!(m.mic.chunks[0].host_start_ns, 0);
        assert_eq!(
            read_chunk(&dir, Track::Mic, &m.mic.chunks[0]).unwrap(),
            ramp(100, 0.5)
        );
    }

    /// Rolling into the next chunk writes the close with the opening, and a
    /// deferred close is written once its delay is over.
    #[test]
    fn rollover_and_flush_keep_the_disk_manifest_current() {
        let root = tempfile::tempdir().unwrap();
        let mut w = writer(root.path());
        w.defer_close_writes(Duration::ZERO);
        w.begin_segment(Track::System, 0, None, 100, 0).unwrap();
        w.append(Track::System, 0, &ramp(150, 0.0)).unwrap();
        let on_disk = load_manifest(w.dir()).unwrap();
        assert_eq!(on_disk.system.chunks[0].frames, Some(100));
        assert_eq!(on_disk.system.chunks[1].frames, None);
        // Create, segment, first chunk, then the close and the next opening
        // in one write.
        let before = w.stats();
        assert_eq!(before.manifest_writes, 4);
        assert_eq!(before.chunk_fsyncs, 1);
        assert_eq!(before.audio_bytes, 600);

        w.defer_close_writes(Duration::from_millis(1));
        w.append(Track::System, 1_500_000_000, &ramp(50, 150.0))
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        w.flush_due().unwrap();
        let on_disk = load_manifest(w.dir()).unwrap();
        assert_eq!(on_disk.system.chunks[1].frames, Some(100));
        assert_eq!(on_disk, *w.manifest());
        let after = w.stats();
        assert_eq!(after.manifest_writes, 5);
        assert_eq!(after.chunk_fsyncs, 2);
        // The flush wrote exactly the manifest now on disk.
        assert_eq!(
            after.manifest_bytes - before.manifest_bytes,
            fs::metadata(w.dir().join(MANIFEST_FILE)).unwrap().len()
        );
    }
}
