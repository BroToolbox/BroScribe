//! `import --manifest`: batch import in three resumable phases (design §6.5,
//! plan §13.5).
//!
//! Every manifest is one **batch** ([`Manifest::id`]: its path and content).
//! The batch's jobs are listed in `<lib>/.queue/batch-<id>.json` (and each
//! new job's input names the batch); phases 2 and 3 and the cleanup work on
//! those jobs only, so batches never enroll from, resolve or delete each
//! other's work.
//!
//! 1. **Analyze** each meeting as its own queue job (kind [`MANIFEST_KIND`],
//!    key `source_id` or SHA-256): decode, analyze, embed the clean parts of
//!    its speaker hints, write `<job dir>/hints.json`, then publish
//!    `meeting.json` with `processing.status = analyzed` (in that order). A
//!    meeting already in the library (same SHA-256, and the same `source_id`
//!    when both have one) gets an embedding-only pass over its stored
//!    segments instead.
//! 2. **Enroll** people from the hints of the batch's jobs (this run and
//!    interrupted earlier ones), pooled per person and checked with
//!    [`identity::hint_agreement`]. Enrollment waits until every entry of the
//!    batch is analyzed: while another live process still analyzes one, the
//!    batch stops and says so.
//! 3. **Resolve** the batch's meetings (merge once on `analyzed → done`,
//!    then resolve), then remove its finished jobs, except those another
//!    unfinished batch still lists.
//!
//! People forgotten while a batch runs are fenced off using its persisted
//! start time (the people snapshot generation), including after interruption.
//! Stale work never brings a forgotten identity or voice back.
//!
//! Killing the process at any point and running the batch again gives the
//! same meetings, people and voiceprints.

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Instant;

use broscribe_core::config::SetupProgress;
use broscribe_core::identity::{self, AgreementReason, MAX_CONFIRMED};
use broscribe_core::index::Index;
use broscribe_core::lock::QueueLock;
use broscribe_core::names::validate_name;
use broscribe_core::ops::{self, HINTS_JSON};
use broscribe_core::process::ProcessProbe;
use broscribe_core::queue::{Job, JobId, JobState, LeaseHandle, MANIFEST_KIND, Queue};
use broscribe_core::schema::{
    self, Handling, LanguageSpan, Meeting, Participant, Person, Processing, ProcessingStatus,
    Source, Strength, Track, TrackRole, Voiceprint, VoiceprintKind, VoiceprintOrigin,
    VoiceprintSource, Voiceprints,
};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result, WriteTx, fsutil};
use broscribe_models::ModelSet;
use chrono::{DateTime, FixedOffset, TimeDelta};
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::analyze::{AnalyzeOptions, TRACK_ID, analyze_with_languages, segment_runs};
use crate::audio::{decode_16k, file_sha256};
use crate::finish::{file_mtime, finish_meeting, title_from_path};
use crate::hints::{HINTS_SCHEMA_VERSION, HintsFile, embed_hints, group_hints};
use crate::import::{
    check_handling, models_used, place_audio, recording, stage_audio, verify_staged_copy,
};
use crate::manifest::{
    Manifest, ManifestIssue, ManifestMeeting, ManifestParticipant, check_hint_names, fold,
    match_person, people_names, person_key,
};

#[derive(Clone)]
pub struct BatchOptions {
    /// `None`: the configured handling. `move` is not available for
    /// manifest imports.
    pub handling: Option<Handling>,
    pub encoder: std::sync::Arc<dyn broscribe_platform::AudioEncoder>,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            handling: None,
            encoder: std::sync::Arc::new(
                broscribe_platform::ffmpeg_encoder::FfmpegEncoder::locate(),
            ),
        }
    }
}

/// What a batch did; counts cover this run.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BatchSummary {
    /// Valid meetings in the manifest.
    pub meetings: usize,
    /// Manifest entries skipped because of problems.
    pub issues: Vec<ManifestIssue>,
    /// Meetings decoded and analyzed in this run.
    pub analyzed: usize,
    /// Meetings already in the library that got an embedding-only pass.
    pub existing: usize,
    /// Meetings whose analysis checkpoint was reused.
    pub reused: usize,
    /// Meetings another process was working on.
    pub busy: usize,
    pub failed: Vec<MeetingFailure>,
    /// People created from `people[]`.
    pub people_seeded: usize,
    /// People created from hints.
    pub people_created: usize,
    /// Hinted people left out because they were forgotten while the batch
    /// ran (counted per meeting).
    pub forgotten: usize,
    pub people: Vec<PersonEnrollment>,
    /// Hint voiceprints added.
    pub voiceprints_added: usize,
    /// Meetings resolved (now `done`).
    pub resolved: usize,
    /// Over-split speakers merged away.
    pub merged_speakers: usize,
    pub auto_named: usize,
    pub suggested: usize,
    /// Finished job folders removed from `.queue/`.
    pub jobs_removed: usize,
    /// Things the user should know (e.g. how to enroll their own voice).
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MeetingFailure {
    /// Index in the manifest's `meetings[]`.
    pub index: usize,
    pub source_id: Option<String>,
    pub audio_path: PathBuf,
    pub code: &'static str,
    pub message: String,
}

/// Hint enrollment of one person across the batch.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PersonEnrollment {
    pub person_id: PersonId,
    pub name: String,
    pub is_self: bool,
    /// Meetings with hint samples for this person.
    pub meetings: usize,
    pub samples: usize,
    pub kept: usize,
    pub enrolled: bool,
    pub reason: AgreementReason,
}

/// Simulated crashes for the resume tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum FailPoint {
    /// Inside the phase-1 commit, after `hints.json`, before `meeting.json`.
    AfterHints,
    AfterEntry,
    AfterPhase1,
    AfterPhase2,
    FinishBeforeIndex,
    /// Inside a single-file import's commit, right after `meeting.json`.
    ImportAfterSave,
    /// Inside a recording's final commit, after the audio moved into the
    /// meeting folder, before `meeting.json`.
    RecordAfterRename,
}

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_AT: std::cell::Cell<Option<FailPoint>> =
        const { std::cell::Cell::new(None) };
}

pub(crate) fn failpoint(p: FailPoint) -> Result<()> {
    #[cfg(test)]
    if FAIL_AT.get() == Some(p) {
        FAIL_AT.set(None);
        return Err(Error::Internal(format!("failpoint {p:?}")));
    }
    let _ = p;
    Ok(())
}

fn is_failpoint(e: &Error) -> bool {
    matches!(e, Error::Internal(m) if m.starts_with("failpoint"))
}

#[cfg(test)]
thread_local! {
    /// Test hook run between `finish_batch`'s readiness check and phase 2,
    /// to interleave another process's work there.
    pub(crate) static BEFORE_PHASE2: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Test hook run between phase 2's agreement computation (outside the
    /// library lock) and its commit, to change the evidence meanwhile.
    pub(crate) static BEFORE_ENROLL_COMMIT: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

/// Why a phase-complete batch fails a job without a published analysis.
const INTERRUPTED_ANALYSIS: &str = "the analysis was interrupted before it was published; run \
     `broscribe import --manifest <file>` again";

/// Why an interrupted phase 1 fails its analyzed entries on resume.
const STRANDED_ANALYSIS: &str = "the import of this manifest stopped before every entry was \
     analyzed; run `broscribe import --manifest <file>` again to finish it";

/// Enrollment rounds whose checkpoints changed between comparing their
/// evidence and committing it, before the batch stops for this run.
const ENROLL_ATTEMPTS: usize = 3;

/// Analysis passes of a manifest entry whose Me evidence changed before its
/// commit, the bound recordings use as well.
const ME_PASSES: usize = 3;

/// Whether `job`'s checkpoint holds another manifest entry's hints than the
/// one this batch recorded for it.
fn stale_checkpoint(fingerprints: &BTreeMap<JobId, String>, job: &JobId, h: &HintsFile) -> bool {
    fingerprints
        .get(job)
        .is_some_and(|expected| expected != &h.entry_fingerprint)
}

fn stale_note(job: &JobId) -> String {
    format!(
        "job {job}: hints changed in another manifest; rerun this manifest to regenerate its \
         checkpoint"
    )
}

/// `<lib>/.queue/batch-<id>.json`: the jobs of one manifest batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct BatchFile {
    id: String,
    created_at: DateTime<FixedOffset>,
    #[serde(default)]
    jobs: BTreeSet<JobId>,
    /// Complete manifest membership, including entries whose preflight failed.
    #[serde(default)]
    entries: BTreeMap<usize, Option<JobId>>,
    #[serde(default)]
    sealed: bool,
    #[serde(default)]
    phase1_complete: bool,
    #[serde(default)]
    failed_jobs: BTreeSet<JobId>,
    /// Shared jobs must not substitute another batch's hint evidence on resume.
    #[serde(default)]
    fingerprints: BTreeMap<JobId, String>,
    /// The process running `import --manifest` on this batch now; cleared
    /// when that run ends. Its unclaimed entries wait for it rather than
    /// being failed as interrupted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<u32>,
}

/// Whether this batch is being imported right now: its owner process is
/// alive.
fn owned(batch: &BatchFile, probe: &dyn ProcessProbe) -> bool {
    batch.owner.is_some_and(|pid| probe.is_alive(pid))
}

/// Whether batch `id` is being imported right now (see [`owned`]).
#[cfg(test)]
pub(crate) fn batch_active(queue: &Queue, id: &str, probe: &dyn ProcessProbe) -> Result<bool> {
    Ok(load_batch(queue, id)?.is_some_and(|b| owned(&b, probe)))
}

/// Whether any batch listing `job` among its members is being imported
/// right now. Its entries without a checkpoint are not interrupted; they
/// are still waiting for their turn in that import. A later batch can
/// share a job with an interrupted one, so membership decides, not the
/// batch the job was first enqueued by.
pub(crate) fn job_batch_active(
    queue: &Queue,
    job: &JobId,
    probe: &dyn ProcessProbe,
) -> Result<bool> {
    Ok(list_batches(queue)?
        .iter()
        .any(|b| b.jobs.contains(job) && owned(b, probe)))
}

/// Clears the owner of batch `id` if it is still `pid` (a later run of the
/// same manifest by another process keeps its own).
fn release_batch(env: &Env<'_>, queue: &Queue, id: &str, pid: u32) -> Result<()> {
    env.lib.write(|_| {
        let Some(mut batch) = load_batch(queue, id)? else {
            return Ok(());
        };
        if batch.owner != Some(pid) {
            return Ok(());
        }
        batch.owner = None;
        fsutil::write_json_atomic(&batch_path(queue, id), &batch)
    })
}

const BATCH_PREFIX: &str = "batch-";

fn batch_path(queue: &Queue, id: &str) -> PathBuf {
    queue.dir().join(format!("{BATCH_PREFIX}{id}.json"))
}

fn load_batch(queue: &Queue, id: &str) -> Result<Option<BatchFile>> {
    match fsutil::read_json(&batch_path(queue, id)) {
        Ok(b) => Ok(Some(b)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Every recorded (unfinished) batch, oldest first.
fn list_batches(queue: &Queue) -> Result<Vec<BatchFile>> {
    let dir = queue.dir();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let name = entry.map_err(|e| Error::io_at(dir, e))?.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|n| n.strip_prefix(BATCH_PREFIX))
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        out.extend(load_batch(queue, id)?);
    }
    out.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(out)
}

/// The library meeting a manifest entry is, decided by the library files:
/// the meeting with this entry's audio ([`EntryHash::matches`]), else none.
/// `already_exists` when the entry's `source_id` belongs to a meeting with
/// other audio, or the audio's meeting has another `source_id`.
///
/// An unreadable `meeting.json` is skipped only when its folder is proven
/// to be another meeting ([`crate::finish::suspicion`]): it holds no audio
/// of this content, and its derived `meeting.md` records complete
/// provenance naming none of the entry's hashes (recording or original
/// source fingerprint) nor its `source_id`. Otherwise — its files name the
/// entry, or an earlier version wrote its `meeting.md` without that
/// provenance so nothing rules it out — it refuses the entry
/// (`invalid_input` naming the folder) unless a readable meeting already
/// answers both its audio and its `source_id`, so a meeting that cannot be
/// read is never imported a second time.
fn canonical_meeting(
    lib: &Library,
    hash: &EntryHash,
    source_id: Option<&str>,
) -> Result<Option<Meeting>> {
    use crate::finish::{Sought, Suspicion};
    let hashes: Vec<&str> = std::iter::once(&hash.sha256)
        .chain(&hash.legacy)
        .map(String::as_str)
        .collect();
    let judge = |dir: &Path| -> Result<Option<Suspicion>> {
        if crate::finish::holds_audio(dir, &hash.sha256, hash.bytes)? {
            return Ok(Some(Suspicion::Named));
        }
        Ok(crate::finish::suspicion(
            dir,
            Sought::Entry {
                hashes: &hashes,
                source_id,
            },
        ))
    };
    let mut by_sha: Option<Meeting> = None;
    let mut by_source: Option<Meeting> = None;
    let mut suspect: Option<(PathBuf, Error, Suspicion)> = None;
    for dir in lib.list_meeting_dirs()? {
        let m = match lib.load_meeting_at(&dir) {
            Ok(m) => m,
            Err(error) => {
                if suspect.is_none()
                    && let Some(why) = judge(&dir)?
                {
                    suspect = Some((dir, error, why));
                } else {
                    tracing::warn!(code = error.code(), "skipping an unreadable meeting file");
                }
                continue;
            }
        };
        let sha_match = m.recording.as_ref().is_some_and(|r| hash.matches(r));
        let source_match = source_id.is_some() && m.source_id.as_deref() == source_id;
        if source_match && by_source.is_none() {
            by_source = Some(m.clone());
        }
        if sha_match && by_sha.is_none() {
            by_sha = Some(m);
        }
    }
    let sid = source_id.unwrap_or_default();
    let found = match (by_sha, by_source) {
        (Some(a), Some(b)) if a.id != b.id => Err(Error::AlreadyExists(format!(
            "source_id {sid:?} belongs to meeting {}, but this audio is meeting {}",
            b.id, a.id
        ))),
        (Some(a), _) => match (source_id, a.source_id.as_deref()) {
            (Some(new), Some(old)) if new != old => Err(Error::AlreadyExists(format!(
                "this audio is already meeting {} with source_id {old:?}, not {new:?}",
                a.id
            ))),
            _ => Ok(Some(a)),
        },
        (None, Some(b)) => Err(Error::AlreadyExists(format!(
            "source_id {sid:?} belongs to meeting {}, which has other audio",
            b.id
        ))),
        (None, None) => Ok(None),
    }?;
    let answered = found
        .as_ref()
        .is_some_and(|a| source_id.is_none_or(|s| a.source_id.as_deref() == Some(s)));
    match suspect {
        Some((dir, error, why)) if !answered => {
            Err(why.refusal(&dir, &error, "this entry's meeting"))
        }
        _ => Ok(found),
    }
}

/// The model of a speaker centroid that is not `model`, if any.
fn foreign_model<'m>(m: &'m Meeting, model: &str) -> Option<&'m str> {
    m.speakers
        .iter()
        .filter_map(|s| s.centroid.as_ref())
        .map(|c| c.model.as_str())
        .find(|x| *x != model)
}

struct Run<'r, 'a> {
    env: &'r Env<'a>,
    queue: Queue,
    model: String,
    handling: Handling,
    /// The batch ID ([`Manifest::id`]).
    batch: String,
    /// Persisted start of this batch's person snapshot, not of one entry.
    started_at: DateTime<FixedOffset>,
    /// People after seeding, for matching hints and participants.
    people: Vec<Person>,
    encoder: Option<&'r dyn broscribe_platform::AudioEncoder>,
    redact_decode_errors: bool,
}

/// Imports a manifest (see the module docs). Per-meeting problems are
/// reported in the summary; only library-wide errors abort.
pub fn import_manifest(
    env: &Env<'_>,
    models: &mut ModelSet,
    manifest: &Manifest,
    opts: &BatchOptions,
) -> Result<BatchSummary> {
    import_manifest_inner(env, models, manifest, opts, false)
}

/// Imports an inline MCP manifest without returning or persisting raw
/// decoder diagnostics that can contain input file details.
pub fn import_manifest_without_decode_details(
    env: &Env<'_>,
    models: &mut ModelSet,
    manifest: &Manifest,
    opts: &BatchOptions,
) -> Result<BatchSummary> {
    import_manifest_inner(env, models, manifest, opts, true)
}

fn import_manifest_inner(
    env: &Env<'_>,
    models: &mut ModelSet,
    manifest: &Manifest,
    opts: &BatchOptions,
    redact_decode_errors: bool,
) -> Result<BatchSummary> {
    let handling = check_handling(opts.handling.unwrap_or(env.config.import_handling))?;
    // A person `people[]` would create under an invalid name refuses the
    // whole manifest before anything is written.
    people_names(&manifest.people)?;
    // Entries with the user's own name or e-mail (`setup`) are the user.
    let mut marked = manifest.clone();
    marked.mark_self(&env.config.setup);
    let mut expansion_errors: BTreeMap<_, _> = marked
        .meetings
        .iter_mut()
        .filter_map(|entry| {
            entry
                .expand_tracks(env.decoders)
                .err()
                .map(|e| (entry.index, e))
        })
        .collect();
    let manifest = &marked;
    let summary = BatchSummary {
        meetings: manifest.meetings.len(),
        issues: manifest.issues.clone(),
        ..BatchSummary::default()
    };
    let model = models.embedder.model_id().to_owned();
    let queue = Queue::open(env.lib.root())?;
    // Hash and validate every input before sealing the complete membership;
    // no analysis starts until the batch record and all jobs are durable.
    // Two entries that resolve to one job key (the same recording without a
    // `source_id`) would share a job and lose all but one entry's hints: the
    // later one is reported as its own failure instead.
    let mut keys: BTreeMap<String, usize> = BTreeMap::new();
    let inputs: Vec<_> = manifest
        .meetings
        .iter()
        .map(|e| {
            if let Some(error) = expansion_errors.remove(&e.index) {
                return Err(error);
            }
            // A hint that would create a person under an invalid name fails
            // its own entry only.
            check_hint_names(e).map_err(Error::InvalidManifest)?;
            if !e.audio_path.is_file() {
                return Err(Error::NotFound(e.audio_path.display().to_string()));
            }
            let hash = entry_hash(e)?;
            canonical_meeting(env.lib, &hash, e.source_id.as_deref())?;
            let key = e.source_id.clone().unwrap_or_else(|| hash.sha256.clone());
            if let Some(first) = keys.get(&key) {
                return Err(Error::InvalidManifest(format!(
                    "entry {} is the same recording as entry {first}; give each a distinct \
                     source_id or merge them into one entry",
                    e.index
                )));
            }
            keys.insert(key, e.index);
            Ok(hash)
        })
        .collect();
    let owner = env.probe.current_pid();
    let batch = env.lib.write(|tx| {
        let mut batch = load_batch(&queue, &manifest.id)?.unwrap_or_else(|| BatchFile {
            id: manifest.id.clone(),
            created_at: schema::now(),
            jobs: BTreeSet::new(),
            entries: BTreeMap::new(),
            sealed: false,
            phase1_complete: false,
            failed_jobs: BTreeSet::new(),
            fingerprints: BTreeMap::new(),
            owner: None,
        });
        for (entry, input) in manifest.meetings.iter().zip(&inputs) {
            let job = if let Ok(hash) = input {
                let key = entry.source_id.as_deref().unwrap_or(&hash.sha256);
                Some(queue.enqueue(
                    tx,
                    MANIFEST_KIND,
                    key,
                    serde_json::json!({
                        "audio_path": entry.audio_path,
                        "sources": entry.sources,
                        "sha256": hash.sha256,
                        "source_id": entry.source_id,
                        "index": entry.index,
                        "batch": manifest.id,
                    }),
                )?)
            } else {
                None
            };
            batch.jobs.extend(job.clone());
            if let Some(job) = &job {
                batch
                    .fingerprints
                    .insert(job.clone(), entry_fingerprint(entry));
            }
            batch.entries.insert(entry.index, job);
        }
        batch.sealed = true;
        batch.phase1_complete = false;
        batch.failed_jobs.clear();
        batch.owner = Some(owner);
        fsutil::write_json_atomic(&batch_path(&queue, &manifest.id), &batch)?;
        Ok(batch)
    })?;
    // The batch is ours until this run ends, however it ends.
    let result = run_sealed(
        env,
        models,
        manifest,
        opts,
        handling,
        model,
        queue.clone(),
        &batch,
        inputs,
        summary,
        redact_decode_errors,
    );
    let released = release_batch(env, &queue, &manifest.id, owner);
    let summary = result?;
    released?;
    Ok(summary)
}

/// Phase 1 over the sealed `batch`, then enrollment and resolution.
#[allow(clippy::too_many_arguments)]
fn run_sealed(
    env: &Env<'_>,
    models: &mut ModelSet,
    manifest: &Manifest,
    opts: &BatchOptions,
    handling: Handling,
    model: String,
    queue: Queue,
    batch: &BatchFile,
    inputs: Vec<Result<EntryHash>>,
    mut summary: BatchSummary,
    redact_decode_errors: bool,
) -> Result<BatchSummary> {
    summary.people_seeded = seed_people(env, manifest, batch.created_at)?;
    let mut run = Run {
        env,
        queue,
        model,
        handling,
        batch: manifest.id.clone(),
        started_at: batch.created_at,
        encoder: Some(opts.encoder.as_ref()),
        redact_decode_errors,
        people: env.lib.list_people()?,
    };

    // Phase 1.
    let total = manifest.meetings.len();
    for (n, (entry, input)) in manifest.meetings.iter().zip(inputs).enumerate() {
        env.say(format_args!(
            "[{}/{total}] {}",
            n + 1,
            entry.audio_path.display()
        ));
        let result = match input {
            Ok(hash) => run.phase1(
                models,
                entry,
                &hash,
                batch.entries[&entry.index]
                    .as_ref()
                    .expect("preflight enqueued the job"),
                &mut summary,
            ),
            Err(e) => Err(run.decode_error(e)),
        };
        if let Err(e) = result {
            if is_failpoint(&e) {
                return Err(e);
            }
            env.say(format_args!("  failed: {e}"));
            summary.failed.push(MeetingFailure {
                index: entry.index,
                source_id: entry.source_id.clone(),
                audio_path: entry.audio_path.clone(),
                code: e.code(),
                message: e.to_string(),
            });
            if let Some(job) = &batch.entries[&entry.index] {
                run.record_failed_job(job)?;
            }
        }
        failpoint(FailPoint::AfterEntry)?;
    }
    env.lib.write(|_| {
        let mut current = load_batch(&run.queue, &run.batch)?
            .ok_or_else(|| Error::NotFound("batch record".into()))?;
        current.phase1_complete = true;
        fsutil::write_json_atomic(&batch_path(&run.queue, &run.batch), &current)
    })?;
    failpoint(FailPoint::AfterPhase1)?;
    run.finish_batch(&mut summary)?;
    Ok(summary)
}

/// Audio identity of a manifest entry ([`entry_hash`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryHash {
    /// What a new import records: the file's SHA-256, or for sources the
    /// hash of their contents and mapping, an absent track index being
    /// track 0 (the track that is decoded).
    pub sha256: String,
    /// Total size of the hashed files.
    pub bytes: u64,
    /// Hashes earlier imports recorded for the same sources, before absent
    /// and 0 track indexes were one identity: the mapping exactly as
    /// written, and with every track 0 left absent. Library meetings keep
    /// them, so a rerun must still find those meetings.
    legacy: Vec<String>,
    /// Each source file's SHA-256, in `sources` order, from the same pass
    /// as `sha256` (empty for a single file).
    files: Vec<String>,
}

impl EntryHash {
    /// Whether `recording` holds this entry's audio.
    pub fn matches(&self, recording: &schema::Recording) -> bool {
        std::iter::once(&self.sha256)
            .chain(&self.legacy)
            .any(|h| recording.sha256 == *h || recording.source_sha256.as_ref() == Some(h))
    }
}

/// Content and mapping jointly identify a multi-source meeting, not merely its
/// first file. Hints and source paths do not change audio identity; neither
/// does writing the default track 0 as `null` or as `0`.
pub fn entry_hash(entry: &ManifestMeeting) -> Result<EntryHash> {
    if entry.sources.is_empty() {
        let (sha256, bytes) = file_sha256(&entry.audio_path)?;
        return Ok(EntryHash {
            sha256,
            bytes,
            legacy: Vec::new(),
            files: Vec::new(),
        });
    }
    let mut files = Vec::with_capacity(entry.sources.len());
    let mut bytes = 0u64;
    for source in &entry.sources {
        let (sha, size) = file_sha256(&source.path)?;
        bytes = bytes.saturating_add(size);
        files.push(sha);
    }
    let hash = |track: fn(Option<usize>) -> Option<usize>| -> Result<String> {
        let identities: Vec<_> = entry
            .sources
            .iter()
            .zip(&files)
            .map(|(s, sha)| (sha, track(s.audio_track_index), s.role, s.offset_s))
            .collect();
        let encoded =
            serde_json::to_vec(&identities).map_err(|e| Error::Internal(e.to_string()))?;
        Ok(schema::sha256_hex(&encoded))
    };
    let sha256 = hash(|t| Some(t.unwrap_or(0)))?;
    let mut legacy = Vec::new();
    for old in [hash(|t| t)?, hash(|t| t.filter(|&i| i != 0))?] {
        if old != sha256 && !legacy.contains(&old) {
            legacy.push(old);
        }
    }
    Ok(EntryHash {
        sha256,
        bytes,
        legacy,
        files,
    })
}

/// Hash of the whole entry except its position in the manifest, so
/// reordering an otherwise identical manifest keeps its checkpoints.
fn entry_fingerprint(entry: &ManifestMeeting) -> String {
    let entry = ManifestMeeting {
        index: 0,
        ..entry.clone()
    };
    schema::sha256_hex(&serde_json::to_vec(&entry).expect("manifest entry is serializable"))
}

fn import_languages(env: &Env<'_>, entry: &ManifestMeeting, duration_s: f64) -> Vec<LanguageSpan> {
    entry
        .language
        .as_ref()
        .or(env.config.default_language.as_ref())
        .map(|lang| LanguageSpan {
            lang: lang.clone(),
            from_s: 0.0,
            to_s: duration_s,
        })
        .into_iter()
        .collect()
}

fn embed_source_hints(
    entry: &ManifestMeeting,
    audio: &[Vec<f32>],
    turns: &[crate::analyze::Turn],
    speakers: &[schema::Speaker],
    known: &[Person],
    models: &ModelSet,
) -> Result<(
    Vec<crate::hints::HintPerson>,
    Vec<crate::hints::HintSpeaker>,
)> {
    let mut people = Vec::new();
    let mut labelled = Vec::new();
    for (i, (source, samples)) in entry.sources.iter().zip(audio).enumerate() {
        let track = format!("t{i}");
        let track_speakers: Vec<_> = speakers
            .iter()
            .filter(|s| s.track == track)
            .cloned()
            .collect();
        let local_turns: Vec<_> = turns
            .iter()
            .filter(|t| track_speakers.iter().any(|s| s.id == t.speaker))
            .map(|t| crate::analyze::Turn {
                start: t.start - source.offset_s,
                end: t.end - source.offset_s,
                speaker: t.speaker,
            })
            .collect();
        let mut hints = entry.hints.clone();
        for hint in &mut hints {
            for range in &mut hint.ranges {
                range.0 -= source.offset_s;
                range.1 -= source.offset_s;
            }
        }
        if let Some(hint) = &source.person_hint {
            hints.push(crate::manifest::SpeakerHint {
                name: hint.name.clone(),
                email: hint.email.clone(),
                is_self: hint.is_self,
                ranges: vec![(
                    (0.0f64).max(-source.offset_s),
                    samples.len() as f64 / 16000.0,
                )],
            });
        }
        let groups = group_hints(&hints, known);
        let (mut p, s) = embed_hints(
            samples,
            &local_turns,
            &track_speakers,
            &groups,
            models.embedder.as_ref(),
        )?;
        for person in &mut p {
            for sample in &mut person.samples {
                sample.start += source.offset_s;
                sample.end += source.offset_s;
            }
        }
        people.extend(p);
        labelled.extend(s);
    }
    Ok((people, labelled))
}

/// Finishes every batch an earlier `import --manifest` left unfinished:
/// enrollment and resolution over each batch's own jobs (no inference). A
/// batch with a meeting another live process is still analyzing is left
/// for later, with a note.
pub fn resume_batches(env: &Env<'_>, models: &ModelSet) -> Result<BatchSummary> {
    let queue = Queue::open(env.lib.root())?;
    let mut summary = BatchSummary::default();
    for batch in list_batches(&queue)? {
        let mut run = Run {
            env,
            queue: queue.clone(),
            model: models.embedder.model_id().to_owned(),
            handling: env.config.import_handling,
            batch: batch.id,
            started_at: batch.created_at,
            people: Vec::new(),
            encoder: None,
            redact_decode_errors: false,
        };
        run.finish_batch(&mut summary)?;
    }
    Ok(summary)
}

/// Creates the `people[]` entries that match nobody; returns how many.
fn seed_people(
    env: &Env<'_>,
    manifest: &Manifest,
    started_at: DateTime<FixedOffset>,
) -> Result<usize> {
    if manifest.people.is_empty() {
        return Ok(0);
    }
    // Every name is checked before the first person is written.
    let names = people_names(&manifest.people)?;
    ops::with_index(env.lib, |tx, index| {
        let mut people = tx.library().list_people()?;
        let mut created = 0;
        for (mp, name) in manifest.people.iter().zip(&names) {
            let mut forgotten = false;
            for key in mp.name.iter().chain(mp.email.iter()) {
                if ops::forgotten_since(tx.library(), key)?.is_some_and(|at| at >= started_at) {
                    forgotten = true;
                    break;
                }
            }
            if forgotten {
                continue;
            }
            if match_person(&people, mp.name.as_deref(), mp.email.as_deref()).is_some() {
                continue;
            }
            let mut p = Person::new(tx.library().new_person_id(), *name);
            p.emails.extend(mp.email.clone());
            tx.save_person(&p)?;
            index.upsert_person(tx, &p)?;
            people.push(p);
            created += 1;
        }
        Ok(created)
    })
}

impl Run<'_, '_> {
    fn decode_error(&self, error: Error) -> Error {
        if self.redact_decode_errors {
            crate::import::redact_decode_error(error)
        } else {
            error
        }
    }

    /// A shared analysis can have been resolved by an earlier batch. A new
    /// batch must resolve it again after enrolling that batch's evidence.
    fn reopen_done(&self, id: &JobId) -> Result<()> {
        self.env.lib.write(|tx| {
            let job = self.queue.get(id)?;
            if job.state == JobState::Done && !self.held(&job) {
                self.queue.set_state(tx, id, JobState::Analyzed, None)?;
            }
            Ok(())
        })
    }

    fn phase1(
        &self,
        models: &mut ModelSet,
        entry: &ManifestMeeting,
        hash: &EntryHash,
        job: &JobId,
        summary: &mut BatchSummary,
    ) -> Result<()> {
        let lib = self.env.lib;
        let path = &entry.audio_path;
        if !path.is_file() {
            return Err(Error::NotFound(path.display().to_string()));
        }
        let existing = canonical_meeting(lib, hash, entry.source_id.as_deref())?;
        if let Some(m) = &existing
            && self.checkpoint_ok(job, m, &hash.sha256, entry)
        {
            self.env.say("  reusing the earlier analysis");
            summary.reused += 1;
            return Ok(());
        }
        self.reopen_done(job)?;
        let Some(lease) = self.queue.claim(lib, job, self.env.probe)? else {
            self.env.say("  another process is working on it");
            summary.busy += 1;
            return Ok(());
        };
        let result = match &existing {
            Some(m) => self.embed_existing(models, &lease, entry, hash, m),
            None => self.analyze_new(models, &lease, entry, hash),
        };
        match result {
            Ok(forgotten) => {
                lease.release(lib)?;
                match existing {
                    Some(_) => summary.existing += 1,
                    None => summary.analyzed += 1,
                }
                summary.forgotten += forgotten;
                Ok(())
            }
            Err(e) => {
                let e = self.decode_error(e);
                if !is_failpoint(&e) {
                    let _ = lease.fail(lib, e.to_string());
                }
                let _ = lease.release(lib);
                Err(e)
            }
        }
    }

    /// The checkpoint is reused only when the meeting is analyzed or done and
    /// the job's `hints.json` matches this entry and meeting.
    fn checkpoint_ok(&self, job: &JobId, m: &Meeting, sha: &str, entry: &ManifestMeeting) -> bool {
        if !matches!(
            m.processing.status,
            ProcessingStatus::Analyzed | ProcessingStatus::Done
        ) {
            return false;
        }
        let Ok(h) = HintsFile::read(&self.queue.job_dir(job).join(HINTS_JSON)) else {
            return false;
        };
        h.meeting_id == m.id
            && h.sha256 == sha
            && h.source_id == entry.source_id
            && h.entry_fingerprint == entry_fingerprint(entry)
            && h.model == self.model
    }

    fn participants(&self, entry: &ManifestMeeting) -> Vec<Participant> {
        let me = self.people.iter().find(|p| p.is_self);
        entry
            .participants
            .iter()
            .map(|p: &ManifestParticipant| {
                let matched = match_person(&self.people, p.name.as_deref(), p.email.as_deref());
                let is_self = p.is_self || matched.is_some_and(|m| m.is_self);
                let person_id = if is_self {
                    me.map(|m| m.id.clone())
                } else {
                    matched.map(|m| m.id.clone())
                };
                Participant {
                    name: p.name.clone(),
                    email: p.email.clone(),
                    is_self,
                    evidence: Vec::new(),
                    person_id,
                }
            })
            .collect()
    }

    /// Returns how many hinted people were dropped as forgotten meanwhile.
    /// A mic + system pair analyzed while Me's evidence changed (a forget)
    /// publishes nothing and is analyzed again, at most [`ME_PASSES`] times.
    fn analyze_new(
        &self,
        models: &mut ModelSet,
        lease: &LeaseHandle,
        entry: &ManifestMeeting,
        hash: &EntryHash,
    ) -> Result<usize> {
        for pass in 1..=ME_PASSES {
            if let Some(forgotten) = self.analyze_new_once(models, lease, entry, hash)? {
                return Ok(forgotten);
            }
            if pass < ME_PASSES {
                self.env
                    .say("  your voice changed during the analysis; analyzing again");
            }
        }
        Err(Error::LibraryBusy(format!(
            "{}: your voice kept changing during the analysis; run the batch again",
            entry.audio_path.display()
        )))
    }

    /// One analysis pass of [`Self::analyze_new`]: `None`, publishing
    /// nothing, when Me's evidence changed before the commit.
    fn analyze_new_once(
        &self,
        models: &mut ModelSet,
        lease: &LeaseHandle,
        entry: &ManifestMeeting,
        hash: &EntryHash,
    ) -> Result<Option<usize>> {
        let env = self.env;
        let (sha, bytes) = (hash.sha256.as_str(), hash.bytes);
        let path = &entry.audio_path;
        let started = Instant::now();
        // Snapshot before any inference; the commit fences on it.
        let me =
            crate::multitrack::me_evidence(env, &entry.sources, models.embedder.model_id(), &[])?;
        let mut prior = Meeting::new(env.lib.new_meeting_id()?, Source::Import, schema::now());
        prior.participants = self.participants(entry);
        prior.hotwords = env.config.hotwords.clone();
        if models.asr.accepts_hotwords() {
            models.asr.set_hotwords(&crate::reprocess::hotwords(
                env.lib,
                &env.config.hotwords,
                &prior,
            )?)?;
        }
        let (analysis, people, speakers, created_at, source_audio) = if entry.sources.is_empty() {
            let decoded = decode_16k(env.decoders, path)?;
            let languages = import_languages(env, entry, decoded.duration_s());
            let analysis = analyze_with_languages(
                &decoded.samples,
                models,
                AnalyzeOptions::default(),
                &languages,
            )?;
            let groups = group_hints(&entry.hints, &self.people);
            let (people, speakers) = embed_hints(
                &decoded.samples,
                &analysis.diar_segments,
                &analysis.speakers,
                &groups,
                models.embedder.as_ref(),
            )?;
            (analysis, people, speakers, decoded.info.created_at, None)
        } else {
            let language = entry
                .language
                .as_ref()
                .or(env.config.default_language.as_ref());
            let languages: Vec<_> = language
                .map(|lang| LanguageSpan {
                    lang: lang.clone(),
                    from_s: 0.0,
                    to_s: f64::MAX,
                })
                .into_iter()
                .collect();
            let (analysis, audio) = crate::multitrack::analyze_sources_with_audio(
                env,
                &entry.sources,
                models,
                &languages,
                me.as_ref(),
            )?;
            let (people, speakers) = embed_source_hints(
                entry,
                &audio,
                &analysis.diar_segments,
                &analysis.speakers,
                &self.people,
                models,
            )?;
            let created_at = env.decoders.probe(path).ok().and_then(|i| i.created_at);
            (analysis, people, speakers, created_at, Some(audio))
        };

        let started_at = entry
            .started_at
            .or(created_at)
            .or_else(|| file_mtime(path))
            .unwrap_or_else(schema::now);
        let mut meeting = Meeting::new(env.lib.new_meeting_id()?, Source::Import, started_at);
        meeting.source_id = entry.source_id.clone();
        meeting.title = entry.title.clone().or_else(|| title_from_path(path));
        if entry.title.is_some() {
            meeting.title_source = Some(schema::TitleSource::User);
        }
        meeting.duration_s = analysis.duration_s;
        if let Some(lang) = entry
            .language
            .clone()
            .or_else(|| env.config.default_language.clone())
        {
            meeting.languages.push(LanguageSpan {
                lang,
                from_s: 0.0,
                to_s: analysis.duration_s,
            });
        }
        meeting.participants = self.participants(entry);
        meeting.hotwords = env.config.hotwords.clone();
        meeting.tracks = if entry.sources.is_empty() {
            vec![Track {
                id: TRACK_ID.to_owned(),
                role: TrackRole::Mixed,
            }]
        } else {
            entry
                .sources
                .iter()
                .enumerate()
                .map(|(i, s)| Track {
                    id: format!("t{i}"),
                    role: s.role,
                })
                .collect()
        };
        meeting.speakers = analysis.speakers;
        meeting.segments = analysis.segments;
        meeting.models = models_used(env, &self.model, true);
        meeting.processing = Processing {
            status: ProcessingStatus::Analyzed,
            duration_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
            error: None,
            echo_report: None,
        };
        let mut ext = crate::finish::audio_ext(path);
        let staged =
            if let Some(audio) = source_audio.filter(|_| self.handling != Handling::Reference) {
                let encoder = self
                    .encoder
                    .ok_or_else(|| Error::Internal("analysis needs an encoder".into()))?;
                ext = encoder.extension().to_owned();
                // This attempt's own file: no other holder writes or removes it.
                let encoded = crate::import::staged_audio_path(lease, &ext);
                if let Err(e) = crate::multitrack::encode_sources(
                    env.decoders,
                    &entry.sources,
                    &audio,
                    encoder,
                    &encoded,
                ) {
                    discard_staged(&encoded);
                    return Err(e);
                }
                Some(encoded)
            } else if entry.sources.is_empty() {
                let staged = stage_audio(lease, path, self.handling, &ext)?;
                // The file may have changed since the batch hashed it: the
                // copy must hold exactly what `sha` records and was analysed.
                if let Some(copy) = &staged {
                    verify_staged_copy(copy, path, sha, bytes)?;
                }
                staged
            } else {
                None
            };
        let mut me_changed = false;
        let published = (|| -> Result<usize> {
            meeting.recording = Some(recording(path, sha, bytes, self.handling, &ext));
            if !entry.sources.is_empty() {
                // The sources may have changed since the batch hashed them,
                // before or after they were decoded: hash them again now and
                // publish only when they still hold what `sha` records. Every
                // source hash recorded comes from this one pass. (A change
                // undone before this pass goes unseen.)
                let current = entry_hash(entry)?;
                if current.sha256 != sha {
                    return Err(Error::Internal(format!(
                        "the sources of {} do not match what was analysed; import it again",
                        path.display()
                    )));
                }
                let rec = meeting.recording.as_mut().expect("recording set");
                rec.source_sha256 = Some(sha.to_owned());
                if self.handling == Handling::Reference {
                    rec.sources = entry
                        .sources
                        .iter()
                        .zip(current.files)
                        .map(|(s, sha256)| schema::RecordingSource {
                            path: s.path.clone(),
                            track_index: s.audio_track_index.unwrap_or(0),
                            offset_s: s.offset_s,
                            sha256,
                        })
                        .collect();
                } else if let Some(path) = &staged {
                    (rec.sha256, rec.bytes) = file_sha256(path)?;
                }
            }
            let mut hints = HintsFile {
                schema_version: HINTS_SCHEMA_VERSION,
                meeting_id: meeting.id.clone(),
                sha256: sha.to_owned(),
                source_id: entry.source_id.clone(),
                entry_fingerprint: entry_fingerprint(entry),
                model: self.model.clone(),
                analysis_started_at: Some(self.started_at),
                people,
                speakers,
            };
            lease.commit(env.lib, JobState::Analyzed, |tx, dir| {
                // A forget since the snapshot: this analysis may name or
                // verify Me with the forgotten voice; publish none of it.
                if let Some(me) = &me
                    && !me.still_current(tx.library())?
                {
                    me_changed = true;
                    return Err(Error::LibraryBusy(
                        "your voice changed during the analysis".into(),
                    ));
                }
                if let Some(m) = canonical_meeting(tx.library(), hash, entry.source_id.as_deref())?
                {
                    return Err(Error::AlreadyExists(format!(
                        "{} was imported meanwhile as {}; run the batch again",
                        path.display(),
                        m.id
                    )));
                }
                let forgotten = hints.drop_forgotten(tx.library())?;
                // Checkpoint order: hints first, then the analyzed meeting.
                hints.write(&dir.join(HINTS_JSON))?;
                failpoint(FailPoint::AfterHints)?;
                let mdir = tx.create_meeting_dir(&meeting)?;
                place_audio(staged.as_deref(), &mdir, &ext)?;
                tx.save_meeting(&meeting)?;
                Index::open(tx.root())?.upsert_meeting(tx, &meeting)?;
                Ok(forgotten)
            })
        })();
        if published.is_err()
            && let Some(staged) = &staged
        {
            discard_staged(staged);
        }
        if me_changed {
            return Ok(None);
        }
        published.map(Some)
    }

    /// A meeting already in the library: hint embeddings from its stored
    /// segments (no ASR, no diarization), and the manifest's metadata merged
    /// in where the existing values were derived from the file. Returns how
    /// many hinted people were dropped as forgotten meanwhile.
    fn embed_existing(
        &self,
        models: &mut ModelSet,
        lease: &LeaseHandle,
        entry: &ManifestMeeting,
        hash: &EntryHash,
        existing: &Meeting,
    ) -> Result<usize> {
        let env = self.env;
        let sha = hash.sha256.as_str();
        let path = &entry.audio_path;
        self.env
            .say(format_args!("  already in the library as {}", existing.id));
        let (people, speakers, created_at) = if entry.sources.is_empty() {
            let decoded = decode_16k(env.decoders, path)?;
            let groups = group_hints(&entry.hints, &self.people);
            let (people, speakers) = embed_hints(
                &decoded.samples,
                &segment_runs(existing),
                &existing.speakers,
                &groups,
                models.embedder.as_ref(),
            )?;
            (people, speakers, decoded.info.created_at)
        } else {
            let audio = entry
                .sources
                .iter()
                .map(|s| crate::multitrack::decode_source(env.decoders, s))
                .collect::<Result<Vec<_>>>()?;
            let (people, speakers) = embed_source_hints(
                entry,
                &audio,
                &segment_runs(existing),
                &existing.speakers,
                &self.people,
                models,
            )?;
            (
                people,
                speakers,
                env.decoders.probe(path).ok().and_then(|i| i.created_at),
            )
        };
        let original = existing
            .recording
            .as_ref()
            .and_then(|r| r.original_path.clone());
        let mut derived_titles: Vec<String> = title_from_path(path).into_iter().collect();
        derived_titles.extend(original.as_deref().and_then(title_from_path));
        let derived_times: Vec<DateTime<FixedOffset>> = [
            created_at,
            file_mtime(path),
            original.as_deref().and_then(file_mtime),
        ]
        .into_iter()
        .flatten()
        .collect();
        let participants = self.participants(entry);
        let mut hints = HintsFile {
            schema_version: HINTS_SCHEMA_VERSION,
            meeting_id: existing.id.clone(),
            sha256: sha.to_owned(),
            source_id: entry.source_id.clone(),
            entry_fingerprint: entry_fingerprint(entry),
            model: self.model.clone(),
            analysis_started_at: Some(self.started_at),
            people,
            speakers,
        };
        lease.commit(env.lib, JobState::Analyzed, |tx, dir| {
            match canonical_meeting(tx.library(), hash, entry.source_id.as_deref())? {
                Some(m) if m.id == existing.id => {}
                _ => {
                    return Err(Error::AlreadyExists(format!(
                        "{}: meeting {} changed meanwhile; run the batch again",
                        path.display(),
                        existing.id
                    )));
                }
            }
            let forgotten = hints.drop_forgotten(tx.library())?;
            hints.write(&dir.join(HINTS_JSON))?;
            let m = tx.update_meeting(&existing.id, |m| {
                merge_metadata(m, entry, participants, &derived_titles, &derived_times);
                Ok(())
            })?;
            Index::open(tx.root())?.upsert_meeting(tx, &m)?;
            Ok(forgotten)
        })
    }

    /// Whether a live process holds the job's lease.
    fn held(&self, job: &Job) -> bool {
        self.queue.is_held(job, self.env.probe)
    }

    /// Phases 2 and 3 over this batch's jobs, once every one of them is
    /// analyzed. Jobs whose analysis failed are left out (phase 1 reported
    /// them).
    fn finish_batch(&mut self, summary: &mut BatchSummary) -> Result<()> {
        let Some(batch) = load_batch(&self.queue, &self.batch)? else {
            return Ok(());
        };
        if !batch.sealed || !batch.phase1_complete {
            summary.notes.push(
                "batch membership or phase 1 is incomplete; rerun the manifest before enrollment"
                    .into(),
            );
            if !owned(&batch, self.env.probe) {
                self.fail_stranded(&batch, summary)?;
            }
            return Ok(());
        }
        let mut ready = Vec::new();
        let mut analyzing = 0;
        let mut stale = 0;
        let mut interrupted = Vec::new();
        for id in &batch.jobs {
            let job = match self.queue.get(id) {
                Ok(j) => j,
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            if self.held(&job) {
                analyzing += 1;
            } else if batch.failed_jobs.contains(id) {
                continue;
            } else {
                match HintsFile::read(&self.queue.job_dir(id).join(HINTS_JSON)) {
                    Ok(hints) if stale_checkpoint(&batch.fingerprints, id, &hints) => {
                        summary.notes.push(stale_note(id));
                        stale += 1;
                    }
                    Ok(hints) if self.env.lib.meeting_exists(&hints.meeting_id)? => ready.push(job),
                    // No published analysis: only an import still running
                    // elsewhere that lists the job can publish it.
                    Ok(_) | Err(Error::NotFound(_)) => {
                        if self.importing_elsewhere(id)? {
                            analyzing += 1;
                        } else {
                            interrupted.push(job);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        for job in &interrupted {
            if !self.fail_interrupted(job, summary)? {
                analyzing += 1;
            }
        }
        if analyzing > 0 {
            summary.busy = summary.busy.max(analyzing);
            summary.notes.push(format!(
                "{analyzing} meeting(s) of this batch are still being analyzed by another \
                 process; enrollment waits for every meeting of the batch: run it again later"
            ));
            return Ok(());
        }
        if stale > 0 {
            return Ok(());
        }
        self.env.say("enrolling people from hints");
        self.people = self.env.lib.list_people()?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_PHASE2.take() {
            hook();
        }
        if !self.phase2(&ready, &batch.fingerprints, summary)? {
            return Ok(());
        }
        failpoint(FailPoint::AfterPhase2)?;
        self.env.say("resolving speakers");
        self.phase3(&ready, summary)?;
        self.env.say(format_args!(
            "done: {} resolved, {} named automatically, {} suggested",
            summary.resolved, summary.auto_named, summary.suggested
        ));
        Ok(())
    }

    /// Whether an import running in another process lists `id`: its phase 1
    /// may still publish the job's analysis.
    fn importing_elsewhere(&self, id: &JobId) -> Result<bool> {
        let me = self.env.probe.current_pid();
        Ok(list_batches(&self.queue)?
            .iter()
            .any(|b| b.jobs.contains(id) && b.owner != Some(me) && owned(b, self.env.probe)))
    }

    /// Fails a job of this phase-complete batch whose analysis was never
    /// published and that no running import will publish (interrupted
    /// between its checkpoint and its meeting, or its meeting deleted since):
    /// it is reported and left out like a phase-1 failure, so the batch
    /// finishes without it instead of waiting for it forever. A job already
    /// `failed` (the worker fails interrupted entries) counts no second
    /// attempt. Returns `false` when another process holds the job now.
    fn fail_interrupted(&self, job: &Job, summary: &mut BatchSummary) -> Result<bool> {
        let lib = self.env.lib;
        let error = Error::InvalidInput(INTERRUPTED_ANALYSIS.into());
        if job.state != JobState::Failed {
            if let Some(lease) = self.queue.claim(lib, &job.id, self.env.probe)? {
                let failed = lease.fail(lib, error.to_string());
                let released = lease.release(lib);
                failed?;
                released?;
            } else if self.held(&self.queue.get(&job.id)?) {
                return Ok(false);
            }
            // Otherwise not claimable (no attempts left): nothing to count.
        }
        summary.failed.push(job_failure(job, &error));
        self.record_failed_job(&job.id)?;
        Ok(true)
    }

    /// An import interrupted before its phase 1 ended leaves analyzed
    /// entries that no resume can enroll: only running the manifest again
    /// finishes the batch. Each checkpointed job of it fails, counting an
    /// attempt, unless a batch that can still finish it lists it too, so the
    /// worker stops retrying it after
    /// [`MAX_ATTEMPTS`](broscribe_core::queue::MAX_ATTEMPTS) runs instead of
    /// waiting forever. Running the manifest again restarts the job and
    /// reuses its checkpoint.
    fn fail_stranded(&self, batch: &BatchFile, summary: &mut BatchSummary) -> Result<()> {
        let lib = self.env.lib;
        let batches = list_batches(&self.queue)?;
        for id in &batch.jobs {
            if !self.queue.job_dir(id).join(HINTS_JSON).is_file() {
                continue;
            }
            let finishable = batches.iter().any(|b| {
                b.id != batch.id
                    && b.jobs.contains(id)
                    && ((b.sealed && b.phase1_complete) || owned(b, self.env.probe))
            });
            if finishable {
                continue;
            }
            let job = match self.queue.get(id) {
                Ok(j) => j,
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            if job.state == JobState::Done || self.held(&job) {
                continue;
            }
            let Some(lease) = self.queue.claim(lib, id, self.env.probe)? else {
                continue;
            };
            let error = Error::InvalidInput(STRANDED_ANALYSIS.into());
            let failed = lease.fail(lib, error.to_string());
            let released = lease.release(lib);
            failed?;
            released?;
            summary.failed.push(job_failure(&job, &error));
        }
        Ok(())
    }

    /// Records `id` among this batch's failed jobs: left out of enrollment
    /// and removed with the finished batch.
    fn record_failed_job(&self, id: &JobId) -> Result<()> {
        self.env.lib.write(|_| {
            let mut batch = load_batch(&self.queue, &self.batch)?
                .ok_or_else(|| Error::NotFound("batch record".into()))?;
            batch.failed_jobs.insert(id.clone());
            fsutil::write_json_atomic(&batch_path(&self.queue, &self.batch), &batch)
        })
    }

    /// Enrolls people from the hints of the batch's jobs. Returns `false`,
    /// enrolling nothing, when another manifest replaced one of the batch's
    /// checkpoints since `finish_batch` checked it, or when the evidence kept
    /// changing while it was compared.
    ///
    /// Person IDs are given under the library lock (linear in the hints);
    /// the N×N agreement of every person's pooled samples is computed outside
    /// it; the voiceprints are committed under the lock again only while
    /// every checkpoint still holds the evidence that was compared, so a
    /// concurrent forget or replaced checkpoint never enrolls stale evidence.
    fn phase2(
        &self,
        jobs: &[Job],
        fingerprints: &BTreeMap<JobId, String>,
        summary: &mut BatchSummary,
    ) -> Result<bool> {
        let model = self.model.clone();
        let (mut created, mut forgotten) = (0, 0);
        let mut committed = None;
        for _ in 0..ENROLL_ATTEMPTS {
            let resolved = match self.resolve_hint_people(jobs, fingerprints)? {
                Ok(resolved) => resolved,
                Err(stale) => {
                    summary.people_created += created;
                    summary.notes.extend(stale.iter().map(stale_note));
                    return Ok(false);
                }
            };
            created += resolved.created;
            forgotten += resolved.forgotten;
            let plans = enrollment_plans(resolved.snapshot.values().flatten(), &model)?;
            #[cfg(test)]
            if let Some(hook) = BEFORE_ENROLL_COMMIT.take() {
                hook();
            }
            if let Some(done) = self.commit_enrollment(jobs, &resolved.snapshot, &plans)? {
                committed = Some((done, resolved.other_model));
                break;
            }
        }
        summary.people_created += created;
        let Some(((enrollments, added), other_model)) = committed else {
            summary.notes.push(
                "the hint evidence of this batch kept changing while it was compared; nothing \
                 was enrolled: run the batch again"
                    .into(),
            );
            return Ok(false);
        };
        summary.voiceprints_added += added;
        summary.forgotten += forgotten;
        if summary.forgotten > 0 {
            summary.notes.push(format!(
                "{} hinted people were forgotten while this batch ran; their hints were not used \
                 (a new manifest run may enroll them again)",
                summary.forgotten
            ));
        }
        if other_model > 0 {
            summary.notes.push(format!(
                "{other_model} meeting(s) of this batch were analyzed with another embedding \
                 model; their hints were not used: run the batch again to embed them with {model}"
            ));
        }
        for e in &enrollments {
            if e.enrolled {
                continue;
            }
            let who = if e.is_self {
                "Your voice"
            } else {
                e.name.as_str()
            };
            let why = match e.reason {
                AgreementReason::InsufficientEvidence => {
                    "no other hinted voices to compare with (insufficient_evidence)"
                }
                AgreementReason::TooFewSamples => "fewer than 2 clean hinted pieces",
                AgreementReason::NoMajority => "the hinted pieces disagree",
                AgreementReason::NotDistinct => "the hinted pieces are not distinct from others",
                AgreementReason::Agreed => continue,
            };
            let how = if e.is_self {
                "enroll it with `broscribe setup --enroll-me <meeting> --range a-b`".to_owned()
            } else {
                "name their speaker with `broscribe speakers name`".to_owned()
            };
            summary
                .notes
                .push(format!("{who} was not enrolled: {why}; {how}"));
        }
        summary.people.extend(enrollments);
        Ok(true)
    }

    /// A job's checkpoint as enrollment compares it: fenced at this batch's
    /// person snapshot, without the people forgotten since, with how many
    /// were dropped. Another model's checkpoint is left as read (its hints
    /// are not used); `None` without a checkpoint.
    fn read_checkpoint(
        &self,
        lib: &Library,
        path: &std::path::Path,
    ) -> Result<Option<(HintsFile, usize)>> {
        let mut file = match HintsFile::read(path) {
            Ok(f) => f,
            Err(Error::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        if file.model != self.model {
            return Ok(Some((file, 0)));
        }
        // A shared job can have been analyzed for a newer batch; this batch
        // must still fence against its own generation.
        file.analysis_started_at = Some(self.started_at);
        let dropped = file.drop_forgotten(lib)?;
        Ok(Some((file, dropped)))
    }

    /// Phase 2 under the library lock: gives each hinted person of the
    /// batch's checkpoints a person ID (creating or promoting people) and
    /// saves the checkpoints that changed. `Err` lists the checkpoints
    /// another manifest replaced; nothing is changed then.
    fn resolve_hint_people(
        &self,
        jobs: &[Job],
        fingerprints: &BTreeMap<JobId, String>,
    ) -> Result<std::result::Result<ResolvedHints, Vec<JobId>>> {
        let setup = &self.env.config.setup;
        ops::with_index(self.env.lib, |tx, index| {
            let lib = tx.library().clone();
            let mut read = Vec::with_capacity(jobs.len());
            for job in jobs {
                let path = self.queue.job_dir(&job.id).join(HINTS_JSON);
                let checkpoint = self.read_checkpoint(&lib, &path)?;
                read.push((&job.id, path, checkpoint));
            }
            let stale: Vec<JobId> = read
                .iter()
                .filter(|(id, _, f)| {
                    f.as_ref()
                        .is_some_and(|(f, _)| stale_checkpoint(fingerprints, id, f))
                })
                .map(|(id, _, _)| (*id).clone())
                .collect();
            if !stale.is_empty() {
                return Ok(Err(stale));
            }
            // Every person this may create has a valid name: checked before
            // the first write, so nothing is created when one has not.
            for (_, _, checkpoint) in &read {
                let Some((file, _)) = checkpoint else {
                    continue;
                };
                if file.model != self.model {
                    continue;
                }
                for hp in &file.people {
                    validate_name(new_hint_person_name(
                        setup,
                        hp.is_self,
                        hp.name.as_deref(),
                        hp.email.as_deref(),
                    ))?;
                }
            }
            let mut people = lib.list_people()?;
            let mut resolved = ResolvedHints {
                snapshot: BTreeMap::new(),
                created: 0,
                forgotten: 0,
                other_model: 0,
            };
            for (id, path, checkpoint) in read {
                let Some((mut file, dropped)) = checkpoint else {
                    resolved.snapshot.insert(id.clone(), None);
                    continue;
                };
                if file.model != self.model {
                    resolved.other_model += 1;
                    resolved.snapshot.insert(id.clone(), Some(file));
                    continue;
                }
                resolved.forgotten += dropped;
                let mut changed = dropped > 0;
                for hp in &mut file.people {
                    let (pid, new) = resolve_hint_person(tx, index, &mut people, setup, hp)?;
                    resolved.created += usize::from(new);
                    if hp.person_id.as_ref() != Some(&pid) {
                        hp.person_id = Some(pid);
                        changed = true;
                    }
                }
                let keys: BTreeMap<String, Option<PersonId>> = file
                    .people
                    .iter()
                    .map(|p| (p.person_key.clone(), p.person_id.clone()))
                    .collect();
                for sp in &mut file.speakers {
                    let pid = keys.get(&sp.person_key).cloned().flatten();
                    if sp.person_id != pid {
                        sp.person_id = pid;
                        changed = true;
                    }
                }
                if changed {
                    file.write(&path)?;
                }
                resolved.snapshot.insert(id.clone(), Some(file));
            }
            Ok(Ok(resolved))
        })
    }

    /// Phase 2's commit under the library lock: the hint voiceprints of the
    /// agreed people, only while every checkpoint still holds the evidence
    /// the plans compared (`None` otherwise, changing nothing). User
    /// confirmations are never touched; existing people only gain samples.
    fn commit_enrollment(
        &self,
        jobs: &[Job],
        snapshot: &HintSnapshot,
        plans: &[EnrollmentPlan],
    ) -> Result<Option<(Vec<PersonEnrollment>, usize)>> {
        let model = &self.model;
        self.env.lib.write(|tx| {
            let lib = tx.library().clone();
            for job in jobs {
                let path = self.queue.job_dir(&job.id).join(HINTS_JSON);
                let current = self.read_checkpoint(&lib, &path)?.map(|(f, _)| f);
                if snapshot.get(&job.id) != Some(&current) {
                    return Ok(None);
                }
            }
            let people = lib.list_people()?;
            let mut enrollments = Vec::with_capacity(plans.len());
            let mut added = 0;
            for plan in plans {
                let (pid, samples, agreement) = (&plan.person, &plan.samples, &plan.agreement);
                let person = people.iter().find(|p| &p.id == pid);
                let meetings: BTreeSet<&MeetingId> = samples.iter().map(|s| &s.meeting).collect();
                if agreement.enrolled && person.is_some() {
                    let mut vps = lib.load_voiceprints(pid)?;
                    let known: BTreeSet<String> =
                        vps.embeddings.iter().map(|e| e.id.clone()).collect();
                    let new: Vec<Voiceprint> = agreement
                        .kept
                        .iter()
                        .map(|&i| hint_voiceprint(pid, model, &samples[i]))
                        .filter(|v| !known.contains(&v.id))
                        .collect();
                    if !new.is_empty() {
                        added += new.len();
                        vps.upsert(new);
                        cap_confirmed(&mut vps, model)?;
                        tx.save_voiceprints(&vps)?;
                    }
                }
                enrollments.push(PersonEnrollment {
                    person_id: pid.clone(),
                    name: person.map(|p| p.name.clone()).unwrap_or_default(),
                    is_self: person.is_some_and(|p| p.is_self),
                    meetings: meetings.len(),
                    samples: samples.len(),
                    kept: if agreement.enrolled {
                        agreement.kept.len()
                    } else {
                        0
                    },
                    enrolled: agreement.enrolled,
                    reason: agreement.reason,
                });
            }
            Ok(Some((enrollments, added)))
        })
    }

    /// Resolves the batch's meetings (merge only on `analyzed → done`), then
    /// removes its finished jobs. Meetings with speaker centroids of another
    /// embedding model are skipped with a note, and keep the batch open.
    fn phase3(&self, jobs: &[Job], summary: &mut BatchSummary) -> Result<()> {
        let lib = self.env.lib;
        let thresholds = self.env.thresholds(&self.model).copied();
        let mut unfinished = false;
        for job in jobs {
            let result = (|| -> Result<()> {
                let hints = match HintsFile::read(&self.queue.job_dir(&job.id).join(HINTS_JSON)) {
                    Ok(h) => h,
                    Err(Error::NotFound(_)) => return Ok(()),
                    Err(e) => return Err(e),
                };
                let meeting = if lib.meeting_exists(&hints.meeting_id)? {
                    Some(lib.load_meeting(&hints.meeting_id)?)
                } else {
                    None
                };
                if let Some(other) = meeting.as_ref().and_then(|m| foreign_model(m, &self.model)) {
                    summary.notes.push(format!(
                        "meeting {} has speaker centroids of {other}, not {}: not resolved; \
                     re-embed it with `broscribe models use speaker_embedding {}`, then run \
                     the batch again",
                        hints.meeting_id, self.model, self.model
                    ));
                    unfinished = true;
                    return Ok(());
                }
                self.reopen_done(&job.id)?;
                let Some(lease) = self.queue.claim(lib, &job.id, self.env.probe)? else {
                    summary.busy += 1;
                    unfinished = true;
                    return Ok(());
                };
                // A meeting deleted meanwhile: the job just ends.
                let result = lease.commit(lib, JobState::Done, |tx, _| match &meeting {
                    Some(_) => {
                        let mut index = Index::open(tx.root())?;
                        finish_meeting(
                            tx,
                            &mut index,
                            &hints.meeting_id,
                            &self.model,
                            thresholds.as_ref(),
                        )
                        .map(Some)
                    }
                    None => Ok(None),
                });
                if let Err(e) = &result {
                    let _ = lease.fail(lib, e.to_string());
                }
                let released = lease.release(lib);
                let f = result?;
                released?;
                if let Some(f) = f {
                    summary.resolved += 1;
                    summary.merged_speakers += f.merged;
                    summary.auto_named += f.auto;
                    summary.suggested += f.suggested;
                }
                Ok(())
            })();
            if let Err(error) = result {
                if is_failpoint(&error) {
                    return Err(error);
                }
                summary.failed.push(job_failure(job, &error));
                self.record_failed_job(&job.id)?;
            }
        }
        summary.jobs_removed += self.cleanup(jobs, !unfinished)?;
        Ok(())
    }

    /// Removes the batch's `done` job folders that no live holder has and no
    /// other unfinished batch lists; when `finished`, the batch record too.
    fn cleanup(&self, jobs: &[Job], finished: bool) -> Result<usize> {
        let lib = self.env.lib;
        let stale = TimeDelta::from_std(self.queue.stale_after()).unwrap_or(TimeDelta::MAX);
        lib.write(|_tx| {
            let mut batches = list_batches(&self.queue)?;
            let failed = batches
                .iter()
                .find(|b| b.id == self.batch)
                .map(|b| b.failed_jobs.clone())
                .unwrap_or_default();
            let others: BTreeSet<JobId> = batches
                .iter()
                .filter(|b| b.id != self.batch)
                .flat_map(|b| b.jobs.iter().cloned())
                .collect();
            let _q = QueueLock::acquire(lib.root(), QueueLock::DEFAULT_WAIT)?;
            let mut removed = 0;
            let ids: BTreeSet<_> = jobs
                .iter()
                .map(|j| j.id.clone())
                .chain(
                    batches
                        .iter()
                        .filter(|b| b.id == self.batch)
                        .flat_map(|b| b.jobs.iter().cloned()),
                )
                .chain(failed.iter().cloned())
                .collect();
            let mut deleted = BTreeSet::new();
            for id in ids {
                if others.contains(&id) {
                    continue;
                }
                let job = match self.queue.get(&id) {
                    Ok(job) => job,
                    Err(Error::NotFound(_)) => {
                        deleted.insert(id);
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                // A fresh lease, live holder or not, keeps the folder: its
                // holder may still be committing into it.
                let held = job
                    .lease
                    .as_ref()
                    .is_some_and(|l| l.is_fresh(schema::now(), stale));
                if (job.state != JobState::Done && !(finished && failed.contains(&id))) || held {
                    continue;
                }
                let dir = self.queue.job_dir(&id);
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(Error::io_at(&dir, e)),
                }
                deleted.insert(id);
            }
            for batch in &mut batches {
                batch.jobs.retain(|id| !deleted.contains(id));
                batch.failed_jobs.retain(|id| !deleted.contains(id));
                batch.fingerprints.retain(|id, _| !deleted.contains(id));
                for job in batch.entries.values_mut() {
                    if job.as_ref().is_some_and(|id| deleted.contains(id)) {
                        *job = None;
                    }
                }
                fsutil::write_json_atomic(&batch_path(&self.queue, &batch.id), batch)?;
            }
            if finished {
                let path = batch_path(&self.queue, &self.batch);
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(Error::io_at(&path, e)),
                }
            }
            Ok(removed)
        })
    }
}

/// A batch job's failure, described by the manifest entry in its input.
fn job_failure(job: &Job, error: &Error) -> MeetingFailure {
    MeetingFailure {
        index: job.input.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        source_id: job
            .input
            .get("source_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        audio_path: job
            .input
            .get("audio_path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_default(),
        code: error.code(),
        message: error.to_string(),
    }
}

/// Removes this attempt's staged audio after its analysis failed: the path
/// is the attempt's own (`crate::import::staged_audio_path`).
fn discard_staged(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != ErrorKind::NotFound
    {
        tracing::warn!("removing staged audio {}: {e}", path.display());
    }
}

/// Hint checkpoints of a batch as phase 2 compares them, by job: `None`
/// for a job without one.
type HintSnapshot = BTreeMap<JobId, Option<HintsFile>>;

/// Phase 2's resolution of hinted people (see `Run::resolve_hint_people`).
struct ResolvedHints {
    snapshot: HintSnapshot,
    created: usize,
    forgotten: usize,
    other_model: usize,
}

/// One person's pooled hint samples and their agreement.
struct EnrollmentPlan {
    person: PersonId,
    samples: Vec<PooledSample>,
    agreement: identity::Agreement,
}

/// Pools every hinted person's samples of `model` over the batch and checks
/// their agreement against the other people's samples and labelled
/// speakers (never unhinted clusters). Quadratic in the samples, so it
/// runs outside the library lock.
fn enrollment_plans<'f>(
    files: impl IntoIterator<Item = &'f HintsFile>,
    model: &str,
) -> Result<Vec<EnrollmentPlan>> {
    let mut pooled: BTreeMap<PersonId, Vec<PooledSample>> = BTreeMap::new();
    let mut labelled: Vec<(PersonId, Vec<f32>)> = Vec::new();
    for f in files.into_iter().filter(|f| f.model == model) {
        for hp in &f.people {
            let Some(pid) = &hp.person_id else { continue };
            let slot = pooled.entry(pid.clone()).or_default();
            for s in &hp.samples {
                slot.push(PooledSample {
                    vector: s.vector()?,
                    meeting: f.meeting_id.clone(),
                    sha: f.sha256.clone(),
                    start: s.start,
                    end: s.end,
                });
            }
        }
        for sp in &f.speakers {
            if let Some(pid) = &sp.person_id {
                labelled.push((pid.clone(), sp.vector()?));
            }
        }
    }
    let agreements: Vec<identity::Agreement> = pooled
        .iter()
        .map(|(pid, samples)| {
            let vectors: Vec<&[f32]> = samples.iter().map(|s| s.vector.as_slice()).collect();
            let between: Vec<&[f32]> = pooled
                .iter()
                .filter(|(other, _)| *other != pid)
                .flat_map(|(_, s)| s.iter().map(|x| x.vector.as_slice()))
                .chain(
                    labelled
                        .iter()
                        .filter(|(other, _)| other != pid)
                        .map(|(_, v)| v.as_slice()),
                )
                .collect();
            identity::hint_agreement(&vectors, &between)
        })
        .collect();
    Ok(pooled
        .into_iter()
        .zip(agreements)
        .map(|((person, samples), agreement)| EnrollmentPlan {
            person,
            samples,
            agreement,
        })
        .collect())
}

/// Who a hinted person is, decided over `people` without writing anything.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HintPick {
    /// An existing person (for the user: the Me person).
    Existing(PersonId),
    /// An existing person who becomes the Me person.
    Promote(PersonId),
    /// Nobody matches: a new person.
    New { name: String, emails: Vec<String> },
}

/// The person a hint belongs to: its recorded ID while that person exists,
/// else a match by e-mail or name, else a new person. For the user: the Me
/// person, else the person with an e-mail or the name saved by `setup`, else
/// the person matching the hint's own e-mail or name (either is promoted to
/// Me), else a new Me ([`new_hint_person_name`]).
pub(crate) fn pick_hint_person(
    people: &[Person],
    setup: &SetupProgress,
    is_self: bool,
    person_id: Option<&PersonId>,
    name: Option<&str>,
    email: Option<&str>,
) -> HintPick {
    if is_self {
        if let Some(me) = people.iter().find(|p| p.is_self) {
            return HintPick::Existing(me.id.clone());
        }
        let known = setup
            .emails
            .iter()
            .find_map(|e| match_person(people, None, Some(e)))
            .or_else(|| match_person(people, setup.name.as_deref(), None))
            .or_else(|| match_person(people, name, email));
        if let Some(p) = known {
            return HintPick::Promote(p.id.clone());
        }
    } else {
        if let Some(id) = person_id
            && people.iter().any(|p| &p.id == id)
        {
            return HintPick::Existing(id.clone());
        }
        if let Some(p) = match_person(people, name, email) {
            return HintPick::Existing(p.id.clone());
        }
    }
    let name = new_hint_person_name(setup, is_self, name, email).to_owned();
    let own = if is_self {
        setup.emails.as_slice()
    } else {
        &[]
    };
    let mut emails: Vec<String> = Vec::new();
    for e in email.into_iter().chain(own.iter().map(String::as_str)) {
        let e = e.trim();
        if !e.is_empty() && !emails.iter().any(|x| fold(x) == fold(e)) {
            emails.push(e.to_owned());
        }
    }
    HintPick::New { name, emails }
}

/// The name [`pick_hint_person`] gives a new person: for the user the name
/// saved by `setup`, else the hint's, else "Me"; for anyone else the hint's
/// name, else its e-mail. Blank values count as absent. Writers store it
/// only through [`validate_name`].
fn new_hint_person_name<'a>(
    setup: &'a SetupProgress,
    is_self: bool,
    name: Option<&'a str>,
    email: Option<&'a str>,
) -> &'a str {
    let given = |s: Option<&'a str>| s.map(str::trim).filter(|s| !s.is_empty());
    if is_self {
        given(setup.name.as_deref()).or(given(name)).unwrap_or("Me")
    } else {
        given(name).or(given(email)).unwrap_or_default()
    }
}

/// Applies [`pick_hint_person`] to the library: promotes or creates the
/// person. Returns the ID and whether the person was created.
fn resolve_hint_person(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    people: &mut Vec<Person>,
    setup: &SetupProgress,
    hp: &crate::hints::HintPerson,
) -> Result<(PersonId, bool)> {
    let pick = pick_hint_person(
        people,
        setup,
        hp.is_self,
        hp.person_id.as_ref(),
        hp.name.as_deref(),
        hp.email.as_deref(),
    );
    match pick {
        HintPick::Existing(id) => Ok((id, false)),
        HintPick::Promote(id) => {
            let p = people
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or_else(|| Error::Internal(format!("person {id} vanished")))?;
            p.is_self = true;
            tx.save_person(p)?;
            index.upsert_person(tx, p)?;
            Ok((id, false))
        }
        HintPick::New { name, emails } => {
            let mut p = Person::new(tx.library().new_person_id(), validate_name(&name)?);
            p.is_self = hp.is_self;
            p.emails = emails;
            tx.save_person(&p)?;
            index.upsert_person(tx, &p)?;
            let id = p.id.clone();
            people.push(p);
            Ok((id, true))
        }
    }
}

/// One hint embedding pooled for enrollment.
struct PooledSample {
    vector: Vec<f32>,
    meeting: MeetingId,
    sha: String,
    start: f64,
    end: f64,
}

fn hint_voiceprint(person: &PersonId, model: &str, s: &PooledSample) -> Voiceprint {
    Voiceprint {
        id: schema::voiceprint_id(
            person,
            model,
            VoiceprintOrigin::Recording { sha256: &s.sha },
            s.start,
            s.end,
            VoiceprintKind::Hint,
        ),
        model: model.to_owned(),
        dim: s.vector.len(),
        vector_b64: schema::encode_vector(&s.vector),
        source: VoiceprintSource {
            kind: VoiceprintKind::Hint,
            meeting_id: Some(s.meeting.clone()),
            sha256: Some(s.sha.clone()),
            start: s.start,
            end: s.end,
            ranges: vec![[s.start as f32, s.end as f32]],
            speaker_id: None,
            speaker_namespace: String::new(),
        },
        device: None,
        strength: Strength::Confirmed,
        created_at: schema::now(),
    }
}

/// Keeps at most [`MAX_CONFIRMED`] diverse confirmed samples of `model`.
pub(crate) fn cap_confirmed(vps: &mut Voiceprints, model: &str) -> Result<()> {
    let idx: Vec<usize> = (0..vps.embeddings.len())
        .filter(|&i| {
            let e = &vps.embeddings[i];
            e.model == model && e.strength == Strength::Confirmed
        })
        .collect();
    if idx.len() <= MAX_CONFIRMED {
        return Ok(());
    }
    let vectors = idx
        .iter()
        .map(|&i| vps.embeddings[i].vector())
        .collect::<Result<Vec<_>>>()?;
    let keep: BTreeSet<usize> = identity::cap_samples(&vectors, MAX_CONFIRMED)
        .into_iter()
        .map(|k| idx[k])
        .collect();
    let drop: BTreeSet<usize> = idx.into_iter().filter(|i| !keep.contains(i)).collect();
    let mut i = 0;
    vps.embeddings.retain(|_| {
        let k = !drop.contains(&i);
        i += 1;
        k
    });
    Ok(())
}

/// Merges a manifest entry into an existing meeting: `source_id` when it has
/// none, new participants, and title / start time where the existing ones
/// were derived from the file.
fn merge_metadata(
    m: &mut Meeting,
    entry: &ManifestMeeting,
    participants: Vec<Participant>,
    derived_titles: &[String],
    derived_times: &[DateTime<FixedOffset>],
) {
    if m.source_id.is_none() {
        m.source_id = entry.source_id.clone();
    }
    for p in participants {
        let key = participant_key(&p);
        if !m.participants.iter().any(|q| participant_key(q) == key) {
            m.participants.push(p);
        }
    }
    if let Some(t) = &entry.title
        && m.title_source
            .is_none_or(|source| source == schema::TitleSource::File)
        && m.title
            .as_ref()
            .is_none_or(|cur| derived_titles.contains(cur))
    {
        m.title = Some(t.clone());
        m.title_source = Some(schema::TitleSource::User);
    }
    if let Some(s) = entry.started_at
        && derived_times.contains(&m.started_at)
    {
        m.started_at = s;
    }
    if m.languages.is_empty()
        && let Some(lang) = &entry.language
    {
        m.languages.push(LanguageSpan {
            lang: lang.clone(),
            from_s: 0.0,
            to_s: m.duration_s,
        });
    }
}

fn participant_key(p: &Participant) -> String {
    if p.is_self {
        return "self".to_owned();
    }
    if let Some(id) = &p.person_id {
        return id.to_string();
    }
    person_key(p.name.as_deref(), p.email.as_deref(), false)
}

#[cfg(test)]
mod tests;
