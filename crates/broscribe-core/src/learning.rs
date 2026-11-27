//! Confirmation-driven identity reevaluation requests (design §3.1, §6.4).
//!
//! When confirmed voice evidence changes (a speaker named or corrected on
//! any surface, confirmation samples that actually changed a person's
//! voiceprints, a forgotten voice or person), the write that changes it
//! first records a request in the library's queue, under the same library
//! lock: one coalescing [`REIDENTIFY_KIND`] job per speaker-embedding model,
//! keyed by the model. The pipeline's worker runs it: the source meetings
//! first, then every other `done` meeting in short per-meeting writes, with
//! the same planner and acceptance policy as `identity re-resolve`.
//!
//! The job's input ([`ReidentifyInput`]) carries a request generation
//! (`requested`, bumped by every request), the generation the last
//! complete pass covered (`processed`), the source meetings still to
//! process first, the busy meetings deferred to a later run, the current
//! pass's checkpoint ([`Pass`]) and the report of what it changed
//! ([`LearningReport`]). Requests edit the stored input under the queue
//! lock ([`Queue::request`]) and the worker edits it through its lease
//! ([`crate::queue::LeaseHandle::update_in`]), so neither overwrites the
//! other. No embeddings are stored in the job: a pass reads voiceprints from
//! the library and checks [`evidence_fingerprint`] under the lock of every
//! publication.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;

use chrono::{DateTime, FixedOffset, TimeDelta};
use serde::{Deserialize, Serialize};

use crate::config::Thresholds;
use crate::ops::{SpeakerChange, SpeakerState};
use crate::queue::{JOB_JSON, Job, JobId, JobState, Queue};
use crate::schema::{Assignment, now};
use crate::{Error, Library, MeetingId, Result, WriteTx, fsutil, paths};

/// Queue kind of identity reevaluation; the key is the speaker-embedding
/// model ID.
pub const REIDENTIFY_KIND: &str = "reidentify";

/// The reevaluation job of `model`.
pub fn job_id(model: &str) -> JobId {
    JobId::for_key(REIDENTIFY_KIND, model)
}

/// A recorded reevaluation request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LearningRequest {
    pub job_id: JobId,
    /// The request generation this request raised the job to.
    pub generation: u64,
    /// The meeting reevaluated first, when the request came from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_meeting: Option<MeetingId>,
}

/// The input of a [`REIDENTIFY_KIND`] job.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReidentifyInput {
    pub model: String,
    /// Request generation: every request adds one.
    #[serde(default)]
    pub requested: u64,
    /// The generation the last complete pass covered.
    #[serde(default)]
    pub processed: u64,
    /// Meetings to reevaluate first, most recent request first. Not
    /// `meeting_id`: the job works on no single meeting, so other writers'
    /// busy checks never wait for it.
    #[serde(default)]
    pub source_meetings: Vec<MeetingId>,
    /// Source of the latest request, retained after the worklist is drained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_source: Option<MeetingId>,
    /// `done` meetings another job was working on: retried later.
    #[serde(default)]
    pub deferred: Vec<MeetingId>,
    /// The pass in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<Pass>,
    /// A publication that started but whose completion was not recorded
    /// (a crash between the meeting file and its derived state): the next
    /// run checks which of its changes reached the meeting and repairs
    /// their derived state and counts once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<InFlight>,
    /// What the passes since the last idle request changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<LearningReport>,
}

impl ReidentifyInput {
    /// The input of `job` (`None`: the job's stored input does not parse;
    /// a request then starts afresh rather than refuse the user's write).
    pub fn of(job: &Job) -> Option<Self> {
        Self::of_value(&job.input)
    }

    pub fn of_value(input: &serde_json::Value) -> Option<Self> {
        serde_json::from_value(input.clone()).ok()
    }

    pub fn to_value(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self)
            .map_err(|e| Error::Internal(format!("reidentify input does not serialise: {e}")))
    }
}

/// The checkpoint of one pass over the library.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pass {
    /// The request generation the pass started for. A later request
    /// restarts the pass (its sources first).
    pub generation: u64,
    /// The model's thresholds when the pass started; a change restarts it.
    #[serde(default)]
    pub thresholds: Option<Thresholds>,
    /// The sweep visited every meeting ID up to this one (sorted order).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<MeetingId>,
    /// The sweep is complete; only deferred meetings remain.
    #[serde(default)]
    pub swept: bool,
}

impl Pass {
    pub fn new(generation: u64, thresholds: Option<Thresholds>) -> Self {
        Self {
            generation,
            thresholds,
            cursor: None,
            swept: false,
        }
    }
}

/// The changes of one meeting publication, recorded before its files are
/// written so recovery can tell whether they were.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InFlight {
    pub meeting_id: MeetingId,
    pub source: bool,
    pub changes: Vec<SpeakerChange>,
}

/// What reevaluation changed, by speaker: each counts once by its net
/// change since the report started (first state → latest state).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LearningReport {
    /// The request generation of the last update.
    pub generation: u64,
    /// Newly automatic names (the speaker was not auto-named before).
    pub newly_named: usize,
    /// Automatic names replaced by another person.
    pub replaced: usize,
    /// Automatic names that became a suggestion or unassigned.
    pub demoted: usize,
    /// New or changed suggestions.
    pub suggested: usize,
    /// Suggestions removed.
    pub cleared: usize,
    /// Meetings waiting because another job works on them.
    pub deferred: usize,
    pub meetings: Vec<AffectedMeeting>,
    /// Meetings the reevaluation could not read or plan, skipped so later
    /// meetings still run; the next request tries them again.
    #[serde(default)]
    pub unreadable: Vec<UnreadableMeeting>,
}

/// A meeting reevaluation skipped: its ID and the stable error code only
/// (never a path, name or text).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadableMeeting {
    pub meeting_id: MeetingId,
    pub code: String,
}

/// One meeting reevaluation changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AffectedMeeting {
    pub meeting_id: MeetingId,
    /// A source meeting of a request (reevaluated first).
    pub source: bool,
    pub newly_named: usize,
    pub replaced: usize,
    pub demoted: usize,
    pub suggested: usize,
    pub cleared: usize,
    /// The speakers, with their state before the first and after the latest
    /// change.
    pub changes: Vec<SpeakerChange>,
}

/// The category one net change counts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    NewlyNamed,
    Replaced,
    Demoted,
    Suggested,
    Cleared,
}

fn category(before: &SpeakerState, after: &SpeakerState) -> Option<Category> {
    if before.assignment == after.assignment && before.person_id == after.person_id {
        return None;
    }
    Some(match (before.assignment, after.assignment) {
        (Assignment::Auto, Assignment::Auto) => Category::Replaced,
        (_, Assignment::Auto) => Category::NewlyNamed,
        (Assignment::Auto, _) => Category::Demoted,
        (_, Assignment::Suggested) => Category::Suggested,
        _ => Category::Cleared,
    })
}

impl AffectedMeeting {
    fn recount(&mut self) {
        let (mut n, mut r, mut d, mut s, mut c) = (0, 0, 0, 0, 0);
        for ch in &self.changes {
            match category(&ch.before, &ch.after) {
                Some(Category::NewlyNamed) => n += 1,
                Some(Category::Replaced) => r += 1,
                Some(Category::Demoted) => d += 1,
                Some(Category::Suggested) => s += 1,
                Some(Category::Cleared) => c += 1,
                None => {}
            }
        }
        (
            self.newly_named,
            self.replaced,
            self.demoted,
            self.suggested,
            self.cleared,
        ) = (n, r, d, s, c);
    }
}

impl LearningReport {
    /// Folds one meeting's committed `changes` in: a speaker changed again
    /// keeps its first `before`; one back where it started drops out.
    pub fn record(&mut self, meeting: &MeetingId, source: bool, changes: &[SpeakerChange]) {
        if changes.is_empty() {
            return;
        }
        let i = match self.meetings.iter().position(|m| m.meeting_id == *meeting) {
            Some(i) => i,
            None => {
                self.meetings.push(AffectedMeeting {
                    meeting_id: meeting.clone(),
                    source,
                    newly_named: 0,
                    replaced: 0,
                    demoted: 0,
                    suggested: 0,
                    cleared: 0,
                    changes: Vec::new(),
                });
                self.meetings.len() - 1
            }
        };
        let entry = &mut self.meetings[i];
        entry.source |= source;
        for ch in changes {
            match entry
                .changes
                .iter_mut()
                .find(|e| e.speaker_id == ch.speaker_id)
            {
                Some(e) => e.after = ch.after.clone(),
                None => entry.changes.push(ch.clone()),
            }
        }
        entry
            .changes
            .retain(|c| category(&c.before, &c.after).is_some());
        entry.changes.sort_by_key(|c| c.speaker_id);
        entry.recount();
        if entry.changes.is_empty() {
            self.meetings.remove(i);
        }
        self.recount();
    }

    fn recount(&mut self) {
        self.newly_named = self.meetings.iter().map(|m| m.newly_named).sum();
        self.replaced = self.meetings.iter().map(|m| m.replaced).sum();
        self.demoted = self.meetings.iter().map(|m| m.demoted).sum();
        self.suggested = self.meetings.iter().map(|m| m.suggested).sum();
        self.cleared = self.meetings.iter().map(|m| m.cleared).sum();
        // Source meetings first, then by ID.
        self.meetings.sort_by(|a, b| {
            b.source
                .cmp(&a.source)
                .then(a.meeting_id.cmp(&b.meeting_id))
        });
    }

    /// Records `meeting` as unreadable with `code` (an [`Error::code`]).
    pub fn mark_unreadable(&mut self, meeting: &MeetingId, code: &str) {
        self.mark_readable(meeting);
        self.unreadable.push(UnreadableMeeting {
            meeting_id: meeting.clone(),
            code: code.to_owned(),
        });
        self.unreadable
            .sort_by(|a, b| a.meeting_id.cmp(&b.meeting_id));
    }

    /// `meeting` was read since: no longer unreadable.
    pub fn mark_readable(&mut self, meeting: &MeetingId) {
        self.unreadable.retain(|u| u.meeting_id != *meeting);
    }

    /// Short counts for progress lines and logs: never names or text.
    pub fn summary(&self) -> String {
        let mut line = format!(
            "{} meeting(s) updated: {} named, {} replaced, {} demoted, {} suggested, {} cleared; \
             {} waiting",
            self.meetings.len(),
            self.newly_named,
            self.replaced,
            self.demoted,
            self.suggested,
            self.cleared,
            self.deferred
        );
        if !self.unreadable.is_empty() {
            line.push_str(&format!("; {} unreadable", self.unreadable.len()));
        }
        line
    }
}

/// Records a reevaluation request for `model` in this write, before the
/// caller changes any evidence: the generation goes up, `source` (when
/// given) moves to the front of the source meetings, and a finished job is
/// queued again (its report starts afresh). A running job keeps running
/// and picks the request up at its next per-meeting update.
pub fn request(
    tx: &mut WriteTx<'_>,
    model: &str,
    source: Option<&MeetingId>,
) -> Result<LearningRequest> {
    if model.trim().is_empty() {
        return Err(Error::InvalidInput(
            "a reevaluation request needs an embedding model".into(),
        ));
    }
    let queue = Queue::open(tx.root())?;
    let mut generation = 0;
    queue.request(tx, REIDENTIFY_KIND, model, |job| {
        let idle = job.is_none_or(|j| matches!(j.state, JobState::Done | JobState::Failed));
        let mut input = job.and_then(ReidentifyInput::of).unwrap_or_default();
        input.model = model.to_owned();
        if idle {
            input.report = None;
        }
        input.requested += 1;
        input.requested_source = source.cloned();
        if let Some(m) = source {
            input.source_meetings.retain(|x| x != m);
            input.source_meetings.insert(0, m.clone());
        }
        generation = input.requested;
        input.to_value()
    })?;
    Ok(LearningRequest {
        job_id: job_id(model),
        generation,
        source_meeting: source.cloned(),
    })
}

/// Requests reevaluation, without a source meeting, for every model in
/// `models` and every model that already has a reevaluation job: what
/// forgetting a voice or person invalidates.
pub fn request_all(tx: &mut WriteTx<'_>, models: BTreeSet<String>) -> Result<Vec<LearningRequest>> {
    let mut models = models;
    for job in Queue::open(tx.root())?.list(REIDENTIFY_KIND)? {
        models.insert(job.key);
    }
    models
        .into_iter()
        .filter(|m| !m.trim().is_empty())
        .map(|m| request(tx, &m, None))
        .collect()
}

/// A fingerprint of every person's stored voice evidence (the bytes of each
/// `voiceprints.json` and whether the person exists), compared under the
/// lock of each reevaluation publication: any sample change (confirmation,
/// enrollment, caption promotion, weak sync, forget) changes it. People are
/// hashed in ID order and each file is streamed through one incremental
/// hasher: deterministic, and never holds a whole library's evidence in
/// memory.
pub fn evidence_fingerprint(lib: &Library) -> Result<String> {
    use sha2::{Digest, Sha256};
    let dir = lib.people_dir();
    let mut ids: Vec<String> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| crate::PersonId::parse(n).is_ok())
            .collect(),
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(Error::io_at(&dir, e)),
    };
    ids.sort();
    let mut all = Sha256::new();
    for id in ids {
        let person = dir.join(&id);
        all.update(id.as_bytes());
        all.update([u8::from(person.join(paths::PERSON_MD).is_file())]);
        match hash_file(&person.join(paths::VOICEPRINTS_JSON))? {
            Some(digest) => all.update(digest),
            None => all.update(b"-"),
        }
        all.update([0]);
    }
    Ok(crate::schema::hex(&all.finalize()))
}

/// SHA-256 of the file at `path`, streamed in fixed-size blocks; `None`
/// when it does not exist.
pub(crate) fn hash_file(path: &std::path::Path) -> Result<Option<[u8; 32]>> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io_at(path, e)),
    };
    let mut hasher = Sha256::new();
    let mut block = [0u8; 16 * 1024];
    loop {
        match file.read(&mut block) {
            Ok(0) => break,
            Ok(n) => hasher.update(&block[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::io_at(path, e)),
        }
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&hasher.finalize());
    Ok(Some(digest))
}

/// What `learning status` shows: the reevaluation job of one model.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LearningStatus {
    pub job_id: JobId,
    pub model: String,
    pub state: JobState,
    /// A live worker holds it now.
    pub running: bool,
    pub requested: u64,
    pub processed: u64,
    /// Meetings still to be reevaluated first.
    pub source_meetings: Vec<MeetingId>,
    /// Source of the latest request even after it was processed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_source: Option<MeetingId>,
    /// `done` meetings waiting for another job to finish with them.
    pub deferred: Vec<MeetingId>,
    /// When a job that only waits for deferred meetings runs again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<FixedOffset>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Retained after the job is done, until the next request after it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<LearningReport>,
}

/// The reevaluation status of `model`, read from `job.json` without taking
/// any lock or creating anything; `None` when no request was ever made.
pub fn status(lib: &Library, model: &str) -> Result<Option<LearningStatus>> {
    let id = job_id(model);
    let path = lib
        .root()
        .join(paths::QUEUE_DIR)
        .join(id.as_str())
        .join(JOB_JSON);
    let job: Job = match fsutil::read_json(&path) {
        Ok(job) => job,
        Err(Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let input = ReidentifyInput::of(&job).unwrap_or_default();
    let stale_after = TimeDelta::from_std(Queue::DEFAULT_HEARTBEAT * Queue::STALE_INTERVALS)
        .unwrap_or(TimeDelta::MAX);
    // A lease stamped in the future (clock moved back) is not fresh either.
    let running = matches!(job.state, JobState::Running | JobState::Queued)
        && job
            .lease
            .as_ref()
            .is_some_and(|l| l.is_fresh(now(), stale_after));
    Ok(Some(LearningStatus {
        job_id: id,
        model: model.to_owned(),
        state: job.state,
        running,
        requested: input.requested,
        processed: input.processed,
        source_meetings: input.source_meetings,
        requested_source: input.requested_source,
        deferred: input.deferred,
        not_before: job.not_before,
        error: job.error,
        report: input.report,
    }))
}

#[cfg(test)]
mod tests;
