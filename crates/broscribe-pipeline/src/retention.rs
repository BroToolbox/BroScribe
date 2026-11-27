//! Applying recording retention (design §7.2, plan §§9.10, 10): deletes the
//! recordings [`broscribe_core::retention::eligible`] marks as due, with
//! their intermediates, and keeps `recording.delete_after` current.
//!
//! What a deletion removes: the owned recording in the meeting folder,
//! playback caches under `<cache dir>/playback/` named by the recording's
//! SHA-256, and staged `audio.*` copies left in finished queue job folders
//! of the same recording. Transcripts, enrichment, metadata and voice
//! embeddings stay. A referenced original is never touched.
//!
//! Triggers: workers apply retention after import, batch, reprocess, recording
//! finalization and confirmation jobs; CLI/MCP naming operations also use
//! [`apply_to`]. [`apply`] scans meetings and `people/*/enrollment/*.wav`;
//! never run retention on reads. Only counts are logged. [`explain`] reads
//! each meeting's verdict with its reasons (the due date, the shown speakers
//! a `wait_until_named` recording waits for, the jobs holding it) and
//! changes nothing.
//!
//! A queue record that cannot be read is counted, never acted on: no claim
//! can run or commit it, so it holds no recording back, and its folder is
//! left alone (`queue prune` quarantines it on request).
//!
//! Each meeting is decided again under the library lock; `meeting.json` is
//! saved as `deleted` before the files go, so an interrupted deletion leaves
//! only files that the next run removes.

use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use broscribe_core::config::Config;
use broscribe_core::index::Index;
use broscribe_core::process::ProcessProbe;
use broscribe_core::queue::{Job, JobId, JobState, Queue, job_refers_to};
use broscribe_core::retention::{
    self, DeleteReason, PLAYBACK_CACHE_DIR, RetentionStatus, Verdict, is_playback_cache_of,
};
use broscribe_core::schema::{self, Handling, Meeting, RecordingState};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result};
use chrono::{DateTime, FixedOffset, TimeDelta};
use serde::Serialize;

/// Options of one retention run.
#[derive(Clone, Debug, Default)]
pub struct RetentionOptions {
    /// Report what would happen; change nothing.
    pub dry_run: bool,
    /// Only these meetings; `None`: every meeting of the library.
    pub meetings: Option<Vec<MeetingId>>,
    /// The application cache directory (playback caches live under
    /// [`PLAYBACK_CACHE_DIR`] inside it); `None`: no caches to clean.
    pub cache_dir: Option<PathBuf>,
    /// The time to decide at; `None`: now.
    pub now: Option<DateTime<FixedOffset>>,
}

/// What a run did (or, with `dry_run`, would do). Counts only: no titles,
/// no paths.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RetentionReport {
    pub dry_run: bool,
    /// Meetings looked at.
    pub checked: usize,
    /// Recordings deleted (or due, under `dry_run`).
    pub deleted: Vec<Deletion>,
    /// Due recordings left alone because a queue job works on the meeting.
    pub kept_for_live_jobs: usize,
    /// Recorded size of those recordings: a later run deletes them once the
    /// job is over.
    pub kept_for_live_jobs_bytes: u64,
    /// Meetings whose `recording.delete_after` was updated (or would be).
    pub due_dates_updated: usize,
    /// Leftover files of earlier, interrupted deletions removed (meetings
    /// with leftovers, under `dry_run`).
    pub leftovers_removed: usize,
    /// Enrollment WAVs removed (or due under `dry_run`).
    pub enrollment_files_removed: usize,
    /// Meetings whose `meeting.json` could not be read.
    pub unreadable: usize,
    /// People whose `person.md` could not be read while their folder holds
    /// an enrollment WAV: it is left alone, and whether it is due is
    /// unknown.
    pub unreadable_people: usize,
    /// Queue records that could not be read (left alone; see the module
    /// docs).
    pub unreadable_jobs: usize,
}

/// One meeting's retention verdict with what decided it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MeetingRetention {
    pub meeting_id: MeetingId,
    /// The verdict, its due date and the blocking shown speakers
    /// ([`retention::explain`]).
    #[serde(flatten)]
    pub status: RetentionStatus,
    /// The queue jobs holding the recording back (what makes the verdict
    /// `live_jobs`); empty for any other.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub live_jobs: Vec<JobId>,
}

/// What [`explain`] read: identifiers, reasons and counts only.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RetentionExplanation {
    /// In library order.
    pub meetings: Vec<MeetingRetention>,
    /// Meetings whose `meeting.json` could not be read.
    pub unreadable: usize,
    /// Queue records that could not be read.
    pub unreadable_jobs: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Deletion {
    pub meeting_id: MeetingId,
    pub reason: DeleteReason,
    /// Size of the recording file deleted (its recorded size under `dry_run`).
    pub bytes: u64,
    /// Files removed: the recording plus intermediates (0 under `dry_run`).
    pub files: usize,
}

/// `retention apply [--dry-run]`: decides every meeting (or
/// `opts.meetings`) and applies the verdicts.
pub fn apply(
    lib: &Library,
    config: &Config,
    probe: &dyn ProcessProbe,
    opts: &RetentionOptions,
) -> Result<RetentionReport> {
    apply_with(lib, config, None, probe, opts)
}

/// [`apply`] under the policy configured in `data_dir` now, read again
/// under the lock before each deletion: the user may switch the policy
/// while a long-running worker or a run is under way, and a recording the
/// current policy keeps is never deleted.
pub fn apply_current(
    lib: &Library,
    data_dir: &Path,
    probe: &dyn ProcessProbe,
    opts: &RetentionOptions,
) -> Result<RetentionReport> {
    let config = Config::load(data_dir)?;
    apply_reloading(lib, &config, data_dir, probe, opts)
}

/// [`apply_current`] for a caller that already holds `config`: it picks
/// the candidates; every deletion and due-date write decides again under
/// the lock with the policy read from `data_dir` there.
pub fn apply_reloading(
    lib: &Library,
    config: &Config,
    data_dir: &Path,
    probe: &dyn ProcessProbe,
    opts: &RetentionOptions,
) -> Result<RetentionReport> {
    apply_with(lib, config, Some(data_dir), probe, opts)
}

/// `data_dir`: reload the policy from it under the lock before each write.
fn apply_with(
    lib: &Library,
    config: &Config,
    data_dir: Option<&Path>,
    probe: &dyn ProcessProbe,
    opts: &RetentionOptions,
) -> Result<RetentionReport> {
    let now = opts.now.unwrap_or_else(schema::now);
    let (queue, jobs, unreadable_jobs) = if opts.dry_run {
        match Queue::open_existing(lib.root())? {
            Some(queue) => {
                let snapshot = queue.snapshot()?;
                let invalid = snapshot.invalid.len();
                (Some(queue), snapshot.jobs, invalid)
            }
            None => (None, Vec::new(), 0),
        }
    } else {
        let queue = Queue::open(lib.root())?;
        let snapshot = queue.snapshot()?;
        let invalid = snapshot.invalid.len();
        (Some(queue), snapshot.jobs, invalid)
    };
    let mut report = RetentionReport {
        dry_run: opts.dry_run,
        unreadable_jobs,
        ..RetentionReport::default()
    };

    let meetings = candidates(lib, opts.meetings.as_deref(), &mut report.unreadable)?;
    for (dir, meeting) in meetings {
        report.checked += 1;
        let live = queue
            .as_ref()
            .is_some_and(|queue| has_live_jobs(&jobs, queue, probe, &meeting, now));
        if let Some(reason) = delete_reason(retention::eligible(&meeting, config, now, live)) {
            if opts.dry_run {
                report.deleted.push(Deletion {
                    meeting_id: meeting.id.clone(),
                    reason,
                    bytes: meeting.recording.as_ref().map_or(0, |r| r.bytes),
                    files: 0,
                });
            } else if let Some(queue) = queue.as_ref()
                && let Some(d) =
                    delete_one(lib, config, data_dir, probe, queue, &meeting.id, now, opts)?
            {
                report.deleted.push(d);
            }
            continue;
        }
        if live {
            if retention::eligible(&meeting, config, now, false).is_delete() {
                report.kept_for_live_jobs += 1;
                report.kept_for_live_jobs_bytes +=
                    meeting.recording.as_ref().map_or(0, |r| r.bytes);
            }
            continue;
        }
        if meeting
            .recording
            .as_ref()
            .is_some_and(|r| r.state == RecordingState::Deleted)
            && owned_file(&dir, &meeting).is_some()
        {
            let files = remove_leftovers(lib, queue.as_ref(), probe, &meeting.id, now, opts)?;
            report.leftovers_removed += if opts.dry_run {
                usize::from(files > 0)
            } else {
                files
            };
            continue;
        }
        let due = retention::delete_after(&meeting, config);
        if meeting
            .recording
            .as_ref()
            .is_some_and(|r| r.delete_after != due)
            && (opts.dry_run || sync_due_date(lib, config, data_dir, &meeting.id)?)
        {
            report.due_dates_updated += 1;
        }
    }
    let enrollments = remove_enrollments(lib, config, data_dir, now, opts.dry_run)?;
    report.enrollment_files_removed = enrollments.removed;
    report.unreadable_people = enrollments.unreadable_people;
    tracing::info!(
        dry_run = report.dry_run,
        checked = report.checked,
        deleted = report.deleted.len(),
        kept_for_live_jobs = report.kept_for_live_jobs,
        due_dates_updated = report.due_dates_updated,
        leftovers_removed = report.leftovers_removed,
        unreadable_jobs = report.unreadable_jobs,
        "retention"
    );
    Ok(report)
}

/// The retention verdict of every meeting (or of `meetings`) at `now`
/// (`None`: now) under `config`, with its reasons: the core's
/// [`retention::explain`] with the same live-job rule [`apply`] uses.
/// Reads only: nothing is deleted, synced or repaired.
pub fn explain(
    lib: &Library,
    config: &Config,
    probe: &dyn ProcessProbe,
    meetings: Option<&[MeetingId]>,
    now: Option<DateTime<FixedOffset>>,
) -> Result<RetentionExplanation> {
    let now = now.unwrap_or_else(schema::now);
    let mut out = RetentionExplanation::default();
    let candidates = candidates(lib, meetings, &mut out.unreadable)?;
    let (queue, jobs) = match Queue::open_existing(lib.root())? {
        Some(queue) => {
            let snapshot = queue.snapshot()?;
            out.unreadable_jobs = snapshot.invalid.len();
            (Some(queue), snapshot.jobs)
        }
        None => (None, Vec::new()),
    };
    for (_, meeting) in candidates {
        let live_jobs: Vec<JobId> = match &queue {
            Some(queue) => jobs
                .iter()
                .filter(|j| job_refers_to(j, &meeting) && is_live(j, queue, probe, now))
                .map(|j| j.id.clone())
                .collect(),
            None => Vec::new(),
        };
        let status = retention::explain(&meeting, config, now, !live_jobs.is_empty());
        let held = status.verdict == Verdict::Keep(retention::KeepReason::LiveJobs);
        out.meetings.push(MeetingRetention {
            meeting_id: meeting.id.clone(),
            status,
            live_jobs: if held { live_jobs } else { Vec::new() },
        });
    }
    Ok(out)
}

/// The trigger after identity completion and naming operations: applies
/// retention to `meetings` only.
pub fn apply_to(
    lib: &Library,
    config: &Config,
    probe: &dyn ProcessProbe,
    cache_dir: Option<&Path>,
    meetings: &[MeetingId],
) -> Result<RetentionReport> {
    apply(
        lib,
        config,
        probe,
        &RetentionOptions {
            dry_run: false,
            meetings: Some(meetings.to_vec()),
            cache_dir: cache_dir.map(Path::to_path_buf),
            now: None,
        },
    )
}

fn delete_reason(v: Verdict) -> Option<DeleteReason> {
    match v {
        Verdict::Delete(r) => Some(r),
        Verdict::Keep(_) => None,
    }
}

/// The meetings to decide with their folders, read without the lock;
/// `unreadable` counts the ones whose `meeting.json` cannot be read.
fn candidates(
    lib: &Library,
    only: Option<&[MeetingId]>,
    unreadable: &mut usize,
) -> Result<Vec<(PathBuf, Meeting)>> {
    let dirs = match only {
        Some(ids) => {
            let mut dirs = Vec::new();
            for id in ids {
                match lib.meeting_dir(id) {
                    Ok(d) => dirs.push(d),
                    Err(Error::NotFound(_)) => {}
                    Err(Error::Schema(_)) => *unreadable += 1,
                    Err(e) => return Err(e),
                }
            }
            dirs
        }
        None => lib.list_meeting_dirs()?,
    };
    let mut out = Vec::with_capacity(dirs.len());
    for dir in dirs {
        match lib.load_meeting_at(&dir) {
            Ok(m) => out.push((dir, m)),
            Err(Error::Schema(_)) => *unreadable += 1,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// A job that is not finished (queued, running, analyzed) or whose lease is
/// held by a live process. LLM jobs (enrichment, templates) read only the
/// transcript, never the recording, so they never hold retention back.
fn is_live(job: &Job, queue: &Queue, probe: &dyn ProcessProbe, now: DateTime<FixedOffset>) -> bool {
    if job.kind == crate::enrich::ENRICH_KIND || job.kind == crate::template::TEMPLATE_KIND {
        return false;
    }
    if matches!(
        job.state,
        JobState::Queued | JobState::Running | JobState::Analyzed
    ) {
        return true;
    }
    let stale_after = TimeDelta::from_std(queue.stale_after()).unwrap_or(TimeDelta::MAX);
    job.lease
        .as_ref()
        .is_some_and(|lease| lease.is_held(now, stale_after, probe))
}

fn has_live_jobs(
    jobs: &[Job],
    queue: &Queue,
    probe: &dyn ProcessProbe,
    meeting: &Meeting,
    now: DateTime<FixedOffset>,
) -> bool {
    jobs.iter()
        .any(|j| job_refers_to(j, meeting) && is_live(j, queue, probe, now))
}

/// The recording file inside the meeting folder `dir`, for owned recordings
/// with a plain relative path (never an absolute path or one leaving the
/// folder).
fn owned_file(dir: &Path, meeting: &Meeting) -> Option<PathBuf> {
    let rec = meeting.recording.as_ref()?;
    if rec.handling == Handling::Reference {
        return None;
    }
    let plain = rec
        .path
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    if !plain || rec.path.as_os_str().is_empty() {
        return None;
    }
    Some(dir.join(&rec.path))
}

/// Removes a file; `false` when it did not exist.
fn remove(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

/// Removes the meeting's intermediates: playback caches and staged copies in
/// finished job folders of the same recording. Returns how many files went.
fn remove_intermediates(
    queue: Option<&Queue>,
    probe: &dyn ProcessProbe,
    meeting: &Meeting,
    now: DateTime<FixedOffset>,
    cache_dir: Option<&Path>,
    dry_run: bool,
) -> Result<usize> {
    let Some(rec) = meeting.recording.as_ref() else {
        return Ok(0);
    };
    let mut removed = 0;
    if let Some(cache) = cache_dir {
        let dir = cache.join(PLAYBACK_CACHE_DIR);
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries {
                let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
                let name = entry.file_name();
                if !name
                    .to_str()
                    .is_some_and(|n| is_playback_cache_of(n, &rec.sha256))
                {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    if !dry_run {
                        fs::remove_dir_all(&path).map_err(|e| Error::io_at(&path, e))?;
                    }
                    removed += 1;
                } else if dry_run || remove(&path)? {
                    removed += 1;
                }
            }
        }
    }
    if let Some(queue) = queue {
        for job in queue.jobs()? {
            if !job_refers_to(&job, meeting) || is_live(&job, queue, probe, now) {
                continue;
            }
            // A journaled move, or a move that could not be undone, may hold the
            // user's only copy of the original file.
            if queue.holds_only_copy(&job) {
                continue;
            }
            let dir = queue.job_dir(&job.id);
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
                let staged = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("audio."));
                if staged && entry.path().is_file() && (dry_run || remove(&entry.path())?) {
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

/// Deletes one meeting's recording if, re-read under the lock, it is still
/// due — under the policy read again from `data_dir` there, when given.
/// `None` when it no longer is.
#[allow(clippy::too_many_arguments)]
fn delete_one(
    lib: &Library,
    config: &Config,
    data_dir: Option<&Path>,
    probe: &dyn ProcessProbe,
    queue: &Queue,
    id: &MeetingId,
    now: DateTime<FixedOffset>,
    opts: &RetentionOptions,
) -> Result<Option<Deletion>> {
    lib.write(|tx| {
        let mut fresh = None;
        let config = policy_now(config, data_dir, &mut fresh)?;
        let dir = tx.library().meeting_dir(id)?;
        let mut meeting = tx.library().load_meeting_at(&dir)?;
        let live = has_live_jobs(&queue.jobs()?, queue, probe, &meeting, now);
        let Some(reason) = delete_reason(retention::eligible(&meeting, config, now, live)) else {
            return Ok(None);
        };
        let file = owned_file(&dir, &meeting);
        let bytes = match &file {
            Some(p) => fs::metadata(p).map(|m| m.len()).unwrap_or(0),
            None => 0,
        };
        if let Some(rec) = meeting.recording.as_mut() {
            rec.state = RecordingState::Deleted;
            rec.delete_after = None;
        }
        meeting.updated_at = schema::now();
        // The record first: an interruption leaves files the next run removes.
        tx.save_meeting(&meeting)?;
        let mut files = 0;
        if let Some(p) = &file
            && remove(p)?
        {
            files += 1;
        }
        files += remove_intermediates(
            Some(queue),
            probe,
            &meeting,
            now,
            opts.cache_dir.as_deref(),
            false,
        )?;
        Index::open(tx.root())?.upsert_meeting(tx, &meeting)?;
        Ok(Some(Deletion {
            meeting_id: meeting.id.clone(),
            reason,
            bytes,
            files,
        }))
    })
}

/// Finishes an interrupted deletion; returns how many files were removed.
/// Takes the library lock only when something is left to remove.
fn remove_leftovers(
    lib: &Library,
    queue: Option<&Queue>,
    probe: &dyn ProcessProbe,
    id: &MeetingId,
    now: DateTime<FixedOffset>,
    opts: &RetentionOptions,
) -> Result<usize> {
    let scan = |lib: &Library, dry_run: bool| -> Result<usize> {
        let dir = lib.meeting_dir(id)?;
        let meeting = lib.load_meeting_at(&dir)?;
        let live = match queue {
            Some(queue) => has_live_jobs(&queue.jobs()?, queue, probe, &meeting, now),
            None => false,
        };
        if live || meeting.recording.as_ref().map(|r| r.state) != Some(RecordingState::Deleted) {
            return Ok(0);
        }
        let Some(path) = owned_file(&dir, &meeting) else {
            return Ok(0);
        };
        let mut files = usize::from(path.exists() && (dry_run || remove(&path)?));
        files += remove_intermediates(
            queue,
            probe,
            &meeting,
            now,
            opts.cache_dir.as_deref(),
            dry_run,
        )?;
        Ok(files)
    };
    let found = scan(lib, true)?;
    if opts.dry_run || found == 0 {
        return Ok(found);
    }
    lib.write(|tx| scan(tx.library(), false))
}

/// Sets `recording.delete_after` to the policy's date, re-read under the
/// lock (with the policy, when `data_dir` is given); `false` when it already
/// matched.
fn sync_due_date(
    lib: &Library,
    config: &Config,
    data_dir: Option<&Path>,
    id: &MeetingId,
) -> Result<bool> {
    lib.write(|tx| {
        let mut fresh = None;
        let config = policy_now(config, data_dir, &mut fresh)?;
        let mut meeting = tx.library().load_meeting(id)?;
        let due = retention::delete_after(&meeting, config);
        let Some(rec) = meeting.recording.as_mut() else {
            return Ok(false);
        };
        if rec.delete_after == due {
            return Ok(false);
        }
        rec.delete_after = due;
        meeting.updated_at = schema::now();
        tx.save_meeting(&meeting)?;
        Ok(true)
    })
}

/// The policy to act on under the lock: read again from `data_dir` (into
/// `fresh`) when given, else `config`.
fn policy_now<'c>(
    config: &'c Config,
    data_dir: Option<&Path>,
    fresh: &'c mut Option<Config>,
) -> Result<&'c Config> {
    match data_dir {
        Some(dir) => Ok(fresh.insert(Config::load(dir)?)),
        None => Ok(config),
    }
}

#[cfg(test)]
thread_local! {
    /// Test hook run between the unlocked enrollment scan and the locked
    /// deletion, to change the policy there.
    static BEFORE_ENROLLMENT_LOCK: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

/// What a scan of the kept enrollment recordings found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EnrollmentScan {
    /// Due recordings (deleted ones, when the scan deletes).
    removed: usize,
    /// Person folders holding an enrollment recording whose profile couldn't
    /// be loaded (`person.md` missing, not a regular file, unreadable or
    /// invalid): whether it is due is unknown, so it stays.
    unreadable_people: usize,
}

/// Enrollment is owned audio, independent of meeting retention overrides.
/// Takes the library lock only to delete files, deciding again there under
/// the policy read from `data_dir`, when given; that locked scan is the one
/// reported, so a profile that became readable or unreadable since the
/// first scan is counted as the deletion found it.
fn remove_enrollments(
    lib: &Library,
    config: &Config,
    data_dir: Option<&Path>,
    now: DateTime<FixedOffset>,
    dry_run: bool,
) -> Result<EnrollmentScan> {
    if matches!(
        config.retention,
        broscribe_core::config::Retention::KeepForever
    ) {
        return Ok(EnrollmentScan::default());
    }
    let scan = scan_enrollments(lib, config, now, false)?;
    if dry_run || scan.removed == 0 {
        return Ok(scan);
    }
    #[cfg(test)]
    if let Some(hook) = BEFORE_ENROLLMENT_LOCK.take() {
        hook();
    }
    lib.write(|tx| {
        let mut fresh = None;
        let config = policy_now(config, data_dir, &mut fresh)?;
        if matches!(
            config.retention,
            broscribe_core::config::Retention::KeepForever
        ) {
            return Ok(EnrollmentScan::default());
        }
        scan_enrollments(tx.library(), config, now, true)
    })
}

/// Counts the enrollment recordings due for deletion, deleting them when
/// `delete` (the caller then holds the library lock), and the person folders
/// holding one whose profile couldn't be loaded, whatever the reason.
fn scan_enrollments(
    lib: &Library,
    config: &Config,
    now: DateTime<FixedOffset>,
    delete: bool,
) -> Result<EnrollmentScan> {
    let mut scan = EnrollmentScan::default();
    let mut people = Vec::new();
    for id in person_folders(lib)? {
        match load_regular_profile(lib, &id) {
            Some(person) => people.push(person),
            None => {
                if holds_enrollment_recording(&lib.person_dir(&id).join("enrollment"))? {
                    scan.unreadable_people += 1;
                }
            }
        }
    }
    if scan.unreadable_people > 0 {
        // Counts only: the path would name the person.
        tracing::warn!(
            unreadable_people = scan.unreadable_people,
            "enrollment recordings of unreadable profiles left alone"
        );
    }
    for person in people {
        let dir = lib.person_dir(&person.id).join("enrollment");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::io_at(&dir, e)),
        };
        let committed = lib
            .load_voiceprints(&person.id)?
            .embeddings
            .iter()
            .any(|v| v.source.kind == schema::VoiceprintKind::Enrollment);
        for entry in entries {
            let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
            if !is_enrollment_recording(&path, &metadata) {
                continue;
            }
            let due = match config.retention {
                broscribe_core::config::Retention::KeepForever => false,
                broscribe_core::config::Retention::DeleteImmediately { .. } => committed,
                broscribe_core::config::Retention::DeleteAfterDays { days } => {
                    let created = chrono::DateTime::<chrono::Utc>::from(
                        recorded_at(metadata.created(), || metadata.modified())
                            .map_err(|e| Error::io_at(&path, e))?,
                    );
                    now.signed_duration_since(created) >= TimeDelta::days(i64::from(days))
                }
            };
            if due && (!delete || remove(&path)?) {
                scan.removed += 1;
            }
        }
    }
    Ok(scan)
}

/// The IDs of the person folders in the library, sorted: every visible
/// folder named like a person ID, whether its `person.md` can be loaded or
/// not (`Library::list_people_with_skipped` passes over a folder without a
/// `person.md` file, which may still hold an enrollment recording).
fn person_folders(lib: &Library) -> Result<Vec<PersonId>> {
    let dir = lib.people_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(&dir, e)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
        let is_dir = entry
            .file_type()
            .map_err(|e| Error::io_at(&entry.path(), e))?
            .is_dir();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_dir || name.starts_with('.') {
            continue;
        }
        if let Ok(id) = PersonId::parse(name) {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}

/// The person's profile, when `person.md` is a regular file that loads.
/// Any other type (missing, a directory, a symbolic link, a FIFO, a socket
/// or a device) is never opened: reading a FIFO would block the pass.
fn load_regular_profile(lib: &Library, id: &PersonId) -> Option<schema::Person> {
    let path = lib.person_dir(id).join(broscribe_core::paths::PERSON_MD);
    let regular = fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file());
    regular.then(|| lib.load_person(id).ok()).flatten()
}

/// A regular `.wav` file: what retention counts as an enrollment recording.
fn is_enrollment_recording(path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.is_file() && path.extension().is_some_and(|ext| ext == "wav")
}

/// Whether the enrollment folder `dir` holds an enrollment recording.
fn holds_enrollment_recording(dir: &Path) -> Result<bool> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::io_at(dir, e)),
    };
    for entry in entries {
        let path = entry.map_err(|e| Error::io_at(dir, e))?.path();
        let metadata = fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
        if is_enrollment_recording(&path, &metadata) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// When an enrollment file was written: its creation time, else (file
/// systems without one, such as tmpfs or NFS) its modification time, which
/// enrollment never changes after writing.
fn recorded_at(
    created: std::io::Result<std::time::SystemTime>,
    modified: impl FnOnce() -> std::io::Result<std::time::SystemTime>,
) -> std::io::Result<std::time::SystemTime> {
    created.or_else(|_| modified())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{ImportOptions, import_file};
    use crate::testutil::{Fixture, models};
    use broscribe_core::PersonId;
    use broscribe_core::config::Retention;
    use broscribe_core::ops;
    use broscribe_core::paths;
    use broscribe_core::schema::{Assignment, RetentionOverride};

    /// Imports a two-voice recording; `voice` (0..=2) makes the content unique.
    fn import(
        fx: &Fixture,
        name: &str,
        voice: usize,
        handling: Handling,
        started: Option<&str>,
    ) -> MeetingId {
        let file = fx.wav(name, &[(Some(voice), 3.0), (Some(voice + 1), 3.0)]);
        let opts = ImportOptions {
            handling: Some(handling),
            started_at: started.map(|s| DateTime::parse_from_rfc3339(s).unwrap()),
            ..ImportOptions::default()
        };
        import_file(&fx.env(), &mut models(), &file, &opts)
            .unwrap()
            .meeting_id
    }

    fn run(fx: &Fixture, cache: Option<&Path>, dry_run: bool) -> RetentionReport {
        apply(
            &fx.lib,
            &fx.config,
            &fx.probe,
            &RetentionOptions {
                dry_run,
                cache_dir: cache.map(Path::to_path_buf),
                ..RetentionOptions::default()
            },
        )
        .unwrap()
    }

    fn trigger(fx: &Fixture, id: &MeetingId) -> RetentionReport {
        apply_to(
            &fx.lib,
            &fx.config,
            &fx.probe,
            None,
            std::slice::from_ref(id),
        )
        .unwrap()
    }

    fn edit(fx: &Fixture, id: &MeetingId, f: impl FnOnce(&mut Meeting)) {
        fx.lib
            .write(|tx| {
                tx.update_meeting(id, |m| {
                    f(m);
                    Ok(())
                })
            })
            .unwrap();
    }

    fn audio(fx: &Fixture, id: &MeetingId) -> PathBuf {
        let m = fx.lib.load_meeting(id).unwrap();
        fx.lib
            .meeting_dir(id)
            .unwrap()
            .join(&m.recording.unwrap().path)
    }

    fn state(fx: &Fixture, id: &MeetingId) -> RecordingState {
        fx.lib.load_meeting(id).unwrap().recording.unwrap().state
    }

    #[test]
    fn delete_immediately_removes_audio_and_caches_once() {
        let mut fx = Fixture::new();
        let id = import(&fx, "a.wav", 0, Handling::Copy, None);
        let sha = fx.lib.load_meeting(&id).unwrap().recording.unwrap().sha256;
        let cache = fx.input.join("cache");
        let playback = cache.join(PLAYBACK_CACHE_DIR);
        fs::create_dir_all(&playback).unwrap();
        fs::write(playback.join(format!("{sha}.caf")), b"pcm").unwrap();
        fs::write(playback.join("other.caf"), b"pcm").unwrap();
        let file = audio(&fx, &id);
        assert!(file.is_file());

        // Keep forever (default): nothing happens.
        let r = run(&fx, Some(&cache), false);
        assert_eq!(r.checked, 1);
        assert!(r.deleted.is_empty());
        assert!(file.is_file());

        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        // Dry run: reported, nothing touched.
        let r = run(&fx, Some(&cache), true);
        assert_eq!(r.deleted.len(), 1);
        assert_eq!(r.deleted[0].reason, DeleteReason::Immediately);
        assert!(file.is_file());
        assert_eq!(state(&fx, &id), RecordingState::Available);

        let r = run(&fx, Some(&cache), false);
        assert_eq!(r.deleted.len(), 1);
        assert_eq!(r.deleted[0].files, 2);
        assert!(r.deleted[0].bytes > 0);
        assert!(!file.exists());
        assert!(!playback.join(format!("{sha}.caf")).exists());
        assert!(playback.join("other.caf").exists());
        let m = fx.lib.load_meeting(&id).unwrap();
        assert_eq!(m.recording.as_ref().unwrap().state, RecordingState::Deleted);
        // Transcript and identity stay.
        assert!(!m.segments.is_empty() && !m.speakers.is_empty());
        let md =
            fs::read_to_string(fx.lib.meeting_dir(&id).unwrap().join(paths::MEETING_MD)).unwrap();
        assert!(md.contains("state: deleted"), "{md}");
        assert!(fx.lib.meeting_audio_path(&m).unwrap().is_none());

        // Idempotent.
        let r = run(&fx, Some(&cache), false);
        assert!(r.deleted.is_empty() && r.leftovers_removed == 0);

        // An interrupted deletion (record saved, file left) is finished.
        fs::write(&file, b"left over").unwrap();
        let r = run(&fx, None, false);
        assert_eq!(r.leftovers_removed, 1);
        assert!(!file.exists());
    }

    #[test]
    fn deleted_recording_cleans_intermediates_without_main_file() {
        let fx = Fixture::new();
        let id = import(&fx, "gone.wav", 0, Handling::Copy, None);
        let sha = fx.lib.load_meeting(&id).unwrap().recording.unwrap().sha256;
        fs::remove_file(audio(&fx, &id)).unwrap();
        edit(&fx, &id, |m| {
            m.recording.as_mut().unwrap().state = RecordingState::Deleted
        });
        let cache = fx.input.join("cache");
        let playback = cache.join(PLAYBACK_CACHE_DIR);
        fs::create_dir_all(&playback).unwrap();
        let cached = playback.join(format!("{sha}.wav"));
        fs::write(&cached, b"synthetic").unwrap();
        assert_eq!(run(&fx, Some(&cache), true).leftovers_removed, 1);
        assert!(cached.exists());
        assert_eq!(run(&fx, Some(&cache), false).leftovers_removed, 1);
        assert!(!cached.exists());
        assert_eq!(run(&fx, Some(&cache), false).leftovers_removed, 0);
        assert_eq!(run(&fx, Some(&cache), true).leftovers_removed, 0);
    }

    #[test]
    fn retention_never_removes_audio_a_conflicted_move_kept() {
        use crate::import::{MOVE_CONFLICT, MoveJournal, MoveState};
        use broscribe_core::queue::IMPORT_KIND;
        let mut fx = Fixture::new();
        let id = import(&fx, "dup.wav", 0, Handling::Copy, None);
        let rec = fx.lib.load_meeting(&id).unwrap().recording.unwrap();
        // Recorded the way a multi-source manifest import is: the source's
        // hash stays the meeting's even once its recording is deleted.
        edit(&fx, &id, |m| {
            m.recording.as_mut().unwrap().source_sha256 = Some(rec.sha256.clone());
        });
        // A move of the same content ended in the duplicate conflict: the
        // meeting already has it, another file took the original path, so
        // the moved audio stays in its job folder, the user's only copy.
        let queue = Queue::open(fx.lib.root()).unwrap();
        let job = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    IMPORT_KIND,
                    &rec.sha256,
                    serde_json::json!({"handling": "move"}),
                )
            })
            .unwrap();
        let lease = queue.claim(&fx.lib, &job, &fx.probe).unwrap().unwrap();
        let kept = lease.job_dir().join("audio.wav");
        fs::copy(fx.input.join("dup.wav"), &kept).unwrap();
        let journal = MoveJournal {
            source: fx.input.join("dup.wav"),
            staged: "audio.wav".into(),
            dest: None,
            sha256: rec.sha256.clone(),
            bytes: rec.bytes,
            renamed: true,
            state: MoveState::Verified,
            modified: None,
        };
        broscribe_core::fsutil::write_json_atomic(&lease.job_dir().join(MOVE_CONFLICT), &journal)
            .unwrap();
        lease
            .fail_keeping(&fx.lib, "already in the library", kept.clone())
            .unwrap();
        lease.release(&fx.lib).unwrap();
        let content = fs::read(&kept).unwrap();

        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        let r = run(&fx, None, false);
        assert_eq!(r.deleted.len(), 1);
        assert_eq!(state(&fx, &id), RecordingState::Deleted);
        assert_eq!(fs::read(&kept).unwrap(), content);
        // The leftover sweep of an interrupted deletion leaves it too.
        let file = audio(&fx, &id);
        fs::write(&file, b"left over").unwrap();
        assert_eq!(run(&fx, None, false).leftovers_removed, 1);
        assert!(!file.exists());
        assert_eq!(fs::read(&kept).unwrap(), content);
        assert!(queue.get(&job).unwrap().keeps_file());
    }

    #[test]
    fn retention_without_work_takes_no_library_lock() {
        let fx = Fixture::new();
        let id = import(&fx, "clean.wav", 0, Handling::Copy, None);
        fs::remove_file(audio(&fx, &id)).unwrap();
        edit(&fx, &id, |m| {
            m.recording.as_mut().unwrap().state = RecordingState::Deleted
        });
        let person = broscribe_core::schema::Person::new(fx.lib.new_person_id(), "Ari Example");
        fx.lib.write(|tx| tx.save_person(&person)).unwrap();
        let dir = fx.lib.person_dir(&person.id).join("enrollment");
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("synthetic.wav");
        fs::write(&wav, b"synthetic").unwrap();
        // Another writer holds the lock throughout.
        let busy = fx
            .lib
            .clone()
            .with_lock_wait(std::time::Duration::from_millis(50));
        let busy_run = |config: &Config, dry_run: bool| {
            apply(
                &busy,
                config,
                &fx.probe,
                &RetentionOptions {
                    dry_run,
                    ..RetentionOptions::default()
                },
            )
        };
        fx.lib
            .write(|_| {
                // Keep forever, nothing left over: no lock for either pass.
                let r = busy_run(&fx.config, false)?;
                assert_eq!((r.leftovers_removed, r.enrollment_files_removed), (0, 0));
                // A dry run reports due enrollment audio without the lock.
                let mut config = fx.config.clone();
                config.retention = Retention::DeleteAfterDays { days: 0 };
                assert_eq!(busy_run(&config, true)?.enrollment_files_removed, 1);
                Ok(())
            })
            .unwrap();
        assert!(wav.exists());
    }

    #[test]
    fn enrollment_age_falls_back_to_modification_time() {
        let modified = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7);
        let unsupported = std::io::Error::new(ErrorKind::Unsupported, "no birth time");
        assert_eq!(
            recorded_at(Err(unsupported), || Ok(modified)).unwrap(),
            modified
        );
        let created = std::time::SystemTime::UNIX_EPOCH;
        assert_eq!(recorded_at(Ok(created), || Ok(modified)).unwrap(), created);
    }

    #[test]
    fn enrollment_retention_uses_creation_time_and_requires_committed_prints() {
        let mut fx = Fixture::new();
        let person = broscribe_core::schema::Person::new(fx.lib.new_person_id(), "Ari Example");
        fx.lib.write(|tx| tx.save_person(&person)).unwrap();
        let dir = fx.lib.person_dir(&person.id).join("enrollment");
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("synthetic.wav");
        fs::write(&wav, b"synthetic").unwrap();
        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: true,
        };
        run(&fx, None, false);
        assert!(wav.exists());
        let mut prints = schema::Voiceprints::empty(person.id.clone());
        prints.embeddings.push(schema::Voiceprint {
            id: "synthetic".into(),
            model: "test".into(),
            dim: 1,
            vector_b64: schema::encode_vector(&[1.0]),
            source: schema::VoiceprintSource {
                kind: schema::VoiceprintKind::Enrollment,
                meeting_id: None,
                sha256: None,
                start: 0.0,
                end: 1.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: schema::Strength::Confirmed,
            created_at: schema::now(),
        });
        fx.lib.write(|tx| tx.save_voiceprints(&prints)).unwrap();
        let dry = RetentionOptions {
            dry_run: true,
            ..Default::default()
        };
        assert_eq!(
            apply(&fx.lib, &fx.config, &fx.probe, &dry)
                .unwrap()
                .enrollment_files_removed,
            1
        );
        assert!(wav.exists());
        assert_eq!(run(&fx, None, false).enrollment_files_removed, 1);
        assert!(!wav.exists());
        fs::write(&wav, b"synthetic").unwrap();
        fx.config.retention = Retention::DeleteAfterDays { days: 30 };
        let created =
            chrono::DateTime::<chrono::Utc>::from(fs::metadata(&wav).unwrap().created().unwrap())
                .fixed_offset();
        let before = RetentionOptions {
            now: Some(created + TimeDelta::days(29)),
            ..Default::default()
        };
        assert_eq!(
            apply(&fx.lib, &fx.config, &fx.probe, &before)
                .unwrap()
                .enrollment_files_removed,
            0
        );
        let due = RetentionOptions {
            now: Some(created + TimeDelta::days(30)),
            ..Default::default()
        };
        assert_eq!(
            apply(&fx.lib, &fx.config, &fx.probe, &due)
                .unwrap()
                .enrollment_files_removed,
            1
        );
    }

    /// Saves an invented person with valid enrollment voiceprints and a kept
    /// reading under `processed`, and checks a dry run counts the reading as
    /// due; returns the person's folder and the reading.
    fn person_with_kept_reading(fx: &mut Fixture) -> (PathBuf, PathBuf) {
        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        let person = broscribe_core::schema::Person::new(fx.lib.new_person_id(), "Ari Example");
        fx.lib.write(|tx| tx.save_person(&person)).unwrap();
        let mut prints = schema::Voiceprints::empty(person.id.clone());
        prints.embeddings.push(schema::Voiceprint {
            id: "synthetic".into(),
            model: "test".into(),
            dim: 1,
            vector_b64: schema::encode_vector(&[1.0]),
            source: schema::VoiceprintSource {
                kind: schema::VoiceprintKind::Enrollment,
                meeting_id: None,
                sha256: None,
                start: 0.0,
                end: 1.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: schema::Strength::Confirmed,
            created_at: schema::now(),
        });
        fx.lib.write(|tx| tx.save_voiceprints(&prints)).unwrap();
        let folder = fx.lib.person_dir(&person.id);
        fs::create_dir_all(folder.join("enrollment")).unwrap();
        let wav = folder.join("enrollment/synthetic.wav");
        fs::write(&wav, b"synthetic").unwrap();
        // Readable, the reading is due.
        let dry = run(fx, None, true);
        assert_eq!(dry.enrollment_files_removed, 1);
        assert_eq!(dry.unreadable_people, 0);
        (folder, wav)
    }

    /// A person folder with valid enrollment voiceprints and a kept reading
    /// whose `person.md` is missing, a directory, or invalid.
    /// Whether the reading is due is unknown, so every pass counts the
    /// folder and leaves the reading alone.
    #[test]
    fn enrollment_beside_an_unloadable_profile_is_counted_and_kept() {
        for what in ["missing", "directory", "invalid"] {
            let mut fx = Fixture::new();
            let (folder, wav) = person_with_kept_reading(&mut fx);

            let profile = folder.join(paths::PERSON_MD);
            match what {
                "missing" => fs::remove_file(&profile).unwrap(),
                "directory" => {
                    fs::remove_file(&profile).unwrap();
                    fs::create_dir(&profile).unwrap();
                }
                _ => fs::write(&profile, b"\xff\xfe not a profile").unwrap(),
            }
            for policy in [
                Retention::DeleteImmediately {
                    wait_until_named: false,
                },
                Retention::DeleteImmediately {
                    wait_until_named: true,
                },
                Retention::DeleteAfterDays { days: 1 },
            ] {
                fx.config.retention = policy;
                let dry = run(&fx, None, true);
                assert_eq!(dry.unreadable_people, 1, "{what} {policy:?}");
                assert_eq!(dry.enrollment_files_removed, 0, "{what} {policy:?}");
            }
            fx.config.retention = Retention::DeleteImmediately {
                wait_until_named: false,
            };
            let real = run(&fx, None, false);
            assert_eq!(real.unreadable_people, 1, "{what}");
            assert_eq!(real.enrollment_files_removed, 0, "{what}");
            assert!(wav.is_file(), "{what}");

            // Without a reading there is nothing unknown.
            fs::remove_file(&wav).unwrap();
            assert_eq!(run(&fx, None, true).unreadable_people, 0, "{what}");
        }
    }

    /// A `person.md` that is a FIFO, or a symbolic link to a valid profile,
    /// is never opened: the pass finishes, counts the folder and leaves the
    /// reading alone. Reading the FIFO would wait for a writer forever, so
    /// the passes run on their own thread against a deadline.
    #[cfg(unix)]
    #[test]
    fn enrollment_beside_a_fifo_or_linked_profile_is_counted_without_opening_it() {
        use std::sync::mpsc::{RecvTimeoutError, channel};
        use std::time::Duration;

        let (done, finished) = channel();
        let passes = std::thread::spawn(move || {
            for what in ["fifo", "symlink"] {
                let mut fx = Fixture::new();
                let (folder, wav) = person_with_kept_reading(&mut fx);
                let profile = folder.join(paths::PERSON_MD);
                if what == "fifo" {
                    fs::remove_file(&profile).unwrap();
                    let made = std::process::Command::new("mkfifo")
                        .arg(&profile)
                        .status()
                        .unwrap();
                    assert!(made.success());
                } else {
                    let target = fx._tmp.path().join("linked-person.md");
                    fs::rename(&profile, &target).unwrap();
                    std::os::unix::fs::symlink(&target, &profile).unwrap();
                }
                let dry = run(&fx, None, true);
                assert_eq!(dry.unreadable_people, 1, "{what}");
                assert_eq!(dry.enrollment_files_removed, 0, "{what}");
                let real = run(&fx, None, false);
                assert_eq!(real.unreadable_people, 1, "{what}");
                assert_eq!(real.enrollment_files_removed, 0, "{what}");
                assert!(wav.is_file(), "{what}");
            }
            done.send(()).unwrap();
        });
        match finished.recv_timeout(Duration::from_secs(60)) {
            Ok(()) => passes.join().unwrap(),
            Err(RecvTimeoutError::Timeout) => panic!("a retention pass blocked on person.md"),
            Err(RecvTimeoutError::Disconnected) => {
                if let Err(panic) = passes.join() {
                    std::panic::resume_unwind(panic);
                }
            }
        }
    }

    #[test]
    fn enrichment_jobs_do_not_keep_audio() {
        let mut fx = Fixture::new();
        let id = import(&fx, "enrich.wav", 0, Handling::Copy, None);
        fx.lib
            .write(|tx| crate::enrich::enqueue_enrich(tx, &id))
            .unwrap();
        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        assert_eq!(trigger(&fx, &id).deleted.len(), 1);
    }

    #[test]
    fn wait_until_named_and_overrides() {
        let mut fx = Fixture::new();
        let id = import(&fx, "a.wav", 0, Handling::Copy, None);
        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: true,
        };
        assert!(run(&fx, None, false).deleted.is_empty());

        // A suggestion is not a name.
        edit(&fx, &id, |m| {
            for s in &mut m.speakers {
                s.assignment = Assignment::Suggested;
            }
        });
        assert!(trigger(&fx, &id).deleted.is_empty());
        assert_eq!(state(&fx, &id), RecordingState::Available);

        // Every speaker named, but the user chose to keep this recording.
        edit(&fx, &id, |m| {
            for s in &mut m.speakers {
                s.person_id = Some(PersonId::parse("p-a1a1a1").unwrap());
                s.assignment = Assignment::Confirmed;
            }
            m.recording.as_mut().unwrap().retention_override = Some(RetentionOverride::Keep);
        });
        assert!(trigger(&fx, &id).deleted.is_empty());
        // Without the override, the trigger after naming deletes.
        edit(&fx, &id, |m| {
            m.recording.as_mut().unwrap().retention_override = None;
        });
        assert_eq!(trigger(&fx, &id).deleted.len(), 1);
        assert_eq!(state(&fx, &id), RecordingState::Deleted);

        // Keep forever with a per-meeting "delete now".
        fx.config.retention = Retention::KeepForever;
        let other = import(&fx, "b.wav", 1, Handling::Copy, None);
        edit(&fx, &other, |m| {
            m.recording.as_mut().unwrap().retention_override = Some(RetentionOverride::DeleteNow);
        });
        let r = run(&fx, None, false);
        assert_eq!(r.deleted.len(), 1);
        assert_eq!(r.deleted[0].reason, DeleteReason::Override);
        assert_eq!(state(&fx, &other), RecordingState::Deleted);
    }

    /// "Can't tell" settles a speaker for `wait_until_named` only after
    /// [`retention::DISMISS_GRACE`]: a maintenance pass inside the Undo
    /// window keeps the recording, Undo returns it to waiting, and a pass
    /// after the grace with the dismissal still set deletes it.
    #[test]
    fn wait_until_named_counts_a_dismissal_after_its_grace() {
        let mut fx = Fixture::new();
        let id = import(&fx, "a.wav", 0, Handling::Copy, None);
        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: true,
        };
        let ids: Vec<_> = fx
            .lib
            .load_meeting(&id)
            .unwrap()
            .speakers
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids.len(), 2);
        edit(&fx, &id, |m| {
            let s = m.speaker_mut(ids[0]).unwrap();
            s.person_id = Some(PersonId::parse("p-a1a1a1").unwrap());
            s.assignment = Assignment::Confirmed;
        });
        let set = |dismiss: bool| {
            let fence = ops::SpeakerFence::of(&fx.lib.load_meeting(&id).unwrap());
            ops::with_index(&fx.lib, |tx, index| {
                if dismiss {
                    ops::dismiss_review(tx, index, &id, ids[1], &fence)
                } else {
                    ops::restore_review(tx, index, &id, ids[1], &fence)
                }
            })
            .unwrap()
        };
        let at = |fx: &Fixture, after: TimeDelta| {
            let m = fx.lib.load_meeting(&id).unwrap();
            let dismissed = m.speaker(ids[1]).unwrap().review_dismissed_at;
            apply(
                &fx.lib,
                &fx.config,
                &fx.probe,
                &RetentionOptions {
                    now: Some(dismissed.unwrap_or_else(schema::now) + after),
                    ..RetentionOptions::default()
                },
            )
            .unwrap()
        };

        // The last unresolved speaker dismissed: within the grace, kept.
        assert!(set(true).changed);
        assert!(trigger(&fx, &id).deleted.is_empty());
        assert!(at(&fx, TimeDelta::minutes(5)).deleted.is_empty());
        assert_eq!(state(&fx, &id), RecordingState::Available);
        // Undo: waiting for a name again, however much later.
        assert!(set(false).changed);
        assert!(at(&fx, TimeDelta::hours(1)).deleted.is_empty());
        assert_eq!(state(&fx, &id), RecordingState::Available);
        // Dismissed again and left so: the pass after the grace deletes.
        set(true);
        assert_eq!(
            at(&fx, retention::DISMISS_GRACE).deleted.len(),
            1,
            "settled once the grace has passed"
        );
        assert_eq!(state(&fx, &id), RecordingState::Deleted);
    }

    #[test]
    fn reference_files_are_never_deleted() {
        let mut fx = Fixture::new();
        let id = import(&fx, "ref.wav", 0, Handling::Reference, None);
        fx.config.retention = Retention::DeleteAfterDays { days: 0 };
        edit(&fx, &id, |m| {
            m.recording.as_mut().unwrap().retention_override = Some(RetentionOverride::DeleteNow);
        });
        let r = run(&fx, None, false);
        assert!(r.deleted.is_empty());
        assert!(fx.input.join("ref.wav").is_file());
        assert_eq!(state(&fx, &id), RecordingState::Available);
    }

    #[test]
    fn live_jobs_keep_the_recording() {
        let mut fx = Fixture::new();
        let id = import(&fx, "a.wav", 0, Handling::Copy, None);
        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        let job = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    broscribe_core::queue::REPROCESS_KIND,
                    id.as_str(),
                    serde_json::json!({ "meeting_id": id }),
                )
            })
            .unwrap();
        let r = run(&fx, None, false);
        assert!(r.deleted.is_empty());
        assert_eq!(r.kept_for_live_jobs, 1);
        let recorded = fx.lib.load_meeting(&id).unwrap().recording.unwrap().bytes;
        assert!(recorded > 0);
        assert_eq!(r.kept_for_live_jobs_bytes, recorded);
        assert!(audio(&fx, &id).is_file());

        // A finished job whose lease a live process still holds also counts.
        let lease = queue.claim(&fx.lib, &job, &fx.probe).unwrap().unwrap();
        lease
            .commit(&fx.lib, JobState::Done, |_, _| Ok(()))
            .unwrap();
        assert_eq!(run(&fx, None, false).kept_for_live_jobs, 1);
        lease.release(&fx.lib).unwrap();

        // Finished and released: deleted.
        let r = run(&fx, None, false);
        assert_eq!(r.deleted.len(), 1);
        assert_eq!(state(&fx, &id), RecordingState::Deleted);
    }

    #[test]
    fn delete_after_days_sets_the_date_then_deletes_when_due() {
        let mut fx = Fixture::new();
        let old = import(
            &fx,
            "old.wav",
            0,
            Handling::Copy,
            Some("2026-01-05T10:00:00-03:00"),
        );
        let recent = import(
            &fx,
            "new.wav",
            1,
            Handling::Copy,
            Some("2026-11-10T10:00:00-03:00"),
        );
        fx.config.retention = Retention::DeleteAfterDays { days: 30 };
        let now = DateTime::parse_from_rfc3339("2026-11-14T12:00:00-03:00").unwrap();
        let opts = RetentionOptions {
            now: Some(now),
            ..RetentionOptions::default()
        };
        let r = apply(&fx.lib, &fx.config, &fx.probe, &opts).unwrap();
        assert_eq!(r.deleted.len(), 1);
        assert_eq!(r.deleted[0].meeting_id, old);
        assert_eq!(r.deleted[0].reason, DeleteReason::AfterDays);
        assert_eq!(r.due_dates_updated, 1);
        let rec = fx.lib.load_meeting(&recent).unwrap().recording.unwrap();
        assert_eq!(
            rec.delete_after,
            Some(DateTime::parse_from_rfc3339("2026-12-10T10:00:00-03:00").unwrap())
        );
        assert_eq!(rec.state, RecordingState::Available);
        let md = fs::read_to_string(fx.lib.meeting_dir(&recent).unwrap().join(paths::MEETING_MD))
            .unwrap();
        assert!(md.contains("delete_after"), "{md}");
        // Nothing changes on a second run.
        let r = apply(&fx.lib, &fx.config, &fx.probe, &opts).unwrap();
        assert!(r.deleted.is_empty());
        assert_eq!(r.due_dates_updated, 0);

        // Back to keep forever: the date is cleared.
        fx.config.retention = Retention::KeepForever;
        let r = apply(&fx.lib, &fx.config, &fx.probe, &opts).unwrap();
        assert_eq!(r.due_dates_updated, 1);
        assert_eq!(
            fx.lib
                .load_meeting(&recent)
                .unwrap()
                .recording
                .unwrap()
                .delete_after,
            None
        );
    }

    #[test]
    fn enrollment_kept_when_the_policy_switches_during_the_pass() {
        let fx = Fixture::new();
        let data = fx.input.join("data");
        let mut config = Config::default();
        config.retention = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        config.save(&data).unwrap();
        let person = broscribe_core::schema::Person::new(fx.lib.new_person_id(), "Ari Example");
        fx.lib.write(|tx| tx.save_person(&person)).unwrap();
        let dir = fx.lib.person_dir(&person.id).join("enrollment");
        fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("synthetic.wav");
        fs::write(&wav, b"synthetic").unwrap();
        let mut prints = schema::Voiceprints::empty(person.id.clone());
        prints.embeddings.push(schema::Voiceprint {
            id: "synthetic".into(),
            model: "test".into(),
            dim: 1,
            vector_b64: schema::encode_vector(&[1.0]),
            source: schema::VoiceprintSource {
                kind: schema::VoiceprintKind::Enrollment,
                meeting_id: None,
                sha256: None,
                start: 0.0,
                end: 1.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: schema::Strength::Confirmed,
            created_at: schema::now(),
        });
        fx.lib.write(|tx| tx.save_voiceprints(&prints)).unwrap();
        // The user switches to keep forever after the scan found it due.
        let switch = data.clone();
        BEFORE_ENROLLMENT_LOCK.set(Some(Box::new(move || {
            let mut c = Config::load(&switch).unwrap();
            c.retention = Retention::KeepForever;
            c.save(&switch).unwrap();
        })));
        let r = apply_current(&fx.lib, &data, &fx.probe, &RetentionOptions::default()).unwrap();
        assert_eq!(r.enrollment_files_removed, 0);
        assert!(wav.exists());
    }

    /// A real pass reports what its deleting scan under the library lock
    /// found: a profile that stopped loading after the first scan keeps its
    /// reading and is counted, rather than the first scan's count staying.
    #[test]
    fn enrollment_reports_the_locked_scan_when_a_profile_changes_between_scans() {
        let mut fx = Fixture::new();
        let (folder, wav) = person_with_kept_reading(&mut fx);
        let profile = folder.join(paths::PERSON_MD);
        BEFORE_ENROLLMENT_LOCK.set(Some(Box::new(move || {
            fs::write(&profile, b"\xff\xfe not a profile").unwrap();
        })));
        let real = run(&fx, None, false);
        assert!(
            BEFORE_ENROLLMENT_LOCK.take().is_none(),
            "the lock was taken"
        );
        assert_eq!(real.enrollment_files_removed, 0);
        assert_eq!(real.unreadable_people, 1);
        assert!(wav.is_file());
    }

    /// `explain` gives the core's verdict with the shown speakers holding a
    /// `wait_until_named` recording and the jobs holding one back, and
    /// changes nothing on the way.
    #[test]
    fn explain_names_blocking_speakers_and_live_jobs_without_changing_anything() {
        use broscribe_core::retention::KeepReason;
        let mut fx = Fixture::new();
        let id = import(&fx, "a.wav", 0, Handling::Copy, None);
        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: true,
        };
        let before = fx.lib.load_meeting(&id).unwrap();
        let read = |fx: &Fixture| {
            let only = std::slice::from_ref(&id);
            let mut e = explain(&fx.lib, &fx.config, &fx.probe, Some(only), None).unwrap();
            assert_eq!(e.meetings.len(), 1);
            e.meetings.remove(0)
        };

        let waiting = read(&fx);
        assert_eq!(
            waiting.status.verdict,
            Verdict::Keep(KeepReason::WaitingForNames)
        );
        assert!(waiting.live_jobs.is_empty());
        assert!(!waiting.status.blocking_speakers.is_empty());
        for b in &waiting.status.blocking_speakers {
            let s = before
                .speaker(b.speaker_id)
                .expect("a speaker of the meeting");
            assert!(s.named_person().is_none());
            assert!(b.dismissed_at.is_none());
        }

        let queue = Queue::open(fx.lib.root()).unwrap();
        let job = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    broscribe_core::queue::REPROCESS_KIND,
                    id.as_str(),
                    serde_json::json!({ "meeting_id": id }),
                )
            })
            .unwrap();
        let held = read(&fx);
        assert_eq!(held.status.verdict, Verdict::Keep(KeepReason::LiveJobs));
        assert_eq!(held.live_jobs, vec![job.clone()]);
        assert!(held.status.blocking_speakers.is_empty());

        assert_eq!(fx.lib.load_meeting(&id).unwrap(), before);
        assert!(audio(&fx, &id).is_file());
        assert_eq!(queue.get(&job).unwrap().state, JobState::Queued);
    }

    fn job_id_of(id: &MeetingId) -> JobId {
        JobId::for_key(broscribe_core::queue::REPROCESS_KIND, id.as_str())
    }

    /// A queue record that cannot be read neither blocks the pass nor is
    /// touched by it: counted, its folder left in place.
    #[test]
    fn an_unreadable_queue_record_is_counted_and_left_alone() {
        let mut fx = Fixture::new();
        let id = import(&fx, "a.wav", 0, Handling::Copy, None);
        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        let bad = queue.job_dir(&job_id_of(&id));
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join(broscribe_core::queue::JOB_JSON), b"{ not json").unwrap();
        fs::write(bad.join("audio.wav"), b"synthetic").unwrap();

        let planned = run(&fx, None, true);
        assert_eq!(planned.unreadable_jobs, 1);
        assert_eq!(planned.deleted.len(), 1);
        let applied = run(&fx, None, false);
        assert_eq!(applied.unreadable_jobs, 1);
        assert_eq!(applied.deleted.len(), 1);
        assert_eq!(state(&fx, &id), RecordingState::Deleted);
        assert!(
            bad.join("audio.wav").is_file(),
            "unknown owner: never removed"
        );
        assert!(bad.join(broscribe_core::queue::JOB_JSON).is_file());
    }
}
