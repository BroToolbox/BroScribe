//! Importing media files into the library (design §6.5): one file, or each
//! media file of a folder as its own meeting. Every import runs as a queue
//! job (kind [`IMPORT_KIND`], key = content SHA-256): analysis outside the
//! lock, then one commit through the job's lease.
//!
//! `move` handling (plan §9.9) is journaled in the job folder
//! ([`MOVE_JOURNAL`]) before any destructive step: on the same volume the
//! original is renamed into the job folder; across volumes it is copied,
//! synced and verified (SHA-256) and deleted only after the meeting is
//! published. [`recover_moves`] finishes or rolls back an interrupted move
//! from wherever the file is (original place, job folder or meeting
//! folder). A file that turns out to be in the library already is never
//! deleted.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Instant;

use broscribe_core::index::Index;
use broscribe_core::process::ProcessProbe;
use broscribe_core::queue::{
    self, IMPORT_KIND, Job, JobId, JobState, LeaseHandle, MAX_ATTEMPTS, Queue,
};
use broscribe_core::schema::{
    Handling, LanguageSpan, Meeting, ModelsUsed, Participant, Processing, ProcessingStatus,
    Recording, RecordingState, Source, TitleSource, Track, TrackRole,
};
use broscribe_core::{Error, Library, MeetingId, Result, fsutil, paths};
use broscribe_models::ModelSet;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::analyze::{AnalyzeOptions, TRACK_ID, analyze_with_languages};
use crate::audio::{decode_16k, file_sha256};
use crate::finish::{audio_ext, file_mtime, find_meeting_by_sha, finish_meeting, title_from_path};

pub use broscribe_core::import_files::MEDIA_EXTENSIONS;
use broscribe_core::import_files::folder_media;

/// Flags of one import; unset fields fall back to the file and the config.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ImportOptions {
    /// `None`: the configured handling.
    pub handling: Option<Handling>,
    /// BCP 47 tag; `None`: the configured default language.
    pub language: Option<String>,
    /// Use recognition's language detection even if a default is configured.
    pub auto_language: bool,
    pub title: Option<String>,
    /// Meeting start; `None`: container creation time, else file mtime.
    pub started_at: Option<DateTime<FixedOffset>>,
    pub participants: Vec<Participant>,
    /// Setup Try-it imports are fenced under the library commit lock.
    pub setup_session: Option<(PathBuf, String)>,
}

/// Result of importing one file.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ImportOutcome {
    pub meeting_id: MeetingId,
    pub source_path: PathBuf,
    /// The file was already in the library (same SHA-256): nothing changed.
    pub already_exists: bool,
    /// An interrupted `move` import of this file was published by this
    /// call's recovery: the meeting is new to the caller, who still owes it
    /// the follow-ups the interrupted import never reached (vectors,
    /// retention, enrichment). Never set together with `already_exists`.
    pub recovered: bool,
    pub title: Option<String>,
    pub started_at: DateTime<FixedOffset>,
    pub duration_s: f64,
    pub segments: usize,
    pub speakers: usize,
    /// Speakers named automatically.
    pub auto_named: usize,
    pub suggested: usize,
    pub processing_ms: Option<u64>,
}

impl ImportOutcome {
    fn existing(meeting: &Meeting, path: &Path) -> Self {
        Self {
            meeting_id: meeting.id.clone(),
            source_path: path.to_owned(),
            already_exists: true,
            recovered: false,
            title: meeting.title.clone(),
            started_at: meeting.started_at,
            duration_s: meeting.duration_s,
            segments: meeting.segments.len(),
            speakers: meeting.speakers.len(),
            auto_named: count(meeting, broscribe_core::schema::Assignment::Auto),
            suggested: count(meeting, broscribe_core::schema::Assignment::Suggested),
            processing_ms: meeting.processing.duration_ms,
        }
    }
}

fn count(m: &Meeting, a: broscribe_core::schema::Assignment) -> usize {
    m.speakers.iter().filter(|s| s.assignment == a).count()
}

/// Result of importing a folder: one outcome per media file, failures kept.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FolderReport {
    pub imported: Vec<ImportOutcome>,
    pub failed: Vec<ImportFailure>,
    /// Interrupted `move` imports of other files that this import's
    /// recovery completed ([`MoveOutcome::Completed`]): their meetings are
    /// new to the caller, who owes them the follow-ups of their own import.
    pub completed_moves: Vec<MoveRecovery>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ImportFailure {
    pub path: PathBuf,
    pub code: &'static str,
    pub message: String,
}

/// Resolves the handling of manifest imports, where `move` is not
/// available yet (single-file and folder imports support it).
pub(crate) fn check_handling(handling: Handling) -> Result<Handling> {
    match handling {
        Handling::Move => Err(Error::Unsupported(
            "move handling is not available for manifest imports; use copy or reference".into(),
        )),
        h => Ok(h),
    }
}

pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|e| Error::io_at(path, e))
}

/// The job input of an import; `folder` marks one file of a folder import
/// (a batch), which never enriches on its own when the worker recovers it.
fn import_input(
    env: &Env<'_>,
    path: &Path,
    sha: &str,
    opts: &ImportOptions,
    folder: bool,
) -> Result<serde_json::Value> {
    let created_at = env.decoders.probe(path).ok().and_then(|i| i.created_at);
    let language = if opts.auto_language {
        None
    } else {
        opts.language
            .as_ref()
            .or(env.config.default_language.as_ref())
    };
    Ok(serde_json::json!({
        "path": path,
        "sha256": sha,
        "handling": opts.handling.unwrap_or(env.config.import_handling),
        "language": language,
        "title": opts.title.clone().or_else(|| title_from_path(path)),
        "title_is_user": opts.title.is_some(),
        "started_at": opts.started_at.or(created_at).or_else(|| file_mtime(path)).unwrap_or_else(broscribe_core::schema::now),
        "participants": opts.participants,
        "setup_session": opts.setup_session,
        "folder": folder,
    }))
}

/// Whether an import job's input marks it as one file of a folder import.
pub(crate) fn is_folder_import(input: &serde_json::Value) -> bool {
    input
        .get("folder")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Imports one file as one meeting: hash → dedupe → metadata → handling →
/// analysis → merge → identity resolution → save (`done`) → index.
/// Interrupted moves are recovered first ([`recover_moves`]); when `path`
/// is one of them and its meeting was already saved, that meeting is
/// returned as existing. Other files' moves that recovery completed are not
/// reported: callers that run import follow-ups use
/// [`import_file_with_recoveries`].
///
/// `opts` are resolved afresh and written over any earlier job of the same
/// file that no live process holds: a re-run takes the caller's new flags,
/// and an explicit re-run of a failed job starts its attempts over.
pub fn import_file(
    env: &Env<'_>,
    models: &mut ModelSet,
    path: &Path,
    opts: &ImportOptions,
) -> Result<ImportOutcome> {
    import_file_with_recoveries(env, models, path, opts, &mut Vec::new())
}

/// [`import_file`], adding to `completed` every interrupted `move` import of
/// another file that its recovery completed ([`MoveOutcome::Completed`]),
/// also when the import itself fails: those meetings are new to the caller,
/// who owes them the follow-ups their interrupted import never reached.
pub fn import_file_with_recoveries(
    env: &Env<'_>,
    models: &mut ModelSet,
    path: &Path,
    opts: &ImportOptions,
    completed: &mut Vec<MoveRecovery>,
) -> Result<ImportOutcome> {
    import(
        env,
        models,
        path,
        Request::Direct(opts, false),
        completed,
        true,
    )
}

/// Imports supported media without recovering other import jobs or moving originals.
pub fn import_media_without_moves(
    env: &Env<'_>,
    models: &mut ModelSet,
    path: &Path,
    opts: &ImportOptions,
) -> Result<ImportOutcome> {
    import(
        env,
        models,
        path,
        Request::Direct(opts, false),
        &mut Vec::new(),
        false,
    )
}

/// Both the requested file and its resolved target must be supported media.
pub fn is_media_source(path: &Path) -> bool {
    use broscribe_core::import_files::is_media;
    is_media(path)
        && path
            .canonicalize()
            .is_ok_and(|target| target.is_file() && is_media(&target))
}

/// Decoder diagnostics can contain raw input details. Keep the stable
/// error category when reporting a failure to an MCP client.
pub(crate) fn redact_decode_error(error: Error) -> Error {
    match error {
        Error::Decode(_) => Error::Decode("could not decode the media".into()),
        other => other,
    }
}

/// Resumes queued import `job` with the options stored in it when it was
/// enqueued (handling, title and whether the user typed it, language,
/// date, participants). Its attempts are kept. Other files' moves its
/// recovery completed go to `completed`, as in [`import_file_with_recoveries`].
pub(crate) fn resume_import_job(
    env: &Env<'_>,
    models: &mut ModelSet,
    job: &Job,
    completed: &mut Vec<MoveRecovery>,
) -> Result<ImportOutcome> {
    let path = job
        .input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| Error::InvalidInput(format!("job {} has no source path", job.id)))?;
    import(env, models, &path, Request::Resume(job), completed, true)
}

/// Where an import's options come from.
enum Request<'a> {
    /// A caller's direct import: these options win. `true`: one file of a
    /// folder import.
    Direct(&'a ImportOptions, bool),
    /// The worker resuming a queued job: its stored input wins.
    Resume(&'a Job),
}

impl Request<'_> {
    fn folder(&self) -> bool {
        match self {
            Request::Direct(_, folder) => *folder,
            Request::Resume(job) => is_folder_import(&job.input),
        }
    }
}

fn import(
    env: &Env<'_>,
    models: &mut ModelSet,
    path: &Path,
    request: Request<'_>,
    completed: &mut Vec<MoveRecovery>,
    recover_interrupted_moves: bool,
) -> Result<ImportOutcome> {
    if !recover_interrupted_moves {
        if let Request::Direct(opts, _) = request {
            check_handling(opts.handling.unwrap_or(env.config.import_handling))?;
        }
        if !is_media_source(path) {
            return Err(Error::InvalidInput(
                "choose a supported audio or video file".into(),
            ));
        }
    }
    refuse_library_source(env, path)?;
    for id in discard_abandoned_setup_imports(env.lib, env.probe)? {
        env.say(format_args!(
            "discarded an abandoned setup recording import (job {id})"
        ));
    }
    if let Request::Resume(job) = &request
        && is_setup_import(&job.input)
        && matches!(
            Queue::open(env.lib.root())?.get(&job.id),
            Err(Error::NotFound(_))
        )
    {
        return Err(Error::InvalidInput(
            "setup import discarded: its setup session ended or its recording is gone".into(),
        ));
    }
    // Every completed recovery is handed over before anything else can
    // fail: its job is gone, so nothing else would give it its follow-ups.
    let start = completed.len();
    if recover_interrupted_moves {
        completed.extend(
            recover_moves(env)?
                .into_iter()
                .filter(|r| r.outcome == MoveOutcome::Completed),
        );
    }
    if let Request::Resume(job) = &request
        && let Ok(current) = Queue::open(env.lib.root())?.get(&job.id)
        && current.keeps_file()
    {
        return Err(Error::AlreadyExists(current.error.unwrap_or_else(|| {
            format!("job {} keeps the only copy of a moved file", job.id)
        })));
    }
    if !path.exists()
        && let Some(i) = (start..completed.len()).find(|&i| same_file(&completed[i].source, path))
        && let Some(id) = completed[i].meeting_id.clone()
    {
        let m = env.lib.load_meeting(&id)?;
        // This file's own recovery: its outcome is this call's result.
        let r = completed.remove(i);
        if is_unfinished(env, &m)?
            && let Some(sha) = m.recording.as_ref().map(|r| r.sha256.clone())
        {
            return finish_unfinished(env, &r.source, &sha, &m.id, &request, true);
        }
        // Published by this recovery, not a duplicate: the interrupted
        // import never reached its follow-ups.
        env.say(format_args!(
            "{}: imported as {} (interrupted move completed)",
            path.display(),
            m.id
        ));
        return Ok(ImportOutcome {
            already_exists: false,
            recovered: true,
            ..ImportOutcome::existing(&m, &r.source)
        });
    }
    let path = absolute(path)?;
    if !path.is_file() {
        return Err(Error::InvalidInput(format!(
            "not a file: {}",
            path.display()
        )));
    }
    let (sha, bytes) = file_sha256(&path)?;
    // A queued job runs only on the content it was queued for: never
    // complete it through another file's dedupe.
    if let Request::Resume(job) = &request
        && job.key != sha
    {
        return Err(Error::InvalidInput(format!(
            "{} changed since it was queued; import it again",
            path.display()
        )));
    }
    if let Some(m) = find_meeting_by_sha(env.lib, &sha, bytes)? {
        if is_unfinished(env, &m)? {
            if !recover_interrupted_moves
                && m.recording
                    .as_ref()
                    .is_some_and(|r| r.handling == Handling::Move)
            {
                return Err(Error::InvalidInput("an incomplete Move import exists for this recording; finish it in BroScribe or the CLI".into()));
            }
            return finish_unfinished(env, &path, &sha, &m.id, &request, recover_interrupted_moves);
        }
        env.say(format_args!(
            "{}: already imported as {}",
            path.display(),
            m.id
        ));
        return Ok(ImportOutcome::existing(&m, &path));
    }
    let queue = Queue::open(env.lib.root())?;
    let (lease, stored) = match request {
        Request::Direct(opts, folder) => {
            // Probe the media before taking the library lock.
            let input = import_input(env, &path, &sha, opts, folder)?;
            let lease = claim_direct(env, &queue, &path, &sha, &input, recover_interrupted_moves)?;
            (lease, input)
        }
        Request::Resume(job) => {
            let lease = claim_to_resume(env, &queue, &job.id, &path)?;
            match resumed_input(env, &lease, job, &path, &sha) {
                Ok(input) => (lease, input),
                Err(e) => {
                    let _ = lease.fail(env.lib, e.to_string());
                    let _ = lease.release(env.lib);
                    return Err(e);
                }
            }
        }
    };
    let (handling, resolved) = match resolved_options(&stored) {
        Ok(r) => r,
        Err(e) => {
            let _ = lease.fail(env.lib, e.to_string());
            let _ = lease.release(env.lib);
            return Err(e);
        }
    };
    let result = run_import(env, models, &lease, &path, &sha, bytes, handling, &resolved);
    let result = if recover_interrupted_moves {
        result
    } else {
        result.map_err(redact_decode_error)
    };
    match &result {
        Ok(_) => {
            lease.release(env.lib)?;
            env.lib.write(|tx| queue.remove_finished(tx, IMPORT_KIND))?;
        }
        Err(e)
            if resolved.setup_session.is_some()
                && handling != Handling::Move
                && !is_move_failpoint(e) =>
        {
            // A setup Try-it recording is temporary: its import is never
            // retried, so its job and staged copy go now.
            discard_setup_import(env.lib, &queue, lease)?;
        }
        Err(e) => {
            let mut message = e.to_string();
            // Put a moved file back (or finish a published move) now, still
            // under this attempt's lease; a simulated crash leaves that to
            // the next recovery.
            if handling == Handling::Move
                && !is_move_failpoint(e)
                && let Ok(Some(r)) = settle_move(env, &lease, is_folder_import(&stored))
                && let Some(note) = r.conflict_note()
            {
                message = format!("{message}; {note}");
            }
            let _ = match kept_conflict(env.lib.root(), &lease.job_dir()) {
                Some(kept) => lease.fail_keeping(env.lib, message, kept),
                None => lease.fail(env.lib, message),
            };
            let id = lease.id().clone();
            let _ = lease.release(env.lib);
            // That failure may have been its last attempt.
            let _ = note_stranded_move(env, &queue, &id);
        }
    }
    result
}

/// The input of a resumed job, from the record captured under its claim
/// ([`LeaseHandle::claimed_job`]), never from the listing it was scheduled
/// from: a job whose key or source changed since then is refused. One
/// enqueued by an older build has no resolved handling: its missing choices
/// are resolved now and stored.
fn resumed_input(
    env: &Env<'_>,
    lease: &LeaseHandle,
    listed: &Job,
    path: &Path,
    sha: &str,
) -> Result<serde_json::Value> {
    let claimed = lease.claimed_job();
    fn source(job: &Job) -> Option<&str> {
        job.input.get("path").and_then(serde_json::Value::as_str)
    }
    if claimed.key != sha || claimed.key != listed.key || source(claimed) != source(listed) {
        return Err(Error::InvalidInput(format!(
            "{} changed since it was queued; import it again",
            path.display()
        )));
    }
    let input = claimed.input.clone();
    if input.get("handling").is_some_and(|h| !h.is_null()) {
        return Ok(input);
    }
    let folder = is_folder_import(&input);
    let opts: ImportOptions = serde_json::from_value(input)
        .map_err(|e| Error::InvalidInput(format!("invalid import options: {e}")))?;
    let input = import_input(env, path, sha, &opts, folder)?;
    lease.set_input(env.lib, input.clone())?;
    Ok(input)
}

/// The handling and options a job's input resolved, as stored.
fn resolved_options(input: &serde_json::Value) -> Result<(Handling, ImportOptions)> {
    let mut resolved: ImportOptions = serde_json::from_value(input.clone())
        .map_err(|e| Error::InvalidInput(format!("invalid import options: {e}")))?;
    let handling = resolved.handling.ok_or_else(|| {
        Error::InvalidInput("queued import is missing its resolved handling".into())
    })?;
    // A file-derived title is derived again, never stored as the user's.
    if input
        .get("title_is_user")
        .and_then(serde_json::Value::as_bool)
        == Some(false)
    {
        resolved.title = None;
    }
    Ok((handling, resolved))
}

/// A setup Try-it import of a temporary recording (never a `move`, whose
/// staged file is the user's original).
pub(crate) fn is_setup_import(input: &serde_json::Value) -> bool {
    input.get("setup_session").is_some_and(|s| !s.is_null())
        && input.get("handling").and_then(serde_json::Value::as_str) != Some("move")
}

/// The `(data_dir, session)` a setup Try-it import was fenced on.
fn setup_session_of(input: &serde_json::Value) -> Option<(PathBuf, String)> {
    serde_json::from_value::<Option<(PathBuf, String)>>(
        input.get("setup_session").cloned().unwrap_or_default(),
    )
    .ok()
    .flatten()
}

/// Whether a setup Try-it import can no longer publish: its session was
/// cancelled or replaced, or its temporary recording is gone. An unreadable
/// config proves nothing.
fn setup_import_abandoned(input: &serde_json::Value) -> bool {
    let Some((data_dir, session)) = setup_session_of(input) else {
        return false;
    };
    let gone = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|p| !Path::new(p).is_file());
    gone || broscribe_core::config::Config::load(&data_dir)
        .is_ok_and(|c| c.setup.voice_session.as_ref() != Some(&session))
}

/// Whether queued `job` was a setup Try-it import that has been discarded
/// because its session ended or its recording is gone (not a failure).
pub(crate) fn setup_import_discarded(queue: &Queue, job: &Job) -> bool {
    is_setup_import(&job.input)
        && setup_import_abandoned(&job.input)
        && matches!(queue.get(&job.id), Err(Error::NotFound(_)))
}

/// Removes a setup import's job folder (input, staged copy) through its
/// lease: marked done, then removed with the other finished imports.
fn discard_setup_import(lib: &Library, queue: &Queue, lease: LeaseHandle) -> Result<()> {
    let committed = lease.commit(lib, JobState::Done, |_, _| Ok(()));
    let released = lease.release(lib);
    committed?;
    released?;
    lib.write(|tx| queue.remove_finished(tx, IMPORT_KIND))
        .map(|_| ())
}

/// Discards the setup imports `select` picks (job and staged audio) unless
/// a live process holds them. A job no claim will run again (failed with
/// no attempts left) is closed and removed under the library lock.
fn discard_setup_imports_where(
    lib: &Library,
    probe: &dyn ProcessProbe,
    select: impl Fn(&serde_json::Value) -> bool,
) -> Result<Vec<JobId>> {
    let queue = Queue::open(lib.root())?;
    let mut discarded = Vec::new();
    for job in queue.list(IMPORT_KIND)? {
        if job.state == JobState::Done || !is_setup_import(&job.input) || !select(&job.input) {
            continue;
        }
        match queue.claim(lib, &job.id, probe)? {
            Some(lease) => discard_setup_import(lib, &queue, lease)?,
            None => {
                let closed = lib.write(|tx| match queue.get(&job.id) {
                    Ok(j)
                        if j.state == JobState::Failed
                            && j.attempts >= MAX_ATTEMPTS
                            && j.lease.is_none() =>
                    {
                        queue.set_state(tx, &job.id, JobState::Done, j.error)?;
                        queue.remove_finished(tx, IMPORT_KIND)?;
                        Ok(true)
                    }
                    Ok(_) | Err(Error::NotFound(_)) => Ok(false),
                    Err(e) => Err(e),
                })?;
                if !closed {
                    continue;
                }
            }
        }
        discarded.push(job.id);
    }
    Ok(discarded)
}

/// Discards the setup Try-it imports of setup session `session` (job and
/// staged audio), never `move` imports. Call it after the setup child of
/// that session was killed and reaped: a still-live holder keeps its job.
/// Returns the discarded jobs.
pub fn discard_session_imports(
    lib: &Library,
    probe: &dyn ProcessProbe,
    session: &str,
) -> Result<Vec<JobId>> {
    discard_setup_imports_where(lib, probe, |input| {
        setup_session_of(input).is_some_and(|(_, s)| s == session)
    })
}

/// Discards setup Try-it imports a cancelled or killed setup step left
/// behind (job and staged audio), unless a live process still holds them.
/// Ordinary imports keep their recovery data. Runs before every import and
/// at worker and app start. Returns the discarded jobs.
pub fn discard_abandoned_setup_imports(
    lib: &Library,
    probe: &dyn ProcessProbe,
) -> Result<Vec<JobId>> {
    discard_setup_imports_where(lib, probe, setup_import_abandoned)
}

/// Whether `path` (which may no longer exist) names the canonical `source`.
fn same_file(source: &Path, path: &Path) -> bool {
    if let Ok(p) = fs::canonicalize(path) {
        return p == source;
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match (fs::canonicalize(parent), path.file_name()) {
        (Ok(dir), Some(name)) => dir.join(name) == source,
        _ => false,
    }
}

/// A meeting saved by an interrupted or failed import: finished on the next
/// import of the same file instead of being returned as is.
fn is_unfinished(env: &Env<'_>, m: &Meeting) -> Result<bool> {
    if matches!(
        m.processing.status,
        ProcessingStatus::Analyzed | ProcessingStatus::Failed
    ) {
        return Ok(true);
    }
    if m.processing.status != ProcessingStatus::Done {
        return Ok(false);
    }
    // Older imports could save `done` before indexing and sample syncing.
    // An unfinished durable job proves that publication needs repair.
    let Some(recording) = &m.recording else {
        return Ok(false);
    };
    let queue = Queue::open(env.lib.root())?;
    match queue.get(&broscribe_core::queue::JobId::for_key(
        IMPORT_KIND,
        &recording.sha256,
    )) {
        Ok(job) => Ok(job.state != JobState::Done),
        Err(Error::NotFound(_)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Claims import job `id` for a resume. A claim refused because it just
/// failed the job (a crashed holder's last attempt) or because the job
/// failed for good reports that failure; otherwise another process holds it.
fn claim_to_resume(env: &Env<'_>, queue: &Queue, id: &JobId, path: &Path) -> Result<LeaseHandle> {
    if let Some(lease) = queue.claim(env.lib, id, env.probe)? {
        return Ok(lease);
    }
    match queue.get(id) {
        Ok(job) if job.state == JobState::Failed && job.lease.is_none() => {
            Err(Error::Internal(format!(
                "import of {} failed: {}",
                path.display(),
                job.error.as_deref().unwrap_or("no attempts left")
            )))
        }
        Ok(_) | Err(Error::NotFound(_)) => Err(Error::LibraryBusy(format!(
            "{} is being imported by another process (job {id})",
            path.display()
        ))),
        Err(e) => Err(e),
    }
}

/// Enqueues and claims the import job of `sha` for a caller's direct
/// request, with `input` and attempts afresh. A claim that took over a
/// crashed holder with no attempts left failed the job; this explicit
/// re-run restarts it.
fn claim_direct(
    env: &Env<'_>,
    queue: &Queue,
    path: &Path,
    sha: &str,
    input: &serde_json::Value,
    recover_interrupted_moves: bool,
) -> Result<LeaseHandle> {
    let enqueue = || {
        env.lib.write(|tx| {
            if !recover_interrupted_moves {
                let dir = queue.job_dir(&JobId::for_key(IMPORT_KIND, sha));
                if dir.join(MOVE_JOURNAL).exists() || dir.join(MOVE_CONFLICT).exists() {
                    return Err(Error::InvalidInput(
                        "an incomplete Move import exists for this recording; finish it in BroScribe or the CLI".into(),
                    ));
                }
            }
            queue.enqueue(tx, IMPORT_KIND, sha, input.clone())
        })
    };
    let job = enqueue()?;
    let mut lease = queue.claim(env.lib, &job, env.probe)?;
    if lease.is_none() && queue.get(&job)?.state == JobState::Failed {
        enqueue()?;
        lease = queue.claim(env.lib, &job, env.probe)?;
    }
    let lease = lease.ok_or_else(|| {
        Error::LibraryBusy(format!(
            "{} is being imported by another process",
            path.display()
        ))
    })?;
    if let Err(e) = lease.restart(env.lib, input.clone()) {
        let _ = lease.release(env.lib);
        return Err(e);
    }
    Ok(lease)
}

/// Finishes meeting `id`, saved by an earlier import of `sha` that did not
/// finalize it, under the import job of `sha`. A worker resume claims its
/// own job and keeps its attempts; a direct call enqueues the job (a new
/// one, or an explicit restart of a failed one).
fn finish_unfinished(
    env: &Env<'_>,
    path: &Path,
    sha: &str,
    id: &MeetingId,
    request: &Request<'_>,
    recover_interrupted_moves: bool,
) -> Result<ImportOutcome> {
    let queue = Queue::open(env.lib.root())?;
    // Probe the media before taking the library lock.
    let input = {
        let m = env.lib.load_meeting(id)?;
        import_input(
            env,
            path,
            sha,
            &ImportOptions {
                handling: m.recording.as_ref().map(|r| r.handling),
                language: m.languages.first().map(|l| l.lang.clone()),
                // A file-derived title is derived again, never stored as
                // the user's.
                title: m
                    .title
                    .clone()
                    .filter(|_| m.title_source == Some(TitleSource::User)),
                started_at: Some(m.started_at),
                participants: m.participants.clone(),
                auto_language: false,
                setup_session: None,
            },
            request.folder(),
        )?
    };
    let lease = match request {
        Request::Resume(job) => {
            if job.key != sha {
                return Err(Error::InvalidInput(format!(
                    "{} changed since it was queued; import it again",
                    path.display()
                )));
            }
            let lease = claim_to_resume(env, &queue, &job.id, path)?;
            // The record captured under the claim decides, not the listing.
            let checked = if lease.claimed_job().key == sha {
                lease.set_input(env.lib, input)
            } else {
                Err(Error::InvalidInput(format!(
                    "{} changed since it was queued; import it again",
                    path.display()
                )))
            };
            if let Err(e) = checked {
                let _ = lease.release(env.lib);
                return Err(e);
            }
            lease
        }
        Request::Direct(..) => {
            claim_direct(env, &queue, path, sha, &input, recover_interrupted_moves)?
        }
    };
    let result = resume_import(env, &lease, path, id);
    match &result {
        Ok(_) => {
            lease.release(env.lib)?;
            env.lib.write(|tx| queue.remove_finished(tx, IMPORT_KIND))?;
        }
        Err(e) => {
            let _ = lease.fail(env.lib, e.to_string());
            let _ = lease.release(env.lib);
        }
    }
    result
}

/// Finishes a meeting an earlier import saved but did not finalize: merge
/// (once), resolve, `done`, index and weak samples, under the job's lease.
/// Idempotent; resolves with the current profile's embedding model (a model
/// switch re-embeds centroids without touching `models.speaker_embedding`).
fn resume_import(
    env: &Env<'_>,
    lease: &LeaseHandle,
    path: &Path,
    id: &MeetingId,
) -> Result<ImportOutcome> {
    env.say(format_args!(
        "{}: finishing the earlier import",
        path.display()
    ));
    lease.commit(env.lib, JobState::Done, |tx, _| {
        let model = env.config.profile.speaker_embedding.clone();
        let thresholds = env.thresholds(&model).copied();
        let mut index = Index::open(tx.root())?;
        finish_meeting(tx, &mut index, id, &model, thresholds.as_ref()).map(|_| ())
    })?;
    let saved = env.lib.load_meeting(id)?;
    env.say(format_args!("{}: imported as {}", path.display(), saved.id));
    Ok(ImportOutcome {
        already_exists: false,
        ..ImportOutcome::existing(&saved, path)
    })
}

#[allow(clippy::too_many_arguments)]
fn run_import(
    env: &Env<'_>,
    models: &mut ModelSet,
    lease: &LeaseHandle,
    path: &Path,
    sha: &str,
    bytes: u64,
    handling: Handling,
    opts: &ImportOptions,
) -> Result<ImportOutcome> {
    let started = Instant::now();
    env.say(format_args!("{}: decoding", path.display()));
    let decoded = decode_16k(env.decoders, path)?;
    env.say(format_args!(
        "{}: analyzing {:.0} s",
        path.display(),
        decoded.duration_s()
    ));
    let model = models.embedder.model_id().to_owned();

    let started_at = opts
        .started_at
        .or(decoded.info.created_at)
        .or_else(|| file_mtime(path))
        .unwrap_or_else(broscribe_core::schema::now);
    let mut meeting = Meeting::new(env.lib.new_meeting_id()?, Source::Import, started_at);
    meeting.title = opts.title.clone().or_else(|| title_from_path(path));
    meeting.title_source = Some(if opts.title.is_some() {
        TitleSource::User
    } else {
        TitleSource::File
    });
    meeting.participants = opts.participants.clone();
    meeting.duration_s = decoded.duration_s();
    if let Some(lang) = opts.language.clone() {
        meeting.languages.push(LanguageSpan {
            lang,
            from_s: 0.0,
            to_s: decoded.duration_s(),
        });
    }
    meeting.hotwords = env.config.hotwords.clone();
    if models.asr.accepts_hotwords() {
        models.asr.set_hotwords(&crate::reprocess::hotwords(
            env.lib,
            &env.config.hotwords,
            &meeting,
        )?)?;
    }
    let analysis = analyze_with_languages(
        &decoded.samples,
        models,
        AnalyzeOptions::default(),
        &meeting.languages,
    )?;
    meeting.tracks.push(Track {
        id: TRACK_ID.to_owned(),
        role: TrackRole::Mixed,
    });
    meeting.speakers = analysis.speakers;
    meeting.segments = analysis.segments;
    meeting.models = models_used(env, &model, true);
    meeting.processing = Processing {
        status: ProcessingStatus::Analyzed,
        duration_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
        error: None,
        echo_report: None,
    };
    let ext = audio_ext(path);
    let job_dir = lease.job_dir();
    let mut journal = match handling {
        Handling::Move => Some(stage_move(env, lease, path, sha, bytes, &ext)?),
        Handling::Copy | Handling::Reference => None,
    };
    let staged = match &journal {
        Some(j) => Some(j.staged.clone()),
        None => stage_audio(lease, path, handling, &ext)?,
    };
    // A copy (this attempt's own file) must hold exactly the analysed content.
    if journal.is_none()
        && let Some(copy) = &staged
    {
        verify_staged_copy(copy, path, sha, bytes)?;
    }
    meeting.recording = Some(recording(path, sha, bytes, handling, &ext));

    let thresholds = env.thresholds(&model).copied();
    let id = meeting.id.clone();
    // A move stays unfinished in the queue until the original is gone.
    let state = if journal.is_some() {
        JobState::Running
    } else {
        JobState::Done
    };
    let finished = lease.commit(env.lib, state, |tx, _| {
        // Another process may have imported the same file meanwhile.
        if let Some((data_dir, session)) = &opts.setup_session
            && broscribe_core::config::Config::load(data_dir)?
                .setup
                .voice_session
                .as_ref()
                != Some(session)
        {
            return Err(Error::InvalidInput("setup session superseded".into()));
        }
        if let Some(m) = find_meeting_by_sha(tx.library(), sha, bytes)? {
            return Ok(Err(m));
        }
        let dir = tx.create_meeting_dir(&meeting)?;
        if let Some(j) = journal.as_mut() {
            j.dest = Some(dir.join(format!("audio.{ext}")));
            j.write(env.lib.root(), &job_dir)?;
        }
        place_audio(staged.as_deref(), &dir, &ext)?;
        if journal.is_some() {
            move_failpoint(MoveStep::Placed)?;
        }
        tx.save_meeting(&meeting)?;
        crate::batch::failpoint(crate::batch::FailPoint::ImportAfterSave)?;
        let mut index = Index::open(tx.root())?;
        let f = finish_meeting(tx, &mut index, &id, &model, thresholds.as_ref())?;
        if let Some(j) = journal.as_mut() {
            j.state = MoveState::Committed;
            j.write(env.lib.root(), &job_dir)?;
        }
        Ok(Ok(f))
    });
    let finished = match finished {
        Ok(finished) => finished,
        Err(error) => {
            // This attempt's own copy goes; a moved file is the user's only
            // copy and is left to the move's settlement.
            if journal.is_none()
                && let Some(copy) = &staged
            {
                let _ = remove_if_exists(copy);
            }
            return Err(error);
        }
    };
    let saved = env.lib.load_meeting(match &finished {
        Ok(_) => &id,
        Err(existing) => &existing.id,
    })?;
    if finished.is_err() {
        // Already in the library: the caller's file is never deleted.
        match &journal {
            Some(j) => {
                let kept = lease.commit(env.lib, JobState::Running, |_, _| {
                    rollback_move(j, &job_dir)
                })?;
                if let Some(kept) = kept {
                    return Err(Error::AlreadyExists(duplicate_conflict_message(
                        &j.source, &kept, &saved.id,
                    )));
                }
            }
            None => {
                if let Some(p) = staged {
                    let _ = remove_if_exists(&p);
                }
            }
        }
        return Ok(ImportOutcome::existing(&saved, path));
    }
    if let Some(j) = journal.as_mut() {
        move_failpoint(MoveStep::Committed)?;
        // The original goes only while this attempt still holds the job.
        lease.commit(env.lib, JobState::Done, |_, _| {
            finish_source(env, j, &job_dir)
        })?;
    }
    env.say(format_args!("{}: imported as {}", path.display(), saved.id));
    Ok(ImportOutcome {
        already_exists: false,
        ..ImportOutcome::existing(&saved, path)
    })
}

/// Model IDs of the active profile (the embedding model as loaded).
pub(crate) fn models_used(env: &Env<'_>, embedding_model: &str, asr: bool) -> ModelsUsed {
    let p = &env.config.profile;
    ModelsUsed {
        vad: asr.then(|| p.vad.clone()),
        asr: asr.then(|| p.asr.clone()),
        diarization: Some(p.diarization.clone()),
        speaker_embedding: Some(embedding_model.to_owned()),
    }
}

pub(crate) fn recording(
    path: &Path,
    sha: &str,
    bytes: u64,
    handling: Handling,
    ext: &str,
) -> Recording {
    Recording {
        state: RecordingState::Available,
        handling,
        path: match handling {
            Handling::Reference => path.to_owned(),
            _ => PathBuf::from(format!("audio.{ext}")),
        },
        original_path: Some(path.to_owned()),
        sha256: sha.to_owned(),
        bytes,
        source_sha256: None,
        sources: Vec::new(),
        retention_override: None,
        delete_after: None,
    }
}

/// Where the lease holder's attempt stages its audio in the job folder:
/// `audio.<lease token>.<ext>`. Each attempt owns its own file, so a holder
/// that lost the job can never overwrite or remove the file of the attempt
/// that took it over (or the other way round).
pub(crate) fn staged_audio_path(lease: &LeaseHandle, ext: &str) -> PathBuf {
    lease
        .job_dir()
        .join(format!("audio.{}.{ext}", lease.token()))
}

/// For `copy`: copies the file into this attempt's staging path
/// ([`staged_audio_path`]) outside the lock, through `<staged>.part`, so the
/// commit only renames it into the meeting folder. A failed copy leaves
/// nothing of this attempt behind.
pub(crate) fn stage_audio(
    lease: &LeaseHandle,
    path: &Path,
    handling: Handling,
    ext: &str,
) -> Result<Option<PathBuf>> {
    if handling != Handling::Copy {
        return Ok(None);
    }
    let staged = staged_audio_path(lease, ext);
    let part = part_path(&staged);
    let copied = fs::copy(path, &part)
        .map_err(|e| Error::io_at(&part, e))
        .and_then(|_| fs::rename(&part, &staged).map_err(|e| Error::io_at(&staged, e)));
    if let Err(error) = copied {
        let _ = remove_if_exists(&part);
        return Err(error);
    }
    Ok(Some(staged))
}

/// Checks that this attempt's staged copy holds `sha`/`bytes` (what was
/// analysed); otherwise removes that copy and fails, never publishing audio
/// that differs from the transcript.
pub(crate) fn verify_staged_copy(copy: &Path, source: &Path, sha: &str, bytes: u64) -> Result<()> {
    let verified = file_sha256(copy).is_ok_and(|(got, n)| got == sha && n == bytes);
    if verified {
        return Ok(());
    }
    remove_if_exists(copy)?;
    Err(Error::Internal(format!(
        "the copy of {} does not match what was analysed; import it again",
        source.display()
    )))
}

/// Moves staged audio into the meeting folder (inside the commit).
pub(crate) fn place_audio(staged: Option<&Path>, dir: &Path, ext: &str) -> Result<()> {
    if let Some(src) = staged {
        let dst = dir.join(format!("audio.{ext}"));
        std::fs::rename(src, &dst).map_err(|e| Error::io_at(&dst, e))?;
    }
    Ok(())
}

/// Move journal files of an import job's folder; defined in core so the
/// queue and meeting deletion recognise a folder holding the only copy.
pub use broscribe_core::queue::{MOVE_CONFLICT, MOVE_JOURNAL};

/// Progress of a move; each state is written before the next step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveState {
    /// Nothing moved yet.
    Planned,
    /// Copied into the job folder (cross-volume), not verified yet.
    Copied,
    /// The job folder holds the verified file (renamed or copied).
    Verified,
    /// The meeting is published and owns the file.
    Committed,
    /// The original is gone (or kept because it changed).
    SourceRemoved,
}

/// `move.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MoveJournal {
    /// The original, canonical path.
    pub source: PathBuf,
    /// The file inside the job folder. Stored relative to the job folder
    /// (moving the library keeps it), absolute in memory ([`Self::load`]).
    pub staged: PathBuf,
    /// Its place in the meeting folder; written inside the commit before
    /// the file goes there. Stored relative to the library folder (moving
    /// the library keeps it), absolute in memory ([`Self::load`]); earlier
    /// builds stored it absolute, which is read as is.
    #[serde(default)]
    pub dest: Option<PathBuf>,
    pub sha256: String,
    pub bytes: u64,
    /// Renamed on the same volume (no copy left at the source).
    #[serde(default)]
    pub renamed: bool,
    pub state: MoveState,
    /// The original's modification time, restored if the move is undone by
    /// copying (a forward copy does not keep it on every platform).
    #[serde(default)]
    pub modified: Option<std::time::SystemTime>,
}

impl MoveJournal {
    /// Reads journal `name` ([`MOVE_JOURNAL`] or [`MOVE_CONFLICT`]) of
    /// `job_dir`, with the staged path resolved in that folder and the
    /// destination in the library folder `root`.
    pub fn load(root: &Path, job_dir: &Path, name: &str) -> Result<Self> {
        let mut j: Self = fsutil::read_json(&job_dir.join(name))?;
        j.staged = queue::in_job_dir(job_dir, std::mem::take(&mut j.staged));
        // Joining keeps an absolute path from an earlier build unchanged.
        j.dest = j.dest.map(|d| root.join(d));
        Ok(j)
    }

    fn write(&self, root: &Path, job_dir: &Path) -> Result<()> {
        let stored = Self {
            staged: queue::relative_to_job_dir(job_dir, &self.staged),
            dest: self.dest.as_ref().map(|d| {
                d.strip_prefix(root)
                    .map_or_else(|_| d.clone(), Path::to_owned)
            }),
            ..self.clone()
        };
        fsutil::write_json_atomic(&job_dir.join(MOVE_JOURNAL), &stored)
    }
}

/// An interrupted move found by [`recover_moves`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MoveRecovery {
    pub source: PathBuf,
    /// The meeting that owns the file when the move was completed.
    pub meeting_id: Option<MeetingId>,
    pub outcome: MoveOutcome,
    /// Where the moved audio was kept ([`MoveOutcome::Conflict`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept: Option<PathBuf>,
    /// The interrupted import was one file of a folder import (a batch),
    /// which never enriches on its own.
    pub folder: bool,
}

impl MoveRecovery {
    fn conflict_note(&self) -> Option<String> {
        self.kept
            .as_ref()
            .map(|kept| conflict_message(&self.source, kept))
    }
}

/// Where the moved audio of an unpublished move went, and what the user does.
fn conflict_message(source: &Path, kept: &Path) -> String {
    format!(
        "the move was interrupted before the meeting was saved and {} now holds another file, \
         so the imported audio was kept at {}; {MOVE_IT_OUT}",
        source.display(),
        kept.display()
    )
}

/// A moved file that turned out to be in the library already (meeting `id`)
/// and could not be put back.
fn duplicate_conflict_message(source: &Path, kept: &Path, id: &MeetingId) -> String {
    format!(
        "{} is already in the library as {id}, and it could not be put back because another \
         file now holds that path, so it was kept at {}; {MOVE_IT_OUT}",
        source.display(),
        kept.display()
    )
}

const MOVE_IT_OUT: &str = "move that file out of the library folder before importing it again";

/// The audio a conflicted move kept in `job_dir` ([`MOVE_CONFLICT`]), while
/// it is there.
fn kept_conflict(root: &Path, job_dir: &Path) -> Option<PathBuf> {
    let j = MoveJournal::load(root, job_dir, MOVE_CONFLICT).ok()?;
    j.staged.is_file().then_some(j.staged)
}

/// Why an import source the library owns is refused ([`refuse_library_source`]):
/// the advice without the source's or the library's path.
pub const LIBRARY_SOURCE_REFUSED: &str = "it is inside the library folder, in a folder the \
     library manages; move it out of the library folder before importing it";

/// Refuses an import source the library owns: anything in its internal
/// folders (`.queue/`, `.index/` and every other dot-folder, `people/`) or in
/// a meeting folder, such as audio a conflicted move kept. Importing it would
/// stage over, move or delete it. Other files under the library folder (an
/// inbox the user keeps there) import normally.
fn refuse_library_source(env: &Env<'_>, path: &Path) -> Result<()> {
    let Ok(source) = fs::canonicalize(path) else {
        return Ok(());
    };
    let root = fs::canonicalize(env.lib.root()).unwrap_or_else(|_| env.lib.root().to_owned());
    let Ok(rel) = source.strip_prefix(&root) else {
        return Ok(());
    };
    let internal = rel.components().next().is_some_and(|c| {
        let name = c.as_os_str().to_string_lossy();
        name.starts_with('.') || name == paths::PEOPLE_DIR
    });
    let in_meeting = source
        .ancestors()
        .skip(1)
        .take_while(|dir| *dir != root)
        .any(|dir| dir.join(paths::MEETING_JSON).is_file());
    if internal || in_meeting {
        // Neither path: the caller named the source, and the app shows a
        // refusal's words, which must not name the user's folders.
        return Err(Error::InvalidInput(LIBRARY_SOURCE_REFUSED.into()));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveOutcome {
    /// The meeting was published: the original was removed.
    Completed,
    /// The meeting was not published: the file is back at its original
    /// place (or the extra copy is gone) and the job is marked failed.
    RolledBack,
    /// The meeting was not published and another file now holds the
    /// original path: the moved audio stays in the job folder and the job is
    /// marked failed with a message naming it.
    Conflict,
}

/// Steps a simulated crash can stop at (tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MoveStep {
    /// Journal written, nothing moved.
    Planned,
    /// Renamed or copied, journal not updated.
    Transferred,
    /// Copy journaled, not verified.
    Copied,
    /// Verified, before the commit.
    Verified,
    /// Inside the commit: file in the meeting folder, no `meeting.json`.
    Placed,
    /// Meeting published, original still there.
    Committed,
    /// Original deleted, journal not updated.
    SourceDeleted,
}

#[cfg(test)]
thread_local! {
    pub(crate) static MOVE_FAIL_AT: std::cell::Cell<Option<MoveStep>> =
        const { std::cell::Cell::new(None) };
    /// Treat every move as cross-volume (copy, verify, delete).
    pub(crate) static FORCE_COPY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn move_failpoint(step: MoveStep) -> Result<()> {
    #[cfg(test)]
    if MOVE_FAIL_AT.get() == Some(step) {
        MOVE_FAIL_AT.set(None);
        return Err(Error::Internal(format!("failpoint {step:?}")));
    }
    let _ = step;
    Ok(())
}

fn is_move_failpoint(e: &Error) -> bool {
    matches!(e, Error::Internal(m) if m.starts_with("failpoint"))
}

fn force_copy() -> bool {
    #[cfg(test)]
    {
        FORCE_COPY.get()
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// `<path>.part`, for copies not yet complete.
fn part_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

/// Copies `from` to `to` through `<to>.part`, synced, then renamed. A failed
/// copy removes its own `.part`.
fn copy_synced(from: &Path, to: &Path) -> Result<()> {
    let part = part_path(to);
    let copied = (|| -> std::io::Result<()> {
        let mut source = File::open(from)?;
        let permissions = source.metadata()?.permissions();
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
            // Restrict access at creation: a reader opened before chmod
            // would retain access to the private audio written afterward.
            options.mode(permissions.mode());
        }
        let mut dest = options.open(&part)?;
        // Keep the writable handle for sync_all: Windows cannot flush a
        // read-only handle. Set the copied permissions before writing any
        // content; even a read-only source remains flushable through dest.
        dest.set_permissions(permissions)?;
        std::io::copy(&mut source, &mut dest)?;
        dest.sync_all()
    })()
    .map_err(|e| Error::io_at(&part, e))
    .and_then(|()| fs::rename(&part, to).map_err(|e| Error::io_at(to, e)));
    if let Err(error) = copied {
        let _ = remove_if_exists(&part);
        return Err(error);
    }
    if let Some(dir) = to.parent() {
        fsutil::sync_dir(dir)?;
    }
    Ok(())
}

/// Journals the move, then brings the file into this attempt's staging path
/// ([`staged_audio_path`]): a rename on the same volume, else a synced copy
/// verified against `sha`. The original stays in place in the copy case.
///
/// Every journal write and the rename run under the library lock with the
/// lease checked ([`LeaseHandle::commit`]): a holder that lost the job never
/// journals over, nor moves the original away from, the attempt that took it
/// over. A journal (or conflict) another attempt left is never overwritten:
/// it is settled by recovery first. Only the cross-volume copy runs outside
/// the lock, into this attempt's own file, which a failed verification removes.
fn stage_move(
    env: &Env<'_>,
    lease: &LeaseHandle,
    source: &Path,
    sha: &str,
    bytes: u64,
    ext: &str,
) -> Result<MoveJournal> {
    let root = env.lib.root();
    let job_dir = lease.job_dir();
    let mut j = MoveJournal {
        source: source.to_owned(),
        staged: staged_audio_path(lease, ext),
        dest: None,
        sha256: sha.to_owned(),
        bytes,
        renamed: false,
        state: MoveState::Planned,
        modified: fs::metadata(source).and_then(|m| m.modified()).ok(),
    };
    lease.commit(env.lib, JobState::Running, |_, _| {
        if [MOVE_JOURNAL, MOVE_CONFLICT]
            .iter()
            .any(|name| job_dir.join(name).exists())
        {
            return Err(Error::AlreadyExists(format!(
                "job {} holds an unsettled move; it is recovered before another starts",
                lease.id()
            )));
        }
        j.write(root, &job_dir)
    })?;
    move_failpoint(MoveStep::Planned)?;
    let renamed = !force_copy()
        && lease.commit(env.lib, JobState::Running, |_, _| {
            match fs::rename(source, &j.staged) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::CrossesDevices => return Ok(false),
                Err(e) => return Err(Error::io_at(source, e)),
            }
            fsutil::sync_dir(&job_dir)?;
            if let Some(dir) = source.parent() {
                fsutil::sync_dir(dir)?;
            }
            move_failpoint(MoveStep::Transferred)?;
            // A rename keeps the content.
            j.renamed = true;
            j.state = MoveState::Verified;
            j.write(root, &job_dir)?;
            Ok(true)
        })?;
    if !renamed {
        copy_synced(source, &j.staged)?;
        move_failpoint(MoveStep::Transferred)?;
        let journaled = lease.commit(env.lib, JobState::Running, |_, _| {
            j.state = MoveState::Copied;
            j.write(root, &job_dir)
        });
        if let Err(error) = journaled {
            // This attempt's own copy; the original is still in place.
            let _ = remove_if_exists(&j.staged);
            return Err(error);
        }
        move_failpoint(MoveStep::Copied)?;
        let verified = file_sha256(&j.staged).is_ok_and(|(got, n)| got == sha && n == bytes);
        if !verified {
            remove_if_exists(&j.staged)?;
            // Its journal goes too, while this attempt still holds the job
            // and the journal is still the one naming its copy.
            let staged = j.staged.clone();
            let _ = lease.commit(env.lib, JobState::Running, |_, _| {
                match MoveJournal::load(root, &job_dir, MOVE_JOURNAL) {
                    Ok(current) if current.staged == staged => {
                        remove_if_exists(&job_dir.join(MOVE_JOURNAL))
                    }
                    _ => Ok(()),
                }
            });
            return Err(Error::Internal(format!(
                "the copy of {} does not match the original; it was left in place",
                source.display()
            )));
        }
        lease.commit(env.lib, JobState::Running, |_, _| {
            j.state = MoveState::Verified;
            j.write(root, &job_dir)
        })?;
    }
    move_failpoint(MoveStep::Verified)?;
    Ok(j)
}

/// Whether `path` holds exactly this content (SHA-256 and size).
fn holds_content(path: &Path, sha: &str, bytes: u64) -> bool {
    file_sha256(path).is_ok_and(|(got, n)| got == sha && n == bytes)
}

/// After publishing: deletes the original of a copied move only when the
/// meeting's file hashes to the imported content (a same-size but damaged
/// copy never counts) and the original still has it; otherwise keeps it.
/// Then the move is settled: once its audio has left the job folder nothing
/// there is anyone's only copy, so the journal goes and the finished job's
/// folder can be removed. Audio still in the job folder keeps its journal
/// (`source_removed`), which protects it. Run it under the move's lease.
fn finish_source(env: &Env<'_>, j: &mut MoveJournal, job_dir: &Path) -> Result<()> {
    if j.state != MoveState::SourceRemoved && !j.renamed && j.source.exists() {
        let placed = j
            .dest
            .as_deref()
            .is_some_and(|d| holds_content(d, &j.sha256, j.bytes));
        if !placed {
            env.say(format_args!(
                "{}: left in place (the library's copy could not be verified)",
                j.source.display()
            ));
        } else if holds_content(&j.source, &j.sha256, j.bytes) {
            fs::remove_file(&j.source).map_err(|e| Error::io_at(&j.source, e))?;
            if let Some(dir) = j.source.parent() {
                fsutil::sync_dir(dir)?;
            }
        } else {
            env.say(format_args!(
                "{}: left in place (it differs from the library's copy)",
                j.source.display()
            ));
        }
    }
    move_failpoint(MoveStep::SourceDeleted)?;
    j.state = MoveState::SourceRemoved;
    if j.staged.exists() {
        return j.write(env.lib.root(), job_dir);
    }
    remove_if_exists(&part_path(&j.staged))?;
    remove_if_exists(&job_dir.join(MOVE_JOURNAL))?;
    fsutil::sync_dir(job_dir)?;
    Ok(())
}

/// Undoes an unpublished move: a file already placed in a meeting folder
/// without `meeting.json` goes back to the job folder (and the empty folder
/// is removed); a renamed original goes back to its place; a copy is
/// deleted. Removes the journal. When another file now holds the original
/// path, nothing is overwritten: the staged audio stays, the journal becomes
/// [`MOVE_CONFLICT`] and the staged path is returned.
fn rollback_move(j: &MoveJournal, job_dir: &Path) -> Result<Option<PathBuf>> {
    if let Some(dest) = &j.dest
        && !j.staged.exists()
        && dest.is_file()
        && let Some(meeting_dir) = dest.parent()
        && !meeting_dir.join(paths::MEETING_JSON).exists()
    {
        fs::rename(dest, &j.staged).map_err(|e| Error::io_at(dest, e))?;
        let _ = fs::remove_dir(meeting_dir);
    }
    if j.staged.exists() {
        // The original (still there after a copy, or already linked back by
        // an interrupted restore) makes the staged file redundant.
        let source_matches = j.source.is_file() && holds_content(&j.source, &j.sha256, j.bytes);
        if source_matches {
            remove_if_exists(&j.staged)?;
        } else if !restore_staged(j)? {
            let conflict = job_dir.join(MOVE_CONFLICT);
            fs::rename(job_dir.join(MOVE_JOURNAL), &conflict)
                .map_err(|e| Error::io_at(&conflict, e))?;
            fsutil::sync_dir(job_dir)?;
            return Ok(Some(j.staged.clone()));
        }
    }
    remove_if_exists(&part_path(&j.staged))?;
    remove_if_exists(&job_dir.join(MOVE_JOURNAL))?;
    Ok(None)
}

/// Puts the staged file back at the original path without ever replacing
/// a file there; `false` when one is there. On the same volume a hard link
/// (atomic, keeps the file's metadata); otherwise a verified copy with the
/// original modification time, linked into place. The staged file is
/// removed once the original is back.
fn restore_staged(j: &MoveJournal) -> Result<bool> {
    if j.renamed {
        match fs::hard_link(&j.staged, &j.source) {
            Ok(()) => {
                if let Some(dir) = j.source.parent() {
                    fsutil::sync_dir(dir)?;
                }
                remove_if_exists(&j.staged)?;
                return Ok(true);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
            // No hard links here: fall back to a copy.
            Err(_) => {}
        }
    }
    let part = restore_part_path(&j.source)?;
    let restored = copy_back(j, &part);
    let _ = fs::remove_file(&part);
    if restored? {
        if let Some(dir) = j.source.parent() {
            fsutil::sync_dir(dir)?;
        }
        remove_if_exists(&j.staged)?;
        return Ok(true);
    }
    Ok(false)
}

/// `<dir>/.<name>.broscribe-restore`, beside the original.
fn restore_part_path(source: &Path) -> Result<PathBuf> {
    let (Some(dir), Some(name)) = (source.parent(), source.file_name()) else {
        return Err(Error::InvalidInput(format!(
            "cannot restore to {}",
            source.display()
        )));
    };
    let mut part = std::ffi::OsString::from(".");
    part.push(name);
    part.push(".broscribe-restore");
    Ok(dir.join(part))
}

/// Copies the staged file to `part` (synced, verified, original mtime) and
/// links it to the original path; `false` when a file is already there.
fn copy_back(j: &MoveJournal, part: &Path) -> Result<bool> {
    let mut from = File::open(&j.staged).map_err(|e| Error::io_at(&j.staged, e))?;
    let modified = j
        .modified
        .or_else(|| from.metadata().and_then(|m| m.modified()).ok());
    let mut to = File::create(part).map_err(|e| Error::io_at(part, e))?;
    std::io::copy(&mut from, &mut to).map_err(|e| Error::io_at(part, e))?;
    if let Some(t) = modified {
        to.set_modified(t).map_err(|e| Error::io_at(part, e))?;
    }
    to.sync_all().map_err(|e| Error::io_at(part, e))?;
    drop(to);
    if file_sha256(part)? != (j.sha256.clone(), j.bytes) {
        return Err(Error::Internal(format!(
            "the restored copy of {} failed verification; it stays in the job folder",
            j.source.display()
        )));
    }
    match fs::hard_link(part, &j.source) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(_) => {
            // No hard links on this volume: create the original anew, never
            // over an existing file.
            let mut out = match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&j.source)
            {
                Ok(f) => f,
                Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
                Err(e) => return Err(Error::io_at(&j.source, e)),
            };
            let mut from = File::open(part).map_err(|e| Error::io_at(part, e))?;
            std::io::copy(&mut from, &mut out).map_err(|e| Error::io_at(&j.source, e))?;
            if let Some(t) = modified {
                out.set_modified(t)
                    .map_err(|e| Error::io_at(&j.source, e))?;
            }
            out.sync_all().map_err(|e| Error::io_at(&j.source, e))?;
            Ok(true)
        }
    }
}

/// Finishes the move journaled in the job folder of `lease`: completes it
/// when its meeting was published, else rolls it back. `None` without a
/// journal. `folder`: the import was one file of a folder import.
///
/// Runs under the library lock with the lease checked, so a holder that lost
/// the job never settles (or races) the move of the attempt that took it
/// over, and no takeover can start while this one settles.
fn settle_move(env: &Env<'_>, lease: &LeaseHandle, folder: bool) -> Result<Option<MoveRecovery>> {
    lease.commit(env.lib, JobState::Running, |_, job_dir| {
        settle_held_move(env, job_dir, folder)
    })
}

fn settle_held_move(env: &Env<'_>, job_dir: &Path, folder: bool) -> Result<Option<MoveRecovery>> {
    let mut j = match MoveJournal::load(env.lib.root(), job_dir, MOVE_JOURNAL) {
        Ok(j) => j,
        Err(Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let published = find_meeting_by_sha(env.lib, &j.sha256, j.bytes)?.filter(|m| {
        m.recording.as_ref().is_some_and(|r| {
            r.handling == Handling::Move && r.original_path.as_deref() == Some(j.source.as_path())
        })
    });
    match published {
        Some(m) => {
            finish_source(env, &mut j, job_dir)?;
            Ok(Some(MoveRecovery {
                source: j.source,
                meeting_id: Some(m.id),
                outcome: MoveOutcome::Completed,
                kept: None,
                folder,
            }))
        }
        None => {
            let kept = rollback_move(&j, job_dir)?;
            Ok(Some(MoveRecovery {
                source: j.source,
                meeting_id: None,
                outcome: if kept.is_some() {
                    MoveOutcome::Conflict
                } else {
                    MoveOutcome::RolledBack
                },
                kept,
                folder,
            }))
        }
    }
}

/// Finishes every interrupted move of the library (import jobs with a
/// journal and no live holder): completed when its meeting was published,
/// rolled back otherwise. Idempotent; runs at the start of every import.
/// A move that cannot be settled fails its own job (with a message naming
/// where its audio is) and never blocks the others or the import at hand.
/// A conflicted move fails its job for good, keeping its audio
/// ([`broscribe_core::queue::Job::keeps`]), also when a crash (or an older
/// build) left that unrecorded.
pub fn recover_moves(env: &Env<'_>) -> Result<Vec<MoveRecovery>> {
    let queue = Queue::open(env.lib.root())?;
    let mut out = Vec::new();
    for job in queue.list(IMPORT_KIND)? {
        let dir = queue.job_dir(&job.id);
        match kept_conflict(env.lib.root(), &dir) {
            Some(kept) if job.keeps.is_none() => {
                if let Ok(j) = MoveJournal::load(env.lib.root(), &dir, MOVE_CONFLICT) {
                    let message = conflict_message(&j.source, &kept);
                    if env.lib.write(|tx| {
                        queue.keep_file(tx, &job.id, kept.clone(), message.clone(), env.probe)
                    })? {
                        env.say(format_args!("job {}: {message}", job.id));
                    }
                }
                continue;
            }
            Some(_) => continue,
            None => {
                // The user moved the kept audio away: the conflict is over,
                // and its job has nothing left to protect or retry.
                remove_if_exists(&dir.join(MOVE_CONFLICT))?;
                if job.keeps.is_some() && !job.keeps_file() {
                    if env
                        .lib
                        .write(|tx| queue.remove_released(tx, &job.id, env.probe))?
                    {
                        env.say(format_args!(
                            "job {}: its kept audio was moved away; job removed",
                            job.id
                        ));
                    }
                    continue;
                }
            }
        }
        if job.state == JobState::Done || !dir.join(MOVE_JOURNAL).is_file() {
            continue;
        }
        let Some(lease) = queue.claim(env.lib, &job.id, env.probe)? else {
            note_stranded_move(env, &queue, &job.id)?;
            continue;
        };
        let failure = match settle_move(env, &lease, is_folder_import(&job.input)) {
            Ok(Some(r)) => {
                let failure = match r.outcome {
                    MoveOutcome::Completed => {
                        let id = r.meeting_id.as_ref().expect("completed move has a meeting");
                        match resume_import(env, &lease, &r.source, id) {
                            Ok(_) => {
                                env.say(format_args!("{}: finished moving", r.source.display()));
                                None
                            }
                            Err(e) => Some(format!(
                                "finishing the moved import of {} failed: {e}",
                                r.source.display()
                            )),
                        }
                    }
                    MoveOutcome::RolledBack => {
                        env.say(format_args!(
                            "{}: interrupted move undone",
                            r.source.display()
                        ));
                        Some(
                            "move interrupted before the meeting was saved; the file was put back"
                                .to_owned(),
                        )
                    }
                    MoveOutcome::Conflict => r.conflict_note(),
                };
                if failure.is_none() || r.outcome != MoveOutcome::Completed {
                    out.push(r);
                }
                failure
            }
            Ok(None) => None,
            Err(e) => Some(format!("recovering an interrupted move failed: {e}")),
        };
        let failed = failure.is_some();
        if let Some(message) = failure {
            // A clean rollback was already reported above.
            if !message.starts_with("move interrupted") {
                env.say(format_args!("job {}: {message}", job.id));
            }
            let _ = match kept_conflict(env.lib.root(), &lease.job_dir()) {
                Some(kept) => lease.fail_keeping(env.lib, message, kept),
                None => lease.fail(env.lib, message),
            };
        }
        lease.release(env.lib)?;
        if failed {
            // That failure may have been its last attempt.
            note_stranded_move(env, &queue, &job.id)?;
        }
    }
    if out.iter().any(|r| r.outcome == MoveOutcome::Completed) {
        env.lib.write(|tx| queue.remove_finished(tx, IMPORT_KIND))?;
    }
    Ok(out)
}

/// A journaled move whose job no claim will run again (failed with no
/// attempts left, by its last settlement or just now by the refused claim):
/// its error names where the audio is, so the user can put it back by hand.
fn note_stranded_move(env: &Env<'_>, queue: &Queue, id: &JobId) -> Result<()> {
    let dir = queue.job_dir(id);
    let Ok(j) = MoveJournal::load(env.lib.root(), &dir, MOVE_JOURNAL) else {
        return Ok(());
    };
    let Some(at) = [Some(&j.staged), j.dest.as_ref()]
        .into_iter()
        .flatten()
        .find(|p| p.is_file())
    else {
        return Ok(());
    };
    let note = format!(
        "the move of {} was left unfinished and its audio is at {}; move it back or out of \
         the library folder",
        j.source.display(),
        at.display()
    );
    let needs_note = |job: &Job| {
        job.state == JobState::Failed
            && job.attempts >= MAX_ATTEMPTS
            && job.lease.is_none()
            && !job.error.as_deref().is_some_and(|e| e.contains(&note))
    };
    // Most recoveries find the note already there: check without the lock.
    match queue.get(id) {
        Ok(job) if needs_note(&job) => {}
        Ok(_) | Err(Error::NotFound(_)) => return Ok(()),
        Err(e) => return Err(e),
    }
    env.lib.write(|tx| match queue.get(id) {
        Ok(job) if needs_note(&job) => {
            let error = match job.error {
                Some(e) => format!("{e}; {note}"),
                None => note.clone(),
            };
            queue.set_state(tx, id, JobState::Failed, Some(error))
        }
        Ok(_) | Err(Error::NotFound(_)) => Ok(()),
        Err(e) => Err(e),
    })
}

/// Imports every media file directly inside `dir` (by extension, sorted by
/// name) as its own meeting. A failing file is reported and the rest continue.
pub fn import_folder(
    env: &Env<'_>,
    models: &mut ModelSet,
    dir: &Path,
    opts: &ImportOptions,
) -> Result<FolderReport> {
    import_folder_with_recovery(env, models, dir, opts, true)
}

/// Imports media in a folder without recovering other import jobs or moving originals.
pub fn import_media_folder_without_moves(
    env: &Env<'_>,
    models: &mut ModelSet,
    dir: &Path,
    opts: &ImportOptions,
) -> Result<FolderReport> {
    check_handling(opts.handling.unwrap_or(env.config.import_handling))?;
    import_folder_with_recovery(env, models, dir, opts, false)
}

fn import_folder_with_recovery(
    env: &Env<'_>,
    models: &mut ModelSet,
    dir: &Path,
    opts: &ImportOptions,
    recover_interrupted_moves: bool,
) -> Result<FolderReport> {
    let (files, _) = folder_media(dir).map_err(|e| Error::io_at(dir, e))?;
    let mut report = FolderReport::default();
    // Flags that name one meeting do not apply to every file of a folder.
    let per_file = ImportOptions {
        title: None,
        ..opts.clone()
    };
    for f in files
        .into_iter()
        .filter(|path| recover_interrupted_moves || is_media_source(path))
    {
        match import(
            env,
            models,
            &f,
            Request::Direct(&per_file, true),
            &mut report.completed_moves,
            recover_interrupted_moves,
        ) {
            Ok(o) => report.imported.push(o),
            Err(e) => {
                env.say(format_args!("{}: failed: {e}", f.display()));
                report.failed.push(ImportFailure {
                    path: f,
                    code: e.code(),
                    message: e.to_string(),
                });
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecognitionLog {
        terms: Vec<String>,
        calls: Vec<(Option<String>, Vec<String>)>,
    }
    struct CheckingAsr(Arc<Mutex<RecognitionLog>>);
    impl broscribe_models::Asr for CheckingAsr {
        fn transcribe(&self, _: &[f32]) -> Result<broscribe_models::AsrOutput> {
            panic!("language-aware recognition required")
        }
        fn transcribe_with(
            &self,
            _: &[f32],
            lang: Option<&str>,
        ) -> Result<broscribe_models::AsrOutput> {
            let mut log = self.0.lock().unwrap();
            let terms = log.terms.clone();
            log.calls.push((lang.map(str::to_owned), terms));
            Ok(broscribe_models::AsrOutput {
                text: "synthetic".into(),
                tokens: Vec::new(),
            })
        }
        fn accepts_hotwords(&self) -> bool {
            true
        }
        fn set_hotwords(&mut self, terms: &[String]) -> Result<()> {
            self.0.lock().unwrap().terms = terms.to_vec();
            Ok(())
        }
    }

    #[test]
    fn first_recognition_honors_languages_and_refreshes_hotwords_for_every_import() {
        use crate::batch::{BatchOptions, import_manifest};
        let mut fx = Fixture::new();
        fx.config.default_language = Some("pt".into());
        fx.config.hotwords = vec!["TermOne".into()];
        let mut set = models();
        let log = Arc::new(Mutex::new(RecognitionLog::default()));
        set.asr = Box::new(CheckingAsr(log.clone()));
        let file = fx.wav("single.wav", &[(Some(0), 3.0)]);
        import_file(
            &fx.env(),
            &mut set,
            &file,
            &ImportOptions {
                language: Some("fr".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let calls = std::mem::take(&mut log.lock().unwrap().calls);
        assert!(!calls.is_empty());
        assert!(
            calls
                .iter()
                .all(|(lang, terms)| lang.as_deref() == Some("fr") && terms == &["TermOne"])
        );
        fx.config.hotwords = vec!["TermTwo".into()];
        let folder = fx.input.join("folder");
        fs::create_dir(&folder).unwrap();
        crate::testutil::write_wav(
            &folder.join("folder.wav"),
            16000,
            &crate::testutil::conversation(&[(Some(1), 4.0)]),
        );
        import_folder(&fx.env(), &mut set, &folder, &ImportOptions::default()).unwrap();
        let calls = std::mem::take(&mut log.lock().unwrap().calls);
        assert!(!calls.is_empty());
        assert!(
            calls
                .iter()
                .all(|(lang, terms)| lang.as_deref() == Some("pt") && terms == &["TermTwo"])
        );
        for version in [1, 2] {
            let file = fx.wav(
                &format!("manifest-{version}.wav"),
                &[(Some(version as usize), 5.0)],
            );
            let mut entry = serde_json::json!({"audio_path":file,"language":"de","participants":[{"name":format!("Guest{version}")}]});
            if version == 2 {
                entry["sources"] = serde_json::json!([{"path":file,"role":"participant"}]);
            }
            let manifest = crate::manifest::parse(&serde_json::json!({"format":crate::manifest::FORMAT,"version":version,"meetings":[entry]}).to_string()).unwrap();
            let result = import_manifest(
                &fx.env(),
                &mut set,
                &manifest,
                &BatchOptions {
                    handling: Some(Handling::Reference),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(result.failed.is_empty(), "{:?}", result.failed);
            let calls = std::mem::take(&mut log.lock().unwrap().calls);
            assert!(!calls.is_empty());
            assert!(
                calls
                    .iter()
                    .all(|(lang, terms)| lang.as_deref() == Some("de")
                        && terms.contains(&format!("Guest{version}"))
                        && !terms.contains(&format!("Guest{}", 3 - version)))
            );
        }
    }

    #[test]
    fn queued_import_persists_resolved_options_before_analysis() {
        let mut fx = Fixture::new();
        fx.config.default_language = Some("pt".into());
        let file = fx.wav("queued.wav", &[(Some(0), 3.0)]);
        let opts = ImportOptions {
            handling: Some(Handling::Reference),
            title: Some("Chosen".into()),
            participants: vec![Participant {
                name: Some("Guest Example".into()),
                email: None,
                is_self: false,
                evidence: Vec::new(),
                person_id: None,
            }],
            ..Default::default()
        };
        crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::ImportAfterSave));
        import_file(&fx.env(), &mut models(), &file, &opts).unwrap_err();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let job = queue.list(IMPORT_KIND).unwrap().remove(0);
        assert_eq!(job.input["handling"], "reference");
        assert_eq!(job.input["language"], "pt");
        assert_eq!(job.input["title"], "Chosen");
        assert!(job.input["started_at"].as_str().is_some());
        assert_eq!(job.input["participants"][0]["name"], "Guest Example");
        // The enqueue used to finish a saved meeting also snapshots its own
        // options, not the config changed since the original attempt.
        fx.config.import_handling = Handling::Move;
        fx.config.default_language = Some("de".into());
        crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::FinishBeforeIndex));
        import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap_err();
        let resumed = queue.get(&job.id).unwrap();
        for key in [
            "handling",
            "language",
            "title",
            "started_at",
            "participants",
        ] {
            assert_eq!(resumed.input[key], job.input[key], "{key}");
        }
    }

    #[test]
    fn rollback_conflict_fails_only_its_job_and_keeps_the_audio() {
        for copy in [false, true] {
            let fx = Fixture::new();
            let file = fx.wav("conflict.wav", &[(Some(0), 3.0)]);
            FORCE_COPY.set(copy);
            MOVE_FAIL_AT.set(Some(MoveStep::Verified));
            import_file(&fx.env(), &mut models(), &file, &move_opts()).unwrap_err();
            MOVE_FAIL_AT.set(None);
            FORCE_COPY.set(false);
            fs::write(&file, b"replacement").unwrap();
            let queue = Queue::open(fx.lib.root()).unwrap();
            let job = queue.list(IMPORT_KIND).unwrap().remove(0);
            let dir = queue.job_dir(&job.id);
            let journal = MoveJournal::load(fx.lib.root(), &dir, MOVE_JOURNAL).unwrap();

            let recovered = recover_moves(&fx.env()).unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].outcome, MoveOutcome::Conflict);
            assert_eq!(recovered[0].kept.as_ref(), Some(&journal.staged));
            assert_eq!(fs::read(&file).unwrap(), b"replacement");
            assert_eq!(
                file_sha256(&journal.staged).unwrap(),
                (journal.sha256.clone(), journal.bytes)
            );
            assert!(!dir.join(MOVE_JOURNAL).exists());
            assert!(dir.join(MOVE_CONFLICT).exists());
            let failed = queue.get(&job.id).unwrap();
            assert_eq!(failed.state, JobState::Failed);
            assert!(
                failed
                    .error
                    .as_deref()
                    .unwrap()
                    .contains(&journal.staged.display().to_string()),
                "{failed:?}"
            );
            // Settled once: later recoveries and imports go on.
            assert!(recover_moves(&fx.env()).unwrap().is_empty());
            let other = fx.wav("other.wav", &[(Some(0), 4.0)]);
            import_file(&fx.env(), &mut models(), &other, &ImportOptions::default()).unwrap();
            assert!(journal.staged.exists());
        }
    }

    /// A move that ended in a conflict: its job keeps the moved audio,
    /// returned with the job's ID.
    fn conflicted_move(fx: &Fixture) -> (JobId, PathBuf) {
        let file = fx.wav("conflict.wav", &[(Some(0), 3.0)]);
        MOVE_FAIL_AT.set(Some(MoveStep::Verified));
        import_file(&fx.env(), &mut models(), &file, &move_opts()).unwrap_err();
        MOVE_FAIL_AT.set(None);
        fs::write(&file, b"replacement").unwrap();
        let recovered = recover_moves(&fx.env()).unwrap();
        assert_eq!(recovered[0].outcome, MoveOutcome::Conflict);
        let queue = Queue::open(fx.lib.root()).unwrap();
        let job = queue.list(IMPORT_KIND).unwrap().remove(0);
        assert!(job.keeps_file());
        (job.id, recovered[0].kept.clone().unwrap())
    }

    #[test]
    fn a_kept_file_moved_away_releases_and_removes_its_job() {
        let fx = Fixture::new();
        let (id, kept) = conflicted_move(&fx);
        let queue = Queue::open(fx.lib.root()).unwrap();
        // While the audio is there, recovery leaves the job alone.
        assert!(recover_moves(&fx.env()).unwrap().is_empty());
        assert!(queue.get(&id).unwrap().keeps_file());
        // The user moves it out of the library: nothing is left to keep.
        let rescued = fx._tmp.path().join("rescued.wav");
        fs::rename(&kept, &rescued).unwrap();
        assert!(recover_moves(&fx.env()).unwrap().is_empty());
        assert!(!queue.job_dir(&id).exists());
        assert!(queue.list(IMPORT_KIND).unwrap().is_empty());
        // And it imports again.
        import_file(
            &fx.env(),
            &mut models(),
            &rescued,
            &ImportOptions::default(),
        )
        .unwrap();
    }

    #[test]
    fn a_kept_file_stays_protected_after_the_library_folder_moves() {
        let mut fx = Fixture::new();
        let (id, kept) = conflicted_move(&fx);
        let old_root = fx.lib.root().to_owned();
        let dir = Queue::open(&old_root).unwrap().job_dir(&id);
        let name = kept.file_name().unwrap().to_str().unwrap().to_owned();
        // Stored relative to the job folder.
        let job_path = dir.join(queue::JOB_JSON);
        let mut job: serde_json::Value = fsutil::read_json(&job_path).unwrap();
        assert_eq!(job["keeps"], name.as_str());
        let conflict_path = dir.join(MOVE_CONFLICT);
        let mut conflict: serde_json::Value = fsutil::read_json(&conflict_path).unwrap();
        assert_eq!(conflict["staged"], name.as_str());
        // Earlier builds stored absolute paths: those survive the move too.
        job["keeps"] = serde_json::json!(kept);
        conflict["staged"] = serde_json::json!(kept);
        fsutil::write_json_atomic(&job_path, &job).unwrap();
        fsutil::write_json_atomic(&conflict_path, &conflict).unwrap();

        let new_root = fx._tmp.path().join("moved-library");
        fs::rename(&old_root, &new_root).unwrap();
        fx.lib = Library::open(&new_root).unwrap();
        let queue = Queue::open(&new_root).unwrap();
        let moved = queue.job_dir(&id).join(&name);
        let content = fs::read(&moved).unwrap();
        assert_eq!(queue.get(&id).unwrap().keeps, Some(moved.clone()));
        assert!(queue.get(&id).unwrap().keeps_file());
        assert!(recover_moves(&fx.env()).unwrap().is_empty());
        assert_eq!(fs::read(&moved).unwrap(), content);
        assert!(queue.get(&id).unwrap().keeps_file());
        assert!(queue.claim(&fx.lib, &id, &fx.probe).unwrap().is_none());
    }

    #[test]
    fn a_placed_move_rolls_back_after_the_library_folder_moves() {
        let mut fx = Fixture::new();
        let file = fx.wav("placed.wav", &[(Some(0), 3.0)]);
        let content = fs::read(&file).unwrap();
        // The move stops inside the commit: the audio is in its meeting
        // folder, which has no meeting.json yet.
        MOVE_FAIL_AT.set(Some(MoveStep::Placed));
        import_file(&fx.env(), &mut models(), &file, &move_opts()).unwrap_err();
        MOVE_FAIL_AT.set(None);
        assert!(!file.exists());
        let old_root = fx.lib.root().to_owned();
        let queue = Queue::open(&old_root).unwrap();
        let dir = queue.job_dir(&queue.list(IMPORT_KIND).unwrap().remove(0).id);
        let journal_path = dir.join(MOVE_JOURNAL);
        let mut stored: serde_json::Value = fsutil::read_json(&journal_path).unwrap();
        // Stored relative to the library folder.
        let rel = PathBuf::from(stored["dest"].as_str().unwrap());
        assert!(rel.is_relative(), "{rel:?}");
        assert!(old_root.join(&rel).is_file());
        // An absolute path from an earlier build is read as is.
        let relative = stored["dest"].clone();
        stored["dest"] = serde_json::json!(old_root.join(&rel));
        fsutil::write_json_atomic(&journal_path, &stored).unwrap();
        let legacy = MoveJournal::load(&old_root, &dir, MOVE_JOURNAL).unwrap();
        assert_eq!(legacy.dest, Some(old_root.join(&rel)));
        stored["dest"] = relative;
        fsutil::write_json_atomic(&journal_path, &stored).unwrap();

        let new_root = fx._tmp.path().join("moved-library");
        fs::rename(&old_root, &new_root).unwrap();
        fx.lib = Library::open(&new_root).unwrap();
        let recovered = recover_moves(&fx.env()).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].outcome, MoveOutcome::RolledBack);
        // Found in the moved library and put back.
        assert_eq!(fs::read(&file).unwrap(), content);
        assert_eq!(meeting_folders(&new_root), 0);
        assert_eq!(count_audio(&new_root), 0);
    }

    #[test]
    fn the_last_failed_settlement_notes_where_the_audio_is() {
        let fx = Fixture::new();
        let gone = fx.input.join("gone");
        fs::create_dir_all(&gone).unwrap();
        let file = fx.wav("gone/stranded.wav", &[(Some(0), 3.0)]);
        MOVE_FAIL_AT.set(Some(MoveStep::Verified));
        import_file(&fx.env(), &mut models(), &file, &move_opts()).unwrap_err();
        MOVE_FAIL_AT.set(None);
        // Its folder is gone: every rollback fails to put the audio back.
        fs::remove_dir(&gone).unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let id = queue.list(IMPORT_KIND).unwrap().remove(0).id;
        let staged = MoveJournal::load(fx.lib.root(), &queue.job_dir(&id), MOVE_JOURNAL)
            .unwrap()
            .staged;
        // Staged under the lease token of the attempt that moved it.
        assert!(
            staged
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("audio.") && n != "audio.wav"),
            "{staged:?}"
        );
        let note = format!("its audio is at {}", staged.display());
        let first = queue.get(&id).unwrap().attempts + 1;
        for attempt in first..=MAX_ATTEMPTS {
            recover_moves(&fx.env()).unwrap();
            let job = queue.get(&id).unwrap();
            assert_eq!((job.state, job.attempts), (JobState::Failed, attempt));
            let error = job.error.unwrap();
            assert!(error.starts_with("recovering an interrupted move failed"));
            // Noted by the settlement that used up the last attempt.
            assert_eq!(error.contains(&note), attempt == MAX_ATTEMPTS, "{error}");
        }
        // Noted once.
        let error = queue.get(&id).unwrap().error;
        recover_moves(&fx.env()).unwrap();
        assert_eq!(queue.get(&id).unwrap().error, error);
        assert!(staged.is_file());
    }

    #[test]
    fn rollback_restores_the_original_with_its_modification_time() {
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for copy in [false, true] {
            let fx = Fixture::new();
            let file = fx.wav("restore.wav", &[(Some(0), 3.0)]);
            File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(old)
                .unwrap();
            let content = fs::read(&file).unwrap();
            FORCE_COPY.set(copy);
            MOVE_FAIL_AT.set(Some(MoveStep::Verified));
            import_file(&fx.env(), &mut models(), &file, &move_opts()).unwrap_err();
            MOVE_FAIL_AT.set(None);
            FORCE_COPY.set(false);
            if copy {
                // The original went missing meanwhile: only the copy is left.
                fs::remove_file(&file).unwrap();
            } else {
                assert!(!file.exists());
            }
            let recovered = recover_moves(&fx.env()).unwrap();
            assert_eq!(recovered[0].outcome, MoveOutcome::RolledBack, "copy={copy}");
            assert_eq!(fs::read(&file).unwrap(), content);
            assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), old);
            let queue = Queue::open(fx.lib.root()).unwrap();
            let job = queue.list(IMPORT_KIND).unwrap().remove(0);
            assert_eq!(count_audio(&queue.job_dir(&job.id)), 0);
            let dir = file.parent().unwrap();
            assert_eq!(
                fs::read_dir(dir)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .filter(|e| e
                        .file_name()
                        .to_string_lossy()
                        .contains("broscribe-restore"))
                    .count(),
                0
            );
        }
    }

    #[test]
    fn recover_moves_finishes_an_analyzed_meeting_before_removing_job() {
        let fx = Fixture::new();
        let file = fx.wav("unfinished.wav", &[(Some(0), 3.0)]);
        crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::ImportAfterSave));
        import_file(&fx.env(), &mut models(), &file, &move_opts()).unwrap_err();
        let recovered = recover_moves(&fx.env()).unwrap();
        let id = recovered[0].meeting_id.as_ref().unwrap();
        assert_eq!(
            fx.lib.load_meeting(id).unwrap().processing.status,
            ProcessingStatus::Done
        );
        assert!(
            Queue::open(fx.lib.root())
                .unwrap()
                .list(IMPORT_KIND)
                .unwrap()
                .is_empty()
        );
    }
    use crate::testutil::{Fixture, models};
    use broscribe_core::index::SearchQuery;

    #[test]
    fn single_import_end_to_end_then_dedupe() {
        let fx = Fixture::new();
        let file = fx.wav(
            "Design review.wav",
            &[(Some(0), 4.0), (Some(1), 4.0), (Some(0), 2.0)],
        );
        let mut m = models();
        let out = import_file(&fx.env(), &mut m, &file, &ImportOptions::default()).unwrap();
        assert!(!out.already_exists);
        assert_eq!(out.speakers, 2);
        assert_eq!(out.title.as_deref(), Some("Design review"));
        let meeting = fx.lib.load_meeting(&out.meeting_id).unwrap();
        assert_eq!(meeting.processing.status, ProcessingStatus::Done);
        assert_eq!(
            meeting.models.speaker_embedding.as_deref(),
            Some("nemo-titanet-large")
        );
        let dir = fx.lib.meeting_dir(&out.meeting_id).unwrap();
        assert!(dir.join(paths::MEETING_MD).is_file());
        assert!(dir.join("audio.wav").is_file());
        let md = std::fs::read_to_string(dir.join(paths::MEETING_MD)).unwrap();
        // The fake ASR spreads "hello world" over the speech: "hello" falls
        // in the first voice's turn, "world" mostly in the second's.
        assert!(md.contains("Speaker 1: hello"), "{md}");
        assert!(md.contains("Speaker 2: world"), "{md}");
        let index = Index::open_readonly(fx.lib.root()).unwrap();
        let hits = index
            .search(&SearchQuery {
                text: Some("hello".into()),
                ..SearchQuery::default()
            })
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].meeting_id, out.meeting_id);
        // The import job is cleaned up.
        assert!(
            Queue::open(fx.lib.root())
                .unwrap()
                .list(IMPORT_KIND)
                .unwrap()
                .is_empty()
        );

        // Same content again: the existing meeting, no new one.
        let copy = fx.input.join("again.wav");
        std::fs::copy(&file, &copy).unwrap();
        let again = import_file(&fx.env(), &mut m, &copy, &ImportOptions::default()).unwrap();
        assert!(again.already_exists);
        assert_eq!(again.meeting_id, out.meeting_id);
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
    }

    #[test]
    fn reference_handling_keeps_the_file_in_place() {
        let fx = Fixture::new();
        let file = fx.wav("ref.wav", &[(Some(1), 3.0)]);
        let mut m = models();
        let opts = ImportOptions {
            handling: Some(Handling::Reference),
            title: Some("Kept in place".into()),
            ..ImportOptions::default()
        };
        let out = import_file(&fx.env(), &mut m, &file, &opts).unwrap();
        let meeting = fx.lib.load_meeting(&out.meeting_id).unwrap();
        let rec = meeting.recording.as_ref().unwrap();
        assert_eq!(rec.handling, Handling::Reference);
        assert_eq!(rec.state, RecordingState::Available);
        assert_eq!(rec.path, absolute(&file).unwrap());
        assert_eq!(
            rec.original_path.as_deref(),
            Some(absolute(&file).unwrap().as_path())
        );
        assert_eq!(
            fx.lib.meeting_audio_path(&meeting).unwrap(),
            Some(rec.path.clone())
        );
        let dir = fx.lib.meeting_dir(&out.meeting_id).unwrap();
        assert!(!dir.join("audio.wav").exists());
        assert_eq!(meeting.title.as_deref(), Some("Kept in place"));
        assert_eq!(meeting.title_source, Some(TitleSource::User));
    }

    fn move_opts() -> ImportOptions {
        ImportOptions {
            handling: Some(Handling::Move),
            ..ImportOptions::default()
        }
    }

    /// Files named `name` anywhere below `dir` (hidden folders included).
    fn count_named(dir: &Path, name: &str) -> usize {
        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };
        entries
            .map(|e| e.unwrap().path())
            .map(|p| {
                if p.is_dir() {
                    count_named(&p, name)
                } else {
                    usize::from(p.file_name().is_some_and(|n| n == name))
                }
            })
            .sum()
    }

    /// Audio files (`audio.*` naming a WAV, staged parts included) anywhere
    /// below `dir`.
    fn count_audio(dir: &Path) -> usize {
        let Ok(entries) = fs::read_dir(dir) else {
            return 0;
        };
        entries
            .map(|e| e.unwrap().path())
            .map(|p| {
                if p.is_dir() {
                    count_audio(&p)
                } else {
                    usize::from(
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("audio.") && n.contains(".wav")),
                    )
                }
            })
            .sum()
    }

    /// An import job of `file` claimed by a holder whose process then died
    /// (lease `stale`), then taken over by this process (lease `current`).
    fn overlapping_leases(fx: &Fixture, file: &Path) -> (LeaseHandle, LeaseHandle) {
        let (sha, _) = file_sha256(file).unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let id = fx
            .lib
            .write(|tx| queue.enqueue(tx, IMPORT_KIND, &sha, serde_json::json!({})))
            .unwrap();
        let gone = broscribe_platform::fakes::FakeProcessProbe::new(4242);
        let stale = queue.claim(&fx.lib, &id, &gone).unwrap().unwrap();
        let current = queue.claim(&fx.lib, &id, &fx.probe).unwrap().unwrap();
        assert_ne!(stale.token(), current.token());
        (stale, current)
    }

    #[test]
    fn overlapping_copy_attempts_stage_and_clean_only_their_own_audio() {
        let fx = Fixture::new();
        let file = fx.wav("overlap.wav", &[(Some(0), 3.0)]);
        let (sha, bytes) = file_sha256(&file).unwrap();
        let (stale, current) = overlapping_leases(&fx, &file);
        let mine = stage_audio(&current, &file, Handling::Copy, "wav")
            .unwrap()
            .unwrap();
        // The holder that lost the job stages beside it, never over it.
        let theirs = stage_audio(&stale, &file, Handling::Copy, "wav")
            .unwrap()
            .unwrap();
        assert_ne!(mine, theirs);
        assert_eq!(file_sha256(&mine).unwrap(), (sha.clone(), bytes));
        // A failed verification of the stale attempt removes only its file.
        verify_staged_copy(&theirs, &file, &"0".repeat(64), bytes).unwrap_err();
        assert!(!theirs.exists());
        assert_eq!(file_sha256(&mine).unwrap(), (sha.clone(), bytes));
        verify_staged_copy(&mine, &file, &sha, bytes).unwrap();
        assert!(mine.exists());
        drop(stale);
        current.release(&fx.lib).unwrap();
    }

    #[test]
    fn a_holder_that_lost_a_move_never_journals_moves_or_settles_it() {
        let fx = Fixture::new();
        let file = fx.wav("overlap-move.wav", &[(Some(0), 3.0)]);
        let source = absolute(&file).unwrap();
        let content = fs::read(&source).unwrap();
        let (sha, bytes) = file_sha256(&source).unwrap();
        let (stale, current) = overlapping_leases(&fx, &file);
        let job_dir = current.job_dir();
        // The stale holder can neither journal nor take the original away.
        let lost = stage_move(&fx.env(), &stale, &source, &sha, bytes, "wav").unwrap_err();
        assert!(matches!(lost, Error::LeaseLost(_)), "{lost}");
        assert_eq!(fs::read(&source).unwrap(), content);
        assert!(!job_dir.join(MOVE_JOURNAL).exists());
        let journal = stage_move(&fx.env(), &current, &source, &sha, bytes, "wav").unwrap();
        assert_eq!(journal.staged, staged_audio_path(&current, "wav"));
        // A second move never journals over the one in flight.
        stage_move(&fx.env(), &current, &source, &sha, bytes, "wav").unwrap_err();
        // Nor does the stale holder's settlement touch the current move.
        assert!(matches!(
            settle_move(&fx.env(), &stale, false),
            Err(Error::LeaseLost(_))
        ));
        let kept = MoveJournal::load(fx.lib.root(), &job_dir, MOVE_JOURNAL).unwrap();
        assert_eq!(kept.staged, journal.staged);
        assert_eq!(file_sha256(&kept.staged).unwrap(), (sha, bytes));
        // The current holder still settles its own move: the file goes back.
        let settled = settle_move(&fx.env(), &current, false).unwrap().unwrap();
        assert_eq!(settled.outcome, MoveOutcome::RolledBack);
        assert_eq!(fs::read(&source).unwrap(), content);
        drop(stale);
        current.release(&fx.lib).unwrap();
    }

    #[test]
    fn a_damaged_same_size_library_copy_never_deletes_the_original() {
        let fx = Fixture::new();
        let file = fx.wav("damaged.wav", &[(Some(1), 3.0), (Some(2), 3.0)]);
        let source = absolute(&file).unwrap();
        let content = fs::read(&source).unwrap();
        FORCE_COPY.set(true);
        MOVE_FAIL_AT.set(Some(MoveStep::Committed));
        let err = import_file(&fx.env(), &mut models(), &file, &move_opts()).unwrap_err();
        MOVE_FAIL_AT.set(None);
        FORCE_COPY.set(false);
        assert!(is_move_failpoint(&err), "{err}");
        // Published, original still there; the library copy is then damaged
        // without changing its size.
        let meeting = fx.lib.list_meeting_dirs().unwrap().remove(0);
        let placed = meeting.join("audio.wav");
        let mut damaged = fs::read(&placed).unwrap();
        let last = damaged.len() - 1;
        damaged[last] ^= 0xff;
        fs::write(&placed, &damaged).unwrap();
        assert_eq!(fs::metadata(&placed).unwrap().len(), content.len() as u64);
        let recovered = recover_moves(&fx.env()).unwrap();
        assert_eq!(recovered[0].outcome, MoveOutcome::Completed);
        assert_eq!(fs::read(&source).unwrap(), content, "original deleted");
    }

    #[test]
    fn an_unreadable_neighbor_never_blocks_imports_but_is_never_duplicated() {
        let fx = Fixture::new();
        let first = fx.wav("first.wav", &[(Some(0), 3.0)]);
        let out = import_file(&fx.env(), &mut models(), &first, &ImportOptions::default()).unwrap();
        let dir = fx.lib.meeting_dir(&out.meeting_id).unwrap();
        fs::write(dir.join(paths::MEETING_JSON), b"{ unreadable").unwrap();
        // Another file imports.
        let other = fx.wav("other.wav", &[(Some(1), 4.0)]);
        import_file(&fx.env(), &mut models(), &other, &ImportOptions::default()).unwrap();
        // The same content may be the unreadable meeting: refused, not doubled.
        let again = fx.input.join("again.wav");
        fs::copy(&first, &again).unwrap();
        let error =
            import_file(&fx.env(), &mut models(), &again, &ImportOptions::default()).unwrap_err();
        assert_eq!(error.code(), "invalid_input", "{error}");
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 2);
    }

    #[test]
    fn an_unreadable_meeting_without_its_audio_is_refused_not_duplicated() {
        let fx = Fixture::new();
        let reference = ImportOptions {
            handling: Some(Handling::Reference),
            ..ImportOptions::default()
        };
        let copy = ImportOptions {
            handling: Some(Handling::Copy),
            ..ImportOptions::default()
        };
        // A reference keeps no audio in its folder; this copy lost its own.
        let referenced = fx.wav("referenced.wav", &[(Some(0), 3.0)]);
        let out = import_file(&fx.env(), &mut models(), &referenced, &reference).unwrap();
        let referenced_dir = fx.lib.meeting_dir(&out.meeting_id).unwrap();
        let copied = fx.wav("copied.wav", &[(Some(1), 4.0)]);
        let out = import_file(&fx.env(), &mut models(), &copied, &copy).unwrap();
        let copied_dir = fx.lib.meeting_dir(&out.meeting_id).unwrap();
        let mut removed = 0;
        for entry in fs::read_dir(&copied_dir).unwrap() {
            let path = entry.unwrap().path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("audio."))
            {
                fs::remove_file(path).unwrap();
                removed += 1;
            }
        }
        assert_eq!(removed, 1);
        for dir in [&referenced_dir, &copied_dir] {
            fs::write(dir.join(paths::MEETING_JSON), b"{ unreadable").unwrap();
        }
        // Each original again: its unreadable meeting is reported, not doubled.
        for (file, dir) in [(&referenced, &referenced_dir), (&copied, &copied_dir)] {
            let error = import_file(&fx.env(), &mut models(), file, &reference).unwrap_err();
            assert_eq!(error.code(), "invalid_input", "{error}");
            assert!(
                error.to_string().contains(&dir.display().to_string()),
                "{error}"
            );
        }
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 2);
        // Neither blocks other content.
        let other = fx.wav("other.wav", &[(Some(2), 4.0)]);
        import_file(&fx.env(), &mut models(), &other, &reference).unwrap();
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 3);
    }

    #[test]
    fn an_unreadable_meeting_nothing_rules_out_blocks_imports_until_repaired() {
        let fx = Fixture::new();
        let first = fx.wav("first.wav", &[(Some(0), 3.0)]);
        let out = import_file(&fx.env(), &mut models(), &first, &ImportOptions::default()).unwrap();
        let dir = fx.lib.meeting_dir(&out.meeting_id).unwrap();
        fs::write(dir.join(paths::MEETING_JSON), b"{ unreadable").unwrap();
        // Without its meeting.md nothing proves it is another meeting.
        fs::remove_file(dir.join(paths::MEETING_MD)).unwrap();
        let other = fx.wav("other.wav", &[(Some(1), 4.0)]);
        let error =
            import_file(&fx.env(), &mut models(), &other, &ImportOptions::default()).unwrap_err();
        assert_eq!(error.code(), "invalid_input", "{error}");
        let message = error.to_string();
        assert!(message.contains(&dir.display().to_string()), "{message}");
        assert!(message.contains("rule out"), "{message}");
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
        // Removing the damaged meeting.json, as the refusal says, unblocks it.
        fs::remove_file(dir.join(paths::MEETING_JSON)).unwrap();
        import_file(&fx.env(), &mut models(), &other, &ImportOptions::default()).unwrap();
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
        assert_eq!(meeting_folders(fx.lib.root()), 2);
    }

    /// Folders `YYYY/MM/<meeting>` on disk, with or without `meeting.json`.
    fn meeting_folders(root: &Path) -> usize {
        let digits = |p: &Path, n: usize| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.len() == n && s.bytes().all(|b| b.is_ascii_digit()))
        };
        let dirs = |p: &Path| -> Vec<PathBuf> {
            fs::read_dir(p)
                .map(|it| {
                    it.map(|e| e.unwrap().path())
                        .filter(|p| p.is_dir())
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut n = 0;
        for year in dirs(root).into_iter().filter(|p| digits(p, 4)) {
            for month in dirs(&year).into_iter().filter(|p| digits(p, 2)) {
                n += dirs(&month).len();
            }
        }
        n
    }

    /// The file now lives only in its meeting folder, intact, and nothing
    /// of the move is left behind.
    fn assert_moved(fx: &Fixture, id: &MeetingId, source: &Path, sha: &str) {
        assert!(!source.exists(), "original still at {}", source.display());
        let m = fx.lib.load_meeting(id).unwrap();
        let rec = m.recording.as_ref().unwrap();
        assert_eq!(rec.handling, Handling::Move);
        assert_eq!(rec.state, RecordingState::Available);
        assert_eq!(rec.original_path.as_deref(), Some(source));
        let audio = fx.lib.meeting_audio_path(&m).unwrap().unwrap();
        assert_eq!(file_sha256(&audio).unwrap().0, sha);
        assert_eq!(meeting_folders(fx.lib.root()), 1);
        assert_eq!(count_named(fx.lib.root(), MOVE_JOURNAL), 0);
        assert_eq!(count_audio(fx.lib.root()), 1);
    }

    #[test]
    fn move_on_the_same_volume_and_across_volumes() {
        for copy in [false, true] {
            let fx = Fixture::new();
            let file = fx.wav("moved.wav", &[(Some(1), 3.0), (Some(2), 3.0)]);
            let source = absolute(&file).unwrap();
            let (sha, _) = file_sha256(&file).unwrap();
            FORCE_COPY.set(copy);
            let out = import_file(&fx.env(), &mut models(), &file, &move_opts());
            FORCE_COPY.set(false);
            let out = out.unwrap();
            assert!(!out.already_exists);
            assert_moved(&fx, &out.meeting_id, &source, &sha);
            assert!(
                Queue::open(fx.lib.root())
                    .unwrap()
                    .list(IMPORT_KIND)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn interrupted_moves_recover_from_either_location() {
        use MoveStep::*;
        for copy in [false, true] {
            for step in [
                Planned,
                Transferred,
                Copied,
                Verified,
                Placed,
                Committed,
                SourceDeleted,
            ] {
                if !copy && step == Copied {
                    continue; // a rename has no copied state
                }
                let fx = Fixture::new();
                let file = fx.wav("moved.wav", &[(Some(1), 3.0), (Some(2), 3.0)]);
                let source = absolute(&file).unwrap();
                let (sha, _) = file_sha256(&file).unwrap();
                FORCE_COPY.set(copy);
                MOVE_FAIL_AT.set(Some(step));
                let err = import_file(&fx.env(), &mut models(), &file, &move_opts());
                MOVE_FAIL_AT.set(None);
                let err = err.unwrap_err();
                assert!(is_move_failpoint(&err), "{copy} {step:?}: {err}");
                // Nothing lost: the audio is at its original place, in the
                // job folder or in the meeting folder.
                let copies = usize::from(source.exists()) + count_audio(fx.lib.root());
                assert!(copies >= 1, "{copy} {step:?}: no copy left");

                // The next import recovers, from wherever the file is.
                let out = import_file(&fx.env(), &mut models(), &file, &move_opts());
                FORCE_COPY.set(false);
                let out = out.unwrap();
                let published = matches!(step, Committed | SourceDeleted);
                assert!(!out.already_exists, "{copy} {step:?}");
                assert_eq!(out.recovered, published, "{copy} {step:?}");
                assert_moved(&fx, &out.meeting_id, &source, &sha);

                // Recovery is idempotent.
                assert!(recover_moves(&fx.env()).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn recover_moves_reports_what_it_did() {
        let fx = Fixture::new();
        let file = fx.wav("moved.wav", &[(Some(1), 3.0), (Some(2), 3.0)]);
        let source = absolute(&file).unwrap();
        MOVE_FAIL_AT.set(Some(MoveStep::Committed));
        let err = import_file(&fx.env(), &mut models(), &file, &move_opts()).unwrap_err();
        MOVE_FAIL_AT.set(None);
        assert!(is_move_failpoint(&err));
        let r = recover_moves(&fx.env()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].outcome, MoveOutcome::Completed);
        assert_eq!(r[0].source, source);
        assert!(r[0].meeting_id.is_some());

        let other = fx.wav("other.wav", &[(Some(0), 3.0)]);
        let other_source = absolute(&other).unwrap();
        MOVE_FAIL_AT.set(Some(MoveStep::Verified));
        import_file(&fx.env(), &mut models(), &other, &move_opts()).unwrap_err();
        MOVE_FAIL_AT.set(None);
        assert!(!other.exists(), "renamed into the job folder");
        let r = recover_moves(&fx.env()).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].outcome, MoveOutcome::RolledBack);
        assert_eq!(r[0].meeting_id, None);
        assert!(other_source.is_file(), "put back");
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
    }

    #[test]
    fn restricted_media_imports_redact_decoder_diagnostics_before_saving_errors() {
        let fx = Fixture::new();
        let input = fx.wav("synthetic.wav", &[(Some(0), 3.0)]);
        let withheld = "synthetic decoder private diagnostic";
        let decoder = broscribe_platform::DecoderChain::new(vec![Box::new(
            broscribe_platform::fakes::FakeDecoder::failing(withheld, 0),
        )]);
        let env = Env {
            decoders: &decoder,
            ..fx.env()
        };
        let error =
            import_media_without_moves(&env, &mut models(), &input, &ImportOptions::default())
                .unwrap_err();
        assert_eq!(error.code(), "decode");
        assert!(!error.to_string().contains(withheld));

        let manifest = crate::manifest::parse(
            &serde_json::json!({
                "format":crate::manifest::FORMAT,"version":1,"meetings":[{"audio_path":input}],
            })
            .to_string(),
        )
        .unwrap();
        let result = crate::batch::import_manifest_without_decode_details(
            &env,
            &mut models(),
            &manifest,
            &crate::BatchOptions::default(),
        )
        .unwrap();
        assert_eq!(result.failed[0].code, "decode");
        assert!(!result.failed[0].message.contains(withheld));
        let queue = Queue::open(fx.lib.root()).unwrap();
        let jobs = [IMPORT_KIND, queue::MANIFEST_KIND].map(|kind| queue.list(kind).unwrap());
        for job in jobs.iter().flatten() {
            let error = job.error.as_deref().unwrap();
            assert!(!error.contains(withheld));
        }
    }

    #[test]
    fn restricted_media_import_leaves_interrupted_moves_for_operator_recovery() {
        let fx = Fixture::new();
        let staged = fx.wav("moving.wav", &[(Some(0), 3.0)]);
        let duplicate = fx.input.join("duplicate.wav");
        fs::copy(&staged, &duplicate).unwrap();
        MOVE_FAIL_AT.set(Some(MoveStep::Verified));
        let crash = import_file(&fx.env(), &mut models(), &staged, &move_opts()).unwrap_err();
        MOVE_FAIL_AT.set(None);
        assert!(is_move_failpoint(&crash));
        assert!(!staged.exists());
        let refused = import_media_without_moves(
            &fx.env(),
            &mut models(),
            &duplicate,
            &ImportOptions::default(),
        )
        .unwrap_err();
        assert!(refused.to_string().contains("incomplete Move import"));
        assert!(duplicate.is_file());

        let input = fx.wav("copy.wav", &[(Some(1), 3.0)]);
        import_media_without_moves(&fx.env(), &mut models(), &input, &ImportOptions::default())
            .unwrap();
        assert!(input.is_file());
        assert!(
            !staged.exists(),
            "a restricted import must leave the move journal alone"
        );
        assert_eq!(
            recover_moves(&fx.env()).unwrap()[0].outcome,
            MoveOutcome::RolledBack
        );
        assert!(staged.is_file());
    }

    #[test]
    fn move_never_deletes_a_file_already_in_the_library() {
        let fx = Fixture::new();
        let file = fx.wav("first.wav", &[(Some(0), 3.0), (Some(1), 3.0)]);
        let first =
            import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        let dup = fx.input.join("dup.wav");
        fs::copy(&file, &dup).unwrap();
        let again = import_file(&fx.env(), &mut models(), &dup, &move_opts()).unwrap();
        assert!(again.already_exists);
        assert_eq!(again.meeting_id, first.meeting_id);
        assert!(dup.is_file());
        assert!(file.is_file());
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);
    }

    #[test]
    fn manifest_imports_still_refuse_move() {
        let err = check_handling(Handling::Move).unwrap_err();
        assert_eq!(err.code(), "unsupported");
        assert_eq!(check_handling(Handling::Copy).unwrap(), Handling::Copy);
    }

    #[test]
    fn folder_import_makes_one_meeting_per_media_file() {
        let fx = Fixture::new();
        fx.wav("a.wav", &[(Some(0), 3.0)]);
        fx.wav("b.wav", &[(Some(1), 3.0)]);
        std::fs::write(fx.input.join("notes.txt"), "not media").unwrap();
        let mut m = models();
        let r = import_folder(&fx.env(), &mut m, &fx.input, &ImportOptions::default()).unwrap();
        assert_eq!(r.imported.len(), 2);
        assert!(r.failed.is_empty());
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 2);
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::testutil::{Fixture, models};
    struct RejectDiarizer;
    impl broscribe_models::Diarizer for RejectDiarizer {
        fn diarize(&self, _: &[f32]) -> Result<Vec<broscribe_models::DiarSegment>> {
            Err(Error::Inference("synthetic interruption".into()))
        }
    }

    fn failing_models() -> ModelSet {
        let mut broken = models();
        broken.diarizer = Box::new(RejectDiarizer);
        broken
    }

    fn import_job(fixture: &Fixture, path: &Path) -> Job {
        let (sha, _) = file_sha256(path).unwrap();
        Queue::open(fixture.lib.root())
            .unwrap()
            .get(&broscribe_core::queue::JobId::for_key(IMPORT_KIND, &sha))
            .unwrap()
    }

    #[test]
    fn worker_resume_uses_persisted_handling_and_metadata_not_changed_config() {
        let mut fixture = Fixture::new();
        fixture.config.default_language = Some("pt".into());
        let path = fixture.wav("retry.wav", &[(Some(0), 4.0)]);
        let started_at = DateTime::parse_from_rfc3339("2026-10-01T09:00:00Z").unwrap();
        let opts = ImportOptions {
            handling: Some(Handling::Reference),
            title: Some("Original choice".into()),
            started_at: Some(started_at),
            ..Default::default()
        };
        import_file(&fixture.env(), &mut failing_models(), &path, &opts).unwrap_err();
        fixture.config.import_handling = Handling::Move;
        fixture.config.default_language = Some("de".into());
        let job = import_job(&fixture, &path);
        assert_eq!(job.attempts, 1);
        let result =
            resume_import_job(&fixture.env(), &mut models(), &job, &mut Vec::new()).unwrap();
        let meeting = fixture.lib.load_meeting(&result.meeting_id).unwrap();
        assert!(path.exists());
        assert_eq!(meeting.recording.unwrap().handling, Handling::Reference);
        assert_eq!(meeting.title.as_deref(), Some("Original choice"));
        assert_eq!(meeting.title_source, Some(TitleSource::User));
        assert_eq!(meeting.started_at, started_at);
        assert_eq!(meeting.languages[0].lang, "pt");
    }

    #[test]
    fn direct_rerun_uses_the_callers_new_options() {
        let fixture = Fixture::new();
        let path = fixture.wav("rerun.wav", &[(Some(0), 4.0)]);
        let first = ImportOptions {
            handling: Some(Handling::Move),
            title: Some("First try".into()),
            language: Some("pt".into()),
            ..Default::default()
        };
        import_file(&fixture.env(), &mut failing_models(), &path, &first).unwrap_err();
        assert_eq!(import_job(&fixture, &path).state, JobState::Failed);
        let second = ImportOptions {
            handling: Some(Handling::Reference),
            language: Some("fr".into()),
            ..Default::default()
        };
        let result = import_file(&fixture.env(), &mut models(), &path, &second).unwrap();
        let meeting = fixture.lib.load_meeting(&result.meeting_id).unwrap();
        // A referenced file is never moved.
        assert!(path.exists());
        assert_eq!(meeting.recording.unwrap().handling, Handling::Reference);
        assert_eq!(meeting.title.as_deref(), Some("rerun"));
        assert_eq!(meeting.title_source, Some(TitleSource::File));
        assert_eq!(meeting.languages[0].lang, "fr");
    }

    #[test]
    fn direct_rerun_overwrites_a_job_no_process_holds() {
        let fixture = Fixture::new();
        let path = fixture.wav("queued.wav", &[(Some(0), 4.0)]);
        let (sha, _) = file_sha256(&path).unwrap();
        let queue = Queue::open(fixture.lib.root()).unwrap();
        // Queued earlier with other choices, never started.
        fixture
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    IMPORT_KIND,
                    &sha,
                    serde_json::json!({"path": path, "handling": "move", "title": "Old", "title_is_user": true}),
                )
            })
            .unwrap();
        let opts = ImportOptions {
            handling: Some(Handling::Reference),
            title: Some("New".into()),
            ..Default::default()
        };
        let result = import_file(&fixture.env(), &mut models(), &path, &opts).unwrap();
        let meeting = fixture.lib.load_meeting(&result.meeting_id).unwrap();
        assert!(path.exists());
        assert_eq!(meeting.recording.unwrap().handling, Handling::Reference);
        assert_eq!(meeting.title.as_deref(), Some("New"));
    }

    #[test]
    fn legacy_job_without_handling_resumes_with_current_defaults() {
        let mut fixture = Fixture::new();
        fixture.config.import_handling = Handling::Reference;
        let path = fixture.wav("legacy.wav", &[(Some(0), 4.0)]);
        let (sha, _) = file_sha256(&path).unwrap();
        let queue = Queue::open(fixture.lib.root()).unwrap();
        fixture
            .lib
            .write(|tx| queue.enqueue(tx, IMPORT_KIND, &sha, serde_json::json!({"path": path})))
            .unwrap();
        let job = import_job(&fixture, &path);
        let result =
            resume_import_job(&fixture.env(), &mut models(), &job, &mut Vec::new()).unwrap();
        let meeting = fixture.lib.load_meeting(&result.meeting_id).unwrap();
        assert!(path.exists());
        assert_eq!(meeting.recording.unwrap().handling, Handling::Reference);
        assert_eq!(meeting.title_source, Some(TitleSource::File));
    }

    /// Files of import jobs' folders other than `job.json`.
    fn queue_files(fixture: &Fixture) -> Vec<PathBuf> {
        let queue = Queue::open(fixture.lib.root()).unwrap();
        let mut files = Vec::new();
        for job in queue.list(IMPORT_KIND).unwrap() {
            for entry in fs::read_dir(queue.job_dir(&job.id)).unwrap() {
                let path = entry.unwrap().path();
                if path
                    .file_name()
                    .is_some_and(|n| n != broscribe_core::queue::JOB_JSON)
                {
                    files.push(path);
                }
            }
        }
        files
    }

    #[test]
    fn superseded_setup_session_cannot_publish_an_import() {
        let fixture = Fixture::new();
        let path = fixture.wav("superseded.wav", &[(Some(0), 3.0)]);
        let data = fixture._tmp.path().join("data");
        let opts = ImportOptions {
            handling: Some(Handling::Copy),
            setup_session: Some((data, "old-session".into())),
            ..Default::default()
        };
        let error = import_file(&fixture.env(), &mut models(), &path, &opts).unwrap_err();
        assert!(
            matches!(error, Error::InvalidInput(message) if message == "setup session superseded")
        );
        assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
        // The staged copy and the job went with the cancelled session.
        let queue = Queue::open(fixture.lib.root()).unwrap();
        assert!(queue.list(IMPORT_KIND).unwrap().is_empty());
        assert!(queue_files(&fixture).is_empty());
    }

    #[test]
    fn interrupted_setup_import_is_discarded_but_ordinary_recovery_data_stays() {
        let fixture = Fixture::new();
        let queue = Queue::open(fixture.lib.root()).unwrap();
        let data = fixture._tmp.path().join("data");
        let mut jobs = Vec::new();
        for (name, seconds, session) in [
            ("try-it.wav", 3.0, Some("cancelled-session")),
            ("ordinary.wav", 4.0, None),
        ] {
            let path = fixture.wav(name, &[(Some(0), seconds)]);
            let (sha, _) = file_sha256(&path).unwrap();
            let input = serde_json::json!({
                "path": path,
                "sha256": sha,
                "handling": "copy",
                "setup_session": session.map(|s| (data.clone(), s)),
            });
            let id = fixture
                .lib
                .write(|tx| queue.enqueue(tx, IMPORT_KIND, &sha, input))
                .unwrap();
            // A killed child left its staged copy behind.
            fs::copy(&path, queue.job_dir(&id).join("audio.wav")).unwrap();
            jobs.push(id);
        }
        let unrelated = fixture.wav("unrelated.wav", &[(Some(0), 5.0)]);
        import_file(
            &fixture.env(),
            &mut models(),
            &unrelated,
            &ImportOptions::default(),
        )
        .unwrap();
        assert!(matches!(queue.get(&jobs[0]), Err(Error::NotFound(_))));
        assert!(!queue.job_dir(&jobs[0]).exists());
        assert!(queue.job_dir(&jobs[1]).join("audio.wav").is_file());
        assert_eq!(queue.get(&jobs[1]).unwrap().state, JobState::Queued);
    }

    /// A setup Try-it import job of `session` whose child was killed after
    /// staging: it still holds the job's lease (dead PID) and the copy.
    fn killed_setup_import(fixture: &Fixture, name: &str, seconds: f64, session: &str) -> JobId {
        let queue = Queue::open(fixture.lib.root()).unwrap();
        let data = fixture._tmp.path().join("data");
        let path = fixture.wav(name, &[(Some(0), seconds)]);
        let (sha, _) = file_sha256(&path).unwrap();
        let input = serde_json::json!({
            "path": path,
            "sha256": sha,
            "handling": "copy",
            "setup_session": (data, session),
        });
        let id = fixture
            .lib
            .write(|tx| queue.enqueue(tx, IMPORT_KIND, &sha, input))
            .unwrap();
        let child = broscribe_platform::fakes::FakeProcessProbe::new(4242);
        drop(queue.claim(&fixture.lib, &id, &child).unwrap().unwrap());
        fs::copy(&path, queue.job_dir(&id).join("audio.wav")).unwrap();
        id
    }

    #[test]
    fn killing_a_try_it_child_after_staging_leaves_no_staged_audio() {
        let fixture = Fixture::new();
        let data = fixture._tmp.path().join("data");
        // The app's session is still current: only its kill ends it.
        let mut config = broscribe_core::config::Config::default();
        config.setup.voice_session = Some("killed".into());
        config.save(&data).unwrap();
        let killed = killed_setup_import(&fixture, "killed.wav", 3.0, "killed");
        let other = killed_setup_import(&fixture, "other.wav", 4.0, "other");
        let queue = Queue::open(fixture.lib.root()).unwrap();

        let discarded = discard_session_imports(&fixture.lib, &fixture.probe, "killed").unwrap();
        assert_eq!(discarded, std::slice::from_ref(&killed));
        assert!(!queue.job_dir(&killed).exists());
        assert!(queue.job_dir(&other).join("audio.wav").is_file());
        // No import followed: nothing else was needed.
        assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
    }

    #[test]
    fn startup_sweep_discards_an_exhausted_setup_import() {
        let fixture = Fixture::new();
        let id = killed_setup_import(&fixture, "exhausted.wav", 3.0, "ended");
        let queue = Queue::open(fixture.lib.root()).unwrap();
        // Crashed runs until no claim will run it again.
        for pid in 4243..4243 + MAX_ATTEMPTS {
            let child = broscribe_platform::fakes::FakeProcessProbe::new(pid);
            drop(queue.claim(&fixture.lib, &id, &child).unwrap());
        }
        let job = queue.get(&id).unwrap();
        assert_eq!((job.state, job.attempts), (JobState::Failed, MAX_ATTEMPTS));
        let discarded = discard_abandoned_setup_imports(&fixture.lib, &fixture.probe).unwrap();
        assert_eq!(discarded, std::slice::from_ref(&id));
        assert!(!queue.job_dir(&id).exists());
    }

    #[test]
    fn resuming_an_unfinished_meeting_keeps_the_attempts_until_the_limit() {
        let fixture = Fixture::new();
        let path = fixture.wav("unfinished.wav", &[(Some(0), 3.0)]);
        crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::ImportAfterSave));
        import_file(
            &fixture.env(),
            &mut models(),
            &path,
            &ImportOptions::default(),
        )
        .unwrap_err();
        let queue = Queue::open(fixture.lib.root()).unwrap();
        let id = queue.list(IMPORT_KIND).unwrap().remove(0).id;
        assert_eq!(queue.get(&id).unwrap().attempts, 1);
        // Finishing the saved meeting keeps failing.
        for attempt in 2..=MAX_ATTEMPTS {
            crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::FinishBeforeIndex));
            let job = queue.get(&id).unwrap();
            resume_import_job(&fixture.env(), &mut models(), &job, &mut Vec::new()).unwrap_err();
            let job = queue.get(&id).unwrap();
            assert_eq!((job.state, job.attempts), (JobState::Failed, attempt));
        }
        crate::batch::FAIL_AT.set(None);
        let job = queue.get(&id).unwrap();
        let error =
            resume_import_job(&fixture.env(), &mut models(), &job, &mut Vec::new()).unwrap_err();
        assert!(!matches!(error, Error::LibraryBusy(_)), "{error}");
        assert_eq!(queue.get(&id).unwrap().attempts, MAX_ATTEMPTS);
        // The user's explicit re-run restarts it.
        import_file(
            &fixture.env(),
            &mut models(),
            &path,
            &ImportOptions::default(),
        )
        .unwrap();
    }

    #[test]
    fn direct_rerun_restarts_an_unfinished_import_exhausted_by_crashes() {
        let fixture = Fixture::new();
        let path = fixture.wav("crashed.wav", &[(Some(0), 3.0)]);
        crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::ImportAfterSave));
        import_file(
            &fixture.env(),
            &mut models(),
            &path,
            &ImportOptions::default(),
        )
        .unwrap_err();
        crate::batch::FAIL_AT.set(None);
        let queue = Queue::open(fixture.lib.root()).unwrap();
        let id = queue.list(IMPORT_KIND).unwrap().remove(0).id;
        // Workers crash on it until taking over the last crash uses up its
        // attempts.
        let mut pid = 4243;
        while !queue.is_exhausted(&queue.get(&id).unwrap(), &fixture.probe) {
            let child = broscribe_platform::fakes::FakeProcessProbe::new(pid);
            drop(queue.claim(&fixture.lib, &id, &child).unwrap().unwrap());
            pid += 1;
        }
        // The user's re-run takes it over (failing it) and restarts it, as
        // a new import would.
        let outcome = import_file(
            &fixture.env(),
            &mut models(),
            &path,
            &ImportOptions::default(),
        )
        .unwrap();
        assert_eq!(
            fixture
                .lib
                .load_meeting(&outcome.meeting_id)
                .unwrap()
                .processing
                .status,
            ProcessingStatus::Done
        );
        assert!(queue.list(IMPORT_KIND).unwrap().is_empty());
    }

    #[test]
    fn importing_a_file_inside_the_library_is_refused_and_leaves_it_intact() {
        let fixture = Fixture::new();
        let queue = Queue::open(fixture.lib.root()).unwrap();
        let id = fixture
            .lib
            .write(|tx| queue.enqueue(tx, IMPORT_KIND, "kept", serde_json::json!({})))
            .unwrap();
        let kept = queue.job_dir(&id).join("audio.wav");
        fs::copy(fixture.wav("kept.wav", &[(Some(0), 3.0)]), &kept).unwrap();
        let before = fs::read(&kept).unwrap();
        for handling in [Handling::Copy, Handling::Move, Handling::Reference] {
            let opts = ImportOptions {
                handling: Some(handling),
                ..Default::default()
            };
            let error = import_file(&fixture.env(), &mut models(), &kept, &opts).unwrap_err();
            assert!(
                error.to_string().contains("inside the library folder"),
                "{error}"
            );
            // WRT-04: neither the source's nor the library's path.
            let root = fixture.lib.root().to_string_lossy().into_owned();
            assert!(!error.to_string().contains(&root), "{error}");
            assert!(!error.to_string().contains("audio.wav"), "{error}");
            assert_eq!(fs::read(&kept).unwrap(), before);
        }
        assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
    }

    #[test]
    fn files_in_owned_library_folders_are_refused_but_other_library_files_import() {
        let fixture = Fixture::new();
        let root = fixture.lib.root().to_owned();
        let inbox = root.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        // A user folder inside the library imports with every handling.
        let mut meeting_dir = None;
        for (i, handling) in [Handling::Copy, Handling::Move, Handling::Reference]
            .into_iter()
            .enumerate()
        {
            let path = inbox.join(format!("in-{i}.wav"));
            fs::copy(
                fixture.wav(&format!("in-{i}.wav"), &[(Some(0), 2.0 + i as f64)]),
                &path,
            )
            .unwrap();
            let opts = ImportOptions {
                handling: Some(handling),
                ..Default::default()
            };
            let id = import_file(&fixture.env(), &mut models(), &path, &opts)
                .unwrap()
                .meeting_id;
            let meeting = fixture.lib.load_meeting(&id).unwrap();
            let recording = meeting.recording.unwrap();
            assert_eq!(recording.handling, handling);
            if handling == Handling::Reference {
                assert_eq!(
                    fs::canonicalize(&recording.path).unwrap(),
                    fs::canonicalize(&path).unwrap()
                );
            }
            meeting_dir = Some(fixture.lib.meeting_dir(&id).unwrap());
        }
        // Files in a meeting folder, `people/` or a dot-folder are refused.
        let owned = [
            meeting_dir.unwrap().join("extra.wav"),
            root.join(paths::PEOPLE_DIR).join("voice.wav"),
            root.join(".cache").join("x.wav"),
        ];
        for (i, path) in owned.iter().enumerate() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::copy(
                fixture.wav(&format!("owned-{i}.wav"), &[(Some(1), 3.0 + i as f64)]),
                path,
            )
            .unwrap();
            let error = import_file(
                &fixture.env(),
                &mut models(),
                path,
                &ImportOptions::default(),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("inside the library folder"),
                "{error}"
            );
            assert!(path.is_file());
        }
    }
}

#[cfg(test)]
mod model_switch_tests {
    use super::*;
    use crate::testutil::{Fixture, models};
    use broscribe_core::config::Thresholds;
    use broscribe_core::schema::{
        self, Person, Strength, Voiceprint, VoiceprintKind, VoiceprintSource,
    };
    use broscribe_models::fakes::ToneEmbedder;

    /// An import saved before a speaker-model switch is finished with the
    /// current model, which `reembed` gave its centroids.
    #[test]
    fn resume_after_a_model_switch_resolves_with_the_current_model() {
        const SMALL: &str = "nemo-titanet-small";
        let mut fx = Fixture::new();
        let path = fx.wav("switched.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
        crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::ImportAfterSave));
        import_file(&fx.env(), &mut models(), &path, &ImportOptions::default()).unwrap_err();
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 1);

        // The switch: the profile names the new model and the library is re-embedded.
        fx.config.profile.speaker_embedding = SMALL.into();
        fx.config.identity.insert(
            SMALL.into(),
            Thresholds {
                auto_name: Some(0.9),
                suggest: Some(0.5),
                margin: Some(0.05),
                merge: None,
                auto_merge: None,
            },
        );
        let embedder = ToneEmbedder::new(SMALL, 16);
        crate::reembed(&fx.lib, &fx.chain, &embedder).unwrap();
        let dir = fx.lib.list_meeting_dirs().unwrap().remove(0);
        let m = fx.lib.load_meeting_at(&dir).unwrap();
        let s1 = &m.speakers[0];
        let centroid = s1.centroid.clone().unwrap();
        assert_eq!(centroid.model, SMALL);

        // Ana's voice, known only under the new model.
        let ana = Person::new(fx.lib.new_person_id(), "Ana Example");
        let vector = centroid.vector().unwrap();
        let print = Voiceprint {
            id: "vp-ana-small".into(),
            model: SMALL.into(),
            dim: vector.len(),
            vector_b64: schema::encode_vector(&vector),
            source: VoiceprintSource {
                kind: VoiceprintKind::Enrollment,
                meeting_id: None,
                sha256: Some("a".repeat(64)),
                start: 0.0,
                end: 4.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        };
        fx.lib
            .write(|tx| {
                tx.save_person(&ana)?;
                tx.upsert_voiceprints(&ana.id, vec![print.clone()])
            })
            .unwrap();

        import_file(&fx.env(), &mut models(), &path, &ImportOptions::default()).unwrap();
        let m = fx.lib.load_meeting_at(&dir).unwrap();
        assert_eq!(m.processing.status, ProcessingStatus::Done);
        let named = m.speaker(s1.id).unwrap();
        assert_eq!(named.person_id.as_ref(), Some(&ana.id), "{named:?}");
    }
}
