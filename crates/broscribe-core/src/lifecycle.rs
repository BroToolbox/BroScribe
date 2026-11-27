//! Meeting and recording-session lifecycle shared by the daemon, CLI, MCP and
//! app (design §7.1): user titles that reach a recording's live or stopped
//! session, permanent deletion of one meeting, and the cleanup of a
//! discarded recording.
//!
//! Durable fences, identifiers only (never titles, names or embeddings):
//! - **Session title sidecar** `<journal_root>/<session>/title.json`: the
//!   user's latest title for a recording not yet published as final. It is
//!   written and read only under the library lock, so the final publication
//!   (which reads it again inside its locked commit) sees every edit made
//!   before it and a later edit finds the published meeting instead.
//! - **Discard intent** `<journal_root>/.discard/<session>.json`, outside the
//!   removable journal folder: written before capture stops; while it exists
//!   no final or provisional job of the session is queued, recovered or
//!   published. Removed last, after the session's jobs, provisional meeting
//!   and journal are gone. Once the journal is gone, its missing
//!   `session.json` blocks publication and queueing too.
//! - **Deletion marker** `<library>/.deleting/<meeting>.json`, outside the
//!   meeting folder: written in the short locked commit before anything
//!   changes. The folder is then renamed atomically to
//!   `<library>/.deleted/<meeting>/` (hidden from listings and index
//!   rebuilds) and removed afterwards outside the lock. Recovery
//!   ([`finish_pending_deletions`]) or a repeated [`delete_meeting`] of the
//!   same meeting completes an interrupted deletion, whether or not its
//!   folder was already renamed; the marker goes last. While the marker
//!   exists the meeting ID is not handed out again ([`meeting_deleted`]).
//!
//! Lock order is library → queue, as everywhere. Physical cleanup runs after
//! the locked commit; a partial failure is reported (`cleanup_pending`),
//! never claimed complete, and retried by maintenance or the next delete.
//! [`diagnose`] reports what is pending, identifiers only, and changes
//! nothing.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::index::Index;
use crate::process::ProcessProbe;
use crate::queue::{Job, JobId, JobState, Queue, REVOKED, job_refers_to};
use crate::retention::{PLAYBACK_CACHE_DIR, is_playback_cache_of};
use crate::schema::{self, Handling, Meeting, ProcessingStatus, Source, VoiceprintKind};
use crate::{Error, Library, MeetingId, PersonId, Result, WriteTx, fsutil, learning, ops};

/// Folder of capture journals inside the data folder.
pub const JOURNAL_DIR: &str = "journal";
/// A session journal's manifest (the capture crate writes it).
pub const SESSION_MANIFEST: &str = "session.json";
/// The session's user title sidecar, inside its journal folder.
pub const SESSION_TITLE: &str = "title.json";
/// Discard intents, inside the journal root but outside every session folder.
pub const DISCARD_DIR: &str = ".discard";
/// Deletion markers, inside the library but outside every meeting folder.
pub const DELETING_DIR: &str = ".deleting";
/// Staging folder deleted meeting folders are renamed into before removal.
pub const DELETED_DIR: &str = ".deleted";

/// Refused: the meeting belongs to a recording still capturing or not yet
/// saved; discard that session instead.
pub const RECORDING_ACTIVE: &str = "recording_active";
/// Refused: a job holding a live lease works on the meeting.
pub const JOB_RUNNING: &str = "job_running";
/// Refused: an import job's folder holds the user's only copy of a moved file.
pub const ONLY_COPY_IN_FLIGHT: &str = "only_copy_in_flight";
/// The recording session is not the active or a stopped, unsaved one.
pub const STALE_SESSION: &str = "stale_session";
/// The recording was discarded: nothing of it may be published.
pub const RECORDING_DISCARDED: &str = "recording_discarded";

/// `<data_dir>/journal`.
pub fn journal_root(data_dir: &Path) -> PathBuf {
    data_dir.join(JOURNAL_DIR)
}

/// `<journal_root>/<session_id>`; session IDs are `[A-Za-z0-9_-]+`.
pub fn session_dir(journal_root: &Path, session_id: &str) -> Result<PathBuf> {
    check_session_id(session_id)?;
    Ok(journal_root.join(session_id))
}

fn check_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::InvalidInput(format!(
            "invalid recording session id {session_id:?}"
        )));
    }
    Ok(())
}

/// The recording session a meeting was published from, if it is a recording.
pub fn recording_session(meeting: &Meeting) -> Option<&str> {
    if meeting.source != Source::Recording {
        return None;
    }
    meeting.source_id.as_deref()?.strip_prefix("recording:")
}

/// The `stale_session` error.
pub fn stale_session(session_id: &str) -> Error {
    Error::Service {
        code: STALE_SESSION,
        message: format!("recording session {session_id} is not running or waiting to be saved"),
    }
}

fn discarded(session_id: &str) -> Error {
    Error::Service {
        code: RECORDING_DISCARDED,
        message: format!("recording session {session_id} was discarded"),
    }
}

// ---- session title sidecar ----

/// The user's latest title for a recording session (`title.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTitle {
    pub title: String,
    pub set_at: DateTime<FixedOffset>,
}

/// The session's user title, `None` without one.
pub fn read_session_title(session_dir: &Path) -> Result<Option<SessionTitle>> {
    match fsutil::read_json(&session_dir.join(SESSION_TITLE)) {
        Ok(title) => Ok(Some(title)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Writes `title` unless the sidecar holds a later edit; mode 0600.
fn write_session_title(session_dir: &Path, title: &SessionTitle) -> Result<()> {
    if read_session_title(session_dir)?.is_some_and(|t| t.set_at > title.set_at) {
        return Ok(());
    }
    replace_session_title(session_dir, title)
}

fn replace_session_title(session_dir: &Path, title: &SessionTitle) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(title)
        .map_err(|e| Error::Internal(format!("serializing session title: {e}")))?;
    fsutil::write_atomic_with_mode(&session_dir.join(SESSION_TITLE), &bytes, Some(0o600))
}

// ---- discard intents ----

/// A durable request to discard a recording session; identifiers only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscardIntent {
    pub session_id: String,
    pub created_at: DateTime<FixedOffset>,
    /// Explicit deletion of a legacy journal that cannot be attributed to a library.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    journal_only: bool,
}

/// `<journal_root>/.discard/<session>.json`.
pub fn discard_intent_path(journal_root: &Path, session_id: &str) -> Result<PathBuf> {
    check_session_id(session_id)?;
    Ok(journal_root
        .join(DISCARD_DIR)
        .join(format!("{session_id}.json")))
}

/// Records the intent durably (fsynced) before anything else of a discard.
pub fn write_discard_intent(journal_root: &Path, session_id: &str) -> Result<()> {
    write_discard(journal_root, session_id, false)
}

/// Commit a discard of the journal itself, without touching any library.
pub fn write_legacy_discard_intent(journal_root: &Path, session_id: &str) -> Result<()> {
    write_discard(journal_root, session_id, true)
}

fn write_discard(journal_root: &Path, session_id: &str, journal_only: bool) -> Result<()> {
    let path = discard_intent_path(journal_root, session_id)?;
    let dir = journal_root.join(DISCARD_DIR);
    fsutil::create_dir_durable(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::io_at(&dir, e))?;
    }
    let intent = DiscardIntent {
        session_id: session_id.to_owned(),
        created_at: schema::now(),
        journal_only,
    };
    let bytes = serde_json::to_vec(&intent)
        .map_err(|e| Error::Internal(format!("serializing discard intent: {e}")))?;
    fsutil::write_atomic_with_mode(&path, &bytes, Some(0o600))
}

pub fn has_discard_intent(journal_root: &Path, session_id: &str) -> bool {
    discard_intent_path(journal_root, session_id).is_ok_and(|p| p.exists())
}

/// Whether the confirmed discard must leave library files and jobs alone.
pub fn is_legacy_discard(journal_root: &Path, session_id: &str) -> Result<bool> {
    match fsutil::read_json::<DiscardIntent>(&discard_intent_path(journal_root, session_id)?) {
        Ok(intent) if intent.session_id == session_id => Ok(intent.journal_only),
        Ok(_) => Err(Error::Schema(
            "discard intent names a different session".into(),
        )),
        Err(Error::NotFound(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Sessions with a discard intent, sorted.
pub fn discard_intents(journal_root: &Path) -> Result<Vec<String>> {
    let dir = journal_root.join(DISCARD_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(&dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
        let name = entry.file_name();
        if let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json"))
            && check_session_id(id).is_ok()
        {
            out.push(id.to_owned());
        }
    }
    out.sort();
    Ok(out)
}

fn remove_discard_intent(journal_root: &Path, session_id: &str) -> Result<()> {
    remove_file(&discard_intent_path(journal_root, session_id)?).map(|_| ())
}

/// Whether nothing of the session in `journal_dir` may be queued or
/// published: it has a discard intent, or its journal manifest is gone
/// (discarded and cleaned up, or already published and removed). Check it
/// under the library lock that also commits the queueing or publication.
pub fn publication_blocked(journal_dir: &Path, session_id: &str) -> bool {
    let intent = journal_dir
        .parent()
        .is_some_and(|root| has_discard_intent(root, session_id));
    intent || !journal_dir.join(SESSION_MANIFEST).is_file()
}

/// [`publication_blocked`] as the `recording_discarded` error.
pub fn check_publication(journal_dir: &Path, session_id: &str) -> Result<()> {
    if publication_blocked(journal_dir, session_id) {
        return Err(discarded(session_id));
    }
    Ok(())
}

/// Meetings published from recording `session_id` (provisional or final),
/// for a caller holding the library lock. Every meeting folder a record job
/// creates is named from its journal's start minute (UTC,
/// `YYYY/MM/YYYY-MM-DD-HHMM…`), so while the journal under `journal_root`
/// says when the session started only that minute's folders are read, each
/// checked against the `source_id` in its `meeting.json`; one of them that
/// cannot be read may be the session's own, so the lookup fails, as the
/// record job's publication does. Without a manifest that reads as this
/// session's (removed after publication, or damaged) every meeting is read
/// and unreadable ones are skipped.
pub fn session_meetings(
    lib: &Library,
    journal_root: &Path,
    session_id: &str,
) -> Result<Vec<Meeting>> {
    let journal = session_dir(journal_root, session_id)?;
    let source = format!("recording:{session_id}");
    let of_session = |m: &Meeting| {
        m.source == Source::Recording && m.source_id.as_deref() == Some(source.as_str())
    };
    let mut out = Vec::new();
    let Some(started) = journal_start(&journal, session_id) else {
        for dir in lib.list_meeting_dirs()? {
            match lib.load_meeting_at(&dir) {
                Ok(m) if of_session(&m) => out.push(m),
                Ok(_) => {}
                Err(e) => tracing::warn!(code = e.code(), "skipping an unreadable meeting"),
            }
        }
        return Ok(out);
    };
    let month = lib
        .root()
        .join(started.format("%Y").to_string())
        .join(started.format("%m").to_string());
    let minute = start_minute(&started);
    for name in file_names(&month)? {
        let dir = month.join(&name);
        if !name.starts_with(&minute) || !dir.join(crate::paths::MEETING_JSON).is_file() {
            continue;
        }
        let meeting = lib.load_meeting_at(&dir)?;
        if of_session(&meeting) {
            out.push(meeting);
        }
    }
    Ok(out)
}

/// When the journal in `dir` of session `session_id` started, from its
/// manifest; `None` when it is missing or does not read as this session's.
fn journal_start(dir: &Path, session_id: &str) -> Option<DateTime<chrono::Utc>> {
    fs::read(dir.join(SESSION_MANIFEST))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ManifestStart>(&bytes).ok())
        .filter(|m| m.session_id == session_id)
        .map(|m| m.started_at)
}

/// The start minute meeting folder names begin with (`%Y-%m-%d-%H%M`).
fn start_minute(started: &DateTime<chrono::Utc>) -> String {
    started.format("%Y-%m-%d-%H%M").to_string()
}

// ---- titles ----

/// What [`set_meeting_title`] did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TitleOutcome {
    pub meeting_id: MeetingId,
    pub title: String,
    /// The recording session whose title sidecar was updated too (a
    /// provisional meeting of a recording not yet saved): tell the daemon
    /// (`record.title`) so its panel follows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Renames a meeting as the user's explicit choice ([`ops::set_title`]).
/// For the provisional meeting of a recording not yet saved, the session's
/// title sidecar is updated in the same write, so the final publication
/// keeps this title unless a later edit replaces it.
pub fn set_meeting_title(
    lib: &Library,
    journal_root: &Path,
    id: &MeetingId,
    title: &str,
) -> Result<TitleOutcome> {
    set_meeting_title_checked(lib, journal_root, id, title, || Ok(()))
}

/// [`set_meeting_title`] with cancellation/admission checked inside its
/// canonical write transaction, including the recording's title sidecar.
pub fn set_meeting_title_checked(
    lib: &Library,
    journal_root: &Path,
    id: &MeetingId,
    title: &str,
    check: impl FnOnce() -> Result<()>,
) -> Result<TitleOutcome> {
    let title = ops::validate_title(title)?;
    ops::with_index_checked(lib, check, |tx, index| {
        let meeting = ops::set_title(tx, index, id, &title)?;
        let mut session_id = None;
        if meeting.processing.status != ProcessingStatus::Done
            && let Some(session) = recording_session(&meeting)
        {
            let dir = session_dir(journal_root, session)?;
            if !publication_blocked(&dir, session) {
                write_session_title(
                    &dir,
                    &SessionTitle {
                        title: title.clone(),
                        set_at: meeting.title_set_at.unwrap_or_else(schema::now),
                    },
                )?;
                session_id = Some(session.to_owned());
            }
        }
        Ok(TitleOutcome {
            meeting_id: meeting.id,
            title,
            session_id,
        })
    })
}

/// The canonical title fields, before display fallbacks or a new user edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingTitle {
    pub text: Option<String>,
    pub source: Option<schema::TitleSource>,
    pub set_at: Option<DateTime<FixedOffset>>,
}

impl From<&Meeting> for MeetingTitle {
    fn from(meeting: &Meeting) -> Self {
        Self {
            text: meeting.title.clone(),
            source: meeting.title_source,
            set_at: meeting.title_set_at,
        }
    }
}

/// Replays a title edit, preserving the original provenance and timestamp.
/// A provisional recording's user-title sidecar must match before the
/// library lock is released, so final publication sees the restored title.
pub fn restore_meeting_title_checked(
    lib: &Library,
    journal_root: &Path,
    id: &MeetingId,
    title: &MeetingTitle,
    check: impl FnOnce() -> Result<()>,
) -> Result<MeetingTitle> {
    ops::with_index_checked(lib, check, |tx, index| {
        let mut meeting = tx.library().load_meeting(id)?;
        if meeting.processing.status != ProcessingStatus::Done
            && let Some(session) = recording_session(&meeting)
        {
            let dir = session_dir(journal_root, session)?;
            if !publication_blocked(&dir, session) {
                if let (Some(schema::TitleSource::User), Some(text), Some(set_at)) =
                    (title.source, &title.text, title.set_at)
                {
                    // The fence already checked the latest title under the
                    // lock. An Undo can intentionally restore an older edit.
                    replace_session_title(
                        &dir,
                        &SessionTitle {
                            title: text.clone(),
                            set_at,
                        },
                    )?;
                } else if remove_file(&dir.join(SESSION_TITLE))? {
                    fsutil::sync_dir(&dir)?;
                }
            }
        }
        meeting.title.clone_from(&title.text);
        meeting.title_source = title.source;
        meeting.title_set_at = title.set_at;
        meeting.updated_at = schema::now();
        tx.save_meeting(&meeting)?;
        index.upsert_meeting(tx, &meeting)?;
        Ok(MeetingTitle::from(&meeting))
    })
}

/// What [`set_session_title`] did.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SessionTitleOutcome {
    /// The sidecar holds the title; `meeting_id` is the provisional meeting
    /// renamed with it, if any.
    Saved {
        session_id: String,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        meeting_id: Option<MeetingId>,
    },
    /// The recording was saved already: rename the meeting instead.
    AlreadyPublished { meeting_id: MeetingId },
}

/// Sets the user title of a recording session that is capturing or stopped
/// but not yet saved, under the library lock: the sidecar and any
/// provisional meeting of the session. `verify` runs under the lock before
/// anything changes and must refuse a session the caller does not own (the
/// daemon checks it is the active or a stopped session). A session already
/// published answers `already_published`; a discarded or unknown one is
/// `stale_session`.
pub fn set_session_title(
    lib: &Library,
    journal_root: &Path,
    session_id: &str,
    title: &str,
    verify: impl FnOnce() -> Result<()>,
) -> Result<SessionTitleOutcome> {
    let title = ops::validate_title(title)?;
    let dir = session_dir(journal_root, session_id)?;
    ops::with_index(lib, |tx, index| {
        let meetings = session_meetings(tx.library(), journal_root, session_id)?;
        if let Some(saved) = meetings
            .iter()
            .find(|m| m.processing.status == ProcessingStatus::Done)
        {
            return Ok(SessionTitleOutcome::AlreadyPublished {
                meeting_id: saved.id.clone(),
            });
        }
        if publication_blocked(&dir, session_id) {
            return Err(stale_session(session_id));
        }
        verify()?;
        let at = schema::now();
        write_session_title(
            &dir,
            &SessionTitle {
                title: title.clone(),
                set_at: at,
            },
        )?;
        let mut renamed = None;
        for mut meeting in meetings {
            if meeting.offer_user_title(&title, at) {
                meeting.updated_at = schema::now();
                tx.save_meeting(&meeting)?;
                index.upsert_meeting(tx, &meeting)?;
            }
            renamed = Some(meeting.id);
        }
        Ok(SessionTitleOutcome::Saved {
            session_id: session_id.to_owned(),
            title,
            meeting_id: renamed,
        })
    })
}

// ---- tags ----

/// Applies a recording session's tag changes, already journaled, to the
/// meetings a "Process now" published for it (never a saved final one),
/// under the library lock: additions clear an earlier removal, removals are
/// recorded in `tags_removed`, so the final pass, which keeps the
/// provisional meeting's tags and removals, cannot bring a removed tag back.
/// Returns the meetings changed. The meetings are found as
/// [`session_meetings`] finds them, from the journal under `journal_root`.
pub fn apply_session_tags(
    lib: &Library,
    journal_root: &Path,
    session_id: &str,
    add: &[String],
    remove: &[String],
) -> Result<Vec<MeetingId>> {
    if add.is_empty() && remove.is_empty() {
        return Ok(Vec::new());
    }
    check_session_id(session_id)?;
    ops::with_index(lib, |tx, index| {
        let mut changed = Vec::new();
        for mut meeting in session_meetings(tx.library(), journal_root, session_id)? {
            if meeting.processing.status == ProcessingStatus::Done {
                continue;
            }
            let mut touched = false;
            for tag in add {
                touched |= meeting.add_tag(tag);
            }
            for tag in remove {
                touched |= meeting.remove_tag(tag);
            }
            if touched {
                meeting.updated_at = schema::now();
                tx.save_meeting(&meeting)?;
                index.upsert_meeting(tx, &meeting)?;
                changed.push(meeting.id);
            }
        }
        Ok(changed)
    })
}

// ---- deletion ----

/// Where deletion finds the journals and caches it may have to clean.
#[derive(Clone, Debug)]
pub struct DeleteOptions {
    /// `<data_dir>/journal` ([`journal_root`]).
    pub journal_root: PathBuf,
    /// The application cache directory (playback caches); `None`: none.
    pub cache_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteState {
    /// `--dry-run`: what deleting would do; nothing changed.
    Planned,
    /// Deleted, files included.
    Deleted,
    /// The meeting is gone from the library; some files are still being
    /// removed (`cleanup_failures`), retried by maintenance.
    CleanupPending,
}

/// What deleting a meeting removes (or, `planned`, would remove). Person
/// profiles, their other samples and enrollments, referenced originals and
/// separately exported files always stay.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeleteReport {
    pub meeting_id: MeetingId,
    pub dry_run: bool,
    pub state: DeleteState,
    /// Audio the library owns, relative to the library folder.
    pub owned_audio: Vec<String>,
    /// Original files the meeting only referenced: never removed.
    pub referenced_originals_kept: Vec<String>,
    /// The owned audio was moved into the library: it is the user's only copy.
    pub moved_original: bool,
    /// Voice samples learned from this meeting (people keep the rest).
    pub voice_samples: usize,
    /// People who lose samples (their profiles stay).
    pub people_affected: usize,
    pub playback_caches: usize,
    /// Queued, failed or stale jobs of the meeting cancelled.
    pub jobs_cancelled: usize,
    /// What could not be removed yet (no names or titles).
    pub cleanup_failures: Vec<String>,
}

/// `.deleting/<meeting>.json`: identifiers only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct DeletionMarker {
    meeting_id: MeetingId,
    #[serde(default)]
    session_id: Option<String>,
    /// Remove the session journal too, and the discard intent this deletion
    /// wrote to fence it (a saved recording whose journal remained).
    #[serde(default)]
    owns_journal: bool,
    #[serde(default)]
    recording_sha256: Vec<String>,
    /// Revoked jobs whose folders go once no process holds them.
    #[serde(default)]
    revoked: Vec<JobId>,
    /// Finished jobs whose staged `audio.*` copies go.
    #[serde(default)]
    finished: Vec<JobId>,
    #[serde(default)]
    cache_dir: Option<PathBuf>,
    journal_root: PathBuf,
    created_at: DateTime<FixedOffset>,
}

/// Everything deletion decided before changing anything.
struct Plan {
    report: DeleteReport,
    dir: PathBuf,
    revoke: Vec<JobId>,
    finished: Vec<JobId>,
    /// Voiceprint IDs to remove, per person.
    samples: Vec<(PersonId, BTreeSet<String>)>,
    models: BTreeSet<String>,
    session_id: Option<String>,
    owns_journal: bool,
    shas: Vec<String>,
}

/// How strictly a plan refuses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode<'a> {
    /// A user's delete: refuse unsaved recordings, live jobs, only copies.
    User,
    /// Cleaning up discarded session `0`: its record jobs are revoked even
    /// while held; other live jobs still refuse.
    Discard(&'a str),
    /// Finishing a committed deletion: refuse nothing, revoke everything.
    Resume,
}

fn refuse(code: &'static str, message: String) -> Error {
    Error::Service { code, message }
}

fn plan(
    lib: &Library,
    probe: &dyn ProcessProbe,
    opts: &DeleteOptions,
    id: &MeetingId,
    mode: Mode<'_>,
) -> Result<Plan> {
    let dir = lib.meeting_dir(id)?;
    let meeting = lib.load_meeting_at(&dir)?;
    let session_id = recording_session(&meeting).map(str::to_owned);
    let mut owns_journal = false;
    if let Some(session) = &session_id {
        let journal = session_dir(&opts.journal_root, session)?;
        if journal.join(SESSION_MANIFEST).is_file() {
            let discarding = mode == Mode::Discard(session.as_str());
            if meeting.processing.status == ProcessingStatus::Done {
                owns_journal = !discarding;
            } else if mode == Mode::User {
                return Err(refuse(
                    RECORDING_ACTIVE,
                    format!(
                        "meeting {id} belongs to recording {session}, which is still recording or \
                         not saved yet; discard that recording instead"
                    ),
                ));
            }
        }
    }
    let mut revoke = Vec::new();
    let mut finished = Vec::new();
    if lib.root().join(crate::paths::QUEUE_DIR).is_dir() {
        let queue = Queue::open(lib.root())?;
        let mut jobs = queue.jobs()?;
        jobs.sort_by(|a, b| a.id.cmp(&b.id));
        for job in jobs {
            let of_session = session_id
                .as_deref()
                .is_some_and(|s| job.input.get("session_id").and_then(|v| v.as_str()) == Some(s));
            if !of_session && !job_refers_to(&job, &meeting) {
                continue;
            }
            if mode != Mode::Resume && queue.holds_only_copy(&job) {
                return Err(refuse(
                    ONLY_COPY_IN_FLIGHT,
                    format!(
                        "an import job ({}) holds the only copy of this meeting's moved original; \
                         let it finish or move the file out of the library first",
                        job.id
                    ),
                ));
            }
            if job.state == JobState::Done && !queue.is_held(&job, probe) {
                finished.push(job.id);
                continue;
            }
            if already_revoked(&job) {
                revoke.push(job.id);
                continue;
            }
            let forced = mode == Mode::Resume
                || matches!(mode, Mode::Discard(s) if of_session && session_id.as_deref() == Some(s));
            if !forced && (queue.is_held(&job, probe) || job.state == JobState::Analyzed) {
                return Err(refuse(
                    JOB_RUNNING,
                    format!(
                        "a {} job is working on meeting {id}; try again when it ends",
                        job.kind
                    ),
                ));
            }
            revoke.push(job.id);
        }
    }
    let (samples, models) = meeting_samples(lib, id)?;
    let voice_samples = samples.iter().map(|(_, ids)| ids.len()).sum();
    let rel = |p: &Path| {
        p.strip_prefix(lib.root())
            .unwrap_or(p)
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    };
    let mut owned_audio = Vec::new();
    let mut referenced = Vec::new();
    let mut moved_original = false;
    let mut shas = Vec::new();
    if let Some(rec) = &meeting.recording {
        shas.extend(
            std::iter::once(rec.sha256.clone())
                .chain(rec.source_sha256.clone())
                .filter(|s| !s.is_empty()),
        );
        if rec.handling == Handling::Reference {
            referenced.push(rec.path.display().to_string());
        } else if !rec.path.as_os_str().is_empty()
            && rec
                .path
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            let path = dir.join(&rec.path);
            if path.exists() {
                owned_audio.push(rel(&path));
            }
            moved_original = rec.handling == Handling::Move;
        }
        referenced.extend(rec.sources.iter().map(|s| s.path.display().to_string()));
    }
    let playback_caches = opts
        .cache_dir
        .as_deref()
        .map_or(0, |c| playback_caches(c, &shas).len());
    Ok(Plan {
        report: DeleteReport {
            meeting_id: meeting.id.clone(),
            dry_run: true,
            state: DeleteState::Planned,
            owned_audio,
            referenced_originals_kept: referenced,
            moved_original,
            voice_samples,
            people_affected: samples.len(),
            playback_caches,
            jobs_cancelled: revoke.len(),
            cleanup_failures: Vec::new(),
        },
        dir,
        revoke,
        finished,
        samples,
        models,
        session_id,
        owns_journal,
        shas,
    })
}

fn already_revoked(job: &Job) -> bool {
    job.state == JobState::Failed && job.error.as_deref() == Some(REVOKED)
}

/// `people/<id>/` folders (with or without `person.md`), sorted.
fn person_ids(lib: &Library) -> Result<Vec<PersonId>> {
    let dir = lib.people_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(&dir, e)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| PersonId::parse(n).ok())
        {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}

fn playback_caches(cache_dir: &Path, shas: &[String]) -> Vec<PathBuf> {
    let dir = cache_dir.join(PLAYBACK_CACHE_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| shas.iter().any(|sha| is_playback_cache_of(n, sha)))
        })
        .map(|e| e.path())
        .collect()
}

fn marker_path(root: &Path, id: &MeetingId) -> PathBuf {
    root.join(DELETING_DIR).join(format!("{id}.json"))
}

fn staged_path(root: &Path, id: &MeetingId) -> PathBuf {
    root.join(DELETED_DIR).join(id.as_str())
}

/// Whether a deletion of meeting `id` is committed and not finished. The
/// deletion itself is fenced by the library lock, job revocation and the
/// folder rename; this keeps the ID reserved until the marker goes, so no
/// new meeting is given an ID whose pending cleanup would remove it
/// (`Library::new_meeting_id`), and lets [`delete_meeting`] resume it.
pub fn meeting_deleted(lib: &Library, id: &MeetingId) -> bool {
    marker_path(lib.root(), id).exists()
}

/// The committed deletion marker of `id`, if any. An unreadable marker is an
/// error, never a reason to start over: [`diagnose`] reports it.
fn read_marker(root: &Path, id: &MeetingId) -> Result<Option<DeletionMarker>> {
    match fsutil::read_json(&marker_path(root, id)) {
        Ok(marker) => Ok(Some(marker)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Where a committed deletion found its journals and caches.
fn marker_opts(marker: &DeletionMarker) -> DeleteOptions {
    DeleteOptions {
        journal_root: marker.journal_root.clone(),
        cache_dir: marker.cache_dir.clone(),
    }
}

/// What is left of a committed deletion whose folder already moved:
/// identifiers and counts the marker still knows.
fn marker_report(marker: &DeletionMarker) -> DeleteReport {
    DeleteReport {
        meeting_id: marker.meeting_id.clone(),
        dry_run: false,
        state: DeleteState::Deleted,
        owned_audio: Vec::new(),
        referenced_originals_kept: Vec::new(),
        moved_original: false,
        voice_samples: 0,
        people_affected: 0,
        playback_caches: marker
            .cache_dir
            .as_deref()
            .map_or(0, |c| playback_caches(c, &marker.recording_sha256).len()),
        jobs_cancelled: marker.revoked.len(),
        cleanup_failures: Vec::new(),
    }
}

/// What deleting `id` would remove, without changing anything (no lock).
/// Refuses as [`delete_meeting`] would. A deletion already committed is
/// never refused: the plan shows what finishing it still does.
pub fn delete_plan(
    lib: &Library,
    probe: &dyn ProcessProbe,
    opts: &DeleteOptions,
    id: &MeetingId,
) -> Result<DeleteReport> {
    let Some(marker) = read_marker(lib.root(), id)? else {
        return Ok(plan(lib, probe, opts, id, Mode::User)?.report);
    };
    if !lib.meeting_exists(id)? {
        let mut report = marker_report(&marker);
        report.dry_run = true;
        report.state = DeleteState::Planned;
        return Ok(report);
    }
    let plan = plan(lib, probe, &marker_opts(&marker), id, Mode::Resume)?;
    let mut report = plan.report;
    report.jobs_cancelled = marker
        .revoked
        .iter()
        .chain(plan.revoke.iter().filter(|j| !marker.revoked.contains(j)))
        .count();
    Ok(report)
}

/// What [`delete_meeting`]'s locked section found.
enum Admitted {
    /// A new deletion, committed now.
    New(DeleteReport, DeletionMarker),
    /// A deletion committed earlier and not finished.
    Pending(DeletionMarker),
}

/// Permanently deletes meeting `id`: its folder (transcript, generated
/// outputs, owned audio), index rows and unreferenced text vectors, voice
/// samples learned from it (requesting the identity recheck first),
/// playback caches and staged copies, and its queued, failed or stale jobs.
/// Person profiles, other samples, referenced originals and exports stay.
/// Refused before any change: a recording not yet saved
/// ([`RECORDING_ACTIVE`]), a live job ([`JOB_RUNNING`]), a move holding the
/// only copy ([`ONLY_COPY_IN_FLIGHT`]). A deletion of `id` already
/// committed (its marker remains) is finished instead, even when its folder
/// was already renamed away; it is reported `cleanup_pending` until every
/// step is done.
pub fn delete_meeting(
    lib: &Library,
    probe: &dyn ProcessProbe,
    opts: &DeleteOptions,
    id: &MeetingId,
) -> Result<DeleteReport> {
    let admitted = ops::with_index(lib, |tx, index| {
        if let Some(marker) = read_marker(tx.root(), id)? {
            return Ok(Admitted::Pending(marker));
        }
        let plan = plan(tx.library(), probe, opts, id, Mode::User)?;
        let (report, marker) = commit(tx, index, probe, opts, plan, true)?;
        Ok(Admitted::New(report, marker))
    })?;
    match admitted {
        Admitted::New(mut report, marker) => {
            finish(lib, probe, &marker, &mut report);
            Ok(report)
        }
        Admitted::Pending(marker) => Ok(resume_reported(lib, probe, &marker)),
    }
}

/// The locked commit: marker first, then fences, evidence, index and the
/// atomic rename. A user's delete (`refuse_held`) re-checks, under the queue
/// lock that revokes them, that none of the meeting's jobs was claimed since
/// the plan: a claimed one refuses the delete and removes the marker before
/// anything else changed, the identity recheck included (it is requested
/// only once the delete is admitted, still before the evidence changes).
/// Steps after that which fail leave the marker for recovery and are
/// reported, not undone.
fn commit(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    probe: &dyn ProcessProbe,
    opts: &DeleteOptions,
    plan: Plan,
    refuse_held: bool,
) -> Result<(DeleteReport, DeletionMarker)> {
    let Plan {
        mut report,
        dir,
        revoke,
        finished,
        samples,
        models,
        session_id,
        owns_journal,
        shas,
    } = plan;
    report.dry_run = false;
    report.state = DeleteState::Deleted;
    let marker = DeletionMarker {
        meeting_id: report.meeting_id.clone(),
        session_id,
        owns_journal,
        recording_sha256: shas,
        revoked: revoke,
        finished,
        cache_dir: opts.cache_dir.clone(),
        journal_root: opts.journal_root.clone(),
        created_at: schema::now(),
    };
    let path = marker_path(tx.root(), &marker.meeting_id);
    if let Some(parent) = path.parent() {
        fsutil::create_dir_durable(parent)?;
    }
    fsutil::write_json_atomic(&path, &marker)?;
    if refuse_held {
        let queue = Queue::open(tx.root())?;
        if let Some(held) = queue.revoke_unless_held(tx, &marker.revoked, probe)? {
            remove_file(&path)?;
            return Err(refuse(
                JOB_RUNNING,
                format!(
                    "a job ({held}) started working on meeting {}; try again when it ends",
                    marker.meeting_id
                ),
            ));
        }
    }
    // Requested before the evidence changes, as every sample-changing write
    // does; never for a delete refused above.
    let requested = if models.is_empty() {
        Ok(())
    } else {
        learning::request_all(tx, models).map(|_| ())
    };
    if let Err(e) = requested.and_then(|()| apply(tx, index, probe, &marker, &dir, &samples)) {
        report.state = DeleteState::CleanupPending;
        report.cleanup_failures.push(e.code().to_owned());
    }
    Ok((report, marker))
}

type LearnedSamples = Vec<(PersonId, BTreeSet<String>)>;

/// Voice samples learned from meeting `id` (enrollments excluded), per
/// person, and the embedding models they use.
fn meeting_samples(lib: &Library, id: &MeetingId) -> Result<(LearnedSamples, BTreeSet<String>)> {
    let mut samples = Vec::new();
    let mut models = BTreeSet::new();
    for person in person_ids(lib)? {
        let prints = lib.load_voiceprints(&person)?;
        let ids: BTreeSet<String> = prints
            .embeddings
            .iter()
            .filter(|e| {
                e.source.kind != VoiceprintKind::Enrollment
                    && e.source.meeting_id.as_ref() == Some(id)
            })
            .map(|e| {
                models.insert(e.model.clone());
                e.id.clone()
            })
            .collect();
        if !ids.is_empty() {
            samples.push((person, ids));
        }
    }
    Ok((samples, models))
}

fn remove_samples(tx: &mut WriteTx<'_>, samples: &[(PersonId, BTreeSet<String>)]) -> Result<()> {
    for (person, ids) in samples {
        let mut prints = tx.library().load_voiceprints(person)?;
        let before = prints.embeddings.len();
        prints.embeddings.retain(|e| !ids.contains(&e.id));
        if prints.embeddings.len() != before {
            tx.save_voiceprints(&prints)?;
        }
    }
    Ok(())
}

/// The changes after the marker; idempotent (recovery runs them again).
fn apply(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    probe: &dyn ProcessProbe,
    marker: &DeletionMarker,
    dir: &Path,
    samples: &[(PersonId, BTreeSet<String>)],
) -> Result<()> {
    if marker.owns_journal
        && let Some(session) = &marker.session_id
    {
        write_discard_intent(&marker.journal_root, session)?;
    }
    let queue = Queue::open(tx.root())?;
    for id in &marker.revoked {
        queue.revoke(tx, id, probe)?;
    }
    remove_samples(tx, samples)?;
    index.remove_meeting(tx, &marker.meeting_id)?;
    let staged = staged_path(tx.root(), &marker.meeting_id);
    if let Some(parent) = staged.parent() {
        fsutil::create_dir_durable(parent)?;
    }
    if staged.exists() {
        fs::remove_dir_all(&staged).map_err(|e| Error::io_at(&staged, e))?;
    }
    fs::rename(dir, &staged).map_err(|e| Error::io_at(dir, e))?;
    // Last: the meeting is already gone from listings and the index.
    index.prune_vectors(tx)?;
    Ok(())
}

/// Physical cleanup after the commit; the marker goes last, only when
/// everything else is gone.
fn finish(
    lib: &Library,
    probe: &dyn ProcessProbe,
    marker: &DeletionMarker,
    report: &mut DeleteReport,
) {
    // A commit whose changes did not all apply keeps its marker for recovery.
    let applied = report.state != DeleteState::CleanupPending;
    let failures = cleanup(lib, probe, marker, applied);
    if !failures.is_empty() || !applied {
        report.state = DeleteState::CleanupPending;
        report.cleanup_failures.extend(failures);
    }
}

/// Physical cleanup; removes the marker last, and only when `applied` (the
/// locked changes all went through) and nothing else is left.
fn cleanup(
    lib: &Library,
    probe: &dyn ProcessProbe,
    marker: &DeletionMarker,
    applied: bool,
) -> Vec<String> {
    let mut failures = Vec::new();
    let root = lib.root();
    if lib.meeting_exists(&marker.meeting_id).unwrap_or(true) {
        failures.push("meeting folder not moved yet".to_owned());
        return failures;
    }
    let staged = staged_path(root, &marker.meeting_id);
    match fs::remove_dir_all(&staged) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => failures.push(format!("staging folder: {}", e.kind())),
    }
    if let Some(cache) = &marker.cache_dir {
        for path in playback_caches(cache, &marker.recording_sha256) {
            let removed = if path.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            if let Err(e) = removed
                && e.kind() != ErrorKind::NotFound
            {
                failures.push(format!("playback cache: {}", e.kind()));
            }
        }
    }
    if let Ok(queue) = Queue::open(root) {
        for id in &marker.finished {
            remove_staged_audio(&queue, probe, id, &mut failures);
        }
        for id in &marker.revoked {
            failures.extend(remove_revoked_job(lib, &queue, probe, id));
        }
    }
    if marker.owns_journal
        && let Some(session) = &marker.session_id
    {
        let journal_root = &marker.journal_root;
        // A saved recording's journal is stopped; anything else is left alone.
        match session_dir(journal_root, session) {
            Ok(dir) => match journal_capture(&dir, session) {
                JournalCapture::Missing | JournalCapture::Stopped => {
                    if let Err(e) = remove_tree(&dir) {
                        failures.push(format!("recording journal: {}", e.code()));
                    }
                }
                other => failures.push(format!("recording journal left in place: {other:?}")),
            },
            Err(e) => failures.push(format!("recording journal: {}", e.code())),
        }
        if failures.is_empty()
            && let Err(e) = remove_discard_intent(journal_root, session)
        {
            failures.push(format!("discard intent: {}", e.code()));
        }
    }
    if failures.is_empty()
        && applied
        && let Err(e) = remove_file(&marker_path(root, &marker.meeting_id))
    {
        failures.push(format!("deletion marker: {}", e.code()));
    }
    failures
}

/// Removes revoked job `id`'s folder; what is left, if anything. The queue
/// never removes a folder holding a moved original (`already_exists`).
fn remove_revoked_job(
    lib: &Library,
    queue: &Queue,
    probe: &dyn ProcessProbe,
    id: &JobId,
) -> Option<String> {
    match lib.write(|tx| queue.remove_revoked(tx, id, probe)) {
        Ok(true) => None,
        Ok(false) => Some(format!("job {id} is still running")),
        Err(Error::AlreadyExists(_)) => {
            Some(format!("job {id} keeps the only copy of a moved file"))
        }
        Err(e) => Some(format!("job {id}: {}", e.code())),
    }
}

/// Removes staged `audio.*` copies from a finished job's folder (retention
/// does the same for deleted recordings), unless a process holds the job.
fn remove_staged_audio(
    queue: &Queue,
    probe: &dyn ProcessProbe,
    id: &JobId,
    failures: &mut Vec<String>,
) {
    let Ok(job) = queue.get(id) else {
        return;
    };
    if queue.is_held(&job, probe) || queue.holds_only_copy(&job) {
        return;
    }
    let dir = queue.job_dir(id);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let staged = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("audio."));
        if staged
            && entry.path().is_file()
            && let Err(e) = remove_file(&entry.path())
        {
            failures.push(format!("staged copy: {}", e.code()));
        }
    }
}

/// How old an atomic-write temporary of a dead writer must be before it
/// counts as orphaned and maintenance removes it (a write on a shared
/// folder may be another machine's, in flight).
const ORPHAN_TEMP_MIN_AGE: Duration = Duration::from_secs(10 * 60);

/// The folders written atomically besides [`Library::temp_dirs`] and
/// [`Queue::temp_dirs`]: the deletion markers and the discard intents
/// (under the library lock), and the user's templates
/// (`<data_dir>/templates`, [`crate::templates::save_user`], written under
/// no lock: only the writer-alive and age rule keeps their writes in
/// flight).
fn lifecycle_temp_dirs(root: &Path, data_dir: &Path) -> [PathBuf; 3] {
    [
        root.join(DELETING_DIR),
        journal_root(data_dir).join(DISCARD_DIR),
        crate::templates::dir(data_dir),
    ]
}

/// Removes the atomic-write temporaries ([`fsutil::orphan_temps`]: a writer
/// no longer alive, at least ten minutes old) everywhere the library and
/// `data_dir` are written atomically: [`Library::temp_dirs`] and the
/// lifecycle's own folders (the deletion markers, the discard intents under
/// `data_dir`'s journal and `data_dir`'s templates) under the library lock,
/// and [`Queue::temp_dirs`] under the queue lock too. Writes in flight (a
/// live writer, this process, a recent file) and every other file stay.
/// Returns how many were removed. For the agent's periodic maintenance and
/// `library repair`.
pub fn sweep_orphan_temps(
    lib: &Library,
    data_dir: &Path,
    probe: &dyn ProcessProbe,
) -> Result<usize> {
    lib.write(|tx| {
        let mut removed = tx.sweep_orphan_temps(probe, ORPHAN_TEMP_MIN_AGE)?;
        for dir in lifecycle_temp_dirs(tx.root(), data_dir) {
            removed += fsutil::sweep_orphan_temps(&dir, probe, ORPHAN_TEMP_MIN_AGE)?;
        }
        if let Some(queue) = Queue::open_existing(tx.root())? {
            removed += queue.sweep_orphan_temps(tx, probe, ORPHAN_TEMP_MIN_AGE)?;
        }
        Ok(removed)
    })
}

/// Completes every deletion whose marker remains (a crash or failure after
/// the commit). Returns how many finished now; ones still pending stay for
/// the next run. Atomic-write temporaries a dead writer left among the
/// markers go too (only that exact pattern; nothing else is touched).
pub fn finish_pending_deletions(lib: &Library, probe: &dyn ProcessProbe) -> Result<usize> {
    let dir = lib.root().join(DELETING_DIR);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::io_at(&dir, e)),
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    if let Err(e) = lib.write(|_| fsutil::sweep_orphan_temps(&dir, probe, ORPHAN_TEMP_MIN_AGE)) {
        tracing::warn!(
            code = e.code(),
            "removing orphaned deletion-marker temporaries"
        );
    }
    let mut done = 0;
    for path in paths {
        let marker: DeletionMarker = match fsutil::read_json(&path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(code = e.code(), "unreadable meeting deletion marker");
                continue;
            }
        };
        match resume(lib, probe, &marker) {
            Ok(report) if report.state == DeleteState::Deleted => done += 1,
            Ok(_) => {}
            Err(e) => tracing::warn!(code = e.code(), "finishing a meeting deletion"),
        }
    }
    Ok(done)
}

/// [`resume`] for a caller that asked to delete: a committed deletion whose
/// locked steps fail again is still committed, so it reads
/// `cleanup_pending` with the failure's code rather than an error.
fn resume_reported(
    lib: &Library,
    probe: &dyn ProcessProbe,
    marker: &DeletionMarker,
) -> DeleteReport {
    resume(lib, probe, marker).unwrap_or_else(|e| {
        let mut report = marker_report(marker);
        report.state = DeleteState::CleanupPending;
        report.cleanup_failures.push(e.code().to_owned());
        report
    })
}

/// Redoes the commit when the folder is still in place, then cleans up.
/// `deleted` once everything, the marker last, is gone; `cleanup_pending`
/// with what is left otherwise.
fn resume(
    lib: &Library,
    probe: &dyn ProcessProbe,
    marker: &DeletionMarker,
) -> Result<DeleteReport> {
    let opts = marker_opts(marker);
    let (mut report, marker) = ops::with_index(lib, |tx, index| {
        if !tx.library().meeting_exists(&marker.meeting_id)? {
            // The folder already moved (a crash or failure after the
            // rename): the index rows and unreferenced text vectors may
            // still be there; removing them again is harmless.
            index.remove_meeting(tx, &marker.meeting_id)?;
            index.prune_vectors(tx)?;
            return Ok((marker_report(marker), marker.clone()));
        }
        let plan = plan(tx.library(), probe, &opts, &marker.meeting_id, Mode::Resume)?;
        if !plan.models.is_empty() {
            learning::request_all(tx, plan.models.clone())?;
        }
        let mut redo = marker.clone();
        for id in plan.revoke {
            if !redo.revoked.contains(&id) {
                redo.revoked.push(id);
            }
        }
        for id in plan.finished {
            if !redo.finished.contains(&id) {
                redo.finished.push(id);
            }
        }
        fsutil::write_json_atomic(&marker_path(tx.root(), &redo.meeting_id), &redo)?;
        apply(tx, index, probe, &redo, &plan.dir, &plan.samples)?;
        let mut report = plan.report;
        report.dry_run = false;
        report.state = DeleteState::Deleted;
        report.jobs_cancelled = redo.revoked.len();
        Ok((report, redo))
    })?;
    let failures = cleanup(lib, probe, &marker, true);
    if !failures.is_empty() {
        report.state = DeleteState::CleanupPending;
        report.cleanup_failures = failures;
    }
    Ok(report)
}

// ---- discard ----

/// What [`discard_session`] did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DiscardReport {
    pub session_id: String,
    /// Provisional meetings of the session deleted.
    pub meetings_deleted: usize,
    /// Record jobs of the session revoked.
    pub jobs_cancelled: usize,
    /// Folders an unpublished attempt left (and the samples learned under
    /// their meeting ID) removed.
    pub unpublished_removed: usize,
    /// Everything is gone and the intent removed.
    pub complete: bool,
    /// Set when the session had been saved before the discard: it stays.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_meeting: Option<MeetingId>,
    pub failures: Vec<String>,
}

/// Who may still be capturing into a session's journal, as the caller knows
/// it; [`discard_session`] erases a journal only when no capture can still
/// write it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureOwnership {
    /// No capture of this data folder runs in any process: the agent's
    /// startup, under its single-instance lock and before it admits a
    /// capture. A journal still marked `recording` is a crash's orphan
    /// whatever PID it names (PIDs are reused).
    NoneRunning,
    /// The running agent; `live` when it still captures this session or is
    /// still stopping it.
    Agent { live: bool },
}

/// A session journal's capture state, read from its manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalCapture {
    /// No manifest (never written, or already removed).
    Missing,
    /// Capture closed the journal.
    Stopped,
    /// Marked `recording` by process `owner_pid`.
    Recording { owner_pid: u32 },
    /// The manifest does not read as this session's: never treated as
    /// permission to erase.
    Unreadable,
}

/// The few manifest fields lifecycle decisions need (the capture crate owns
/// the full format).
#[derive(Deserialize)]
struct ManifestOwner {
    session_id: String,
    state: String,
    owner_pid: u32,
}

/// When the journal's session started (UTC), as the record job names the
/// meeting folder it creates.
#[derive(Deserialize)]
struct ManifestStart {
    session_id: String,
    started_at: DateTime<chrono::Utc>,
}

/// File in a record job's folder naming the meeting it publishes; the
/// pipeline's record job writes it before anything moves into the library.
pub const RECORD_MEETING_ID_FILE: &str = "meeting-id";

/// Files a record job places in a meeting folder before `meeting.json`.
fn is_record_output(name: &str) -> bool {
    name.starts_with("audio.") || name == "roster.json" || name == "captions.jsonl"
}

/// Removes what record jobs of `session_id` left from attempts that failed
/// before publishing: voice samples learned under the meeting ID their job
/// folder names (the identity recheck requested first) and that meeting's
/// folder without `meeting.json`. Ownership is proven by the job (its input
/// names the session), the exact meeting ID it recorded, a folder name that
/// ends in that ID and (when the journal still says) starts with the
/// session's start minute, and no published meeting of that ID. Only the
/// files a record job places there are removed; anything else stays (and
/// keeps the folder). Returns how many folders were cleaned.
fn remove_unpublished_outputs(
    tx: &mut WriteTx<'_>,
    journal: &Path,
    session_id: &str,
) -> Result<usize> {
    if !tx.root().join(crate::paths::QUEUE_DIR).is_dir() {
        return Ok(0);
    }
    let started = journal_start(journal, session_id).map(|t| start_minute(&t));
    let queue = Queue::open(tx.root())?;
    let mut cleaned = 0;
    for job in queue.jobs()? {
        if job.input.get("session_id").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
        let Ok(text) = fs::read_to_string(queue.job_dir(&job.id).join(RECORD_MEETING_ID_FILE))
        else {
            continue;
        };
        let Ok(id) = MeetingId::parse(text.trim()) else {
            continue;
        };
        if tx.library().meeting_exists(&id)? {
            // Published: the meeting deletion covers it.
            continue;
        }
        let (samples, models) = meeting_samples(tx.library(), &id)?;
        if !models.is_empty() {
            learning::request_all(tx, models)?;
        }
        remove_samples(tx, &samples)?;
        for dir in unpublished_meeting_dirs(tx.root(), &id, started.as_deref())? {
            let mut kept = false;
            for entry in fs::read_dir(&dir).map_err(|e| Error::io_at(&dir, e))? {
                let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
                let name = entry.file_name();
                if entry.path().is_file() && name.to_str().is_some_and(is_record_output) {
                    remove_file(&entry.path())?;
                } else {
                    kept = true;
                }
            }
            if !kept {
                fs::remove_dir(&dir).map_err(|e| Error::io_at(&dir, e))?;
            }
            cleaned += 1;
        }
    }
    Ok(cleaned)
}

/// Meeting folders `YYYY/MM/<name>` that hold no `meeting.json`, each with
/// the meeting ID its name ends in (`-<6 hex>`) when it does; sorted.
fn folders_without_meeting(root: &Path) -> Result<Vec<(PathBuf, Option<MeetingId>)>> {
    let digits = |p: &Path, n: usize| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|s| s.len() == n && s.bytes().all(|b| b.is_ascii_digit()))
    };
    let subdirs = |dir: &Path| -> Result<Vec<PathBuf>> {
        match fs::read_dir(dir) {
            Ok(entries) => Ok(entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(Error::io_at(dir, e)),
        }
    };
    let mut out = Vec::new();
    for year in subdirs(root)?.into_iter().filter(|p| digits(p, 4)) {
        for month in subdirs(&year)?.into_iter().filter(|p| digits(p, 2)) {
            for dir in subdirs(&month)? {
                if dir.join(crate::paths::MEETING_JSON).exists() {
                    continue;
                }
                let id = dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.rsplit_once('-'))
                    .and_then(|(_, hex)| MeetingId::parse(&format!("m-{hex}")).ok());
                out.push((dir, id));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Meeting folders of meeting `id` that hold no `meeting.json` (what a
/// record attempt that failed before publishing left), sorted, their name
/// starting with `started` (the meeting's start, `%Y-%m-%d-%H%M`) when
/// given. Matched by the stable ID suffix, never by the title part of the
/// name, so a title changed between attempts still finds the earlier
/// folder. Read-only; call it under the library lock that also commits what
/// it decides.
pub fn unpublished_meeting_dirs(
    root: &Path,
    id: &MeetingId,
    started: Option<&str>,
) -> Result<Vec<PathBuf>> {
    Ok(folders_without_meeting(root)?
        .into_iter()
        .filter(|(dir, found)| {
            found.as_ref() == Some(id)
                && started.is_none_or(|s| {
                    dir.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(s))
                })
        })
        .map(|(dir, _)| dir)
        .collect())
}

/// Reads the capture state of the journal in `dir` of session `session_id`.
pub fn journal_capture(dir: &Path, session_id: &str) -> JournalCapture {
    let path = dir.join(SESSION_MANIFEST);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return JournalCapture::Missing,
        Err(_) => return JournalCapture::Unreadable,
    };
    match serde_json::from_slice::<ManifestOwner>(&bytes) {
        Ok(m) if m.session_id == session_id && m.state == "stopped" => JournalCapture::Stopped,
        Ok(m) if m.session_id == session_id && m.state == "recording" => {
            JournalCapture::Recording {
                owner_pid: m.owner_pid,
            }
        }
        _ => JournalCapture::Unreadable,
    }
}

/// Whether a capture may still write the journal: one the agent says it
/// still runs or stops, or a `recording` journal whose owner is another
/// process that is alive. A `recording` journal of this process the agent no
/// longer captures, of a dead process, or found while no capture runs at all
/// is an orphan of a crash or a failed stop.
fn capture_may_be_live(
    state: JournalCapture,
    ownership: CaptureOwnership,
    probe: &dyn ProcessProbe,
) -> bool {
    match (ownership, state) {
        (CaptureOwnership::Agent { live: true }, _) => true,
        (CaptureOwnership::NoneRunning, _) => false,
        (CaptureOwnership::Agent { live: false }, JournalCapture::Recording { owner_pid }) => {
            owner_pid != probe.current_pid() && probe.is_alive(owner_pid)
        }
        (CaptureOwnership::Agent { live: false }, _) => false,
    }
}

/// Cleans up a recording whose [`DiscardIntent`] exists: revokes its record
/// jobs (live leases included: their commits then fail), deletes any
/// provisional meeting through the deletion service, removes the journal
/// (audio, sidecars, reader evidence, calendar snapshot) and finally the
/// intent. Idempotent. Nothing is touched while a capture may still write
/// the journal (`ownership`, the manifest's owner and the probe decide); a
/// journal whose manifest does not read as this session's is left in place
/// and reported, and the intent stays. Name-only people created during the
/// session stay, as everywhere.
pub fn discard_session(
    lib: &Library,
    probe: &dyn ProcessProbe,
    opts: &DeleteOptions,
    session_id: &str,
    ownership: CaptureOwnership,
) -> Result<DiscardReport> {
    let mut report = DiscardReport {
        session_id: session_id.to_owned(),
        ..DiscardReport::default()
    };
    let journal = session_dir(&opts.journal_root, session_id)?;
    if !has_discard_intent(&opts.journal_root, session_id) {
        report.complete = true;
        return Ok(report);
    }
    let capture = journal_capture(&journal, session_id);
    if capture_may_be_live(capture, ownership, probe) {
        report
            .failures
            .push("the recording is still capturing".to_owned());
        return Ok(report);
    }
    if is_legacy_discard(&opts.journal_root, session_id)? {
        // This intent was committed only after verifying a readable legacy
        // manifest. A partial removal can already have erased its manifest.
        lib.write(|_| {
            remove_tree(&journal)?;
            fsutil::sync_dir(&opts.journal_root)?;
            remove_discard_intent(&opts.journal_root, session_id)
        })?;
        report.complete = true;
        return Ok(report);
    }
    let discards = opts.journal_root.join(DISCARD_DIR);
    if discards.is_dir()
        && let Err(e) =
            lib.write(|_| fsutil::sweep_orphan_temps(&discards, probe, ORPHAN_TEMP_MIN_AGE))
    {
        tracing::warn!(
            code = e.code(),
            "removing orphaned discard-intent temporaries"
        );
    }
    let (markers, revoked) = ops::with_index(lib, |tx, index| {
        let meetings = session_meetings(tx.library(), &opts.journal_root, session_id)?;
        if let Some(saved) = meetings
            .iter()
            .find(|m| m.processing.status == ProcessingStatus::Done)
        {
            report.saved_meeting = Some(saved.id.clone());
            return Ok((Vec::new(), Vec::new()));
        }
        // Admission first: every provisional meeting's plan refuses (a live
        // job of another kind, an only copy in flight) before anything
        // changes, identity recheck requests included.
        let plans = meetings
            .iter()
            .map(|meeting| {
                plan(
                    tx.library(),
                    probe,
                    opts,
                    &meeting.id,
                    Mode::Discard(session_id),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        // Before any job is revoked or its folder removed: what an attempt
        // that failed before publishing left under the meeting ID its job
        // folder names.
        report.unpublished_removed = remove_unpublished_outputs(tx, &journal, session_id)?;
        let mut markers = Vec::new();
        for plan in plans {
            let (_, marker) = commit(tx, index, probe, opts, plan, false)?;
            markers.push(marker);
        }
        let queue = Queue::open(tx.root())?;
        let mut revoked = Vec::new();
        for job in queue.jobs()? {
            if job.input.get("session_id").and_then(|v| v.as_str()) == Some(session_id) {
                queue.revoke(tx, &job.id, probe)?;
                revoked.push(job.id);
            }
        }
        Ok((markers, revoked))
    })?;
    if report.saved_meeting.is_some() {
        remove_discard_intent(&opts.journal_root, session_id)?;
        report.complete = true;
        return Ok(report);
    }
    report.meetings_deleted = markers.len();
    report.jobs_cancelled = revoked.len();
    // This pass's deletions and any an earlier pass left pending: the intent
    // stays until every provisional meeting of the session is fully gone.
    report
        .failures
        .extend(finish_session_deletions(lib, probe, session_id));
    if let Ok(queue) = Queue::open(lib.root()) {
        for id in &revoked {
            report
                .failures
                .extend(remove_revoked_job(lib, &queue, probe, id));
        }
    }
    match capture {
        JournalCapture::Unreadable => report
            .failures
            .push("recording journal left in place: its manifest is not this session's".into()),
        _ => {
            if let Err(e) = remove_tree(&journal) {
                report
                    .failures
                    .push(format!("recording journal: {}", e.code()));
            }
        }
    }
    // The live-preview decisions of a recording that will never be saved.
    if let Err(e) = crate::live_identity::remove_sidecar(lib.root(), session_id) {
        report
            .failures
            .push(format!("live preview decisions: {}", e.code()));
    }
    if report.failures.is_empty() {
        remove_discard_intent(&opts.journal_root, session_id)?;
        report.complete = true;
    }
    Ok(report)
}

/// Finishes the meeting deletions recorded for recording `session_id`;
/// returns what is still left (empty when all are done).
fn finish_session_deletions(
    lib: &Library,
    probe: &dyn ProcessProbe,
    session_id: &str,
) -> Vec<String> {
    let dir = lib.root().join(DELETING_DIR);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut left = Vec::new();
    for entry in entries.flatten() {
        let Ok(marker) = fsutil::read_json::<DeletionMarker>(&entry.path()) else {
            continue;
        };
        if marker.session_id.as_deref() != Some(session_id) {
            continue;
        }
        match resume(lib, probe, &marker) {
            Ok(report) if report.state == DeleteState::Deleted => {}
            Ok(_) => left.push(format!(
                "deletion of meeting {} is pending",
                marker.meeting_id
            )),
            Err(e) => left.push(format!("meeting {}: {}", marker.meeting_id, e.code())),
        }
    }
    left
}

// ---- diagnostics ----

/// Where a committed deletion stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionStage {
    /// The meeting folder is still in place (interrupted before the rename).
    FolderInPlace,
    /// The folder was renamed into `.deleted/` and is not removed yet.
    Staged,
    /// Only jobs, caches, a journal or the marker itself are left.
    Cleanup,
}

/// One committed deletion not finished; identifiers only.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PendingDeletion {
    pub meeting_id: MeetingId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub stage: DeletionStage,
    /// Jobs the deletion revoked whose folders it still removes.
    pub revoked_jobs: usize,
    pub created_at: DateTime<FixedOffset>,
}

/// What the lifecycle's fences leave pending, for `doctor` and `library
/// check`: identifiers and counts only (never titles, names, folder names
/// or paths). Nothing is repaired: [`delete_meeting`] or
/// [`finish_pending_deletions`] finishes a deletion, [`discard_session`] a
/// discard.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LifecycleDiagnosis {
    /// In marker file-name order.
    pub pending_deletions: Vec<PendingDeletion>,
    /// `.deleting/*.json` markers that do not read as one.
    pub unreadable_deletion_markers: usize,
    /// `.deleted/` entries no deletion marker accounts for.
    pub unaccounted_staged: usize,
    /// Recording sessions with a discard intent, sorted.
    pub pending_discards: Vec<String>,
    /// Meetings with a folder holding no `meeting.json`: a record attempt in
    /// flight or one that failed before publishing (an earlier title
    /// included). Sorted, deduplicated.
    pub unpublished_meetings: Vec<MeetingId>,
    /// Folders holding no `meeting.json` whose name ends in no meeting ID.
    pub unidentified_folders: usize,
    /// What [`sweep_orphan_temps`] would remove: atomic-write temporaries
    /// of writers no longer alive, old enough, in the library root, the
    /// meeting, people, forget-tombstone, live-sidecar, index, queue and
    /// job folders, the deletion markers, the discard intents and the
    /// user's templates.
    pub orphan_temp_files: usize,
}

/// Reads the lifecycle state of `lib` and of `data_dir` (its journals and
/// templates) without changing anything (no lock: a snapshot that a
/// concurrent deletion or discard may already have moved on from).
pub fn diagnose(
    lib: &Library,
    data_dir: &Path,
    probe: &dyn ProcessProbe,
) -> Result<LifecycleDiagnosis> {
    let root = lib.root();
    let mut out = LifecycleDiagnosis {
        pending_discards: discard_intents(&journal_root(data_dir))?,
        ..LifecycleDiagnosis::default()
    };
    let mut marked = BTreeSet::new();
    let deleting = root.join(DELETING_DIR);
    for name in file_names(&deleting)? {
        if Path::new(&name).extension().is_none_or(|x| x != "json") {
            continue;
        }
        let marker: DeletionMarker = match fsutil::read_json(&deleting.join(&name)) {
            Ok(marker) => marker,
            Err(Error::NotFound(_)) => continue,
            Err(_) => {
                out.unreadable_deletion_markers += 1;
                continue;
            }
        };
        let stage = if lib.meeting_exists(&marker.meeting_id)? {
            DeletionStage::FolderInPlace
        } else if staged_path(root, &marker.meeting_id).exists() {
            DeletionStage::Staged
        } else {
            DeletionStage::Cleanup
        };
        marked.insert(marker.meeting_id.as_str().to_owned());
        out.pending_deletions.push(PendingDeletion {
            meeting_id: marker.meeting_id,
            session_id: marker.session_id,
            stage,
            revoked_jobs: marker.revoked.len(),
            created_at: marker.created_at,
        });
    }
    out.unaccounted_staged = file_names(&root.join(DELETED_DIR))?
        .iter()
        .filter(|name| !marked.contains(name.as_str()))
        .count();
    let mut unpublished = BTreeSet::new();
    for (_, id) in folders_without_meeting(root)? {
        match id {
            Some(id) => {
                unpublished.insert(id);
            }
            None => out.unidentified_folders += 1,
        }
    }
    out.unpublished_meetings = unpublished.into_iter().collect();
    let mut temp_dirs = lib.temp_dirs()?;
    temp_dirs.extend(lifecycle_temp_dirs(root, data_dir));
    if let Some(queue) = Queue::open_existing(root)? {
        temp_dirs.extend(queue.temp_dirs()?);
    }
    for dir in temp_dirs {
        out.orphan_temp_files += fsutil::orphan_temps(&dir, probe, ORPHAN_TEMP_MIN_AGE)?.len();
    }
    Ok(out)
}

/// The entry names of `dir` that are valid UTF-8, sorted; none when it is
/// missing.
fn file_names(dir: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(dir, e)),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(dir, e))?;
        if let Ok(name) = entry.file_name().into_string() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

// ---- helpers ----

fn remove_file(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

fn remove_tree(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

#[cfg(test)]
mod tests;
