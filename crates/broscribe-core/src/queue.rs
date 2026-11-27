//! On-disk, resumable job queue with leases under `<library>/.queue/`
//! (design §3.1).
//!
//! Each job is a folder `<library>/.queue/<job-id>/` holding `job.json` plus
//! any files the job wants to keep between runs (for example `hints.json`).
//! The job ID is derived from `(kind, key)`, so enqueueing the same work twice
//! returns the existing job, or explicitly restarts it when it failed.
//!
//! A process claims a job by writing a lease (process ID, random token, time)
//! under the queue lock (`<library>/.queue/.lock`). The claim's
//! [`LeaseHandle`] keeps the record exactly as that claim wrote it
//! ([`LeaseHandle::claimed_job`]): the run processes that input, never one
//! read before the claim. While the handle lives, a heartbeat thread renews
//! the lease every interval (5 s by default). A lease that has not been
//! renewed for three intervals, or whose process is gone, can be reclaimed.
//! Only the lease holder commits results: [`LeaseHandle::commit`] re-reads
//! `job.json` and refuses with `lease_lost` if the token changed.
//!
//! Leases are stamped with the wall clock, which every process on the
//! machine shares. After a sleep, or when the clock jumps forward, a live
//! holder's lease may look stale until its next heartbeat and be taken over;
//! when the clock jumps back, a stamp lies in the future, and one more than
//! [`Queue::stale_after`] ahead counts as stale too (a live holder renews it
//! within one interval; a crashed one, maybe with its PID reused, never
//! would). Either way the token fence keeps the earlier holder from
//! committing: a takeover costs repeated work, never a double commit.
//!
//! Attempts count failures and takeovers of a crashed holder's lease, not
//! plain claims; see [`MAX_ATTEMPTS`]. A holder stopped on purpose (the
//! daemon shutting its own worker down) is no crash:
//! [`Queue::release_stopped_holder`] hands its job back without an attempt.
//!
//! A job whose `job.json` cannot be read is left out of listings and
//! scheduling ([`Queue::snapshot`] reports it), so it never blocks the other
//! jobs or the library writes that check them; it is neither claimed nor
//! removed by accident, and [`Queue::quarantine_invalid`] moves it aside.
//! [`Queue::prune`] removes finished jobs on explicit request only, never a
//! held job, an unreadable record or the user's only copy of a file.
//!
//! Locks: fresh claims, heartbeats and releases take only the queue lock.
//! Reclaims also take the library lock, excluding commits throughout their
//! writes. Commits, failures and the [`WriteTx`]-taking methods hold the
//! library lock and take the queue lock briefly inside it. The order is
//! always library → queue, never the reverse.
//!
//! Notes for callers:
//! - Manifest-import job folders (with their `hints.json`) must not be removed
//!   before the batch's resolve phase has finished; [`Queue::remove_finished`]
//!   only removes `done` jobs.
//! - Meeting dedupe is decided by the library files (`recording.sha256`,
//!   `source_id` in `meeting.json`), not only by queue keys: the queue may have
//!   been cleaned or deleted while the meetings remain.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, TimeDelta};
use serde::{Deserialize, Serialize};

use crate::library::{Library, WriteTx};
use crate::lock::{LockGuard, QueueLock};
use crate::process::ProcessProbe;
use crate::schema::{now, sha256_hex};
use crate::{Error, MeetingId, Result, fsutil, paths};

/// File holding a job's record inside its folder.
pub const JOB_JSON: &str = "job.json";
/// Failures plus crashed runs (counted when their lease is taken over) a job
/// may use before it is left failed for explicit retry: a job runs at most
/// this many times. Plain claims do not count: manifest batches claim each
/// job once per phase.
pub const MAX_ATTEMPTS: u32 = 3;

/// Queue kind of LLM enrichment jobs; the key is the meeting ID.
pub const ENRICH_KIND: &str = "enrich";
/// Queue kind of meeting-template jobs; the key is
/// [`template_job_key`]`(meeting, template)`.
pub const TEMPLATE_KIND: &str = "template";
/// Queue kind of the confirmation-samples job (the pipeline's worker runs
/// it): it publishes a person's voiceprints and index row, never
/// `meeting.json`.
pub const CONFIRM_KIND: &str = "confirm-samples";
/// Queue kind of the playback-cache job: the 16 kHz WAV cache a meeting's
/// recording plays from.
pub const PLAYBACK_KIND: &str = "playback-cache";
/// Queue kind of the search-vector refresh of one meeting.
pub const VECTORS_KIND: &str = "refresh-vectors";
/// Queue kind of a single-file import, keyed by the recording hash.
pub const IMPORT_KIND: &str = "import";
/// Queue kind of one manifest meeting, keyed by its stable source ID.
pub const MANIFEST_KIND: &str = "manifest-meeting";
/// Queue kind of audio reprocessing, keyed by the meeting ID.
pub const REPROCESS_KIND: &str = "reprocess";
/// Finalize a stopped recording's whole journal, then remove the journal
/// only after the encoded audio is verified. Key: the session ID.
pub const RECORD_FINAL_KIND: &str = "record-final";
/// Process a recording through a durable chunk cutoff without deleting
/// journal data. Key: the session ID and both tracks' cutoff sequences.
pub const RECORD_PROVISIONAL_KIND: &str = "record-provisional";

/// File journaling an import `move` inside the job's folder (the pipeline's
/// import writes it); while it exists the folder may hold the user's only
/// copy of the original.
pub const MOVE_JOURNAL: &str = "move.json";
/// The journal of a move that could not be undone because another file took
/// the original path; kept beside the staged audio, never retried.
pub const MOVE_CONFLICT: &str = "move-conflict.json";

/// Error of a job [`Queue::revoke`] cancelled because the meeting or
/// recording it worked on was deleted or discarded.
pub const REVOKED: &str = "cancelled: its meeting or recording was deleted or discarded";
/// Terminal error recorded when the user cancels a generation.
pub const GENERATION_CANCELLED: &str = "generation cancelled by the user";

fn generation_run(kind: &str) -> Option<String> {
    matches!(kind, ENRICH_KIND | TEMPLATE_KIND).then(|| format!("{:032x}", rand::random::<u128>()))
}

/// Lease token a revoked job keeps: matches no holder.
const REVOKED_TOKEN: &str = "revoked";

/// The key of the template job of `(meeting, template_id)`: one
/// reservation per meeting and template.
pub fn template_job_key(meeting: &MeetingId, template_id: &str) -> String {
    format!("{meeting}/{template_id}")
}

/// What a job does, for people: "Generating summary", "Running <template
/// name>", "Refreshing search"… An unknown kind shows as itself.
pub fn kind_label(kind: &str, input: &serde_json::Value) -> String {
    let text = |k: &str| {
        input
            .get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match kind {
        ENRICH_KIND => "Generating summary".to_owned(),
        TEMPLATE_KIND => format!(
            "Running {}",
            text("template_name")
                .or_else(|| text("template_id"))
                .unwrap_or("template")
        ),
        IMPORT_KIND => "Importing and transcribing a recording".to_owned(),
        MANIFEST_KIND => "Importing meetings".to_owned(),
        REPROCESS_KIND => "Transcribing again".to_owned(),
        RECORD_FINAL_KIND => "Transcribing a recording".to_owned(),
        RECORD_PROVISIONAL_KIND => "Making a draft transcript".to_owned(),
        CONFIRM_KIND => "Learning a voice".to_owned(),
        crate::learning::REIDENTIFY_KIND => "Checking other speakers".to_owned(),
        PLAYBACK_KIND => "Preparing playback".to_owned(),
        VECTORS_KIND => "Refreshing search".to_owned(),
        _ => "Background job".to_owned(),
    }
}

/// Error recorded on an interactive job whose holder went away.
const INTERRUPTED: &str = "interrupted";

const JOB_ID_PREFIX: &str = "j-";
const JOB_ID_HEX: usize = 16;

/// A job ID: `j-` followed by 16 lowercase hex digits of
/// SHA-256(`kind` NUL `key`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct JobId(String);

impl JobId {
    /// The ID of the job for `(kind, key)`.
    pub fn for_key(kind: &str, key: &str) -> Self {
        let digest = sha256_hex(format!("{kind}\0{key}").as_bytes());
        Self(format!("{JOB_ID_PREFIX}{}", &digest[..JOB_ID_HEX]))
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.strip_prefix(JOB_ID_PREFIX) {
            Some(hex)
                if hex.len() == JOB_ID_HEX
                    && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
            {
                Ok(Self(s.to_owned()))
            }
            _ => Err(Error::InvalidInput(format!(
                "not a job ID (expected j- and 16 hex digits): {s:?}"
            ))),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for JobId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl TryFrom<String> for JobId {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        Self::parse(&s)
    }
}

impl From<JobId> for String {
    fn from(id: JobId) -> String {
        id.0
    }
}

/// Progress of a job. Leases are separate: a job keeps its state while a
/// process holds it, except that claiming a `queued` or `failed` job makes it
/// `running`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Analyzed,
    Done,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Analyzed => "analyzed",
            JobState::Done => "done",
            JobState::Failed => "failed",
        }
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who holds a job and when they last renewed it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub pid: u32,
    /// Random per claim; a holder whose token no longer matches has lost the job.
    pub token: String,
    pub renewed_at: DateTime<FixedOffset>,
}

impl Lease {
    /// Whether the lease was renewed within `stale_after` of `at`, on
    /// either side: a renewal stamped more than `stale_after` after `at` is
    /// stale too (the clock went back since; a live holder renews with the
    /// new clock within one interval, a crashed one never will). The one
    /// freshness rule of every lease reader; `stale_after` is
    /// [`Queue::stale_after`].
    pub fn is_fresh(&self, at: DateTime<FixedOffset>, stale_after: TimeDelta) -> bool {
        let age = at.signed_duration_since(self.renewed_at);
        age <= stale_after && age >= -stale_after
    }

    /// Whether its holder still holds it at `at`: fresh ([`Self::is_fresh`])
    /// and its process alive. A lease not held may be taken over.
    pub fn is_held(
        &self,
        at: DateTime<FixedOffset>,
        stale_after: TimeDelta,
        probe: &dyn ProcessProbe,
    ) -> bool {
        self.is_fresh(at, stale_after) && probe.is_alive(self.pid)
    }
}

/// What [`LeaseHandle::update_in`] does to the job's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// The job is complete (`done`).
    Done,
    /// More work remains: `queued` again at the end of the queue, not to be
    /// claimed before `not_before` when set.
    Requeue {
        not_before: Option<DateTime<FixedOffset>>,
    },
}

/// `job.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub kind: String,
    /// Dedupe key within `kind`, e.g. a SHA-256 or a manifest `source_id`.
    pub key: String,
    pub input: serde_json::Value,
    pub state: JobState,
    #[serde(default)]
    pub lease: Option<Lease>,
    /// Latest measured fraction for this lease. It is useful only while held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    /// Failures plus crash takeovers so far (see [`MAX_ATTEMPTS`]); plain
    /// claims do not count.
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub error: Option<String>,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
    /// Changes on each new generation, even when its reservation ID is reused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_run: Option<String>,
    /// A file in the job folder that is the user's only copy of something
    /// (the audio of a `move` that could not be put back). While it exists
    /// the job is never claimed, re-enqueued or removed. Stored relative to
    /// the job folder (so moving the library keeps it), absolute in memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keeps: Option<PathBuf>,
    /// Not to be claimed before this time: a job that only waits for busy
    /// meetings to become free ([`LeaseHandle::update_in`] with
    /// [`Next::Requeue`]). Any new request clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<FixedOffset>>,
}

impl Job {
    /// The saved meeting a job explicitly targets, when it has one.
    pub fn meeting_id(&self) -> Option<MeetingId> {
        MeetingId::parse(self.input.get("meeting_id")?.as_str()?).ok()
    }

    /// Whether the job still keeps its file ([`Self::keeps`]).
    pub fn keeps_file(&self) -> bool {
        self.keeps.as_deref().is_some_and(Path::exists)
    }

    /// A job a user is waiting on in a terminal (`"interactive": true` in its
    /// input, e.g. a synchronous `enrich`): a crashed or interrupted run is
    /// marked `failed` ("interrupted") and never run again by a worker; only
    /// an explicit new request restarts it.
    pub fn is_interactive(&self) -> bool {
        self.input
            .get("interactive")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    }

    /// Whether [`Queue::claim`] may take the job once its lease is free: not
    /// `done`, not keeping the user's file, not `failed` for good (its
    /// attempts used up, or an interactive run) and not waiting for its
    /// `not_before` time. Schedulers use this so they never pick a job the
    /// claim would refuse.
    pub fn is_claimable(&self) -> bool {
        self.is_claimable_at(now())
    }

    fn is_claimable_at(&self, at: DateTime<FixedOffset>) -> bool {
        !self.not_before.is_some_and(|t| at < t) && self.claimable_state()
    }

    /// [`Self::is_claimable`] whatever the time: not `done`, not failed for
    /// good and not keeping the user's file.
    fn claimable_state(&self) -> bool {
        match self.state {
            JobState::Done => false,
            JobState::Failed if self.attempts >= MAX_ATTEMPTS || self.is_interactive() => false,
            _ => !self.keeps_file(),
        }
    }

    /// When a scheduler applying `retry_delay` (by attempts spent) may take
    /// the job, seen at `at`: its `not_before`, and, once an attempt was
    /// spent, `retry_delay(attempts)` after its last update (a stamp ahead
    /// of `at`, after the clock went back, counts from `at`). `None` when no
    /// claim will ever take it ([`Self::claimable_state`]) or the time lies
    /// past the calendar's range.
    fn due_at(
        &self,
        at: DateTime<FixedOffset>,
        retry_delay: &dyn Fn(u32) -> Duration,
    ) -> Option<DateTime<FixedOffset>> {
        if !self.claimable_state() {
            return None;
        }
        let retry = if self.attempts == 0 {
            at
        } else {
            let delay = TimeDelta::from_std(retry_delay(self.attempts)).ok()?;
            self.updated_at.min(at).checked_add_signed(delay)?
        };
        Some(self.not_before.map_or(retry, |t| t.max(retry)))
    }

    /// `queued`, or `running`/`analyzed` under a fresh lease ([`Lease::is_fresh`]:
    /// a crashed holder's job is not active).
    fn is_active(&self, at: DateTime<FixedOffset>, stale_after: TimeDelta) -> bool {
        match self.state {
            JobState::Queued => true,
            JobState::Running | JobState::Analyzed => self
                .lease
                .as_ref()
                .is_some_and(|l| l.is_fresh(at, stale_after)),
            JobState::Done | JobState::Failed => false,
        }
    }
}

/// A job folder whose `job.json` could not be read or does not hold that
/// job: left out of listings and scheduling ([`Queue::snapshot`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InvalidJob {
    pub id: JobId,
    /// Its `job.json`.
    pub path: PathBuf,
    pub error: String,
}

/// Every job record read in one pass of the queue folder, at one time:
/// listings, scheduling order and diagnostics computed from it agree.
#[derive(Clone, Debug)]
pub struct QueueSnapshot {
    /// Readable jobs in scheduler order: `(created_at, id)`.
    pub jobs: Vec<Job>,
    /// Job folders whose record could not be read, by ID.
    pub invalid: Vec<InvalidJob>,
    at: DateTime<FixedOffset>,
    stale_after: TimeDelta,
}

impl QueueSnapshot {
    /// When the records were read.
    pub fn at(&self) -> DateTime<FixedOffset> {
        self.at
    }

    /// Whether a scheduler would still take `job`: claimable
    /// ([`Job::is_claimable`]) and not held by a fresh lease.
    pub fn is_waiting(&self, job: &Job) -> bool {
        job.is_claimable_at(self.at)
            && !job
                .lease
                .as_ref()
                .is_some_and(|l| l.is_fresh(self.at, self.stale_after))
    }

    /// How many waiting jobs ([`Self::is_waiting`]) of every kind run before
    /// job `id` in scheduler order; 0 when it is gone, held or not waiting.
    pub fn jobs_ahead(&self, id: &JobId) -> usize {
        let mut ahead = 0;
        for job in &self.jobs {
            if job.id == *id {
                return if self.is_waiting(job) { ahead } else { 0 };
            }
            if self.is_waiting(job) {
                ahead += 1;
            }
        }
        0
    }

    /// [`Self::jobs_ahead`] of every waiting job, in one pass.
    pub fn ranks(&self) -> HashMap<JobId, usize> {
        let mut out = HashMap::new();
        for job in self.jobs.iter().filter(|j| self.is_waiting(j)) {
            let rank = out.len();
            out.insert(job.id.clone(), rank);
        }
        out
    }

    /// What a scheduler that polls the queue (the app's fallback worker,
    /// without the daemon) has to do, from this one snapshot: the jobs a
    /// worker may take now, when the next waiting one becomes due, and the
    /// exhausted ones to mark failed ([`Queue::fail_exhausted`]). A job is
    /// taken now when a claim would accept it ([`Job::is_claimable`]: a
    /// `failed` job with attempts left included, a job failed for good or
    /// an interrupted interactive run never), no live process holds its
    /// lease ([`Lease::is_held`]), its `not_before` has come and, once an
    /// attempt was spent, `retry_delay(attempts)` has passed since its last
    /// update. The others give [`PendingWork::wake_at`].
    pub fn pending_work(
        &self,
        probe: &dyn ProcessProbe,
        retry_delay: &dyn Fn(u32) -> Duration,
    ) -> PendingWork {
        let mut out = PendingWork::default();
        for job in &self.jobs {
            if exhausted(job, self.at, self.stale_after, probe) {
                out.exhausted.push(job.id.clone());
                continue;
            }
            if job
                .lease
                .as_ref()
                .is_some_and(|l| l.is_held(self.at, self.stale_after, probe))
            {
                continue;
            }
            match job.due_at(self.at, retry_delay) {
                Some(due) if due <= self.at => out.runnable.push(job.id.clone()),
                Some(due) => out.wake_at = Some(out.wake_at.map_or(due, |w| w.min(due))),
                None => {}
            }
        }
        out
    }
}

/// What [`QueueSnapshot::pending_work`] found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingWork {
    /// Jobs a worker may take now, in scheduler order.
    pub runnable: Vec<JobId>,
    /// When the earliest job waiting for its `not_before` or its retry
    /// delay becomes due; `None` when none waits.
    pub wake_at: Option<DateTime<FixedOffset>>,
    /// `running` or `analyzed` jobs whose crashed holder used their last
    /// attempt, or interactive ones whose holder went away
    /// ([`Queue::is_exhausted`]); no claim runs them again.
    pub exhausted: Vec<JobId>,
}

/// What [`Queue::release_stopped_holder`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoppedRelease {
    /// The stopped holder's lease was cleared without counting an attempt:
    /// a `running` job is `queued` again in its place (its `created_at`,
    /// `not_before` and attempts unchanged); any other state is kept.
    Released,
    /// An interactive run ([`Job::is_interactive`]) was stopped: `failed`
    /// ("interrupted") for good, as when its holder crashes; only an
    /// explicit new request restarts it.
    Interrupted,
    /// Nothing changed: the job is gone, holds no lease, or its lease is
    /// another holder's (another PID or token) or was revoked.
    NotHeld,
    /// Nothing changed: no token was given and a process with the
    /// holder's PID is alive (it was not stopped, or its PID was reused);
    /// the lease goes stale and is taken over as a crash.
    HolderAlive,
}

/// What [`Queue::prune`] removes; everything else stays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneCriteria {
    /// Remove `done` jobs.
    pub done: bool,
    /// Remove jobs failed for good: no attempts left, an interrupted
    /// interactive run, or revoked. A `failed` job with attempts left stays
    /// (a worker retries it) and is reported [`PruneKeep::Retryable`].
    pub failed: bool,
    /// Only jobs last updated at least this long ago.
    pub min_age: Duration,
    /// Only jobs of these kinds; empty: every kind. Unreadable records have
    /// no known kind and follow [`Self::quarantine_invalid`] only.
    pub kinds: Vec<String>,
    /// Move the folders of unreadable records whole into
    /// [`Queue::QUARANTINE_DIR`] ([`Queue::quarantine_invalid`]); they are
    /// never deleted.
    pub quarantine_invalid: bool,
    /// At most this many folders removed or quarantined in one run; the
    /// rest is counted in [`PruneReport::remaining`] for a later run.
    pub limit: usize,
}

impl PruneCriteria {
    /// [`Self::limit`] of [`Self::default`].
    pub const DEFAULT_LIMIT: usize = 500;
}

impl Default for PruneCriteria {
    /// `done` jobs only, of every kind and age, up to
    /// [`Self::DEFAULT_LIMIT`]; unreadable records stay where they are.
    fn default() -> Self {
        Self {
            done: true,
            failed: false,
            min_age: Duration::ZERO,
            kinds: Vec::new(),
            quarantine_invalid: false,
            limit: Self::DEFAULT_LIMIT,
        }
    }
}

/// Why [`Queue::prune`] kept a job folder it looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PruneKeep {
    /// The folder holds the user's only copy of a moved original (a kept
    /// file or a move journal): it stays whatever its state, until the
    /// user moves the file out of the library or the import settles it.
    OnlyCopy,
    /// A live holder has the lease: fresh, or its process still alive
    /// within [`Queue::REVOKED_HOLDER_GRACE`] of its last renewal.
    Held,
    /// Revoked, and its holder's process may still write into the folder
    /// ([`Queue::remove_revoked`] waits for it).
    RevokedHolder,
    /// Failed with attempts left: a worker retries it.
    Retryable,
    /// Failed for good, but its folder keeps files besides the record (a
    /// staged source, import hints, a recording's meeting ID): the import,
    /// deletion or discard that owns them removes them.
    HoldsFiles,
    /// Updated more recently than [`PruneCriteria::min_age`].
    TooRecent,
    /// The record cannot be read, so its owner and state are unknown;
    /// quarantine is not requested.
    Unreadable,
    /// A folder without a record that is not empty: unknown owner.
    NoRecord,
}

/// One job folder [`Queue::prune`] removed (or would remove).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PrunedJob {
    pub id: JobId,
    pub kind: String,
    pub state: JobState,
}

/// One job folder [`Queue::prune`] kept, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KeptJob {
    pub id: JobId,
    /// `None` for an unreadable record or a folder without one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub reason: PruneKeep,
}

/// What [`Queue::prune`] did, or with `dry_run` would do: identifiers,
/// kinds, states and counts only (never inputs, errors or paths).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PruneReport {
    pub dry_run: bool,
    /// Readable job records looked at.
    pub checked: usize,
    pub removed: Vec<PrunedJob>,
    /// Unreadable records moved whole into quarantine.
    pub quarantined: Vec<JobId>,
    /// Empty folders without a record (an interrupted enqueue) removed.
    pub empty_folders: usize,
    /// Folders the criteria select or that need attention, kept.
    pub kept: Vec<KeptJob>,
    /// Folders past [`PruneCriteria::limit`], left for a later run.
    pub remaining: usize,
}

/// What [`Queue::prune`] decided for one readable job.
enum PruneDecision {
    /// Not selected by the criteria and nothing to report.
    Skip,
    Keep(PruneKeep),
    Remove,
}

/// The queue folder of one library.
#[derive(Clone, Debug)]
pub struct Queue {
    root: PathBuf,
    dir: PathBuf,
    heartbeat: Duration,
    /// Freeze lease admission/renewal time in heartbeat tests, independently
    /// of host scheduling. Production has neither this field nor its lock.
    #[cfg(test)]
    lease_clock: Option<Arc<std::sync::Mutex<DateTime<FixedOffset>>>>,
}

impl Queue {
    /// Lease renewal interval used outside tests.
    pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(5);
    /// A lease is stale after this many missed renewals.
    pub const STALE_INTERVALS: u32 = 3;
    /// How long after its last renewal the holder of a revoked job, whose
    /// process still looks alive, is waited for before
    /// [`Self::remove_revoked`] removes the folder anyway: the PID may
    /// belong to another process by now, and a revoked holder can no longer
    /// commit.
    pub const REVOKED_HOLDER_GRACE: Duration = Duration::from_secs(600);
    /// Folder (inside the queue folder) holding the folders of jobs whose
    /// record could not be read ([`Self::quarantine_invalid`]).
    pub const QUARANTINE_DIR: &str = ".quarantine";

    /// Opens (and creates if needed) `<lib_root>/.queue/`.
    pub fn open(lib_root: impl AsRef<Path>) -> Result<Self> {
        let root = lib_root.as_ref().to_path_buf();
        let dir = root.join(paths::QUEUE_DIR);
        fsutil::create_dir_durable(&dir)?;
        Ok(Self {
            root,
            dir,
            heartbeat: Self::DEFAULT_HEARTBEAT,
            #[cfg(test)]
            lease_clock: None,
        })
    }

    /// The queue folder of `lib_root` when it exists, creating nothing:
    /// for reads that must not write (diagnostics, dry runs). `None`: no
    /// job was ever queued there.
    pub fn open_existing(lib_root: impl AsRef<Path>) -> Result<Option<Self>> {
        let root = lib_root.as_ref().to_path_buf();
        let dir = root.join(paths::QUEUE_DIR);
        match fs::metadata(&dir) {
            Ok(m) if m.is_dir() => Ok(Some(Self {
                root,
                dir,
                heartbeat: Self::DEFAULT_HEARTBEAT,
                #[cfg(test)]
                lease_clock: None,
            })),
            Ok(_) => Err(Error::Schema(format!("{} is not a folder", dir.display()))),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io_at(&dir, e)),
        }
    }

    /// Sets the heartbeat interval: a test seam (tests use short ones).
    /// Leases go stale after [`Self::STALE_INTERVALS`] intervals.
    pub fn with_heartbeat(mut self, interval: Duration) -> Self {
        self.heartbeat = interval;
        self
    }

    #[inline]
    fn lease_now(&self) -> DateTime<FixedOffset> {
        #[cfg(test)]
        if let Some(clock) = &self.lease_clock {
            return *clock.lock().unwrap_or_else(|error| error.into_inner());
        }
        now()
    }

    /// How long a lease may go unrenewed before it can be reclaimed.
    pub fn stale_after(&self) -> Duration {
        self.heartbeat * Self::STALE_INTERVALS
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Folder of job `id`, where the job may keep extra files.
    pub fn job_dir(&self, id: &JobId) -> PathBuf {
        self.dir.join(id.as_str())
    }

    /// Adds a job for `(kind, key)`, restarting failed work with fresh input
    /// and attempts. Other existing jobs are returned unchanged.
    pub fn enqueue(
        &self,
        _tx: &mut WriteTx<'_>,
        kind: &str,
        key: &str,
        input: serde_json::Value,
    ) -> Result<JobId> {
        if kind.is_empty() {
            return Err(Error::InvalidInput("job kind must not be empty".into()));
        }
        let id = JobId::for_key(kind, key);
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        match self.existing(&id, kind, key)? {
            Some(mut job) => {
                if job.state == JobState::Failed {
                    self.restart_job(&mut job, input)?;
                }
                Ok(id)
            }
            None => self.create_job(id, kind, key, input),
        }
    }

    /// Enqueues an explicit request for `(kind, key)` with `input`, under the
    /// queue lock: a `done` or `failed` job (or one whose holder crashed:
    /// lease not renewed within [`Self::stale_after`]) restarts with this
    /// input and zero attempts; a `queued` or live `running` one is
    /// `library_busy`; without one a job is created. Enrichment
    /// ([`ENRICH_KIND`], keyed by meeting) and templates ([`TEMPLATE_KIND`],
    /// [`template_job_key`]) reserve their work this way, interactive and
    /// queued runs alike.
    pub fn restart_finished(
        &self,
        _tx: &mut WriteTx<'_>,
        kind: &str,
        key: &str,
        input: serde_json::Value,
    ) -> Result<JobId> {
        if kind.is_empty() {
            return Err(Error::InvalidInput("job kind must not be empty".into()));
        }
        let id = JobId::for_key(kind, key);
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        match self.existing(&id, kind, key)? {
            Some(job) if job.is_active(now(), stale_after) => Err(busy_error(kind, key)),
            Some(mut job) => {
                self.restart_job(&mut job, input)?;
                Ok(id)
            }
            None => self.create_job(id, kind, key, input),
        }
    }

    /// Adds a request to the job of `(kind, key)` under the queue lock,
    /// creating the job when missing: `f` gets the job as stored now (or
    /// `None`) and returns its new input, so concurrent requests never
    /// overwrite each other. A `queued` or `running` job keeps its state,
    /// lease, attempts and place: its holder reads the new input at its
    /// next lease-checked update ([`LeaseHandle::update_in`]); returning an
    /// existing job is therefore no acknowledgement that a running pass saw
    /// the request. A `done` or `failed` job is queued again at the back
    /// with zero attempts. Either way it may be claimed at once
    /// (`not_before` is cleared). Returns the job as written.
    pub fn request(
        &self,
        _tx: &mut WriteTx<'_>,
        kind: &str,
        key: &str,
        f: impl FnOnce(Option<&Job>) -> Result<serde_json::Value>,
    ) -> Result<Job> {
        if kind.is_empty() {
            return Err(Error::InvalidInput("job kind must not be empty".into()));
        }
        let id = JobId::for_key(kind, key);
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let Some(mut job) = self.existing(&id, kind, key)? else {
            let input = f(None)?;
            self.create_job(id.clone(), kind, key, input)?;
            return self.get(&id);
        };
        job.input = f(Some(&job))?;
        let at = now();
        if matches!(job.state, JobState::Done | JobState::Failed) {
            job.state = JobState::Queued;
            job.attempts = 0;
            job.error = None;
            job.created_at = at;
            job.generation_run = generation_run(kind);
        }
        job.not_before = None;
        job.updated_at = at;
        self.write_job(&job)?;
        Ok(job)
    }

    /// `library_busy` when the job of `(kind, key)` is queued or running
    /// (under a live lease); see [`Self::restart_finished`].
    pub fn check_idle(&self, kind: &str, key: &str) -> Result<()> {
        if self.busy(kind, key)? {
            return Err(busy_error(kind, key));
        }
        Ok(())
    }

    /// Whether the job of `(kind, key)` is queued or running under a live
    /// lease.
    pub fn busy(&self, kind: &str, key: &str) -> Result<bool> {
        let id = JobId::for_key(kind, key);
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        match self.get(&id) {
            Ok(job) => Ok(job.is_active(now(), stale_after)),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// How many jobs run before job `id`: those a scheduler would still take
    /// ([`Job::is_claimable`], not held by a live lease) ordered before it by
    /// `(created_at, id)`, of every kind, as the daemon's scheduler orders
    /// them. 0 when the job is gone, held or not waiting. To rank several
    /// jobs, read one [`Self::snapshot`] and use [`QueueSnapshot::ranks`].
    pub fn jobs_ahead(&self, id: &JobId) -> Result<usize> {
        Ok(self.snapshot()?.jobs_ahead(id))
    }

    /// Every job record and every unreadable one, read in one pass at one
    /// time (no lock: a snapshot for planning, listings and diagnostics).
    pub fn snapshot(&self) -> Result<QueueSnapshot> {
        let at = now();
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => Some(e),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(Error::io_at(&self.dir, e)),
        };
        let mut jobs = Vec::new();
        let mut invalid = Vec::new();
        for entry in entries.into_iter().flatten() {
            let entry = entry.map_err(|e| Error::io_at(&self.dir, e))?;
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| JobId::parse(n).ok())
            else {
                continue;
            };
            match self.get(&id) {
                Ok(job) => jobs.push(job),
                // A folder without `job.json`: an interrupted enqueue.
                Err(Error::NotFound(_)) => {}
                Err(e) => {
                    tracing::debug!(job = %id, code = e.code(), "skipping unreadable job record");
                    invalid.push(InvalidJob {
                        path: self.job_dir(&id).join(JOB_JSON),
                        id,
                        error: e.to_string(),
                    });
                }
            }
        }
        jobs.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        invalid.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(QueueSnapshot {
            jobs,
            invalid,
            at,
            stale_after,
        })
    }

    /// `library_busy` when a job working on `meeting` (import, reprocessing,
    /// manifest analysis, recording publication, enrichment, sampling…) is
    /// queued or running under a live lease: its input or key names the
    /// meeting ID, one of its recording hashes or its recording session.
    /// Writes that change what every consumer of the meeting shows (speaker
    /// grouping) refuse to race such jobs.
    pub fn check_meeting_idle(&self, meeting: &crate::schema::Meeting) -> Result<()> {
        self.check_meeting_idle_except(meeting, &[])
    }

    /// [`Queue::check_meeting_idle`] ignoring active jobs of the `ignored`
    /// kinds (identity reevaluation ignores [`CONFIRM_KIND`]: it publishes
    /// voiceprints, never the meeting, and the reevaluation's evidence fence
    /// catches what it changes).
    pub fn check_meeting_idle_except(
        &self,
        meeting: &crate::schema::Meeting,
        ignored: &[&str],
    ) -> Result<()> {
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        self.meeting_idle_locked(meeting, ignored)
    }

    /// [`Queue::check_meeting_idle_except`] for a read that must create
    /// nothing (`identity re-resolve --dry-run`): no `.queue/` folder or
    /// lock file is made; a library without the queue lock file never had a
    /// job.
    pub fn check_meeting_idle_readonly(
        lib_root: impl AsRef<Path>,
        meeting: &crate::schema::Meeting,
        ignored: &[&str],
    ) -> Result<()> {
        let root = lib_root.as_ref();
        let Some(_q) = QueueLock::acquire_existing(root, QueueLock::DEFAULT_WAIT)? else {
            return Ok(());
        };
        Self {
            root: root.to_path_buf(),
            dir: root.join(paths::QUEUE_DIR),
            heartbeat: Self::DEFAULT_HEARTBEAT,
            #[cfg(test)]
            lease_clock: None,
        }
        .meeting_idle_locked(meeting, ignored)
    }

    /// The check of [`Queue::check_meeting_idle_except`], under the queue
    /// lock.
    fn meeting_idle_locked(
        &self,
        meeting: &crate::schema::Meeting,
        ignored: &[&str],
    ) -> Result<()> {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let at = now();
        match self.all_jobs()?.into_iter().find(|job| {
            !ignored.contains(&job.kind.as_str())
                && job.is_active(at, stale_after)
                && job_refers_to(job, meeting)
        }) {
            Some(job) => Err(Error::LibraryBusy(format!(
                "{} (a {} job is working on this meeting; try again when it ends)",
                meeting.id, job.kind
            ))),
            None => Ok(()),
        }
    }

    /// Job `id` when it exists, checked to hold `(kind, key)` and not to keep
    /// a file. Must run under the queue lock.
    fn existing(&self, id: &JobId, kind: &str, key: &str) -> Result<Option<Job>> {
        let job = match self.get(id) {
            Ok(job) => job,
            Err(Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        if job.kind != kind || job.key != key {
            return Err(Error::Internal(format!(
                "job {id} holds ({}, {}), not ({kind}, {key})",
                job.kind, job.key
            )));
        }
        if let Some(kept) = job.keeps.as_ref().filter(|_| job.keeps_file()) {
            return Err(Error::AlreadyExists(format!(
                "job {id} keeps {}, the only copy of a moved file; move it out of the \
                 library folder before importing it again",
                kept.display()
            )));
        }
        Ok(Some(job))
    }

    /// Requeues `job` with fresh `input` and attempts, in one atomic write of
    /// `job.json`. Must run under the queue lock.
    fn restart_job(&self, job: &mut Job, input: serde_json::Value) -> Result<()> {
        job.input = input;
        job.generation_run = generation_run(&job.kind);
        job.state = JobState::Queued;
        job.attempts = 0;
        job.error = None;
        job.lease = None;
        job.keeps = None;
        job.not_before = None;
        job.updated_at = now();
        self.write_job(job)
    }

    /// Writes a new `queued` job. Must run under the queue lock.
    fn create_job(
        &self,
        id: JobId,
        kind: &str,
        key: &str,
        input: serde_json::Value,
    ) -> Result<JobId> {
        // A folder without job.json is left over from an interrupted enqueue.
        let dir = self.job_dir(&id);
        fsutil::create_dir_durable(&dir)?;
        let at = now();
        let job = Job {
            id: id.clone(),
            kind: kind.to_owned(),
            key: key.to_owned(),
            input,
            state: JobState::Queued,
            lease: None,
            progress: None,
            attempts: 0,
            error: None,
            created_at: at,
            updated_at: at,
            generation_run: generation_run(kind),
            keeps: None,
            not_before: None,
        };
        self.write_job(&job)?;
        Ok(id)
    }

    /// The job's current record; `not_found` if it does not exist.
    pub fn get(&self, id: &JobId) -> Result<Job> {
        let path = self.job_dir(id).join(JOB_JSON);
        let mut job: Job = fsutil::read_json(&path).map_err(|e| match e {
            Error::NotFound(_) => Error::NotFound(format!("job {id}")),
            e => e,
        })?;
        if job.id != *id {
            return Err(Error::Schema(format!(
                "{} holds job {}",
                path.display(),
                job.id
            )));
        }
        if job.generation_run.is_none() && matches!(job.kind.as_str(), ENRICH_KIND | TEMPLATE_KIND)
        {
            // Jobs queued by previous versions have a stable creation time.
            // Every new request from this version replaces it with a token.
            job.generation_run = Some(format!("legacy-{}", job.created_at.to_rfc3339()));
        }
        if let Some(kept) = job.keeps.take() {
            job.keeps = Some(in_job_dir(&self.job_dir(id), kept));
        }
        Ok(job)
    }

    /// Every readable job of `kind`, oldest first (`(created_at, id)`).
    pub fn list(&self, kind: &str) -> Result<Vec<Job>> {
        Ok(self
            .all_jobs()?
            .into_iter()
            .filter(|j| j.kind == kind)
            .collect())
    }

    /// Takes the job if it has no lease, or its lease is stale (not renewed
    /// for three intervals), or its process is gone. `None` when another live
    /// holder has it, or the job keeps a file ([`Job::keeps_file`]). Taking
    /// over a crashed holder of a `running` or `analyzed` job counts the
    /// crashed run as an attempt; when that uses up the last one the job is
    /// marked `failed` ("worker crashed") instead, so a job runs at most
    /// [`MAX_ATTEMPTS`] times. An interactive job ([`Job::is_interactive`])
    /// is never taken over: a crashed holder's run is marked `failed`
    /// ("interrupted"), and a `failed` interactive job is never claimed.
    /// Starts the heartbeat thread. The handle keeps the record as this
    /// claim wrote it ([`LeaseHandle::claimed_job`]): run that, not a record
    /// read before the claim.
    /// Replacing a lease takes the library lock, excluding in-flight commits.
    pub fn claim(
        &self,
        lib: &Library,
        id: &JobId,
        probe: &dyn ProcessProbe,
    ) -> Result<Option<LeaseHandle>> {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let token = new_token();
        let pid = probe.current_pid();
        // The record as this claim writes it, or `None` when it may not run.
        let claim = || -> Result<Option<Job>> {
            let mut job = self.get(id)?;
            if job.keeps_file() {
                return Ok(None);
            }
            let at = self.lease_now();
            if let Some(lease) = &job.lease {
                if lease.is_held(at, stale_after, probe) {
                    return Ok(None);
                }
                if matches!(job.state, JobState::Running | JobState::Analyzed) {
                    if job.is_interactive() {
                        mark_interrupted(&mut job, at);
                        self.write_job(&job)?;
                        return Ok(None);
                    }
                    job.attempts += 1;
                    if job.attempts >= MAX_ATTEMPTS {
                        mark_crashed(&mut job, at);
                        self.write_job(&job)?;
                        return Ok(None);
                    }
                }
            }
            if !job.is_claimable_at(at) {
                return Ok(None);
            }
            job.progress = None;
            job.lease = Some(Lease {
                pid,
                token: token.clone(),
                renewed_at: at,
            });
            if matches!(job.state, JobState::Queued | JobState::Failed) {
                job.state = JobState::Running;
                job.error = None;
            }
            job.updated_at = at;
            self.write_job(&job)?;
            Ok(Some(job))
        };
        let claimed = {
            let q = self.lock(QueueLock::DEFAULT_WAIT)?;
            let job = self.get(id)?;
            if let Some(lease) = &job.lease {
                if lease.is_held(self.lease_now(), stale_after, probe) {
                    return Ok(None);
                }
                // Never wait for the library while holding the queue lock:
                // the current commit and its heartbeat both need the queue.
                drop(q);
                lib.write(|_| {
                    let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
                    claim()
                })?
            } else {
                claim()?
            }
        };
        Ok(claimed.map(|job| LeaseHandle::start(self.clone(), job, token)))
    }

    /// Whether `job` is `running` or `analyzed` under a crashed holder (stale
    /// or dead lease) whose crashed run was its last attempt, or that is
    /// interactive ([`Job::is_interactive`]): no claim will run it again, and
    /// [`Self::fail_exhausted`] marks it `failed`.
    pub fn is_exhausted(&self, job: &Job, probe: &dyn ProcessProbe) -> bool {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        exhausted(job, now(), stale_after, probe)
    }

    /// Marks every exhausted job (see [`Self::is_exhausted`]) `failed`
    /// ("worker crashed", or "interrupted" for an interactive one) and drops
    /// its lease, so re-enqueueing restarts it. Returns their IDs.
    pub fn fail_exhausted(
        &self,
        _tx: &mut WriteTx<'_>,
        probe: &dyn ProcessProbe,
    ) -> Result<Vec<JobId>> {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let at = now();
        let mut failed = Vec::new();
        for mut job in self.all_jobs()? {
            if exhausted(&job, at, stale_after, probe) {
                if job.is_interactive() {
                    mark_interrupted(&mut job, at);
                } else {
                    job.attempts += 1;
                    mark_crashed(&mut job, at);
                }
                self.write_job(&job)?;
                failed.push(job.id);
            }
        }
        failed.sort();
        Ok(failed)
    }

    /// Sets a job's state and error without checking any lease (for
    /// administrative resets; lease holders use [`LeaseHandle::commit`]).
    pub fn set_state(
        &self,
        _tx: &mut WriteTx<'_>,
        id: &JobId,
        state: JobState,
        error: Option<String>,
    ) -> Result<()> {
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let mut job = self.get(id)?;
        job.state = state;
        job.error = error;
        job.updated_at = now();
        self.write_job(&job)
    }

    /// Fails job `id` for good, keeping `kept` (see [`Job::keeps`]), unless a
    /// live process holds it: recovery of a kept file whose job a crash
    /// left unmarked. Returns whether it was recorded.
    pub fn keep_file(
        &self,
        _tx: &mut WriteTx<'_>,
        id: &JobId,
        kept: PathBuf,
        error: String,
        probe: &dyn ProcessProbe,
    ) -> Result<bool> {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let mut job = self.get(id)?;
        let at = now();
        if job
            .lease
            .as_ref()
            .is_some_and(|l| l.is_held(at, stale_after, probe))
        {
            return Ok(false);
        }
        job.state = JobState::Failed;
        job.error = Some(error);
        job.lease = None;
        job.keeps = Some(kept);
        job.attempts = job.attempts.max(MAX_ATTEMPTS);
        job.updated_at = at;
        self.write_job(&job)?;
        Ok(true)
    }

    /// Removes the folders of `done` jobs of `kind` that no live holder has
    /// (no lease, or a stale one) and that keep no file ([`Job::keeps_file`]).
    /// Returns how many were removed. Callers
    /// must not run this for manifest-import jobs before the batch's resolve
    /// phase has finished: those folders hold the hint embeddings it needs.
    pub fn remove_finished(&self, _tx: &mut WriteTx<'_>, kind: &str) -> Result<usize> {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let at = now();
        let mut removed = 0;
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        for job in self.list(kind)? {
            if job.state != JobState::Done || self.holds_only_copy(&job) {
                continue;
            }
            let held = job
                .lease
                .as_ref()
                .is_some_and(|l| l.is_fresh(at, stale_after));
            if held {
                continue;
            }
            let dir = self.job_dir(&job.id);
            match fs::remove_dir_all(&dir) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io_at(&dir, e)),
            }
        }
        Ok(removed)
    }

    /// Removes job `id` when it kept a file ([`Job::keeps`]) that is gone
    /// (the user moved it away) and no live process holds it: nothing is
    /// left to protect or retry. Returns whether it was removed.
    pub fn remove_released(
        &self,
        _tx: &mut WriteTx<'_>,
        id: &JobId,
        probe: &dyn ProcessProbe,
    ) -> Result<bool> {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let job = match self.get(id) {
            Ok(job) => job,
            Err(Error::NotFound(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        let held = job
            .lease
            .as_ref()
            .is_some_and(|l| l.is_held(now(), stale_after, probe));
        if job.keeps.is_none() || job.keeps_file() || held {
            return Ok(false);
        }
        let dir = self.job_dir(id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(true),
            Err(e) => Err(Error::io_at(&dir, e)),
        }
    }

    /// Every readable job record in scheduler order (no lock: a snapshot
    /// for planning); unreadable ones are left out ([`Self::snapshot`]
    /// reports them).
    pub fn jobs(&self) -> Result<Vec<Job>> {
        Ok(self.snapshot()?.jobs)
    }

    /// Whether `job`'s lease is held: renewed within [`Self::stale_after`]
    /// by a process that is alive.
    pub fn is_held(&self, job: &Job, probe: &dyn ProcessProbe) -> bool {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        job.lease
            .as_ref()
            .is_some_and(|l| l.is_held(self.lease_now(), stale_after, probe))
    }

    /// Publish an observation by the worker process, without touching the
    /// job's state, lease or queue position. Progress is disposable.
    pub fn report_progress(
        &self,
        id: &JobId,
        probe: &dyn ProcessProbe,
        fraction: f64,
    ) -> Result<()> {
        if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
            return Ok(());
        }
        let _lock = self.lock(Duration::from_millis(10))?;
        let mut job = self.get(id)?;
        if !matches!(job.state, JobState::Running | JobState::Analyzed)
            || job
                .lease
                .as_ref()
                .is_none_or(|lease| lease.pid != probe.current_pid())
            || !self.is_held(&job, probe)
        {
            return Ok(());
        }
        let rounded = (fraction * 100.0).round() / 100.0;
        if job.progress != Some(rounded) {
            job.progress = Some(rounded);
            self.store_job(&job, false)?;
        }
        Ok(())
    }

    /// Whether job `job`'s folder holds the user's only copy of a moved
    /// original: a kept file ([`Job::keeps`]), or an unfinished or
    /// conflicting move journal.
    pub fn holds_only_copy(&self, job: &Job) -> bool {
        job.keeps_file() || self.dir_holds_only_copy(&self.job_dir(&job.id))
    }

    /// Cancels just the generation named by the receipt, under the same
    /// library lock as publication and the same queue lock as claiming.
    pub fn cancel_generation(
        &self,
        tx: &mut WriteTx<'_>,
        id: &JobId,
        expected_run: &str,
    ) -> Result<()> {
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let mut job = self.get(id)?;
        if !matches!(job.kind.as_str(), ENRICH_KIND | TEMPLATE_KIND)
            || job.generation_run.as_deref() != Some(expected_run)
        {
            return Err(Error::Service {
                code: "generation_changed",
                message: "This generation has changed. Reload and try again.".into(),
            });
        }
        if job.error.as_deref() == Some(GENERATION_CANCELLED) {
            return Ok(());
        }
        let finished = || Error::Service {
            code: "generation_finished",
            message: "This generation already finished.".into(),
        };
        if !matches!(job.state, JobState::Queued | JobState::Running) && !job.claimable_state() {
            return Err(finished());
        }
        if let Some(meeting_id) = job.meeting_id() {
            match tx.load_meeting(&meeting_id) {
                Ok(meeting)
                    if meeting
                        .published_generation_runs
                        .get(id.as_str())
                        .is_some_and(|run| run == expected_run) =>
                {
                    return Err(finished());
                }
                Ok(_) | Err(Error::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        self.revoke_job(&mut job, GENERATION_CANCELLED)?;
        Ok(())
    }

    /// Revokes job `id` for good, under the queue lock: `failed` with
    /// [`REVOKED`], no attempts left and its lease token replaced, so a
    /// holder's heartbeat, commit and release all find the lease lost and
    /// no claim or scheduler runs it again; an explicit new request
    /// ([`Self::enqueue`]) still restarts it. The holder's PID stays, so
    /// [`Self::remove_revoked`] waits for that process to end. Returns
    /// whether a live process held it.
    pub fn revoke(
        &self,
        _tx: &mut WriteTx<'_>,
        id: &JobId,
        probe: &dyn ProcessProbe,
    ) -> Result<bool> {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let mut job = match self.get(id) {
            Ok(job) => job,
            Err(Error::NotFound(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        let at = now();
        let held = job
            .lease
            .as_ref()
            .is_some_and(|l| l.is_held(at, stale_after, probe));
        self.revoke_job(&mut job, REVOKED)?;
        Ok(held)
    }

    fn revoke_job(&self, job: &mut Job, reason: &str) -> Result<()> {
        job.state = JobState::Failed;
        job.error = Some(reason.into());
        job.attempts = job.attempts.max(MAX_ATTEMPTS);
        job.not_before = None;
        if let Some(lease) = job.lease.as_mut() {
            lease.token = REVOKED_TOKEN.into();
        }
        job.updated_at = now();
        self.write_job(job)
    }

    /// Revokes every job of `ids` ([`Self::revoke`]) in one queue-lock
    /// critical section, unless one of them is held by a live process now:
    /// then nothing changes and that job is returned. A claim also needs the
    /// queue lock, so no job can be claimed between the check and the
    /// revocation.
    pub fn revoke_unless_held(
        &self,
        _tx: &mut WriteTx<'_>,
        ids: &[JobId],
        probe: &dyn ProcessProbe,
    ) -> Result<Option<JobId>> {
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let at = now();
        let mut jobs = Vec::with_capacity(ids.len());
        for id in ids {
            match self.get(id) {
                Ok(job) => {
                    let held = job
                        .lease
                        .as_ref()
                        .is_some_and(|l| l.is_held(at, stale_after, probe));
                    if held && job.error.as_deref() != Some(REVOKED) {
                        return Ok(Some(job.id));
                    }
                    jobs.push(job);
                }
                Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        for mut job in jobs {
            job.state = JobState::Failed;
            job.error = Some(REVOKED.into());
            job.attempts = job.attempts.max(MAX_ATTEMPTS);
            job.not_before = None;
            if let Some(lease) = job.lease.as_mut() {
                lease.token = REVOKED_TOKEN.into();
            }
            job.updated_at = at;
            self.write_job(&job)?;
        }
        Ok(None)
    }

    /// Removes the folder of job `id` revoked by [`Self::revoke`] once no
    /// process that held it can still write into it: its holder is gone, or
    /// its lease was last renewed more than [`Self::REVOKED_HOLDER_GRACE`]
    /// ago (or ahead, after the clock moved back), so a reused PID cannot
    /// keep the folder forever. `true` when the folder is gone; a job
    /// revoked and then requested again is left alone (`true`: it is no
    /// longer this caller's). A folder holding the user's only copy of a
    /// moved original ([`Self::holds_only_copy`]) is never removed:
    /// `already_exists`. An unreadable record is an error, never removed.
    pub fn remove_revoked(
        &self,
        _tx: &mut WriteTx<'_>,
        id: &JobId,
        probe: &dyn ProcessProbe,
    ) -> Result<bool> {
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let dir = self.job_dir(id);
        let job = match self.get(id) {
            Ok(job) => job,
            Err(Error::NotFound(_)) => {
                if self.dir_holds_only_copy(&dir) {
                    return Err(only_copy_error(id, &dir));
                }
                return match fs::remove_dir_all(&dir) {
                    Ok(()) => Ok(true),
                    Err(e) if e.kind() == ErrorKind::NotFound => Ok(true),
                    Err(e) => Err(Error::io_at(&dir, e)),
                };
            }
            Err(e) => return Err(e),
        };
        if job.error.as_deref() != Some(REVOKED) || job.state != JobState::Failed {
            return Ok(true);
        }
        if let Some(lease) = &job.lease
            && probe.is_alive(lease.pid)
        {
            let grace = TimeDelta::from_std(Self::REVOKED_HOLDER_GRACE).unwrap_or(TimeDelta::MAX);
            if lease.is_fresh(now(), grace) {
                return Ok(false);
            }
        }
        if self.holds_only_copy(&job) {
            return Err(only_copy_error(id, &dir));
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(true),
            Err(e) => Err(Error::io_at(&dir, e)),
        }
    }

    /// Moves the folder of job `id`, whose `job.json` cannot be read (see
    /// [`QueueSnapshot::invalid`]), whole into
    /// `.queue/`[`Self::QUARANTINE_DIR`]`/<id>-<unix ms>/`, under the queue
    /// lock: nothing in it is deleted, and the job can be requested again.
    /// Returns the new folder; `None` when the record reads fine (nothing
    /// to do) or the folder holds no record. A folder with a move journal
    /// (the user's only copy of a moved original) stays: `already_exists`.
    pub fn quarantine_invalid(&self, _tx: &mut WriteTx<'_>, id: &JobId) -> Result<Option<PathBuf>> {
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        match self.get(id) {
            Ok(_) | Err(Error::NotFound(_)) => return Ok(None),
            Err(_) => {}
        }
        let dir = self.job_dir(id);
        if self.dir_holds_only_copy(&dir) {
            return Err(only_copy_error(id, &dir));
        }
        let quarantine = self.dir.join(Self::QUARANTINE_DIR);
        fsutil::create_dir_durable(&quarantine)?;
        let target = quarantine.join(format!("{id}-{}", now().timestamp_millis()));
        fs::rename(&dir, &target).map_err(|e| Error::io_at(&dir, e))?;
        fsutil::sync_dir(&self.dir)?;
        fsutil::sync_dir(&quarantine)?;
        Ok(Some(target))
    }

    /// The folders the queue writes atomically into: the queue folder and
    /// each job folder. Read only, no lock.
    pub fn temp_dirs(&self) -> Result<Vec<PathBuf>> {
        let mut dirs = vec![self.dir.clone()];
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(dirs),
            Err(e) => return Err(Error::io_at(&self.dir, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| Error::io_at(&self.dir, e))?;
            if let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| JobId::parse(n).ok())
            {
                dirs.push(self.job_dir(&id));
            }
        }
        Ok(dirs)
    }

    /// Removes the temporary files interrupted atomic writes left in
    /// [`Self::temp_dirs`] ([`fsutil::sweep_orphan_temps`]), under the
    /// queue lock. Returns how many were removed.
    pub fn sweep_orphan_temps(
        &self,
        _tx: &mut WriteTx<'_>,
        probe: &dyn ProcessProbe,
        min_age: Duration,
    ) -> Result<usize> {
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let mut removed = 0;
        for dir in self.temp_dirs()? {
            removed += fsutil::sweep_orphan_temps(&dir, probe, min_age)?;
        }
        Ok(removed)
    }

    /// Hands back job `id` whose holder the caller stopped on purpose (the
    /// daemon's planned shutdown of its own worker child), under the library
    /// and queue locks, without counting the stopped run as an attempt (a
    /// takeover of its lease would: [`Self::claim`]). Only that holder's
    /// lease is touched: it must carry `pid` and, when the caller read the
    /// lease while its child was still alive (so the PID could be nobody
    /// else's), `token`. Without a token the PID alone names the holder, so
    /// a live process with that PID (not stopped after all, or the PID
    /// reused) changes nothing ([`StoppedRelease::HolderAlive`]). A revoked
    /// lease, another holder's lease or none changes nothing either. An
    /// interactive run is failed for good ([`StoppedRelease::Interrupted`]),
    /// never offered to a worker. Call it once the holder was stopped and
    /// reaped: a holder still running finds its lease lost and commits
    /// nothing.
    pub fn release_stopped_holder(
        &self,
        _tx: &mut WriteTx<'_>,
        id: &JobId,
        pid: u32,
        token: Option<&str>,
        probe: &dyn ProcessProbe,
    ) -> Result<StoppedRelease> {
        let _q = self.lock(QueueLock::DEFAULT_WAIT)?;
        let mut job = match self.get(id) {
            Ok(job) => job,
            Err(Error::NotFound(_)) => return Ok(StoppedRelease::NotHeld),
            Err(e) => return Err(e),
        };
        let Some(lease) = &job.lease else {
            return Ok(StoppedRelease::NotHeld);
        };
        if lease.pid != pid
            || lease.token == REVOKED_TOKEN
            || token.is_some_and(|t| t != lease.token)
        {
            return Ok(StoppedRelease::NotHeld);
        }
        if token.is_none() && probe.is_alive(pid) {
            return Ok(StoppedRelease::HolderAlive);
        }
        let at = now();
        if job.is_interactive() && matches!(job.state, JobState::Running | JobState::Analyzed) {
            mark_interrupted(&mut job, at);
            self.write_job(&job)?;
            return Ok(StoppedRelease::Interrupted);
        }
        job.lease = None;
        if job.state == JobState::Running {
            job.state = JobState::Queued;
        }
        job.updated_at = at;
        self.write_job(&job)?;
        Ok(StoppedRelease::Released)
    }

    /// `queue prune`: removes the folders of the finished jobs `criteria`
    /// selects, oldest first, up to its limit; with
    /// [`PruneCriteria::quarantine_invalid`], moves unreadable records into
    /// quarantine ([`Self::quarantine_invalid`], never deleted); removes
    /// empty folders without a record (an interrupted enqueue). Each folder
    /// is decided again under the queue lock just before it goes, inside the
    /// caller's library write, so a claim, request or commit in between
    /// keeps it. Never removed: a job a live holder has (or a revoked one
    /// whose holder may still write), one holding the user's only copy of a
    /// moved original, a failed job that may still be retried or whose
    /// folder keeps other files, an unreadable record, a non-empty folder
    /// without one. Only explicit callers prune; reads never do.
    pub fn prune(
        &self,
        tx: &mut WriteTx<'_>,
        probe: &dyn ProcessProbe,
        criteria: &PruneCriteria,
    ) -> Result<PruneReport> {
        self.run_prune(probe, criteria, Some(tx))
    }

    /// What [`Self::prune`] would do now (`dry_run`): reads one snapshot,
    /// takes no lock and changes nothing.
    pub fn prune_plan(
        &self,
        probe: &dyn ProcessProbe,
        criteria: &PruneCriteria,
    ) -> Result<PruneReport> {
        self.run_prune(probe, criteria, None)
    }

    /// [`Self::prune`] with `tx`, else [`Self::prune_plan`].
    fn run_prune(
        &self,
        probe: &dyn ProcessProbe,
        criteria: &PruneCriteria,
        mut tx: Option<&mut WriteTx<'_>>,
    ) -> Result<PruneReport> {
        let snapshot = self.snapshot()?;
        let stale_after = TimeDelta::from_std(self.stale_after()).unwrap_or(TimeDelta::MAX);
        let apply = tx.is_some();
        let mut report = PruneReport {
            dry_run: !apply,
            checked: snapshot.jobs.len(),
            ..PruneReport::default()
        };
        let mut budget = criteria.limit;
        let keep = |report: &mut PruneReport, id: &JobId, kind: Option<&str>, reason| {
            report.kept.push(KeptJob {
                id: id.clone(),
                kind: kind.map(str::to_owned),
                reason,
            });
        };

        for listed in &snapshot.jobs {
            let _q = if apply {
                Some(self.lock(QueueLock::DEFAULT_WAIT)?)
            } else {
                None
            };
            let fresh;
            let (job, at) = if apply {
                fresh = match self.get(&listed.id) {
                    Ok(job) => job,
                    // Removed meanwhile, or unreadable now (the next run
                    // reports it): nothing to remove.
                    Err(Error::NotFound(_) | Error::Schema(_)) => continue,
                    Err(e) => return Err(e),
                };
                (&fresh, now())
            } else {
                (listed, snapshot.at)
            };
            match self.prune_decision(job, at, stale_after, probe, criteria) {
                PruneDecision::Skip => {}
                PruneDecision::Keep(reason) => {
                    keep(&mut report, &job.id, Some(job.kind.as_str()), reason);
                }
                PruneDecision::Remove if budget == 0 => report.remaining += 1,
                PruneDecision::Remove => {
                    if apply {
                        remove_tree(&self.job_dir(&job.id))?;
                    }
                    budget -= 1;
                    report.removed.push(PrunedJob {
                        id: job.id.clone(),
                        kind: job.kind.clone(),
                        state: job.state,
                    });
                }
            }
        }

        for bad in &snapshot.invalid {
            if self.dir_holds_only_copy(&self.job_dir(&bad.id)) {
                keep(&mut report, &bad.id, None, PruneKeep::OnlyCopy);
                continue;
            }
            if !criteria.quarantine_invalid {
                keep(&mut report, &bad.id, None, PruneKeep::Unreadable);
                continue;
            }
            if budget == 0 {
                report.remaining += 1;
                continue;
            }
            let moved = match tx.as_deref_mut() {
                None => true,
                Some(tx) => match self.quarantine_invalid(tx, &bad.id) {
                    Ok(target) => target.is_some(),
                    // A move journal appeared meanwhile.
                    Err(Error::AlreadyExists(_)) => {
                        keep(&mut report, &bad.id, None, PruneKeep::OnlyCopy);
                        false
                    }
                    Err(e) => return Err(e),
                },
            };
            if moved {
                budget -= 1;
                report.quarantined.push(bad.id.clone());
            }
        }

        for id in self.unrecorded_folders()? {
            let _q = if apply {
                Some(self.lock(QueueLock::DEFAULT_WAIT)?)
            } else {
                None
            };
            let dir = self.job_dir(&id);
            // Gone, or enqueued meanwhile (an enqueue writes the record
            // under the queue lock right after creating the folder).
            if !dir.is_dir() || dir.join(JOB_JSON).exists() {
                continue;
            }
            if self.dir_holds_only_copy(&dir) {
                keep(&mut report, &id, None, PruneKeep::OnlyCopy);
                continue;
            }
            if !dir_is_empty(&dir)? {
                keep(&mut report, &id, None, PruneKeep::NoRecord);
                continue;
            }
            if budget == 0 {
                report.remaining += 1;
                continue;
            }
            if apply {
                match fs::remove_dir(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => continue,
                    Err(e) => return Err(Error::io_at(&dir, e)),
                }
            }
            budget -= 1;
            report.empty_folders += 1;
        }
        tracing::info!(
            dry_run = report.dry_run,
            removed = report.removed.len(),
            quarantined = report.quarantined.len(),
            empty_folders = report.empty_folders,
            kept = report.kept.len(),
            remaining = report.remaining,
            "queue prune"
        );
        Ok(report)
    }

    /// What [`Self::prune`] does with readable `job` at `at`.
    fn prune_decision(
        &self,
        job: &Job,
        at: DateTime<FixedOffset>,
        stale_after: TimeDelta,
        probe: &dyn ProcessProbe,
        criteria: &PruneCriteria,
    ) -> PruneDecision {
        // Reported whatever the criteria: the user's file is in there.
        if self.holds_only_copy(job) {
            return PruneDecision::Keep(PruneKeep::OnlyCopy);
        }
        if !criteria.kinds.is_empty() && !criteria.kinds.contains(&job.kind) {
            return PruneDecision::Skip;
        }
        let selected = match job.state {
            JobState::Done => criteria.done,
            JobState::Failed if failed_for_good(job) => criteria.failed,
            JobState::Failed if criteria.failed => {
                return PruneDecision::Keep(PruneKeep::Retryable);
            }
            _ => false,
        };
        if !selected {
            return PruneDecision::Skip;
        }
        if let Some(lease) = &job.lease {
            let grace = TimeDelta::from_std(Self::REVOKED_HOLDER_GRACE).unwrap_or(TimeDelta::MAX);
            if lease.is_fresh(at, stale_after)
                || (probe.is_alive(lease.pid) && lease.is_fresh(at, grace))
            {
                return PruneDecision::Keep(if lease.token == REVOKED_TOKEN {
                    PruneKeep::RevokedHolder
                } else {
                    PruneKeep::Held
                });
            }
        }
        let min_age = TimeDelta::from_std(criteria.min_age).unwrap_or(TimeDelta::MAX);
        if at.signed_duration_since(job.updated_at) < min_age {
            return PruneDecision::Keep(PruneKeep::TooRecent);
        }
        if job.state == JobState::Failed && dir_holds_files(&self.job_dir(&job.id)) {
            return PruneDecision::Keep(PruneKeep::HoldsFiles);
        }
        PruneDecision::Remove
    }

    /// Job folders (named by a job ID) holding no `job.json`, sorted.
    fn unrecorded_folders(&self) -> Result<Vec<JobId>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io_at(&self.dir, e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| Error::io_at(&self.dir, e))?;
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| JobId::parse(n).ok())
            else {
                continue;
            };
            if entry.path().is_dir() && !entry.path().join(JOB_JSON).exists() {
                out.push(id);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Whether job folder `dir` holds an unfinished or conflicting move
    /// journal (see [`Self::holds_only_copy`]).
    fn dir_holds_only_copy(&self, dir: &Path) -> bool {
        dir.join(MOVE_JOURNAL).exists() || dir.join(MOVE_CONFLICT).exists()
    }

    /// Writes `job.json` durably.
    fn write_job(&self, job: &Job) -> Result<()> {
        self.store_job(job, true)
    }

    /// Writes `job.json` as compact JSON (a lease renewal rewrites it every
    /// interval), with the kept file relative to the job folder. `durable`:
    /// the replacement survives a power loss; lease renewals do without (the
    /// previous renewal is as good after a crash).
    fn store_job(&self, job: &Job, durable: bool) -> Result<()> {
        let dir = self.job_dir(&job.id);
        let path = dir.join(JOB_JSON);
        let stored;
        let job = match &job.keeps {
            Some(kept) => {
                let mut relative = job.clone();
                relative.keeps = Some(relative_to_job_dir(&dir, kept));
                stored = relative;
                &stored
            }
            None => job,
        };
        let mut bytes = serde_json::to_vec(job)
            .map_err(|e| Error::Internal(format!("serialising job {}: {e}", job.id)))?;
        bytes.push(b'\n');
        if durable {
            fsutil::write_atomic(&path, &bytes)
        } else {
            fsutil::write_atomic_volatile(&path, &bytes)
        }
    }

    /// Takes the queue lock (lease bookkeeping). Never take the library lock
    /// while holding it.
    fn lock(&self, wait: Duration) -> Result<LockGuard> {
        QueueLock::acquire(&self.root, wait)
    }

    /// Every readable job record, in scheduler order.
    fn all_jobs(&self) -> Result<Vec<Job>> {
        Ok(self.snapshot()?.jobs)
    }
}

/// A `failed` job no claim takes again: no attempts left, an interrupted
/// interactive run, or revoked.
fn failed_for_good(job: &Job) -> bool {
    job.attempts >= MAX_ATTEMPTS || job.is_interactive() || job.error.as_deref() == Some(REVOKED)
}

/// Whether job folder `dir` keeps files besides its record (and that
/// record's atomic-write temporaries); an unreadable folder counts as
/// keeping some.
fn dir_holds_files(dir: &Path) -> bool {
    let Ok(mut entries) = fs::read_dir(dir) else {
        return true;
    };
    let record = |name: &str| {
        name == JOB_JSON || fsutil::atomic_temp_target(name).is_some_and(|(t, _)| t == JOB_JSON)
    };
    entries.any(|entry| match entry {
        Ok(e) => e.file_name().to_str().is_none_or(|name| !record(name)),
        Err(_) => true,
    })
}

/// Whether folder `dir` has no entries.
fn dir_is_empty(dir: &Path) -> Result<bool> {
    match fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(e) => Err(Error::io_at(dir, e)),
    }
}

/// Removes folder `path` with everything in it; gone already is fine.
fn remove_tree(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

fn only_copy_error(id: &JobId, dir: &Path) -> Error {
    Error::AlreadyExists(format!(
        "job {id} keeps the only copy of a moved file in {}; move it out of the \
         library folder first",
        dir.display()
    ))
}

/// `path`, a file of the job folder `dir`, as stored: relative to `dir`, so
/// moving the library (and its queue) keeps pointing at it.
pub fn relative_to_job_dir(dir: &Path, path: &Path) -> PathBuf {
    match path.strip_prefix(dir) {
        Ok(rel) => rel.to_owned(),
        Err(_) => path
            .file_name()
            .map_or_else(|| path.to_owned(), PathBuf::from),
    }
}

/// A stored path of a file of the job folder `dir`, resolved. Earlier builds
/// stored absolute paths: one outside `dir` (the library was moved since) is
/// taken as the file of that name in `dir`, where such files always are.
pub fn in_job_dir(dir: &Path, stored: PathBuf) -> PathBuf {
    if stored.is_relative() {
        dir.join(stored)
    } else if stored.starts_with(dir) {
        stored
    } else {
        match stored.file_name() {
            Some(name) => dir.join(name),
            None => stored,
        }
    }
}

fn exhausted(
    job: &Job,
    at: DateTime<FixedOffset>,
    stale_after: TimeDelta,
    probe: &dyn ProcessProbe,
) -> bool {
    matches!(job.state, JobState::Running | JobState::Analyzed)
        && (job.attempts + 1 >= MAX_ATTEMPTS || job.is_interactive())
        && job
            .lease
            .as_ref()
            .is_some_and(|l| !l.is_held(at, stale_after, probe))
}

fn mark_crashed(job: &mut Job, at: DateTime<FixedOffset>) {
    job.state = JobState::Failed;
    job.error = Some("worker crashed".into());
    job.lease = None;
    job.updated_at = at;
}

/// An interactive job's holder went away: failed for good, so no worker
/// runs it again (see [`Job::is_interactive`]).
fn mark_interrupted(job: &mut Job, at: DateTime<FixedOffset>) {
    job.state = JobState::Failed;
    job.error = Some(INTERRUPTED.into());
    job.lease = None;
    job.attempts = job.attempts.max(MAX_ATTEMPTS);
    job.updated_at = at;
}

/// Whether `job` works on `meeting`: its input names the meeting
/// (`meeting_id`), a recording hash (`sha256`) or the recording session
/// (`session_id`), or its key is the meeting ID or a recording hash.
pub fn job_refers_to(job: &Job, meeting: &crate::schema::Meeting) -> bool {
    let input = |k: &str| job.input.get(k).and_then(serde_json::Value::as_str);
    if input("meeting_id") == Some(meeting.id.as_str())
        || job.key == meeting.id.as_str()
        || input("session_id").is_some_and(|session| {
            meeting
                .source_id
                .as_deref()
                .and_then(|id| id.strip_prefix("recording:"))
                == Some(session)
        })
    {
        return true;
    }
    let Some(r) = &meeting.recording else {
        return false;
    };
    let sha256 = input("sha256");
    let names = |h: &str| !h.is_empty() && (job.key == h || sha256 == Some(h));
    names(&r.sha256)
        || r.source_sha256.as_deref().is_some_and(names)
        || r.sources.iter().any(|s| names(&s.sha256))
}

fn busy_error(kind: &str, key: &str) -> Error {
    let what = match kind {
        ENRICH_KIND => "an enrichment of this meeting".to_owned(),
        TEMPLATE_KIND => "this template for this meeting".to_owned(),
        other => format!("a {other} job for it"),
    };
    Error::LibraryBusy(format!(
        "{key} ({what} is queued or running; try again when it ends)"
    ))
}

fn new_token() -> String {
    format!(
        "{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    )
}

/// What a heartbeat renewal found.
enum Renewal {
    Renewed,
    Lost,
}

/// A held lease. A heartbeat thread renews it until the handle is dropped or
/// released. Dropping the handle stops the heartbeat but leaves the lease in
/// `job.json`, where it goes stale after three intervals; call
/// [`LeaseHandle::release`] to hand the job back at once.
///
/// Dropping joins the heartbeat thread, which may be waiting up to one
/// interval for the queue lock.
pub struct LeaseHandle {
    queue: Queue,
    /// The record as the claim wrote it, under the queue lock.
    job: Job,
    token: String,
    lost: Arc<AtomicBool>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl LeaseHandle {
    fn start(queue: Queue, job: Job, token: String) -> Self {
        let lost = Arc::new(AtomicBool::new(false));
        let (stop, stopped) = mpsc::channel::<()>();
        let interval = queue.heartbeat;
        // Waiting longer than one interval for the lock would only delay the
        // next attempt; a busy lock is retried on the next tick.
        let thread = {
            let queue = queue.clone();
            let id = job.id.clone();
            let token = token.clone();
            let lost = Arc::clone(&lost);
            std::thread::Builder::new()
                .name(format!("lease-{id}"))
                .spawn(move || {
                    loop {
                        match stopped.recv_timeout(interval) {
                            Err(RecvTimeoutError::Timeout) => {}
                            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                        }
                        match renew(&queue, interval, &id, &token) {
                            Ok(Renewal::Renewed) => {}
                            Ok(Renewal::Lost) => {
                                lost.store(true, Ordering::SeqCst);
                                return;
                            }
                            // Lock busy or a transient I/O error: retry next tick.
                            Err(_) => {}
                        }
                    }
                })
        };
        let thread = match thread {
            Ok(t) => Some(t),
            // Without a heartbeat the lease would go stale under the caller;
            // report it as lost so `commit` refuses instead of racing.
            Err(_) => {
                lost.store(true, Ordering::SeqCst);
                None
            }
        };
        Self {
            queue,
            job,
            token,
            lost,
            stop: Some(stop),
            thread,
        }
    }

    pub fn id(&self) -> &JobId {
        &self.job.id
    }

    /// The job exactly as this claim wrote it under the queue lock: its
    /// kind, key and input as of the claim, its state and attempts after
    /// the claim's takeover accounting, and this handle's lease. Never read
    /// again: run this input, not a record read before the claim. Requests
    /// written since reach the holder through [`Self::update_in`].
    pub fn claimed_job(&self) -> &Job {
        &self.job
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn job_dir(&self) -> PathBuf {
        self.queue.job_dir(&self.job.id)
    }

    /// Whether the heartbeat has seen another holder's token (or the job vanish).
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Runs `f` inside one [`Library::write`] after checking, under the lock,
    /// that this handle still holds the lease (else `lease_lost` and `f` does
    /// not run), then sets the job's `state` and clears its error (a `failed`
    /// state counts an attempt). If `f` fails, the job record is left
    /// unchanged. The lease stays held.
    pub fn commit<T>(
        &self,
        lib: &Library,
        state: JobState,
        f: impl FnOnce(&mut WriteTx<'_>, &Path) -> Result<T>,
    ) -> Result<T> {
        self.held_write(
            lib,
            |tx, dir| {
                let out = f(tx, dir)?;
                Ok((out, state, None))
            },
            None,
        )
    }

    /// Marks the job `failed` with `error` and counts an attempt, if this
    /// handle still holds it. The lease stays held.
    pub fn fail(&self, lib: &Library, error: impl Into<String>) -> Result<()> {
        let error = error.into();
        self.held_write(lib, |_, _| Ok(((), JobState::Failed, Some(error))), None)
    }

    /// Marks the job `failed` with `error` for good: it keeps `kept`, a file
    /// in its folder that is the user's only copy ([`Job::keeps`]), so it is
    /// never claimed, re-enqueued or removed while that file exists, and has
    /// no attempts left once the user moved the file away.
    pub fn fail_keeping(
        &self,
        lib: &Library,
        error: impl Into<String>,
        kept: PathBuf,
    ) -> Result<()> {
        let error = error.into();
        self.held_write(
            lib,
            |_, _| Ok(((), JobState::Failed, Some(error))),
            Some(kept),
        )
    }

    /// Replaces the job's input if this handle still holds the lease (else
    /// `lease_lost`). State, error and attempts are kept; the lease stays held.
    pub fn set_input(&self, lib: &Library, input: serde_json::Value) -> Result<()> {
        self.replace_input(lib, input, false)
    }

    /// Like [`Self::set_input`], for an explicit new request (a user re-run):
    /// also clears the error and restarts the attempts at zero.
    pub fn restart(&self, lib: &Library, input: serde_json::Value) -> Result<()> {
        self.replace_input(lib, input, true)
    }

    fn replace_input(&self, lib: &Library, input: serde_json::Value, restart: bool) -> Result<()> {
        if self.is_lost() {
            return Err(self.lost_error());
        }
        lib.write(|_| {
            let _q = self.queue.lock(QueueLock::DEFAULT_WAIT)?;
            let mut job = self.held_job()?;
            job.input = input;
            if restart {
                job.attempts = 0;
                job.error = None;
            }
            let at = now();
            if let Some(lease) = job.lease.as_mut() {
                lease.renewed_at = at;
            }
            job.updated_at = at;
            self.queue.write_job(&job)
        })
    }

    /// Inside a library write the caller holds: re-reads the job under the
    /// queue lock, checks this handle still holds its lease (else
    /// `lease_lost`; a takeover needs the library lock, so it cannot happen
    /// before the write ends) and lets `f` edit the input as stored now:
    /// requests written since the claim ([`Queue::request`], also under the
    /// library lock) are seen and never overwritten by an older copy. Then
    /// writes the record with `next`: `None` keeps the state,
    /// [`Next::Done`] completes the job, [`Next::Requeue`] puts it back at
    /// the end of the queue (`queued`, `created_at` now, so jobs queued
    /// meanwhile run first), not to be claimed before `not_before`. The
    /// lease stays held; release it afterwards. Attempts are unchanged.
    pub fn update_in<T>(
        &self,
        _tx: &mut WriteTx<'_>,
        next: Option<Next>,
        f: impl FnOnce(&mut serde_json::Value) -> Result<T>,
    ) -> Result<T> {
        if self.is_lost() {
            return Err(self.lost_error());
        }
        let _q = self.queue.lock(QueueLock::DEFAULT_WAIT)?;
        let mut job = self.held_job()?;
        let out = f(&mut job.input)?;
        let at = now();
        match next {
            None => {}
            Some(Next::Done) => {
                job.state = JobState::Done;
                job.error = None;
                job.not_before = None;
            }
            Some(Next::Requeue { not_before }) => {
                job.state = JobState::Queued;
                job.error = None;
                job.created_at = at;
                job.not_before = not_before;
            }
        }
        if let Some(lease) = job.lease.as_mut() {
            lease.renewed_at = at;
        }
        job.updated_at = at;
        self.queue.write_job(&job)?;
        Ok(out)
    }

    /// Stops the heartbeat and clears the lease so the job can be claimed
    /// again at once. A lease already taken by someone else is left alone.
    /// Takes only the queue lock; `lib` is unused.
    pub fn release(mut self, _lib: &Library) -> Result<()> {
        self.stop_heartbeat();
        let _q = self.queue.lock(QueueLock::DEFAULT_WAIT)?;
        let mut job = match self.queue.get(&self.job.id) {
            Ok(job) => job,
            Err(Error::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        if job.lease.as_ref().is_some_and(|l| l.token == self.token) {
            job.lease = None;
            job.updated_at = now();
            self.queue.write_job(&job)?;
        }
        Ok(())
    }

    /// Token-checked write: `f` returns its result plus the state and error
    /// to record; `keeps` makes the job keep that file for good. Holds the
    /// library lock throughout and the queue lock only
    /// while checking the token and while writing `job.json`, so heartbeats
    /// keep renewing while `f` runs.
    fn held_write<T>(
        &self,
        lib: &Library,
        f: impl FnOnce(&mut WriteTx<'_>, &Path) -> Result<(T, JobState, Option<String>)>,
        keeps: Option<PathBuf>,
    ) -> Result<T> {
        if self.is_lost() {
            return Err(self.lost_error());
        }
        lib.write(|tx| {
            {
                let _q = self.queue.lock(QueueLock::DEFAULT_WAIT)?;
                self.held_job()?;
            }
            let dir = self.job_dir();
            let (out, state, error) = f(tx, &dir)?;
            let _q = self.queue.lock(QueueLock::DEFAULT_WAIT)?;
            // Re-read: `f` may have touched the job folder, and the heartbeat
            // renewed the lease meanwhile.
            let mut job = self.held_job()?;
            job.state = state;
            job.error = error;
            if state == JobState::Failed {
                job.attempts += 1;
            }
            if let Some(kept) = keeps {
                job.keeps = Some(kept);
                job.attempts = job.attempts.max(MAX_ATTEMPTS);
            }
            let at = now();
            if let Some(lease) = job.lease.as_mut() {
                lease.renewed_at = at;
            }
            job.updated_at = at;
            self.queue.write_job(&job)?;
            Ok(out)
        })
    }

    /// The job record if this handle still holds its lease, else `lease_lost`.
    /// Must run under the queue lock.
    fn held_job(&self) -> Result<Job> {
        let job = match self.queue.get(&self.job.id) {
            Ok(job) => Some(job),
            Err(Error::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        match job {
            Some(job) if job.lease.as_ref().is_some_and(|l| l.token == self.token) => Ok(job),
            _ => {
                self.lost.store(true, Ordering::SeqCst);
                Err(self.lost_error())
            }
        }
    }

    fn lost_error(&self) -> Error {
        Error::LeaseLost(format!("job {}", self.job.id))
    }

    fn stop_heartbeat(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for LeaseHandle {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

impl fmt::Debug for LeaseHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LeaseHandle")
            .field("id", &self.job.id)
            .field("lost", &self.is_lost())
            .finish_non_exhaustive()
    }
}

/// One heartbeat: under the queue lock (waiting up to `wait`), re-read the job
/// and renew if the token is ours.
fn renew(queue: &Queue, wait: Duration, id: &JobId, token: &str) -> Result<Renewal> {
    let _q = queue.lock(wait)?;
    let mut job = match queue.get(id) {
        Ok(job) => job,
        Err(Error::NotFound(_)) => return Ok(Renewal::Lost),
        Err(e) => return Err(e),
    };
    match job.lease.as_mut() {
        Some(lease) if lease.token == token => {
            lease.renewed_at = queue.lease_now();
            queue.store_job(&job, false)?;
            Ok(Renewal::Renewed)
        }
        _ => Ok(Renewal::Lost),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    use super::*;
    use crate::schema::tests::sample_meeting;

    /// Short cadence; heartbeat freshness assertions use a per-queue clock
    /// rather than assuming every host thread runs within three intervals.
    const BEAT: Duration = Duration::from_millis(150);

    /// A process probe with a fixed own PID and a set of live PIDs.
    struct Probe {
        me: u32,
        alive: HashSet<u32>,
    }

    impl Probe {
        fn new(me: u32, alive: &[u32]) -> Self {
            let mut alive: HashSet<u32> = alive.iter().copied().collect();
            alive.insert(me);
            Self { me, alive }
        }
    }

    impl ProcessProbe for Probe {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }
        fn current_pid(&self) -> u32 {
            self.me
        }
    }

    fn setup() -> (tempfile::TempDir, Library, Queue) {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let queue = Queue::open(tmp.path()).unwrap().with_heartbeat(BEAT);
        (tmp, lib, queue)
    }

    fn enqueue(lib: &Library, q: &Queue, key: &str) -> JobId {
        lib.write(|tx| q.enqueue(tx, "import", key, serde_json::json!({ "path": key })))
            .unwrap()
    }

    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        cond()
    }

    #[test]
    fn job_ids_are_stable_and_validated() {
        let a = JobId::for_key("import", "abc");
        assert_eq!(a, JobId::for_key("import", "abc"));
        assert_ne!(a, JobId::for_key("reembed", "abc"));
        assert_eq!(JobId::parse(a.as_str()).unwrap(), a);
        for bad in ["j-123", "x-0123456789abcdef", "j-0123456789ABCDEF"] {
            assert!(JobId::parse(bad).is_err(), "{bad}");
        }
        let json = serde_json::to_string(&JobState::Analyzed).unwrap();
        assert_eq!(json, "\"analyzed\"");
    }

    #[test]
    fn enqueue_is_idempotent_by_kind_and_key() {
        let (_tmp, lib, q) = setup();
        let a = enqueue(&lib, &q, "sha-1");
        let again = lib
            .write(|tx| q.enqueue(tx, "import", "sha-1", serde_json::json!({ "other": 1 })))
            .unwrap();
        assert_eq!(a, again);
        let job = q.get(&a).unwrap();
        assert_eq!(job.input["path"], "sha-1");
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.attempts, 0);

        let b = enqueue(&lib, &q, "sha-2");
        let other_kind = lib
            .write(|tx| q.enqueue(tx, "reembed", "sha-1", serde_json::Value::Null))
            .unwrap();
        assert_ne!(a, b);
        assert_ne!(a, other_kind);
        let ids: Vec<JobId> = q
            .list("import")
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&a) && ids.contains(&b));
        assert_eq!(q.list("reembed").unwrap().len(), 1);
        assert!(q.list("nothing").unwrap().is_empty());
        assert_eq!(
            q.get(&JobId::for_key("import", "missing"))
                .unwrap_err()
                .code(),
            "not_found"
        );
        assert!(
            lib.write(|tx| q.enqueue(tx, "", "k", serde_json::Value::Null))
                .is_err()
        );
    }

    #[test]
    fn crashed_queue_claims_stop_at_limit_and_reenqueue_restarts() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "crash");
        // The first claim is not a crash; each takeover of a dead holder
        // counts the crashed run, so the job runs MAX_ATTEMPTS times in all.
        let mut runs = 1;
        drop(q.claim(&lib, &id, &Probe::new(1, &[])).unwrap().unwrap());
        assert_eq!(q.get(&id).unwrap().attempts, 0);
        for attempt in 1..MAX_ATTEMPTS {
            let lease = q
                .claim(&lib, &id, &Probe::new(attempt + 1, &[]))
                .unwrap()
                .unwrap();
            runs += 1;
            assert_eq!(q.get(&id).unwrap().attempts, attempt);
            drop(lease);
        }
        assert_eq!(runs, MAX_ATTEMPTS);
        assert!(q.claim(&lib, &id, &Probe::new(99, &[])).unwrap().is_none());
        let failed = q.get(&id).unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.attempts, MAX_ATTEMPTS);
        assert_eq!(failed.error.as_deref(), Some("worker crashed"));
        assert!(failed.lease.is_none());
        assert!(q.claim(&lib, &id, &Probe::new(99, &[])).unwrap().is_none());
        lib.write(|tx| q.enqueue(tx, "import", "crash", serde_json::json!({"retry": true})))
            .unwrap();
        let retry = q.get(&id).unwrap();
        assert_eq!(retry.state, JobState::Queued);
        assert_eq!(retry.attempts, 0);
        assert_eq!(retry.error, None);
        assert_eq!(retry.input["retry"], true);
        assert!(q.claim(&lib, &id, &Probe::new(99, &[])).unwrap().is_some());
    }

    #[test]
    fn normal_claims_do_not_use_up_attempts() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "two-phase");
        let probe = Probe::new(100, &[]);
        // Like a manifest batch: claimed once per phase, released each time.
        for _ in 0..MAX_ATTEMPTS * 2 {
            let lease = q.claim(&lib, &id, &probe).unwrap().unwrap();
            lease
                .commit(&lib, JobState::Analyzed, |_, _| Ok(()))
                .unwrap();
            lease.release(&lib).unwrap();
        }
        assert_eq!(q.get(&id).unwrap().attempts, 0);
        // A crash after all those claims is still taken over.
        drop(q.claim(&lib, &id, &probe).unwrap().unwrap());
        let taker = q.claim(&lib, &id, &Probe::new(200, &[])).unwrap();
        assert!(taker.is_some());
        let job = q.get(&id).unwrap();
        assert_eq!(job.state, JobState::Analyzed);
        assert_eq!(job.attempts, 1);
    }

    #[test]
    fn failures_count_attempts_until_the_job_stays_failed() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "always-fails");
        let probe = Probe::new(100, &[]);
        for attempt in 1..=MAX_ATTEMPTS {
            let lease = q.claim(&lib, &id, &probe).unwrap().unwrap();
            lease.fail(&lib, "decode error").unwrap();
            lease.release(&lib).unwrap();
            let job = q.get(&id).unwrap();
            assert_eq!(job.state, JobState::Failed);
            assert_eq!(job.attempts, attempt);
        }
        assert!(q.claim(&lib, &id, &probe).unwrap().is_none());
        assert_eq!(q.get(&id).unwrap().attempts, MAX_ATTEMPTS);
        lib.write(|tx| q.enqueue(tx, "import", "always-fails", serde_json::Value::Null))
            .unwrap();
        assert_eq!(q.get(&id).unwrap().attempts, 0);
        assert!(q.claim(&lib, &id, &probe).unwrap().is_some());
    }

    #[test]
    fn exhausted_crashed_jobs_are_failed_without_a_claim() {
        let (_tmp, lib, q) = setup();
        // Leases go stale only through a dead holder, never by timing.
        let q = q.with_heartbeat(Duration::from_secs(60));
        let id = enqueue(&lib, &q, "stranded");
        drop(q.claim(&lib, &id, &Probe::new(1, &[])).unwrap().unwrap());
        for pid in 2..MAX_ATTEMPTS {
            drop(q.claim(&lib, &id, &Probe::new(pid, &[])).unwrap().unwrap());
        }
        // The last run: its crash uses up the last attempt.
        let last_pid = MAX_ATTEMPTS;
        let last = q
            .claim(&lib, &id, &Probe::new(last_pid, &[]))
            .unwrap()
            .unwrap();
        let held = enqueue(&lib, &q, "held");
        let _holder = q
            .claim(&lib, &held, &Probe::new(500, &[]))
            .unwrap()
            .unwrap();
        let stranded = q.get(&id).unwrap();
        assert_eq!(stranded.state, JobState::Running);
        assert_eq!(stranded.attempts, MAX_ATTEMPTS - 1);

        // The last holder is still alive and renewing: not exhausted yet.
        let alive = Probe::new(900, &[last_pid, 500]);
        assert!(!q.is_exhausted(&stranded, &alive));
        assert!(
            lib.write(|tx| q.fail_exhausted(tx, &alive))
                .unwrap()
                .is_empty()
        );
        // Then it crashes.
        drop(last);

        let gone = Probe::new(900, &[500]);
        assert!(q.is_exhausted(&stranded, &gone));
        assert!(!q.is_exhausted(&q.get(&held).unwrap(), &gone));
        assert_eq!(
            lib.write(|tx| q.fail_exhausted(tx, &gone)).unwrap(),
            vec![id.clone()]
        );
        let failed = q.get(&id).unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert_eq!(failed.attempts, MAX_ATTEMPTS);
        assert_eq!(failed.error.as_deref(), Some("worker crashed"));
        assert!(failed.lease.is_none());
        assert_eq!(q.get(&held).unwrap().state, JobState::Running);
    }

    #[test]
    fn a_job_keeping_a_file_is_never_run_reenqueued_or_removed() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "conflicted");
        let probe = Probe::new(100, &[]);
        let kept = q.job_dir(&id).join("audio.wav");
        fs::write(&kept, b"the only copy").unwrap();
        let lease = q.claim(&lib, &id, &probe).unwrap().unwrap();
        lease
            .fail_keeping(&lib, "kept at audio.wav", kept.clone())
            .unwrap();
        lease.release(&lib).unwrap();
        let job = q.get(&id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.attempts, MAX_ATTEMPTS);
        assert!(job.keeps_file());
        assert!(q.claim(&lib, &id, &probe).unwrap().is_none());
        let refused = lib
            .write(|tx| q.enqueue(tx, "import", "conflicted", serde_json::Value::Null))
            .unwrap_err();
        assert_eq!(refused.code(), "already_exists");
        // Even marked done by an administrative reset, the folder stays.
        lib.write(|tx| q.set_state(tx, &id, JobState::Done, None))
            .unwrap();
        assert_eq!(lib.write(|tx| q.remove_finished(tx, "import")).unwrap(), 0);
        assert!(kept.is_file());
        // Once the user moved the file away, an explicit re-run restarts it.
        lib.write(|tx| q.set_state(tx, &id, JobState::Failed, None))
            .unwrap();
        fs::remove_file(&kept).unwrap();
        lib.write(|tx| q.enqueue(tx, "import", "conflicted", serde_json::Value::Null))
            .unwrap();
        let restarted = q.get(&id).unwrap();
        assert_eq!(restarted.state, JobState::Queued);
        assert_eq!(restarted.keeps, None);
        assert!(q.claim(&lib, &id, &probe).unwrap().is_some());
    }

    #[test]
    fn set_input_requires_the_lease_and_keeps_progress() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "input");
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        holder
            .commit(&lib, JobState::Analyzed, |_, _| Ok(()))
            .unwrap();
        holder
            .set_input(&lib, serde_json::json!({ "path": "moved" }))
            .unwrap();
        let job = q.get(&id).unwrap();
        assert_eq!(job.input["path"], "moved");
        assert_eq!(job.state, JobState::Analyzed);
        assert_eq!(job.attempts, 0);
        assert_eq!(job.lease.unwrap().token, holder.token());

        let _taker = q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().unwrap();
        let err = holder
            .set_input(&lib, serde_json::json!({ "path": "stale" }))
            .unwrap_err();
        assert_eq!(err.code(), "lease_lost");
        assert_eq!(q.get(&id).unwrap().input["path"], "moved");
    }

    #[test]
    fn restart_replaces_input_and_restarts_attempts() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "restart");
        // Two crashed holders: their takeovers used up two attempts.
        let _first = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        let _second = q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().unwrap();
        let holder = q.claim(&lib, &id, &Probe::new(300, &[])).unwrap().unwrap();
        assert_eq!(q.get(&id).unwrap().attempts, 2);
        holder
            .restart(&lib, serde_json::json!({ "path": "new" }))
            .unwrap();
        let job = q.get(&id).unwrap();
        assert_eq!(job.input["path"], "new");
        assert_eq!(job.attempts, 0);
        assert_eq!(job.lease.unwrap().token, holder.token());
    }

    #[test]
    fn claim_refuses_live_lease_and_reclaims_stale_one() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "k");
        let probe = Probe::new(100, &[]);
        let first = q.claim(&lib, &id, &probe).unwrap().unwrap();
        let job = q.get(&id).unwrap();
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.attempts, 0);
        assert_eq!(job.lease.as_ref().unwrap().pid, 100);
        assert_eq!(job.lease.as_ref().unwrap().token, first.token());
        assert!(q.claim(&lib, &id, &probe).unwrap().is_none());

        // Stop renewing without releasing: the lease goes stale.
        drop(first);
        assert!(q.claim(&lib, &id, &probe).unwrap().is_none());
        std::thread::sleep(q.stale_after() + BEAT * 2);
        let second = q.claim(&lib, &id, &probe).unwrap().unwrap();
        assert_eq!(q.get(&id).unwrap().attempts, 1);

        // Released leases can be claimed at once.
        second.release(&lib).unwrap();
        assert!(q.get(&id).unwrap().lease.is_none());
        let third = q.claim(&lib, &id, &probe).unwrap().unwrap();
        assert_eq!(q.get(&id).unwrap().attempts, 1, "a plain claim is free");
        drop(third);
    }

    #[test]
    fn claim_reclaims_lease_of_dead_process() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "k");
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        // Another process that still sees 100 alive must wait.
        assert!(
            q.claim(&lib, &id, &Probe::new(200, &[100]))
                .unwrap()
                .is_none()
        );
        // One that sees it gone takes over immediately.
        let taker = q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().unwrap();
        let lease = q.get(&id).unwrap().lease.unwrap();
        assert_eq!(lease.pid, 200);
        assert_eq!(lease.token, taker.token());
        // The old holder's heartbeat notices.
        assert!(wait_until(BEAT * 10, || holder.is_lost()));
        assert_eq!(q.get(&id).unwrap().lease.unwrap().token, taker.token());
    }

    #[test]
    fn heartbeat_keeps_lease_fresh_during_long_call() {
        let (_tmp, lib, mut q) = setup();
        let clock = Arc::new(std::sync::Mutex::new(now()));
        q.lease_clock = Some(clock.clone());
        let id = enqueue(&lib, &q, "k");
        let probe = Probe::new(100, &[]);
        let holder = q.claim(&lib, &id, &probe).unwrap().unwrap();
        let claimed_at = q.get(&id).unwrap().lease.unwrap().renewed_at;

        // A long operation advances lease time beyond expiry. Wait for a
        // real background heartbeat, then keep time fixed while observing
        // and attempting the competing claim, however long CI schedules it out.
        for window in 1..=3 {
            let at = claimed_at + TimeDelta::from_std(q.stale_after() * 3 * window).unwrap();
            *clock.lock().unwrap() = at;
            assert!(wait_until(BEAT * 10, || {
                q.get(&id).unwrap().lease.unwrap().renewed_at == at
            }));
        }
        assert!(
            q.claim(&lib, &id, &probe).unwrap().is_none(),
            "a second claimant must not take a renewed lease"
        );
        assert!(!holder.is_lost());
        let n = holder.commit(&lib, JobState::Done, |_, _| Ok(7)).unwrap();
        assert_eq!(n, 7);
        assert_eq!(q.get(&id).unwrap().state, JobState::Done);
    }

    #[test]
    fn commit_with_lost_token_is_refused_without_running_closure() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "k");
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        let taker = q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().unwrap();

        let ran = AtomicUsize::new(0);
        let err = holder
            .commit(&lib, JobState::Done, |_, _| {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .unwrap_err();
        assert_eq!(err.code(), "lease_lost");
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert!(holder.is_lost());
        assert_eq!(holder.fail(&lib, "x").unwrap_err().code(), "lease_lost");
        assert_eq!(q.get(&id).unwrap().state, JobState::Running);

        // Releasing a lost lease leaves the new holder's lease alone.
        holder.release(&lib).unwrap();
        assert_eq!(q.get(&id).unwrap().lease.unwrap().token, taker.token());
        taker.fail(&lib, "decode error").unwrap();
        let job = q.get(&id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.error.as_deref(), Some("decode error"));
    }

    #[test]
    fn commit_runs_meeting_update_and_job_files_in_one_write() {
        let (_tmp, lib, q) = setup();
        let m = sample_meeting();
        lib.write(|tx| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)
        })
        .unwrap();
        let id = enqueue(&lib, &q, "k");
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        let out = holder
            .commit(&lib, JobState::Analyzed, |tx, job_dir| {
                fsutil::write_json_atomic(&job_dir.join("hints.json"), &serde_json::json!([]))?;
                tx.update_meeting(&m.id, |meeting| {
                    meeting.tags.push("imported".into());
                    Ok(())
                })?;
                Ok("ok")
            })
            .unwrap();
        assert_eq!(out, "ok");
        assert!(holder.job_dir().join("hints.json").is_file());
        assert_eq!(q.get(&id).unwrap().state, JobState::Analyzed);
        assert_eq!(
            lib.load_meeting(&m.id).unwrap().tags,
            vec!["imported".to_owned()]
        );

        // A failing closure leaves the job record as it was.
        let err = holder
            .commit(&lib, JobState::Done, |_, _| -> Result<()> {
                Err(Error::Inference("boom".into()))
            })
            .unwrap_err();
        assert_eq!(err.code(), "inference");
        assert_eq!(q.get(&id).unwrap().state, JobState::Analyzed);
    }

    #[test]
    fn resume_after_crash_reclaims_stale_lease_and_keeps_progress() {
        let (tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "k");
        let crashed = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        crashed
            .commit(&lib, JobState::Analyzed, |_, dir| {
                fsutil::write_atomic(&dir.join("hints.json"), b"[]\n")
            })
            .unwrap();
        // The process dies: no release, no more heartbeats.
        drop(crashed);

        // The restarted process (new PID; the old PID was reused and looks alive).
        let lib2 = Library::open(tmp.path()).unwrap();
        let q2 = Queue::open(tmp.path()).unwrap().with_heartbeat(BEAT);
        let probe = Probe::new(300, &[100]);
        assert!(q2.claim(&lib2, &id, &probe).unwrap().is_none());
        std::thread::sleep(q2.stale_after() + BEAT * 2);
        let resumed = q2.claim(&lib2, &id, &probe).unwrap().unwrap();
        let job = q2.get(&id).unwrap();
        assert_eq!(job.state, JobState::Analyzed, "progress is kept");
        assert_eq!(job.attempts, 1);
        assert!(resumed.job_dir().join("hints.json").is_file());
        resumed
            .commit(&lib2, JobState::Done, |_, _| Ok(()))
            .unwrap();
        resumed.release(&lib2).unwrap();

        // Finished jobs are removed; others stay.
        let other = enqueue(&lib2, &q2, "k2");
        let removed = lib2.write(|tx| q2.remove_finished(tx, "import")).unwrap();
        assert_eq!(removed, 1);
        assert!(!q2.job_dir(&id).exists());
        assert_eq!(q2.get(&other).unwrap().state, JobState::Queued);
        lib2.write(|tx| q2.set_state(tx, &other, JobState::Failed, Some("gone".into())))
            .unwrap();
        assert_eq!(q2.get(&other).unwrap().error.as_deref(), Some("gone"));
    }

    #[test]
    fn long_library_write_does_not_stale_lease() {
        let (tmp, lib, mut q) = setup();
        let clock = Arc::new(std::sync::Mutex::new(now()));
        q.lease_clock = Some(clock.clone());
        let id = enqueue(&lib, &q, "k");
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        let claimed_at = q.get(&id).unwrap().lease.unwrap().renewed_at;

        let lib2 = Library::open(tmp.path()).unwrap();
        let mut q2 = Queue::open(tmp.path()).unwrap().with_heartbeat(BEAT);
        q2.lease_clock = Some(clock.clone());
        lib.write(|_tx| {
            let at = claimed_at + TimeDelta::from_std(q.stale_after() + BEAT * 3).unwrap();
            *clock.lock().unwrap() = at;
            // Renewal must publish while this library write is still held:
            // making the heartbeat take the library lock would fail here.
            assert!(wait_until(BEAT * 10, || {
                q.get(&id).unwrap().lease.unwrap().renewed_at == at
            }));
            // Another process (holder's PID looks alive) while the library is busy.
            assert!(
                q2.claim(&lib2, &id, &Probe::new(200, &[100]))
                    .unwrap()
                    .is_none()
            );
            Ok(())
        })
        .unwrap();
        assert!(!holder.is_lost());
        holder.commit(&lib, JobState::Done, |_, _| Ok(())).unwrap();
        assert_eq!(q.get(&id).unwrap().state, JobState::Done);
    }

    #[test]
    fn reclaim_waits_for_commit_and_rechecks_the_refreshed_lease() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "paused-commit");
        let mut holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        holder.stop_heartbeat();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let commit_lib = lib.clone();
        let commit = std::thread::spawn(move || {
            holder.commit(&commit_lib, JobState::Done, |_, _| {
                entered_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                Ok(())
            })
        });
        entered_rx.recv().unwrap();
        std::thread::sleep(q.stale_after() + BEAT);
        let (result_tx, result_rx) = mpsc::channel();
        let claim_lib = lib.clone();
        let claim_queue = q.clone();
        let claim_id = id.clone();
        let reclaim = std::thread::spawn(move || {
            let result = claim_queue.claim(&claim_lib, &claim_id, &Probe::new(200, &[100]));
            result_tx.send(result.map(|lease| lease.is_some())).unwrap();
        });
        assert!(matches!(
            result_rx.recv_timeout(BEAT * 2),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        finish_tx.send(()).unwrap();
        commit.join().unwrap().unwrap();
        assert!(
            !result_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
        );
        reclaim.join().unwrap();
        assert_eq!(q.get(&id).unwrap().state, JobState::Done);
    }

    fn meeting_id() -> MeetingId {
        MeetingId::parse("m-0000e1").unwrap()
    }

    fn enrich_input(n: u32, interactive: bool) -> serde_json::Value {
        serde_json::json!({ "meeting_id": "m-0000e1", "n": n, "interactive": interactive })
    }

    fn restart_enrich(
        q: &Queue,
        tx: &mut WriteTx<'_>,
        m: &MeetingId,
        input: serde_json::Value,
    ) -> Result<JobId> {
        q.restart_finished(tx, ENRICH_KIND, m.as_str(), input)
    }

    #[test]
    fn template_jobs_reserve_one_job_per_meeting_and_template() {
        let (_tmp, lib, q) = setup();
        let m = meeting_id();
        let input = |t: &str| serde_json::json!({ "meeting_id": "m-0000e1", "template_id": t });
        let key = |t: &str| template_job_key(&m, t);
        let bant = lib
            .write(|tx| q.restart_finished(tx, TEMPLATE_KIND, &key("bant"), input("bant")))
            .unwrap();
        assert_eq!(bant, JobId::for_key(TEMPLATE_KIND, "m-0000e1/bant"));
        // The same template is busy; another template and the enrichment are not.
        let again =
            lib.write(|tx| q.restart_finished(tx, TEMPLATE_KIND, &key("bant"), input("bant")));
        assert_eq!(again.unwrap_err().code(), "library_busy");
        assert!(q.busy(TEMPLATE_KIND, &key("bant")).unwrap());
        assert_eq!(
            q.check_idle(TEMPLATE_KIND, &key("bant"))
                .unwrap_err()
                .code(),
            "library_busy"
        );
        let meddic = lib
            .write(|tx| q.restart_finished(tx, TEMPLATE_KIND, &key("meddic"), input("meddic")))
            .unwrap();
        assert_ne!(meddic, bant);
        q.check_idle(ENRICH_KIND, m.as_str()).unwrap();
        assert!(!q.busy(ENRICH_KIND, m.as_str()).unwrap());
    }

    #[test]
    fn jobs_ahead_counts_waiting_jobs_in_scheduler_order() {
        let (_tmp, lib, q) = setup();
        let a = enqueue(&lib, &q, "a");
        std::thread::sleep(Duration::from_millis(5));
        let b = enqueue(&lib, &q, "b");
        std::thread::sleep(Duration::from_millis(5));
        let c = enqueue(&lib, &q, "c");
        assert_eq!(q.jobs_ahead(&a).unwrap(), 0);
        assert_eq!(q.jobs_ahead(&b).unwrap(), 1);
        assert_eq!(q.jobs_ahead(&c).unwrap(), 2);
        // A held job is running, not ahead; a done one is gone from the line.
        let holder = q.claim(&lib, &a, &Probe::new(100, &[])).unwrap().unwrap();
        assert_eq!(q.jobs_ahead(&c).unwrap(), 1);
        assert_eq!(q.jobs_ahead(&a).unwrap(), 0);
        holder.commit(&lib, JobState::Done, |_, _| Ok(())).unwrap();
        holder.release(&lib).unwrap();
        assert_eq!(q.jobs_ahead(&c).unwrap(), 1);
        lib.write(|tx| q.set_state(tx, &b, JobState::Done, None))
            .unwrap();
        assert_eq!(q.jobs_ahead(&c).unwrap(), 0);
        assert_eq!(q.jobs_ahead(&JobId::for_key("x", "gone")).unwrap(), 0);
    }

    #[test]
    fn kind_labels_name_what_a_job_does() {
        let none = serde_json::json!({});
        assert_eq!(kind_label(ENRICH_KIND, &none), "Generating summary");
        assert_eq!(
            kind_label(
                TEMPLATE_KIND,
                &serde_json::json!({"template_name": "BANT insights"})
            ),
            "Running BANT insights"
        );
        assert_eq!(
            kind_label(TEMPLATE_KIND, &serde_json::json!({"template_id": "retro"})),
            "Running retro"
        );
        assert_eq!(kind_label("refresh-vectors", &none), "Refreshing search");
        assert_eq!(kind_label("something-new", &none), "Background job");
    }

    #[test]
    fn explicit_enrich_requests_restart_finished_jobs_only() {
        let (_tmp, lib, q) = setup();
        let m = meeting_id();
        let restart = |n: u32| lib.write(|tx| restart_enrich(&q, tx, &m, enrich_input(n, false)));
        let id = restart(1).unwrap();
        assert_eq!(id, JobId::for_key(ENRICH_KIND, m.as_str()));
        assert_eq!(restart(2).unwrap_err().code(), "library_busy");
        assert_eq!(
            q.check_idle(ENRICH_KIND, m.as_str()).unwrap_err().code(),
            "library_busy"
        );
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        assert_eq!(restart(2).unwrap_err().code(), "library_busy");
        holder.commit(&lib, JobState::Done, |_, _| Ok(())).unwrap();
        holder.release(&lib).unwrap();
        q.check_idle(ENRICH_KIND, m.as_str()).unwrap();

        // Done: restarted with the new input and fresh attempts.
        assert_eq!(restart(3).unwrap(), id);
        let job = q.get(&id).unwrap();
        assert_eq!(
            (job.state, job.attempts, &job.input["n"]),
            (JobState::Queued, 0, &serde_json::json!(3))
        );
        // Failed: restarted too.
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        holder.fail(&lib, "provider error").unwrap();
        holder.release(&lib).unwrap();
        restart(4).unwrap();
        assert_eq!(q.get(&id).unwrap().state, JobState::Queued);
        // A crashed holder's job (lease no longer renewed) is not busy.
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        drop(holder);
        assert!(wait_until(Duration::from_secs(5), || restart(5).is_ok()));
        assert_eq!(q.get(&id).unwrap().input["n"], 5);

        // Queue::enqueue keeps its semantics: done work is left alone (a
        // manifest resume never restarts its finished imports).
        let import = enqueue(&lib, &q, "done-import");
        lib.write(|tx| q.set_state(tx, &import, JobState::Done, None))
            .unwrap();
        let before = q.get(&import).unwrap();
        lib.write(|tx| {
            q.enqueue(
                tx,
                "import",
                "done-import",
                serde_json::json!({ "path": "other" }),
            )
        })
        .unwrap();
        assert_eq!(q.get(&import).unwrap(), before);
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        holder.commit(&lib, JobState::Done, |_, _| Ok(())).unwrap();
        holder.release(&lib).unwrap();
        lib.write(|tx| q.enqueue(tx, ENRICH_KIND, m.as_str(), enrich_input(9, false)))
            .unwrap();
        let job = q.get(&id).unwrap();
        assert_eq!(
            (job.state, &job.input["n"]),
            (JobState::Done, &serde_json::json!(5))
        );
    }

    #[test]
    fn a_concurrent_claim_never_sees_a_half_reset_enrich_job() {
        let (_tmp, lib, q) = setup();
        let m = meeting_id();
        let id = lib
            .write(|tx| restart_enrich(&q, tx, &m, enrich_input(0, false)))
            .unwrap();
        for round in 1..20u32 {
            // The previous request finished.
            let holder = loop {
                if let Some(h) = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap() {
                    break h;
                }
            };
            holder.commit(&lib, JobState::Done, |_, _| Ok(())).unwrap();
            holder.release(&lib).unwrap();
            std::thread::scope(|s| {
                let claimer = s.spawn(|| {
                    for _ in 0..200 {
                        if let Some(h) = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap() {
                            // Only the restarted request can be claimed.
                            let job = q.get(&id).unwrap();
                            assert_eq!(job.input["n"], round);
                            assert_eq!((job.state, job.attempts), (JobState::Running, 0));
                            h.release(&lib).unwrap();
                            return true;
                        }
                    }
                    false
                });
                lib.write(|tx| restart_enrich(&q, tx, &m, enrich_input(round, false)))
                    .unwrap();
                claimer.join().unwrap();
            });
            let job = q.get(&id).unwrap();
            assert_eq!(job.input["n"], round);
            assert!(matches!(job.state, JobState::Queued | JobState::Running));
        }
    }

    #[test]
    fn interrupted_interactive_jobs_fail_and_are_never_run_again() {
        let (_tmp, lib, q) = setup();
        let m = meeting_id();
        let id = lib
            .write(|tx| restart_enrich(&q, tx, &m, enrich_input(1, true)))
            .unwrap();
        assert!(q.get(&id).unwrap().is_interactive());

        // The terminal holding it went away: a takeover fails it instead.
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        drop(holder);
        assert!(q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().is_none());
        let job = q.get(&id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.error.as_deref(), Some("interrupted"));
        assert!(job.lease.is_none() && job.attempts >= MAX_ATTEMPTS);
        assert!(q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().is_none());

        // Ctrl-C: the holder fails it itself; nobody claims it afterwards.
        lib.write(|tx| restart_enrich(&q, tx, &m, enrich_input(2, true)))
            .unwrap();
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        holder.fail(&lib, "interrupted").unwrap();
        holder.release(&lib).unwrap();
        let job = q.get(&id).unwrap();
        assert!(job.state == JobState::Failed && job.attempts < MAX_ATTEMPTS);
        assert!(q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().is_none());

        // A crashed interactive run is exhausted at once.
        lib.write(|tx| restart_enrich(&q, tx, &m, enrich_input(3, true)))
            .unwrap();
        drop(q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap());
        let probe = Probe::new(200, &[]);
        assert!(q.is_exhausted(&q.get(&id).unwrap(), &probe));
        assert_eq!(
            lib.write(|tx| q.fail_exhausted(tx, &probe)).unwrap(),
            std::slice::from_ref(&id)
        );
        assert_eq!(q.get(&id).unwrap().error.as_deref(), Some("interrupted"));

        // An explicit new request restarts it.
        lib.write(|tx| restart_enrich(&q, tx, &m, enrich_input(4, false)))
            .unwrap();
        assert!(q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().is_some());
    }
    #[test]
    fn queue_aborting_worker_child() {
        let Some(root) = std::env::var_os("BROSCRIBE_QUEUE_ABORT_TEST") else {
            return;
        };
        let lib = Library::open(&root).unwrap();
        let queue = Queue::open(&root).unwrap();
        let id = JobId::for_key("import", "abort");
        let _lease = queue
            .claim(&lib, &id, &Probe::new(std::process::id(), &[]))
            .unwrap()
            .unwrap();
        std::process::abort();
    }

    #[test]
    fn queue_real_worker_abort_increases_attempts_then_fails() {
        let (tmp, lib, queue) = setup();
        let id = enqueue(&lib, &queue, "abort");
        // The first run is a plain claim; each later run takes over a crash
        // and counts it: MAX_ATTEMPTS runs in all.
        for attempt in 0..MAX_ATTEMPTS {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "queue::tests::queue_aborting_worker_child"])
                .env("BROSCRIBE_QUEUE_ABORT_TEST", tmp.path())
                .status()
                .unwrap();
            assert!(!status.success(), "worker must actually abort");
            assert_eq!(queue.get(&id).unwrap().attempts, attempt);
        }
        assert!(
            queue
                .claim(&lib, &id, &Probe::new(std::process::id(), &[]))
                .unwrap()
                .is_none()
        );
        let job = queue.get(&id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.attempts, MAX_ATTEMPTS);
        assert_eq!(job.error.as_deref(), Some("worker crashed"));
    }

    #[test]
    fn claimed_job_is_the_record_its_claim_wrote() {
        let (_tmp, lib, q) = setup();
        let request = |n: u32| {
            lib.write(|tx| q.request(tx, "import", "race", |_| Ok(serde_json::json!({ "n": n }))))
                .unwrap()
        };
        let id = request(1).id;
        // A scheduler read the job (input A) and decided to run it…
        let planned = q.get(&id).unwrap();
        assert_eq!(planned.input["n"], 1);
        // …and a request refreshed its input (B) before the claim.
        request(2);
        let lease = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        let claimed = lease.claimed_job();
        assert_eq!(
            claimed.input["n"], 2,
            "the run gets B, not the earlier read"
        );
        assert_eq!((claimed.state, claimed.attempts), (JobState::Running, 0));
        assert_eq!(claimed.lease.as_ref().unwrap().token, lease.token());
        assert_eq!(claimed.lease.as_ref().unwrap().pid, 100);
        let stored = q.get(&id).unwrap();
        assert_eq!(
            (
                &stored.input,
                stored.state,
                stored.attempts,
                &stored.created_at
            ),
            (
                &claimed.input,
                claimed.state,
                claimed.attempts,
                &claimed.created_at
            )
        );

        // A request after the claim leaves this run's input alone; the
        // holder sees it at its next lease-checked update.
        request(3);
        assert_eq!(lease.claimed_job().input["n"], 2);
        let seen = lib
            .write(|tx| lease.update_in(tx, Some(Next::Done), |input| Ok(input["n"].clone())))
            .unwrap();
        assert_eq!(seen, 3);
        lease.release(&lib).unwrap();

        // A takeover's record carries the crashed run's attempt.
        let id = enqueue(&lib, &q, "taken-over");
        drop(q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap());
        let taker = q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().unwrap();
        assert_eq!(taker.claimed_job().attempts, 1);
        assert_eq!(taker.claimed_job().input["path"], "taken-over");
    }

    #[test]
    fn an_unreadable_neighbour_never_blocks_healthy_jobs() {
        let (_tmp, lib, q) = setup();
        let probe = Probe::new(100, &[]);
        let healthy = enqueue(&lib, &q, "healthy");
        let bad = JobId::for_key("import", "broken");
        fs::create_dir_all(q.job_dir(&bad)).unwrap();
        fs::write(q.job_dir(&bad).join(JOB_JSON), b"{ not json").unwrap();

        let snapshot = q.snapshot().unwrap();
        let ids: Vec<&JobId> = snapshot.jobs.iter().map(|j| &j.id).collect();
        assert_eq!(ids, [&healthy]);
        assert_eq!(snapshot.invalid.len(), 1);
        assert_eq!(snapshot.invalid[0].id, bad);
        assert_eq!(snapshot.invalid[0].path, q.job_dir(&bad).join(JOB_JSON));
        assert_eq!(q.jobs().unwrap().len(), 1);
        assert_eq!(q.list("import").unwrap().len(), 1);
        assert_eq!(q.jobs_ahead(&healthy).unwrap(), 0);
        q.check_meeting_idle(&sample_meeting()).unwrap();
        assert!(
            lib.write(|tx| q.fail_exhausted(tx, &probe))
                .unwrap()
                .is_empty()
        );

        // The healthy job runs and is cleaned up as usual.
        let lease = q.claim(&lib, &healthy, &probe).unwrap().unwrap();
        lease.commit(&lib, JobState::Done, |_, _| Ok(())).unwrap();
        lease.release(&lib).unwrap();
        assert_eq!(lib.write(|tx| q.remove_finished(tx, "import")).unwrap(), 1);

        // The unreadable one is never claimed or removed by accident.
        assert_eq!(q.claim(&lib, &bad, &probe).unwrap_err().code(), "schema");
        assert_eq!(
            lib.write(|tx| q.remove_revoked(tx, &bad, &probe))
                .unwrap_err()
                .code(),
            "schema"
        );
        assert!(q.job_dir(&bad).join(JOB_JSON).is_file());

        // Quarantine moves it aside whole; then it can be requested again.
        assert_eq!(
            lib.write(|tx| q.quarantine_invalid(tx, &healthy)).unwrap(),
            None
        );
        let moved = lib
            .write(|tx| q.quarantine_invalid(tx, &bad))
            .unwrap()
            .unwrap();
        assert_eq!(fs::read(moved.join(JOB_JSON)).unwrap(), b"{ not json");
        assert!(moved.starts_with(q.dir().join(Queue::QUARANTINE_DIR)));
        assert!(!q.job_dir(&bad).exists());
        assert!(q.snapshot().unwrap().invalid.is_empty());
        assert_eq!(enqueue(&lib, &q, "broken"), bad);
        assert!(q.claim(&lib, &bad, &probe).unwrap().is_some());

        // A folder holding the only copy of a moved original stays put.
        let kept = JobId::for_key("import", "broken-move");
        fs::create_dir_all(q.job_dir(&kept)).unwrap();
        fs::write(q.job_dir(&kept).join(JOB_JSON), b"[").unwrap();
        fs::write(q.job_dir(&kept).join(MOVE_JOURNAL), b"{}").unwrap();
        let err = lib.write(|tx| q.quarantine_invalid(tx, &kept)).unwrap_err();
        assert_eq!(err.code(), "already_exists");
        assert!(q.job_dir(&kept).join(MOVE_JOURNAL).is_file());
    }

    #[test]
    fn revoked_cleanup_is_bounded_under_pid_reuse_and_keeps_only_copies() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "revoked");
        let mut holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        holder.stop_heartbeat();
        // PID 100 looks alive from now on: still the holder, or reused.
        let reused = Probe::new(300, &[100]);
        assert!(lib.write(|tx| q.revoke(tx, &id, &reused)).unwrap());
        assert!(
            !lib.write(|tx| q.remove_revoked(tx, &id, &reused)).unwrap(),
            "a recent holder may still write into the folder"
        );
        // The revoked holder can no longer commit.
        assert_eq!(
            holder
                .commit(&lib, JobState::Done, |_, _| Ok(()))
                .unwrap_err()
                .code(),
            "lease_lost"
        );
        // Past the grace the folder goes, though PID 100 still looks alive.
        let grace = TimeDelta::from_std(Queue::REVOKED_HOLDER_GRACE).unwrap();
        let mut job = q.get(&id).unwrap();
        job.lease.as_mut().unwrap().renewed_at = now() - grace - TimeDelta::seconds(1);
        q.write_job(&job).unwrap();
        assert!(lib.write(|tx| q.remove_revoked(tx, &id, &reused)).unwrap());
        assert!(!q.job_dir(&id).exists());

        // A stamp far ahead (the clock moved back) is bounded the same way.
        let ahead = enqueue(&lib, &q, "ahead");
        let mut holder = q
            .claim(&lib, &ahead, &Probe::new(100, &[]))
            .unwrap()
            .unwrap();
        holder.stop_heartbeat();
        lib.write(|tx| q.revoke(tx, &ahead, &reused)).unwrap();
        let mut job = q.get(&ahead).unwrap();
        job.lease.as_mut().unwrap().renewed_at = now() + grace + TimeDelta::seconds(60);
        q.write_job(&job).unwrap();
        assert!(
            lib.write(|tx| q.remove_revoked(tx, &ahead, &reused))
                .unwrap()
        );

        // A revoked folder holding the only copy of a moved original stays,
        // under the queue lock, whatever its holder.
        let moved = enqueue(&lib, &q, "moved");
        fs::write(q.job_dir(&moved).join(MOVE_JOURNAL), b"{}").unwrap();
        lib.write(|tx| q.revoke(tx, &moved, &reused)).unwrap();
        let err = lib
            .write(|tx| q.remove_revoked(tx, &moved, &reused))
            .unwrap_err();
        assert_eq!(err.code(), "already_exists");
        assert!(q.job_dir(&moved).join(MOVE_JOURNAL).is_file());
    }

    #[test]
    fn leases_out_of_line_with_the_clock_are_taken_over_but_stay_fenced() {
        let (_tmp, lib, q) = setup();
        for (key, offset) in [
            // Stamped before a sleep (or a forward jump of the clock).
            ("slept", -TimeDelta::hours(1)),
            // Stamped before the clock went back.
            ("clock-back", TimeDelta::hours(1)),
        ] {
            let id = enqueue(&lib, &q, key);
            let mut holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
            holder.stop_heartbeat();
            let mut job = q.get(&id).unwrap();
            job.lease.as_mut().unwrap().renewed_at = now() + offset;
            q.write_job(&job).unwrap();
            assert!(!q.busy("import", key).unwrap(), "{key}");
            // PID 100 looks alive (the holder, or a reused PID).
            let taker = q
                .claim(&lib, &id, &Probe::new(200, &[100]))
                .unwrap()
                .unwrap();
            assert_eq!(taker.claimed_job().attempts, 1, "{key}");
            let err = holder
                .commit(&lib, JobState::Done, |_, _| Ok(()))
                .unwrap_err();
            assert_eq!(err.code(), "lease_lost", "{key}");
            taker.commit(&lib, JobState::Done, |_, _| Ok(())).unwrap();
            assert_eq!(q.get(&id).unwrap().state, JobState::Done);
        }
        // A stamp only slightly ahead (clocks agree within the stale window)
        // is still held.
        let id = enqueue(&lib, &q, "slightly-ahead");
        let mut holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        holder.stop_heartbeat();
        let mut job = q.get(&id).unwrap();
        job.lease.as_mut().unwrap().renewed_at = now() + TimeDelta::milliseconds(50);
        q.write_job(&job).unwrap();
        assert!(
            q.claim(&lib, &id, &Probe::new(200, &[100]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn snapshot_ranks_match_jobs_ahead() {
        let (_tmp, lib, q) = setup();
        let a = enqueue(&lib, &q, "a");
        std::thread::sleep(Duration::from_millis(5));
        let b = enqueue(&lib, &q, "b");
        std::thread::sleep(Duration::from_millis(5));
        let c = enqueue(&lib, &q, "c");
        let _held = q.claim(&lib, &a, &Probe::new(100, &[])).unwrap().unwrap();
        let snapshot = q.snapshot().unwrap();
        let ranks = snapshot.ranks();
        assert_eq!(ranks, HashMap::from([(b.clone(), 0), (c.clone(), 1)]));
        for id in [&a, &b, &c] {
            assert_eq!(snapshot.jobs_ahead(id), q.jobs_ahead(id).unwrap(), "{id}");
            assert_eq!(ranks.get(id).copied().unwrap_or(0), snapshot.jobs_ahead(id));
        }
        assert!(!snapshot.is_waiting(&snapshot.jobs[0]));
    }

    #[test]
    fn renewals_keep_the_persisted_lease_layout() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "renewed");
        let holder = q.claim(&lib, &id, &Probe::new(100, &[])).unwrap().unwrap();
        let claimed_at = holder.claimed_job().lease.as_ref().unwrap().renewed_at;
        assert!(wait_until(BEAT * 10, || {
            q.get(&id).unwrap().lease.unwrap().renewed_at > claimed_at
        }));
        let raw = fs::read(q.job_dir(&id).join(JOB_JSON)).unwrap();
        assert!(!raw.windows(3).any(|w| w == b"\n  "), "compact record");
        // Readers of the stored record (any version) find the same fields.
        let stored: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(stored["lease"]["pid"], 100);
        assert_eq!(stored["lease"]["token"], holder.token());
        assert!(stored["lease"]["renewed_at"].is_string());
        assert_eq!(stored["state"], "running");
        let job: Job = serde_json::from_slice(&raw).unwrap();
        assert_eq!(job.input, holder.claimed_job().input);
    }

    #[test]
    fn sweep_removes_orphan_temps_in_job_folders() {
        let (_tmp, lib, q) = setup();
        let id = enqueue(&lib, &q, "sweep");
        let dead = q.job_dir(&id).join(".job.json.tmp-4242-0000000a");
        let own = q.job_dir(&id).join(".job.json.tmp-100-0000000b");
        let hints = q.job_dir(&id).join("hints.json");
        for f in [&dead, &own, &hints] {
            fs::write(f, b"x").unwrap();
        }
        let removed = lib
            .write(|tx| q.sweep_orphan_temps(tx, &Probe::new(100, &[]), Duration::ZERO))
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!dead.exists() && own.exists() && hints.exists());
        assert_eq!(q.get(&id).unwrap().state, JobState::Queued);
    }

    #[test]
    fn lease_freshness_is_two_sided_and_held_needs_a_live_holder() {
        let at = now();
        let stale = TimeDelta::seconds(15);
        let lease = |offset: TimeDelta| Lease {
            pid: 100,
            token: "t".into(),
            renewed_at: at + offset,
        };
        let alive = Probe::new(1, &[100]);
        let gone = Probe::new(1, &[]);
        for (offset, fresh) in [
            (TimeDelta::zero(), true),
            (-stale, true),
            (stale, true),
            (-stale - TimeDelta::seconds(1), false),
            (stale + TimeDelta::seconds(1), false),
        ] {
            let l = lease(offset);
            assert_eq!(l.is_fresh(at, stale), fresh, "{offset}");
            assert_eq!(l.is_held(at, stale, &alive), fresh, "{offset}");
            assert!(!l.is_held(at, stale, &gone), "{offset}");
        }
    }

    /// A holder stopped on purpose hands its job back without spending an
    /// attempt, and only its own lease is touched: another PID or token, a
    /// live PID named without a token, a takeover's lease or a revoked one
    /// stay as they are; a stopped interactive run is failed for good.
    #[test]
    fn planned_stop_hands_the_job_back_without_an_attempt() {
        let (_tmp, lib, q) = setup();
        let worker = Probe::new(100, &[]);
        let gone = Probe::new(1, &[]);
        let id = enqueue(&lib, &q, "stopped");
        let stop = |pid: u32, token: Option<&str>, probe: &Probe| {
            lib.write(|tx| q.release_stopped_holder(tx, &id, pid, token, probe))
                .unwrap()
        };
        let held = q.claim(&lib, &id, &worker).unwrap().unwrap();
        let token = held.token().to_owned();
        let created_at = held.claimed_job().created_at;
        // Stopped: the heartbeat ends, the lease stays in `job.json`.
        drop(held);
        assert_eq!(stop(200, None, &gone), StoppedRelease::NotHeld);
        assert_eq!(stop(100, Some("another"), &gone), StoppedRelease::NotHeld);
        assert_eq!(stop(100, None, &worker), StoppedRelease::HolderAlive);
        assert_eq!(q.get(&id).unwrap().lease.unwrap().token, token);
        assert_eq!(stop(100, Some(&token), &gone), StoppedRelease::Released);
        let job = q.get(&id).unwrap();
        assert_eq!((job.state, job.attempts), (JobState::Queued, 0));
        assert!(job.lease.is_none());
        assert_eq!(job.created_at, created_at, "keeps its place");
        assert!(job.is_claimable());
        assert_eq!(stop(100, None, &gone), StoppedRelease::NotHeld);

        // A crashed holder's lease taken over: the taker's, never released
        // by the stop of the dead one; the takeover alone spent an attempt.
        drop(q.claim(&lib, &id, &worker).unwrap().unwrap());
        let taker = q.claim(&lib, &id, &Probe::new(200, &[])).unwrap().unwrap();
        assert_eq!(stop(100, None, &gone), StoppedRelease::NotHeld);
        assert_eq!(q.get(&id).unwrap().lease.unwrap().token, taker.token());
        let taken = taker.token().to_owned();
        drop(taker);
        assert_eq!(stop(200, Some(&taken), &gone), StoppedRelease::Released);
        assert_eq!(q.get(&id).unwrap().attempts, 1);

        // Revoked meanwhile: stays revoked.
        drop(q.claim(&lib, &id, &worker).unwrap().unwrap());
        lib.write(|tx| q.revoke(tx, &id, &gone)).unwrap();
        assert_eq!(stop(100, None, &gone), StoppedRelease::NotHeld);
        let job = q.get(&id).unwrap();
        assert_eq!(
            (job.state, job.error.as_deref()),
            (JobState::Failed, Some(REVOKED))
        );

        let interactive = lib
            .write(|tx| {
                q.enqueue(
                    tx,
                    "enrich",
                    "m-1",
                    serde_json::json!({ "interactive": true }),
                )
            })
            .unwrap();
        let held = q.claim(&lib, &interactive, &worker).unwrap().unwrap();
        let token = held.token().to_owned();
        drop(held);
        let stopped = lib
            .write(|tx| q.release_stopped_holder(tx, &interactive, 100, Some(&token), &gone))
            .unwrap();
        assert_eq!(stopped, StoppedRelease::Interrupted);
        let job = q.get(&interactive).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert!(!job.is_claimable(), "never re-run by a worker");
    }

    /// The polling scheduler's view: a failed job with attempts left is
    /// retried after its delay, a `not_before` is kept, held jobs and jobs
    /// failed for good are not offered, a dead interactive holder's job is
    /// exhausted.
    #[test]
    fn pending_work_retries_failed_jobs_after_their_delay_and_keeps_not_before() {
        let (_tmp, lib, q) = setup();
        let me = Probe::new(100, &[]);
        let queued = enqueue(&lib, &q, "queued");
        let failed = enqueue(&lib, &q, "failed");
        let h = q.claim(&lib, &failed, &me).unwrap().unwrap();
        h.fail(&lib, "transient").unwrap();
        h.release(&lib).unwrap();
        let waiting = enqueue(&lib, &q, "waiting");
        let h = q.claim(&lib, &waiting, &me).unwrap().unwrap();
        let later = now() + TimeDelta::hours(2);
        lib.write(|tx| {
            h.update_in(
                tx,
                Some(Next::Requeue {
                    not_before: Some(later),
                }),
                |_| Ok(()),
            )
        })
        .unwrap();
        h.release(&lib).unwrap();
        let held = enqueue(&lib, &q, "held");
        let holder = q.claim(&lib, &held, &me).unwrap().unwrap();
        let interactive = lib
            .write(|tx| {
                q.enqueue(
                    tx,
                    "enrich",
                    "m-1",
                    serde_json::json!({ "interactive": true }),
                )
            })
            .unwrap();
        drop(
            q.claim(&lib, &interactive, &Probe::new(300, &[]))
                .unwrap()
                .unwrap(),
        );
        let for_good = enqueue(&lib, &q, "for-good");
        for _ in 0..MAX_ATTEMPTS {
            let h = q.claim(&lib, &for_good, &me).unwrap().unwrap();
            h.fail(&lib, "again").unwrap();
            h.release(&lib).unwrap();
        }

        let mut snapshot = q.snapshot().unwrap();
        // This plans at the held job's real renewal. Concurrent fsync work
        // must not turn the fixture's 30 ms lease into an accidental expiry.
        snapshot.at = snapshot
            .jobs
            .iter()
            .find(|job| job.id == held)
            .unwrap()
            .lease
            .as_ref()
            .unwrap()
            .renewed_at;
        let not_before = q.get(&waiting).unwrap().not_before;
        let now_work = snapshot.pending_work(&me, &|_: u32| Duration::ZERO);
        assert_eq!(now_work.runnable, vec![queued.clone(), failed.clone()]);
        assert_eq!(now_work.exhausted, vec![interactive]);
        assert_eq!(now_work.wake_at, not_before);

        let backoff = snapshot.pending_work(&me, &|_: u32| Duration::from_secs(3600));
        assert_eq!(backoff.runnable, vec![queued]);
        let retry_at = q.get(&failed).unwrap().updated_at + TimeDelta::hours(1);
        assert_eq!(
            backoff.wake_at,
            Some(retry_at),
            "the earlier of retry and not_before"
        );
        drop(holder);
    }

    fn finished(lib: &Library, q: &Queue, kind: &str, key: &str, probe: &Probe) -> JobId {
        let id = lib
            .write(|tx| q.enqueue(tx, kind, key, serde_json::json!({ "path": key })))
            .unwrap();
        let h = q.claim(lib, &id, probe).unwrap().unwrap();
        h.commit(lib, JobState::Done, |_, _| Ok(())).unwrap();
        h.release(lib).unwrap();
        id
    }

    fn failed_for_good_job(lib: &Library, q: &Queue, key: &str, probe: &Probe) -> JobId {
        let id = enqueue(lib, q, key);
        for _ in 0..MAX_ATTEMPTS {
            let h = q.claim(lib, &id, probe).unwrap().unwrap();
            h.fail(lib, "failed").unwrap();
            h.release(lib).unwrap();
        }
        id
    }

    /// `queue prune` removes only finished jobs no one needs, the dry run
    /// predicts exactly that, and every protected folder is kept with its
    /// reason: the only copy of a moved file, a failed job's staged files,
    /// a retryable job, a live or revoked holder, an unreadable record and
    /// a folder without one. Quarantine moves a record, never deletes it.
    #[test]
    fn prune_removes_only_unprotected_finished_jobs_and_the_dry_run_agrees() {
        let (_tmp, lib, q) = setup();
        let me = Probe::new(100, &[500]);
        let done = finished(&lib, &q, "import", "done", &me);
        let moved = finished(&lib, &q, "import", "moved", &me);
        fs::write(q.job_dir(&moved).join(MOVE_JOURNAL), b"{}").unwrap();
        let dead_end = failed_for_good_job(&lib, &q, "dead-end", &me);
        let staged = failed_for_good_job(&lib, &q, "staged", &me);
        fs::write(q.job_dir(&staged).join("audio.wav"), b"synthetic").unwrap();
        let retryable = enqueue(&lib, &q, "retry");
        let h = q.claim(&lib, &retryable, &me).unwrap().unwrap();
        h.fail(&lib, "transient").unwrap();
        h.release(&lib).unwrap();
        let held = enqueue(&lib, &q, "held");
        let holder = q.claim(&lib, &held, &me).unwrap().unwrap();
        holder.commit(&lib, JobState::Done, |_, _| Ok(())).unwrap();
        let revoked = enqueue(&lib, &q, "revoked");
        drop(
            q.claim(&lib, &revoked, &Probe::new(500, &[]))
                .unwrap()
                .unwrap(),
        );
        lib.write(|tx| q.revoke(tx, &revoked, &me)).unwrap();
        enqueue(&lib, &q, "queued");
        let unreadable = JobId::for_key("import", "garbled");
        fs::create_dir_all(q.job_dir(&unreadable)).unwrap();
        fs::write(q.job_dir(&unreadable).join(JOB_JSON), b"{ garbled").unwrap();
        let empty = JobId::for_key("import", "interrupted");
        fs::create_dir_all(q.job_dir(&empty)).unwrap();
        let foreign = JobId::for_key("import", "foreign");
        fs::create_dir_all(q.job_dir(&foreign)).unwrap();
        fs::write(q.job_dir(&foreign).join("notes.txt"), b"synthetic").unwrap();

        let criteria = PruneCriteria {
            failed: true,
            ..PruneCriteria::default()
        };
        let plan = q.prune_plan(&me, &criteria).unwrap();
        assert!(
            q.job_dir(&done).exists() && q.job_dir(&empty).exists(),
            "a plan changes nothing"
        );
        let applied = lib.write(|tx| q.prune(tx, &me, &criteria)).unwrap();
        assert!(plan.dry_run && !applied.dry_run);
        assert_eq!(plan.removed, applied.removed);
        assert_eq!(plan.kept, applied.kept);
        assert_eq!((plan.empty_folders, applied.empty_folders), (1, 1));
        let removed: Vec<JobId> = applied.removed.iter().map(|j| j.id.clone()).collect();
        assert_eq!(removed, vec![done.clone(), dead_end.clone()]);
        let reason = |id: &JobId| applied.kept.iter().find(|k| k.id == *id).map(|k| k.reason);
        for (id, why) in [
            (&moved, PruneKeep::OnlyCopy),
            (&staged, PruneKeep::HoldsFiles),
            (&retryable, PruneKeep::Retryable),
            (&held, PruneKeep::Held),
            (&revoked, PruneKeep::RevokedHolder),
            (&unreadable, PruneKeep::Unreadable),
            (&foreign, PruneKeep::NoRecord),
        ] {
            assert_eq!(reason(id), Some(why), "{id}");
            assert!(q.job_dir(id).exists(), "{id}");
        }
        assert_eq!(applied.kept.len(), 7);
        assert!(applied.quarantined.is_empty() && applied.remaining == 0);
        for id in [&done, &dead_end, &empty] {
            assert!(!q.job_dir(id).exists(), "{id}");
        }
        let report = serde_json::to_string(&applied).unwrap();
        assert!(!report.contains(&q.dir().display().to_string()), "no paths");

        let quarantine = PruneCriteria {
            done: false,
            quarantine_invalid: true,
            ..PruneCriteria::default()
        };
        let r = lib.write(|tx| q.prune(tx, &me, &quarantine)).unwrap();
        assert_eq!(r.quarantined, vec![unreadable.clone()]);
        assert!(!q.job_dir(&unreadable).exists());
        let moved_aside: Vec<_> = fs::read_dir(q.dir().join(Queue::QUARANTINE_DIR))
            .unwrap()
            .map(|e| e.unwrap().path().join(JOB_JSON))
            .collect();
        assert_eq!(moved_aside.len(), 1);
        assert_eq!(fs::read(&moved_aside[0]).unwrap(), b"{ garbled");
        holder.release(&lib).unwrap();
    }

    /// A plan is no promise: a job requested again after it was planned
    /// is live work when the prune runs, and stays.
    #[test]
    fn prune_decides_again_when_it_runs() {
        let (_tmp, lib, q) = setup();
        let me = Probe::new(100, &[]);
        let id = finished(&lib, &q, "import", "again", &me);
        let plan = q.prune_plan(&me, &PruneCriteria::default()).unwrap();
        assert_eq!(plan.removed.len(), 1);
        lib.write(|tx| {
            q.restart_finished(
                tx,
                "import",
                "again",
                serde_json::json!({ "path": "again" }),
            )
        })
        .unwrap();
        let applied = lib
            .write(|tx| q.prune(tx, &me, &PruneCriteria::default()))
            .unwrap();
        assert!(applied.removed.is_empty());
        assert_eq!(q.get(&id).unwrap().state, JobState::Queued);
    }

    /// Kinds, age and the limit bound what one run removes; the rest is
    /// counted for a later run, oldest first.
    #[test]
    fn prune_is_bounded_by_kind_age_and_limit() {
        let (_tmp, lib, q) = setup();
        let me = Probe::new(100, &[]);
        let a = finished(&lib, &q, "import", "a", &me);
        let b = finished(&lib, &q, "import", "b", &me);
        let c = finished(&lib, &q, "import", "c", &me);
        let other = finished(&lib, &q, "other", "k", &me);
        let ids = |r: &PruneReport| r.removed.iter().map(|j| j.id.clone()).collect::<Vec<_>>();

        let young = PruneCriteria {
            min_age: Duration::from_secs(3600),
            ..PruneCriteria::default()
        };
        let r = lib.write(|tx| q.prune(tx, &me, &young)).unwrap();
        assert!(r.removed.is_empty());
        assert!(r.kept.iter().all(|k| k.reason == PruneKeep::TooRecent));
        assert_eq!(r.kept.len(), 4);

        let only_other = PruneCriteria {
            kinds: vec!["other".into()],
            ..PruneCriteria::default()
        };
        assert_eq!(
            ids(&lib.write(|tx| q.prune(tx, &me, &only_other)).unwrap()),
            vec![other]
        );

        let two = PruneCriteria {
            limit: 2,
            ..PruneCriteria::default()
        };
        let r = lib.write(|tx| q.prune(tx, &me, &two)).unwrap();
        assert_eq!((ids(&r), r.remaining), (vec![a, b], 1));
        let r = lib.write(|tx| q.prune(tx, &me, &two)).unwrap();
        assert_eq!((ids(&r), r.remaining), (vec![c], 0));
    }
}
