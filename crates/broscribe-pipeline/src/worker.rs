//! The queue worker behind `broscribe queue work` (plan §8 items 1, 18, 20):
//! claims queued jobs of every kind through the library queue (leases and
//! lock, so it runs safely next to other processes) and executes them until
//! nothing runnable is left.
//!
//! Job kinds:
//! - [`IMPORT_KIND`]: a single-file import that was enqueued but not finished
//!   (crashed or failed run). Input `{"path","sha256"}` plus the optional
//!   import flags `handling`, `language`, `title`, `started_at`.
//! - [`MANIFEST_KIND`]: manifest meetings. Jobs whose analysis is checkpointed
//!   (`hints.json`) are finished by running the batch's enrolment and resolve
//!   phases; jobs interrupted before that need the manifest again and are
//!   marked failed.
//! - [`CONFIRM_KIND`]: confirmation samples after the app named a speaker
//!   (the app already applied the centroid fallback). Input `{"meeting_id",
//!   "speaker_id","person_id","model"}`. Revalidates the person, the
//!   assignment, the model and forget state outside and again inside the
//!   commit; the samples replace the centroid voiceprint naming added. Voiceprint
//!   IDs are deterministic, so running a job twice changes nothing. A commit
//!   that changes the person's voiceprints first requests identity
//!   reevaluation with the meeting first ([`broscribe_core::learning`]).
//! - [`REIDENTIFY_KIND`]: identity reevaluation of one embedding model after
//!   confirmed evidence changed ([`crate::reidentify`]). Keyed by the model;
//!   input [`broscribe_core::learning::ReidentifyInput`]. A run visits at
//!   most [`crate::reidentify::MEETINGS_PER_RUN`] meetings and then goes to
//!   the back of the queue; a `done` job keeps its report.
//! - [`PLAYBACK_KIND`]: a 16 kHz mono WAV of a recording the player cannot
//!   open natively, at [`playback_cache_path`]. Input `{"meeting_id"}`.
//! - [`VECTORS_KIND`]: search vectors of one meeting whose windows an
//!   in-process identity write changed (the app separating members from a
//!   speaker group while naming or rejecting). Input `{"meeting_id"}`, keyed
//!   by the meeting, so one job stands for any number of such writes. The job
//!   embeds the windows still missing a vector with the text model configured
//!   when it runs; without one there is nothing to embed.
//!
//! A job is runnable when it is `queued`, `running` or `analyzed`, or
//! `failed` with fewer than [`MAX_ATTEMPTS`] claims, and nobody live holds
//! its lease. Each job is tried at most once per [`work_queue`] call, except
//! a reidentify job after a successful run: it runs again when it is
//! claimable again (continued, last in line by then, or requested again by
//! a later job). `done` jobs of the confirm, playback and vectors kinds are
//! removed right away, so enqueueing the same key later runs it again.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use broscribe_core::identity;
use broscribe_core::index::Index;
use broscribe_core::ops::{ConfirmationSample, HINTS_JSON};
use broscribe_core::process::ProcessProbe;
use broscribe_core::queue::{
    CONFIRM_KIND, IMPORT_KIND, Job, JobId, JobState, LeaseHandle, MANIFEST_KIND, PLAYBACK_KIND,
    Queue, REPROCESS_KIND, VECTORS_KIND,
};
use broscribe_core::retention::PLAYBACK_CACHE_DIR;
use broscribe_core::schema::{
    self, Assignment, Meeting, RecordingState, Strength, Voiceprint, VoiceprintKind,
    VoiceprintOrigin, VoiceprintSource,
};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result, SpeakerId, fsutil, paths};
use broscribe_models::{ModelProvider, ModelSet};
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::audio::TARGET_RATE;
use crate::batch::{BatchSummary, MeetingFailure, cap_confirmed, job_batch_active, resume_batches};
#[cfg(test)]
use crate::confirm::confirmation_samples;
use crate::confirm::confirmation_samples_for;
use crate::enrich::{ENRICH_KIND, QueuedRun};
use crate::import::{
    MoveOutcome, MoveRecovery, discard_abandoned_setup_imports, is_folder_import, is_setup_import,
    recover_moves, resume_import_job, setup_import_discarded,
};
use crate::record::{self, RecordInput};
use crate::reidentify::{MEETINGS_PER_RUN, REIDENTIFY_KIND};
use crate::template::TEMPLATE_KIND;
use broscribe_core::queue::{
    RECORD_FINAL_KIND as FINALIZE_KIND, RECORD_PROVISIONAL_KIND as PROVISIONAL_KIND,
};

/// Every kind the worker runs, in no particular order (jobs run oldest first).
pub const WORKER_KINDS: [&str; 11] = [
    IMPORT_KIND,
    MANIFEST_KIND,
    CONFIRM_KIND,
    PLAYBACK_KIND,
    VECTORS_KIND,
    FINALIZE_KIND,
    PROVISIONAL_KIND,
    ENRICH_KIND,
    TEMPLATE_KIND,
    REPROCESS_KIND,
    REIDENTIFY_KIND,
];
/// A `failed` job is retried until it has been claimed this many times.
pub use broscribe_core::queue::MAX_ATTEMPTS;

pub use broscribe_core::ops::playback_cache_path;

/// `event` of one progress line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerEventKind {
    JobStarted,
    Progress,
    JobDone,
    JobFailed,
    /// Nothing runnable is left; the worker is about to return.
    Idle,
}

/// One progress line of `queue work --json-progress`:
/// `{"event","job_id","kind","fraction","message"}` (absent fields omitted).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WorkerEvent {
    pub event: WorkerEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<MeetingId>,
    /// 0.0–1.0 within the job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl WorkerEvent {
    fn job(
        event: WorkerEventKind,
        job: &Job,
        fraction: Option<f64>,
        message: Option<String>,
    ) -> Self {
        Self {
            event,
            job_id: Some(job.id.to_string()),
            kind: Some(job.kind.clone()),
            meeting_id: job.meeting_id(),
            fraction,
            message,
        }
    }

    fn idle() -> Self {
        Self {
            event: WorkerEventKind::Idle,
            job_id: None,
            kind: None,
            meeting_id: None,
            fraction: None,
            message: None,
        }
    }
}

/// Receives the worker's events in order.
pub type EventSink<'a> = &'a mut dyn FnMut(&WorkerEvent);

#[derive(Clone)]
pub struct WorkerOptions {
    /// The app cache folder (holds the playback cache).
    pub cache_dir: PathBuf,
    /// Run at most one job.
    pub once: bool,
    pub encoder: Arc<dyn broscribe_platform::AudioEncoder + Send + Sync>,
    pub data_dir: PathBuf,
    pub secrets: Arc<dyn broscribe_platform::SecretStore>,
    /// The helper's live system-calendar opt-in, checked whenever a recording
    /// consumes native events from its durable calendar snapshot.
    pub calendar_authority: Arc<dyn broscribe_platform::CalendarAuthority>,
}

impl WorkerOptions {
    pub fn new(
        cache_dir: PathBuf,
        encoder: Arc<dyn broscribe_platform::AudioEncoder + Send + Sync>,
        data_dir: PathBuf,
        secrets: Arc<dyn broscribe_platform::SecretStore>,
        calendar_authority: Arc<dyn broscribe_platform::CalendarAuthority>,
    ) -> Self {
        Self {
            cache_dir,
            once: false,
            encoder,
            data_dir,
            secrets,
            calendar_authority,
        }
    }
}
/// Counts of one [`work_queue`] call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WorkerSummary {
    pub done: usize,
    pub failed: usize,
    /// Jobs another process took before this one could.
    pub busy: usize,
    /// Meetings completed by import, final recording or reprocess jobs
    /// (for the caller's post-identity steps, including search vectors).
    pub imported: Vec<MeetingId>,
    /// Meetings whose confirmation jobs completed successfully.
    pub named: Vec<MeetingId>,
    /// Meetings whose identities a reidentify job changed (or repaired);
    /// retention follows them as it follows `named`, and their search
    /// vectors were queued for refresh when their windows changed.
    pub reidentified: Vec<MeetingId>,
    /// Meetings whose search vectors a refresh job brought up to date.
    pub refreshed: Vec<MeetingId>,
    /// A manifest batch was resolved: any manifest meeting may have changed.
    pub batches_resolved: bool,
    /// Retention runs after a completed job that failed; the job keeps its
    /// outcome and the queue goes on.
    pub retention_failures: Vec<String>,
}

/// What running one job gave.
enum Outcome {
    Done(String),
    /// Another live process holds it.
    Busy,
}

/// Runs runnable jobs, oldest first, until none is left (or one with
/// `once`), then emits `idle`. Per-job failures are reported as
/// `job_failed` events and recorded on the job; only queue-wide errors
/// (unreadable queue folder) end the run with an error.
pub fn work_queue(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    opts: &WorkerOptions,
    emit: EventSink<'_>,
) -> Result<WorkerSummary> {
    let queue = Queue::open(env.lib.root())?;
    discard_abandoned_setup_imports_logged(env);
    let mut tried: HashSet<JobId> = HashSet::new();
    let mut models: Option<ModelSet> = None;
    let mut summary = WorkerSummary::default();
    while let Some(job) = next_job(&queue, env.probe, &tried)? {
        tried.insert(job.id.clone());
        let before = (
            summary.imported.len(),
            summary.named.len(),
            summary.reidentified.len(),
        );
        dispatch(
            env,
            provider,
            opts,
            &queue,
            &job,
            &mut models,
            &mut tried,
            emit,
            &mut summary,
        );
        let named: Vec<MeetingId> = summary.named[before.1..]
            .iter()
            .chain(&summary.reidentified[before.2..])
            .cloned()
            .collect();
        if let Err(e) = apply_completed_retention(
            env,
            opts,
            &summary.imported[before.0..],
            &named,
            job.kind == MANIFEST_KIND && summary.batches_resolved,
        ) {
            retention_failed(&job, &e, emit, &mut summary);
        }
        if opts.once {
            break;
        }
    }
    emit(&WorkerEvent::idle());
    Ok(summary)
}

/// Runs only job `id` (a manifest job takes the rest of its batch group
/// along), whatever its turn in the queue; `not_found` when it does not
/// exist. No `idle` event: the caller decides what comes next.
pub fn work_job(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    opts: &WorkerOptions,
    id: &JobId,
    emit: EventSink<'_>,
) -> Result<WorkerSummary> {
    let queue = Queue::open(env.lib.root())?;
    let job = queue.get(id)?;
    discard_abandoned_setup_imports_logged(env);
    let mut tried: HashSet<JobId> = HashSet::from([job.id.clone()]);
    let mut models: Option<ModelSet> = None;
    let mut summary = WorkerSummary::default();
    dispatch(
        env,
        provider,
        opts,
        &queue,
        &job,
        &mut models,
        &mut tried,
        emit,
        &mut summary,
    );
    let named: Vec<MeetingId> = summary
        .named
        .iter()
        .chain(&summary.reidentified)
        .cloned()
        .collect();
    if let Err(e) = apply_completed_retention(
        env,
        opts,
        &summary.imported,
        &named,
        summary.batches_resolved,
    ) {
        retention_failed(&job, &e, emit, &mut summary);
    }
    Ok(summary)
}

/// Startup recovery of the worker: setup Try-it imports a killed or
/// cancelled setup step left behind go, whether or not another import
/// follows. A failure is logged; the queue still runs.
fn discard_abandoned_setup_imports_logged(env: &Env<'_>) {
    match discard_abandoned_setup_imports(env.lib, env.probe) {
        Ok(ids) if !ids.is_empty() => {
            tracing::info!("discarded {} abandoned setup import(s)", ids.len());
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("discarding abandoned setup imports: {e}"),
    }
}

/// Both the daemon's run-one path and the standalone queue worker enforce
/// retention after completion, once audio-consuming leases have been
/// released, under the policy configured now (not when the worker started).
fn apply_completed_retention(
    env: &Env<'_>,
    opts: &WorkerOptions,
    imported: &[MeetingId],
    named: &[MeetingId],
    batches_resolved: bool,
) -> Result<()> {
    let meetings = if batches_resolved {
        None
    } else {
        let mut ids = imported.to_vec();
        ids.extend(named.iter().cloned());
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return Ok(());
        }
        Some(ids)
    };
    #[cfg(test)]
    if RETENTION_FAILS.get() {
        return Err(Error::Internal("failpoint retention".into()));
    }
    crate::retention::apply_current(
        env.lib,
        &opts.data_dir,
        env.probe,
        &crate::retention::RetentionOptions {
            meetings,
            cache_dir: Some(opts.cache_dir.clone()),
            ..Default::default()
        },
    )?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static RETENTION_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Reports a failed post-completion retention run as a progress event of the
/// job that triggered it; the job's own outcome stands.
fn retention_failed(job: &Job, e: &Error, emit: EventSink<'_>, summary: &mut WorkerSummary) {
    let message = format!("retention after completion failed: {e} ({})", e.code());
    tracing::warn!(job = %job.id, "{message}");
    emit(&WorkerEvent::job(
        WorkerEventKind::Progress,
        job,
        None,
        Some(message.clone()),
    ));
    summary.retention_failures.push(message);
}

/// Runs one job by kind and reports it.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    opts: &WorkerOptions,
    queue: &Queue,
    job: &Job,
    models: &mut Option<ModelSet>,
    tried: &mut HashSet<JobId>,
    emit: EventSink<'_>,
    summary: &mut WorkerSummary,
) {
    let mut observe = |event: &WorkerEvent| {
        if matches!(event.event, WorkerEventKind::Progress)
            && let Some(fraction) = event.fraction
            && let Some(id) = event
                .job_id
                .as_deref()
                .and_then(|raw| JobId::parse(raw).ok())
            && let Err(e) = queue.report_progress(&id, env.probe, fraction)
        {
            tracing::debug!(code = e.code(), "could not store live job progress");
        }
        emit(event);
    };
    let emit: EventSink<'_> = &mut observe;
    if job.kind == MANIFEST_KIND {
        manifest_jobs(env, provider, models, queue, job, tried, emit, summary);
        return;
    }
    emit(&WorkerEvent::job(
        WorkerEventKind::JobStarted,
        job,
        Some(0.0),
        None,
    ));
    // The meeting the claimed input names: `job` is only the listing that
    // picked the job, its input may have changed before the claim.
    let mut meeting: Option<MeetingId> = None;
    let result = match job.kind.as_str() {
        IMPORT_KIND => import_job(
            env,
            provider,
            opts,
            models,
            queue,
            job,
            &mut summary.imported,
        ),
        CONFIRM_KIND | PLAYBACK_KIND => {
            leased_job(env, provider, opts, queue, job, emit, &mut meeting)
        }
        VECTORS_KIND => vectors_job(env, opts, queue, job, &mut meeting),
        FINALIZE_KIND | PROVISIONAL_KIND => record_job(
            env,
            provider,
            opts,
            models,
            queue,
            job,
            &mut summary.imported,
        ),
        ENRICH_KIND | TEMPLATE_KIND | REPROCESS_KIND => {
            processing_job(env, provider, opts, queue, job, &mut meeting)
        }
        REIDENTIFY_KIND => reidentify_job(
            env,
            opts,
            queue,
            job,
            emit,
            tried,
            &mut summary.reidentified,
        ),
        other => Err(Error::Internal(format!("no runner for job kind {other}"))),
    };
    if matches!(result, Ok(Outcome::Done(_)))
        && let Some(id) = meeting
    {
        if job.kind == REPROCESS_KIND {
            summary.imported.push(id);
        } else if job.kind == CONFIRM_KIND {
            summary.named.push(id);
        } else if job.kind == VECTORS_KIND {
            summary.refreshed.push(id);
        }
    }
    report(job, result, emit, summary);
}

fn report(job: &Job, result: Result<Outcome>, emit: EventSink<'_>, summary: &mut WorkerSummary) {
    match result {
        Ok(Outcome::Done(message)) => {
            summary.done += 1;
            emit(&WorkerEvent::job(
                WorkerEventKind::JobDone,
                job,
                Some(1.0),
                Some(message),
            ));
        }
        Ok(Outcome::Busy) => {
            summary.busy += 1;
            emit(&WorkerEvent::job(
                WorkerEventKind::JobDone,
                job,
                None,
                Some("left to another process".into()),
            ));
        }
        Err(e) => {
            summary.failed += 1;
            emit(&WorkerEvent::job(
                WorkerEventKind::JobFailed,
                job,
                None,
                Some(format!("{} ({})", e, e.code())),
            ));
        }
    }
}

/// The oldest runnable job not tried yet.
fn next_job(
    queue: &Queue,
    probe: &dyn ProcessProbe,
    tried: &HashSet<JobId>,
) -> Result<Option<Job>> {
    let mut best: Option<Job> = None;
    for kind in WORKER_KINDS {
        for job in queue.list(kind)? {
            if tried.contains(&job.id) || !runnable(queue, &job, probe) {
                continue;
            }
            let older = best
                .as_ref()
                .is_none_or(|b| (job.created_at, &job.id) < (b.created_at, &b.id));
            if older {
                best = Some(job);
            }
        }
    }
    Ok(best)
}

/// Whether the worker should take `job` now (see the module docs): the
/// claim would accept it ([`Job::is_claimable`]; an interrupted or failed
/// synchronous run is the user's to retry) and its lease is free.
pub fn runnable(queue: &Queue, job: &Job, probe: &dyn ProcessProbe) -> bool {
    job.is_claimable() && lease_free(queue, job, probe)
}

/// No lease, or one its holder no longer holds (stale on either side of
/// the clock, or its process gone): [`Queue::is_held`], the one rule every
/// lease reader shares.
fn lease_free(queue: &Queue, job: &Job, probe: &dyn ProcessProbe) -> bool {
    !queue.is_held(job, probe)
}

/// Marks a job failed through a lease of its own (when free).
fn fail_job(env: &Env<'_>, queue: &Queue, id: &JobId, message: &str) -> Result<()> {
    if let Some(lease) = queue.claim(env.lib, id, env.probe)? {
        let failed = lease.fail(env.lib, message);
        let released = lease.release(env.lib);
        failed?;
        released?;
    }
    Ok(())
}

fn model_set<'m>(
    slot: &'m mut Option<ModelSet>,
    provider: &dyn ModelProvider,
    env: &Env<'_>,
) -> Result<&'m mut ModelSet> {
    if slot.is_none() {
        *slot = Some(provider.load(&env.config.profile)?);
    }
    slot.as_mut()
        .ok_or_else(|| Error::Internal("model set not loaded".into()))
}

fn input<T: for<'de> Deserialize<'de>>(job: &Job) -> Result<T> {
    serde_json::from_value(job.input.clone())
        .map_err(|e| Error::InvalidInput(format!("job {} has an invalid input: {e}", job.id)))
}

/// The meeting the input names (`meeting_id`), if any.
fn input_meeting(input: &serde_json::Value) -> Option<MeetingId> {
    input
        .get("meeting_id")
        .and_then(|v| v.as_str())
        .and_then(|s| MeetingId::parse(s.trim()).ok())
}

/// Runs the final or provisional recording job from its claimed input.
/// Loading the models happens under the lease: a failure is recorded on
/// the job (counting an attempt) like any other.
fn record_job(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    opts: &WorkerOptions,
    models: &mut Option<ModelSet>,
    queue: &Queue,
    job: &Job,
    imported: &mut Vec<MeetingId>,
) -> Result<Outcome> {
    let Some(lease) = queue.claim(env.lib, &job.id, env.probe)? else {
        return not_claimed(queue, job);
    };
    let claimed = lease.claimed_job();
    let provisional = claimed.kind == PROVISIONAL_KIND;
    let result = input::<RecordInput>(claimed).and_then(|parsed| {
        record::run(
            env,
            model_set(models, provider, env)?,
            &lease,
            &parsed,
            provisional,
            opts.encoder.as_ref(),
            opts.calendar_authority.as_ref(),
        )
    });
    match result {
        Ok(id) => {
            lease.release(env.lib)?;
            if !env.lib.meeting_exists(&id)? {
                return Ok(Outcome::Done("empty recording discarded".into()));
            }
            if !provisional {
                imported.push(id.clone());
            }
            Ok(Outcome::Done(format!(
                "{} as {id}",
                if provisional {
                    "provisional transcript"
                } else {
                    "final transcript"
                }
            )))
        }
        Err(e) => {
            let _ = lease.fail(env.lib, e.to_string());
            let _ = lease.release(env.lib);
            Err(e)
        }
    }
}

/// Why a claim returned no lease: a claim that took over a crashed holder
/// with no attempts left marks the job failed (`worker crashed`), which is a
/// failure to report; anything else is another live process's job.
fn not_claimed(queue: &Queue, job: &Job) -> Result<Outcome> {
    match queue.get(&job.id) {
        Ok(j) if j.state == JobState::Failed && j.lease.is_none() => {
            Err(Error::Internal(j.error.unwrap_or_else(|| "failed".into())))
        }
        Ok(_) | Err(Error::NotFound(_)) => Ok(Outcome::Busy),
        Err(e) => Err(e),
    }
}

/// Runs an enrichment, template or reprocessing job from its claimed
/// input; `meeting` gets the meeting that input names.
fn processing_job(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    opts: &WorkerOptions,
    queue: &Queue,
    job: &Job,
    meeting: &mut Option<MeetingId>,
) -> Result<Outcome> {
    let Some(lease) = queue.claim(env.lib, &job.id, env.probe)? else {
        return not_claimed(queue, job);
    };
    let claimed = lease.claimed_job();
    let kind = claimed.kind.clone();
    let claimed_input = claimed.input.clone();
    *meeting = input_meeting(&claimed_input);
    // The publication runs inside the lease-checked commit that marks the
    // job done: a worker whose lease was reclaimed publishes nothing.
    let result = match kind.as_str() {
        ENRICH_KIND => crate::enrich::run_enrich_job(
            env,
            &claimed_input,
            &opts.data_dir,
            opts.secrets.as_ref(),
            Some(&lease),
        )
        .map(|run| review_of(run, |o| o.needs_review)),
        TEMPLATE_KIND => crate::template::run_template_job(
            env,
            &claimed_input,
            &opts.data_dir,
            opts.secrets.as_ref(),
            Some(&lease),
        )
        .map(|run| review_of(run, |o| o.needs_review)),
        _ => crate::reprocess::run_reprocess_job_leased(env, &claimed_input, provider, &lease)
            .map(|o| QueuedRun::Ran(o.needs_review)),
    };
    match result {
        Ok(run) => {
            // Outcomes that published nothing (skipped) are closed here.
            lease.commit(env.lib, JobState::Done, |_, _| Ok(()))?;
            lease.release(env.lib)?;
            env.lib.write(|tx| queue.remove_finished(tx, &kind))?;
            let message = match run {
                QueuedRun::Ran(needs_review) => {
                    let mut message = completion_message(&kind, &needs_review);
                    if kind == ENRICH_KIND {
                        message.push_str(&auto_templates_note(env, opts, &claimed_input));
                    }
                    message
                }
                QueuedRun::Skipped(reason) => format!("{kind} skipped: {reason}"),
            };
            Ok(Outcome::Done(message))
        }
        Err(error) => {
            let _ = lease.fail(env.lib, error.to_string());
            let _ = lease.release(env.lib);
            Err(error)
        }
    }
}

/// A generation job's run as the references it left for review.
fn review_of<T>(
    run: QueuedRun<T>,
    needs_review: impl FnOnce(T) -> Vec<String>,
) -> QueuedRun<Vec<String>> {
    match run {
        QueuedRun::Ran(out) => QueuedRun::Ran(needs_review(out)),
        QueuedRun::Skipped(reason) => QueuedRun::Skipped(reason),
    }
}

/// After an automatic enrichment (claimed input `"auto": true`) succeeded,
/// queues `[templates] auto` under the current policy and consent
/// ([`crate::template::queue_auto_templates`]); returns a note for the
/// completion message (empty when nothing was queued). A failure to queue
/// leaves the enrichment's own success standing.
fn auto_templates_note(env: &Env<'_>, opts: &WorkerOptions, input: &serde_json::Value) -> String {
    let auto = input.get("auto").and_then(serde_json::Value::as_bool) == Some(true);
    let Some(meeting) = input_meeting(input).filter(|_| auto) else {
        return String::new();
    };
    match crate::template::queue_auto_templates(env.lib, &opts.data_dir, &meeting) {
        Ok(ids) if ids.is_empty() => String::new(),
        Ok(ids) => format!("; templates queued: {}", ids.join(", ")),
        Err(e) => {
            tracing::warn!("automatic templates not queued: {}", e.code());
            format!("; templates not queued: {}", e.code())
        }
    }
}

/// `<kind> complete`, plus the references the job left for the user to check.
fn completion_message(kind: &str, needs_review: &[String]) -> String {
    if needs_review.is_empty() {
        return format!("{kind} complete");
    }
    format!(
        "{kind} complete; {} item(s) need review: {}",
        needs_review.len(),
        needs_review.join("; ")
    )
}

// ---- import ----

/// Resumes a queued single-file import with the options stored in it when it
/// was enqueued ([`resume_import_job`] claims it itself and records failures
/// on it); its attempts are kept. A completed recovery applies the
/// `auto_enrich` policy the interrupted import never reached, also for the
/// other files' moves the import's recovery completed, whatever this job's
/// own outcome. Models that fail to load never hold interrupted moves
/// back ([`import_without_models`]).
fn import_job(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    opts: &WorkerOptions,
    models: &mut Option<ModelSet>,
    queue: &Queue,
    job: &Job,
    imported: &mut Vec<MeetingId>,
) -> Result<Outcome> {
    let set = match model_set(models, provider, env) {
        Ok(set) => set,
        Err(e) => return import_without_models(env, opts, queue, job, e, imported),
    };
    // A missing source is left to the import: it first recovers an
    // interrupted `move` import of that file.
    let mut completed = Vec::new();
    let result = resume_import_job(env, set, job, &mut completed);
    // Their jobs are gone: these follow-ups are the only ones they get.
    let notes = follow_up_completed_moves(env, &opts.data_dir, &completed, imported);
    let outcome = match result {
        Ok(o) => o,
        Err(Error::LibraryBusy(_)) => return not_claimed(queue, job),
        // Its setup session ended: dropped, not failed.
        Err(_) if setup_import_discarded(queue, job) => {
            return Ok(Outcome::Done(
                "discarded: its setup session ended or its recording is gone".into(),
            ));
        }
        Err(e) => {
            // Errors before the import claimed the job (missing or
            // unreadable source, invalid input) leave it untouched: record
            // them so the attempts limit applies.
            match queue.get(&job.id) {
                Ok(j) if j.updated_at == job.updated_at => {
                    fail_job(env, queue, &job.id, &e.to_string())?;
                }
                _ => {}
            }
            return Err(e);
        }
    };
    if outcome.already_exists || outcome.recovered {
        // `import_file` returns early for a known file, or one its move
        // recovery just published, without touching the job: close it here
        // so it is not picked up again.
        env.lib.write(|tx| {
            match queue.get(&job.id) {
                Ok(j) if j.state != JobState::Done => {
                    queue.set_state(tx, &job.id, JobState::Done, None)?;
                }
                Ok(_) | Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
            queue.remove_finished(tx, IMPORT_KIND).map(|_| ())
        })?;
    }
    let mut message = if outcome.already_exists {
        format!("already imported as {}", outcome.meeting_id)
    } else {
        imported.push(outcome.meeting_id.clone());
        let mut message = format!("imported as {}", outcome.meeting_id);
        let single = !is_folder_import(&job.input) && !is_setup_import(&job.input);
        match queue_auto_enrich(env, &opts.data_dir, single, &outcome.meeting_id) {
            Ok(true) => message.push_str("; enrichment queued"),
            Ok(false) => {}
            Err(e) => message.push_str(&format!("; enrichment not queued: {e}")),
        }
        message
    };
    for note in notes {
        message.push_str("; ");
        message.push_str(&note);
    }
    Ok(Outcome::Done(message))
}

/// The import's models could not be loaded (`error`). Interrupted `move`
/// imports are settled anyway ([`recover_moves`] needs no models, and the
/// user's files must not wait on them) and the meetings they completed get
/// their follow-ups. Then the failure is recorded on this job through a
/// lease of its own, so its attempts count and a model that never loads
/// stops retrying it ([`MAX_ATTEMPTS`]). A job its own recovery finished
/// is done.
fn import_without_models(
    env: &Env<'_>,
    opts: &WorkerOptions,
    queue: &Queue,
    job: &Job,
    error: Error,
    imported: &mut Vec<MeetingId>,
) -> Result<Outcome> {
    let notes = match recover_moves(env) {
        Ok(recovered) => {
            let completed: Vec<MoveRecovery> = recovered
                .into_iter()
                .filter(|r| r.outcome == MoveOutcome::Completed)
                .collect();
            follow_up_completed_moves(env, &opts.data_dir, &completed, imported)
        }
        Err(e) => {
            tracing::warn!("settling interrupted moves failed: {}", e.code());
            Vec::new()
        }
    };
    match queue.get(&job.id) {
        Ok(j) if j.state != JobState::Done => {}
        Ok(_) | Err(Error::NotFound(_)) => {
            let mut message = "nothing left to import: the job was finished meanwhile".to_owned();
            for note in notes {
                message.push_str("; ");
                message.push_str(&note);
            }
            return Ok(Outcome::Done(message));
        }
        Err(e) => return Err(e),
    }
    fail_job(
        env,
        queue,
        &job.id,
        &format!("loading the models failed: {error}"),
    )?;
    Err(error)
}

/// Follow-ups of the interrupted `move` imports of other files that an
/// import's recovery completed: each meeting joins `imported` (search
/// vectors and retention after completion) and gets the enrichment its own
/// job would have queued ([`queue_auto_enrich`], by that job's folder
/// classification). Returns one note per meeting, also logged.
fn follow_up_completed_moves(
    env: &Env<'_>,
    data_dir: &Path,
    completed: &[MoveRecovery],
    imported: &mut Vec<MeetingId>,
) -> Vec<String> {
    let mut notes = Vec::new();
    for r in completed {
        let Some(id) = &r.meeting_id else {
            continue;
        };
        imported.push(id.clone());
        let mut note = format!(
            "interrupted move of {} completed as {id}",
            r.source.display()
        );
        match queue_auto_enrich(env, data_dir, !r.folder, id) {
            Ok(true) => note.push_str(", enrichment queued"),
            Ok(false) => {}
            Err(e) => note.push_str(&format!(", enrichment not queued: {e}")),
        }
        tracing::info!("{note}");
        notes.push(note);
    }
    notes
}

/// Queues enrichment of a recovered single-file import (`single`: neither a
/// file of a folder import, a batch, nor a setup Try-it import) when
/// `auto_enrich = new` and an LLM provider is enabled with consent (read
/// fresh from `data_dir`), as the interrupted import would have. Returns
/// whether a job was queued.
fn queue_auto_enrich(
    env: &Env<'_>,
    data_dir: &Path,
    single: bool,
    meeting: &MeetingId,
) -> Result<bool> {
    if !single {
        return Ok(false);
    }
    let config = broscribe_core::config::Config::load(data_dir)?;
    let registry = broscribe_models::llm::ProviderRegistry::load(data_dir)?;
    if !crate::enrich::auto_enrich_applies(&config, &registry) {
        return Ok(false);
    }
    env.lib
        .write(|tx| crate::enrich::enqueue_enrich(tx, meeting))?;
    Ok(true)
}

// ---- manifest meetings ----

/// Finishes every runnable manifest job together: checkpointed ones through
/// the batch's enrolment and resolve phases (an empty manifest resumes the
/// jobs in the queue), the others are marked failed.
#[allow(clippy::too_many_arguments)]
fn manifest_jobs(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    models: &mut Option<ModelSet>,
    queue: &Queue,
    first: &Job,
    tried: &mut HashSet<JobId>,
    emit: EventSink<'_>,
    summary: &mut WorkerSummary,
) {
    let mut jobs = vec![first.clone()];
    match queue.list(MANIFEST_KIND) {
        Ok(all) => jobs.extend(
            all.into_iter()
                .filter(|j| !tried.contains(&j.id) && runnable(queue, j, env.probe)),
        ),
        Err(e) => {
            emit(&WorkerEvent::job(
                WorkerEventKind::JobStarted,
                first,
                Some(0.0),
                None,
            ));
            report(first, Err(e), emit, summary);
            return;
        }
    }
    let mut checkpointed = Vec::new();
    for job in jobs {
        tried.insert(job.id.clone());
        emit(&WorkerEvent::job(
            WorkerEventKind::JobStarted,
            &job,
            Some(0.0),
            None,
        ));
        if queue.job_dir(&job.id).join(HINTS_JSON).is_file() {
            checkpointed.push(job);
            continue;
        }
        // Not analyzed yet: an import still running a batch that lists this
        // job gets to it in turn (deferred, no attempt counted); only entries
        // no running batch claims fail as interrupted.
        let result = match job_batch_active(queue, &job.id, env.probe) {
            Ok(true) => Ok(Outcome::Busy),
            Ok(false) => {
                let e = Error::InvalidInput(
                    "the analysis was interrupted before its checkpoint; run `broscribe import --manifest <file>` again"
                        .into(),
                );
                fail_job(env, queue, &job.id, &e.to_string()).and(Err(e))
            }
            Err(e) => Err(e),
        };
        report(&job, result, emit, summary);
    }
    if checkpointed.is_empty() {
        return;
    }
    // Enrolment and resolve of every pending batch, each on its own jobs.
    let ran = model_set(models, provider, env).and_then(|set| resume_batches(env, set));
    if ran.is_ok() {
        summary.batches_resolved = true;
    }
    for job in &checkpointed {
        let result = match &ran {
            // Another process holds the library or a job: its turn.
            Err(Error::LibraryBusy(_)) => Ok(Outcome::Busy),
            // Models that do not load, or a resolve that fails, are
            // recorded on every checkpointed job still waiting (an
            // attempt each), so a failure that repeats ends at
            // [`MAX_ATTEMPTS`] instead of looping on every worker run.
            Err(e) => {
                let message = format!("resolving the manifest batch failed: {e}");
                fail_job(env, queue, &job.id, &message).and(Err(Error::Internal(message)))
            }
            Ok(batch) => match queue.get(&job.id) {
                // A job the batch removed: resolved, unless the batch
                // reports its entry failed (and gave it up).
                Err(Error::NotFound(_)) => match batch_failure(batch, job) {
                    Some(f) => Err(Error::Internal(format!("{} ({})", f.message, f.code))),
                    None => Ok(Outcome::Done("resolved".into())),
                },
                Ok(j) if j.state == JobState::Done => Ok(Outcome::Done("resolved".into())),
                Ok(j) if j.state == JobState::Failed => {
                    Err(Error::Internal(j.error.unwrap_or_else(|| "failed".into())))
                }
                Ok(_) => Ok(Outcome::Busy),
                Err(e) => Err(e),
            },
        };
        report(job, result, emit, summary);
    }
}

/// The failure `batch` reports for manifest job `job`'s entry (its
/// manifest index and source ID), if any.
fn batch_failure<'a>(batch: &'a BatchSummary, job: &Job) -> Option<&'a MeetingFailure> {
    let source = job.input.get("source_id").and_then(|v| v.as_str());
    let index = job.input.get("index").and_then(serde_json::Value::as_u64);
    batch
        .failed
        .iter()
        .find(|f| f.source_id.as_deref() == source && index == Some(f.index as u64))
}

// ---- leased kinds ----

/// Claims the job, runs it from its claimed input, then releases it:
/// `done` jobs are removed, failures recorded. `meeting` gets the meeting
/// that input names.
fn leased_job(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    opts: &WorkerOptions,
    queue: &Queue,
    job: &Job,
    emit: EventSink<'_>,
    meeting: &mut Option<MeetingId>,
) -> Result<Outcome> {
    let Some(lease) = queue.claim(env.lib, &job.id, env.probe)? else {
        return not_claimed(queue, job);
    };
    let claimed = lease.claimed_job();
    *meeting = input_meeting(&claimed.input);
    let result = if claimed.kind == CONFIRM_KIND {
        confirm_job(env, provider, &lease, claimed, &opts.data_dir, emit)
    } else {
        playback_job(env, &opts.cache_dir, &lease, claimed, emit)
    };
    match result {
        Ok(message) => {
            lease.release(env.lib)?;
            env.lib.write(|tx| queue.remove_finished(tx, &job.kind))?;
            Ok(Outcome::Done(message))
        }
        Err(e) => {
            let _ = lease.fail(env.lib, e.to_string());
            let _ = lease.release(env.lib);
            Err(e)
        }
    }
}

// ---- reidentify ----

/// Claims the reevaluation job and runs it for at most
/// [`MEETINGS_PER_RUN`] meetings ([`crate::reidentify::run`]), forwarding
/// its counts-only progress lines. After a successful run the job may be
/// tried again in this [`work_queue`] call when it is claimable again: it
/// went back to the queue to continue (last in line by then, so every other
/// job queued meanwhile runs first) or a later job requested it again. A
/// done or waiting job is not claimable, so this never loops.
fn reidentify_job(
    env: &Env<'_>,
    opts: &WorkerOptions,
    queue: &Queue,
    job: &Job,
    emit: EventSink<'_>,
    tried: &mut HashSet<JobId>,
    changed: &mut Vec<MeetingId>,
) -> Result<Outcome> {
    let Some(lease) = queue.claim(env.lib, &job.id, env.probe)? else {
        return not_claimed(queue, job);
    };
    let result = crate::reidentify::run(
        env.lib,
        &lease,
        &opts.data_dir,
        MEETINGS_PER_RUN,
        &mut |line| {
            emit(&WorkerEvent::job(
                WorkerEventKind::Progress,
                job,
                None,
                Some(line.to_owned()),
            ));
        },
    );
    match result {
        Ok(out) => {
            lease.release(env.lib)?;
            changed.extend(out.changed);
            tried.remove(&job.id);
            Ok(Outcome::Done(out.message))
        }
        Err(e) => {
            let _ = lease.fail(env.lib, e.to_string());
            let _ = lease.release(env.lib);
            Err(e)
        }
    }
}

// ---- refresh-vectors ----

/// The `refresh-vectors` job's queue key and input for `meeting`.
pub fn vectors_job_input(meeting: &MeetingId) -> (String, serde_json::Value) {
    (
        meeting.to_string(),
        serde_json::json!({ "meeting_id": meeting }),
    )
}

/// Embedding rounds one job runs while the meeting's windows keep changing
/// under it; a change after the last one queues the job again.
const VECTOR_ROUNDS: usize = 3;

/// Embeds the meeting's windows missing a vector with the text model the
/// config names now (the user may have changed consent since the job was
/// queued). `meeting` gets the meeting the claimed input names.
fn vectors_job(
    env: &Env<'_>,
    opts: &WorkerOptions,
    queue: &Queue,
    job: &Job,
    meeting: &mut Option<MeetingId>,
) -> Result<Outcome> {
    run_vectors_job(env, queue, job, meeting, || {
        let config = broscribe_core::config::Config::load(&opts.data_dir)?;
        crate::vectors::text_model(
            &config,
            &opts.data_dir,
            opts.secrets.as_ref(),
            env.execution,
        )
    })
}

/// Claims the job, embeds until the meeting its claimed input names
/// (`claimed`) stops changing ([`refresh_meeting_vectors`]), then removes
/// it. A separation committed after the last round enqueued the meeting's
/// key while this job still existed, which kept this job; the write that
/// removes it compares the meeting's window revision with the one embedded
/// and queues the job again when it moved, so no request is lost.
fn run_vectors_job(
    env: &Env<'_>,
    queue: &Queue,
    job: &Job,
    claimed: &mut Option<MeetingId>,
    model: impl FnOnce() -> Result<crate::vectors::TextModel>,
) -> Result<Outcome> {
    #[derive(Deserialize)]
    struct Raw {
        meeting_id: String,
    }
    let Some(lease) = queue.claim(env.lib, &job.id, env.probe)? else {
        return not_claimed(queue, job);
    };
    let result = (|| -> Result<_> {
        let raw: Raw = input(lease.claimed_job())?;
        let meeting = MeetingId::parse(raw.meeting_id.trim())?;
        *claimed = Some(meeting.clone());
        let (message, embedded) = refresh_meeting_vectors(env.lib, &model()?, &meeting)?;
        lease.commit(env.lib, JobState::Done, |_, _| Ok(()))?;
        Ok((message, meeting, embedded))
    })();
    let (message, meeting, embedded) = match result {
        Ok(done) => done,
        Err(e) => {
            let _ = lease.fail(env.lib, e.to_string());
            let _ = lease.release(env.lib);
            return Err(e);
        }
    };
    lease.release(env.lib)?;
    let requeued = env.lib.write(|tx| {
        queue.remove_finished(tx, VECTORS_KIND)?;
        let Some(embedded) = embedded else {
            return Ok(false);
        };
        if Index::open_readonly(tx.root())?.window_revision(&meeting)? == embedded {
            return Ok(false);
        }
        let (key, input) = vectors_job_input(&meeting);
        queue.enqueue(tx, VECTORS_KIND, &key, input)?;
        Ok(true)
    })?;
    Ok(Outcome::Done(if requeued {
        format!("{message}; the meeting changed meanwhile, queued again")
    } else {
        message
    }))
}

/// [`crate::vectors::embed_missing`] for `meeting` when `model` is
/// available, again while its windows changed during a round (at most
/// [`VECTOR_ROUNDS`]). Returns the job's completion message and the window
/// revision whose windows were all embedded (`None` without a model).
fn refresh_meeting_vectors(
    lib: &Library,
    model: &crate::vectors::TextModel,
    meeting: &MeetingId,
) -> Result<(String, Option<Option<String>>)> {
    let Some(embedder) = model.embedder() else {
        return Ok((
            "no text-embedding model is available; nothing to embed".into(),
            None,
        ));
    };
    let revision = || Index::open_readonly(lib.root())?.window_revision(meeting);
    let mut embedded = 0;
    let mut current = revision()?;
    let mut round = 1;
    loop {
        let start = current;
        embedded += crate::vectors::embed_missing(lib, embedder, Some(meeting))?.embedded;
        current = revision()?;
        if current == start || round == VECTOR_ROUNDS {
            return Ok((format!("{embedded} search vector(s) embedded"), Some(start)));
        }
        round += 1;
    }
}

// ---- confirm-samples ----

/// A confirm-samples job's input, parsed.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfirmInput {
    pub meeting_id: MeetingId,
    pub speaker_id: SpeakerId,
    pub person_id: PersonId,
    pub model: String,
}

impl ConfirmInput {
    /// The job's queue key: `<meeting>/<speaker>/<person>/<model>`.
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.meeting_id, self.speaker_id, self.person_id, self.model
        )
    }

    /// The job's queue input.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "meeting_id": self.meeting_id,
            "speaker_id": self.speaker_id.to_string(),
            "person_id": self.person_id,
            "model": self.model,
        })
    }

    pub fn from_json(job: &Job) -> Result<Self> {
        #[derive(Deserialize)]
        struct Raw {
            meeting_id: String,
            speaker_id: String,
            person_id: String,
            model: String,
        }
        let raw: Raw = input(job)?;
        Ok(Self {
            meeting_id: MeetingId::parse(raw.meeting_id.trim())?,
            speaker_id: SpeakerId::parse(raw.speaker_id.trim().to_ascii_uppercase().as_str())?,
            person_id: PersonId::parse(raw.person_id.trim())?,
            model: raw.model,
        })
    }
}

/// Why the confirmation no longer applies, if it does not: another active
/// model, the person forgotten, the meeting or speaker gone, the speaker no
/// longer confirmed as this person, or the person's voice forgotten
/// (forget-voice clears the centroids of their speakers).
pub fn confirm_stale_reason(
    lib: &Library,
    inp: &ConfirmInput,
    active_model: &str,
) -> Result<Option<&'static str>> {
    if inp.model != active_model {
        return Ok(Some("the active speaker-embedding model changed"));
    }
    if !lib
        .person_dir(&inp.person_id)
        .join(paths::PERSON_MD)
        .is_file()
    {
        return Ok(Some("the person was forgotten"));
    }
    let meeting = match lib.load_meeting(&inp.meeting_id) {
        Ok(m) => m,
        Err(Error::NotFound(_)) => return Ok(Some("the meeting was deleted")),
        Err(e) => return Err(e),
    };
    let Some(speaker) = meeting.speaker(inp.speaker_id) else {
        return Ok(Some("the speaker no longer exists"));
    };
    if speaker.assignment != Assignment::Confirmed
        || speaker.person_id.as_ref() != Some(&inp.person_id)
    {
        return Ok(Some("the speaker is no longer named as this person"));
    }
    if speaker.centroid.is_none() && speaker.live_promotion.is_none() {
        return Ok(Some("the person's voice was forgotten"));
    }
    Ok(None)
}

fn confirm_job(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    lease: &LeaseHandle,
    job: &Job,
    data_dir: &Path,
    emit: EventSink<'_>,
) -> Result<String> {
    let inp = ConfirmInput::from_json(job)?;
    let live: LiveConfirmationFence = input(job)?;
    // The active model as configured now, not when the worker started.
    let active = broscribe_core::config::Config::load(data_dir)?
        .profile
        .speaker_embedding;
    if let Some(reason) = confirm_stale_reason(env.lib, &inp, &active)? {
        lease.commit(env.lib, JobState::Done, |_, _| Ok(()))?;
        return Ok(format!("skipped: {reason}"));
    }
    // The speaker IDs the samples are taken under; read before sampling, so
    // a reanalysis that lands meanwhile shows as a different namespace.
    let meeting = env.lib.load_meeting(&inp.meeting_id)?;
    if !live.matches(&meeting, inp.speaker_id) {
        lease.commit(env.lib, JobState::Done, |_, _| Ok(()))?;
        return Ok("skipped: the recording-time name changed".into());
    }
    let namespace = meeting.speaker_namespace;
    emit(&WorkerEvent::job(
        WorkerEventKind::Progress,
        job,
        Some(0.1),
        Some("embedding the speaker's turns".into()),
    ));
    let embedder = provider.load_embedder(&inp.model)?;
    let samples = confirmation_samples_for(
        env.lib,
        env.decoders,
        embedder.as_ref(),
        &inp.meeting_id,
        inp.speaker_id,
        Some(&inp.person_id),
    )?;
    emit(&WorkerEvent::job(
        WorkerEventKind::Progress,
        job,
        Some(0.9),
        Some("saving voice samples".into()),
    ));
    // The active model may have changed while the samples were embedded:
    // revalidate against the configuration as it is now, under the lock.
    lease.commit(env.lib, JobState::Done, |tx, _| {
        let active = broscribe_core::config::Config::load(data_dir)?
            .profile
            .speaker_embedding;
        commit_confirmation(tx, &inp, &active, &namespace, &samples, &live)
    })
}

#[derive(Default, Deserialize)]
struct LiveConfirmationFence {
    live_promotion_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    live_speaker_namespace: Option<String>,
}

impl LiveConfirmationFence {
    fn matches(&self, meeting: &broscribe_core::schema::Meeting, speaker: SpeakerId) -> bool {
        self.live_promotion_at.is_none_or(|at| {
            self.live_speaker_namespace.as_deref() == Some(&meeting.speaker_namespace)
                && meeting
                    .speaker(speaker)
                    .and_then(|s| s.live_promotion.as_ref())
                    .is_some_and(|promotion| promotion.at == at)
        })
    }

    fn kind(&self) -> VoiceprintKind {
        if self.live_promotion_at.is_some() {
            VoiceprintKind::LiveConfirmation
        } else {
            VoiceprintKind::Confirmation
        }
    }
}

/// Inside the commit: revalidates, replaces the centroid voiceprint naming
/// added for this meeting with one confirmed voiceprint per sample (the IDs
/// `speakers name` gives them), caps the person's samples and updates the
/// index. `namespace` is the meeting's speaker namespace the samples were
/// taken under: they are stamped with it, and refused when the meeting has
/// been reanalysed since (their speaker ID may name another speaker now).
fn commit_confirmation(
    tx: &mut broscribe_core::WriteTx<'_>,
    inp: &ConfirmInput,
    active_model: &str,
    namespace: &str,
    samples: &[ConfirmationSample],
    live: &LiveConfirmationFence,
) -> Result<String> {
    let lib = tx.library().clone();
    if let Some(reason) = confirm_stale_reason(&lib, inp, active_model)? {
        return Ok(format!("skipped: {reason}"));
    }
    let meeting = lib.load_meeting(&inp.meeting_id)?;
    if meeting.speaker_namespace != namespace {
        return Ok("skipped: the meeting was reanalysed while its samples were taken".into());
    }
    if !live.matches(&meeting, inp.speaker_id) {
        return Ok("skipped: the recording-time name changed".into());
    }
    if samples.is_empty() {
        return Ok("no audio to sample: the centroid voiceprint stays".into());
    }
    let centroid = meeting
        .speaker(inp.speaker_id)
        .and_then(|s| s.centroid.as_ref())
        .map(|c| c.vector())
        .transpose()?;
    let user = broscribe_core::ops::UserSpeakerEvidence::of(&meeting);
    let prints: Vec<Voiceprint> = samples
        .iter()
        .filter(|s| {
            user.allows_sample(
                inp.speaker_id,
                &inp.person_id,
                f64::from(s.start),
                f64::from(s.end),
            )
        })
        .map(|s| {
            sample_voiceprint(
                &inp.person_id,
                &inp.model,
                &meeting,
                inp.speaker_id,
                namespace,
                s,
                live.kind(),
            )
        })
        .collect();
    let n = prints.len();
    if n == 0 {
        return Ok("no eligible audio to sample".into());
    }
    let new_ids: HashSet<String> = prints.iter().map(|p| p.id.clone()).collect();
    let mut vps = lib.load_voiceprints(&inp.person_id)?;
    let before = vps.clone();
    if let Some(c) = &centroid {
        let mut kept = Vec::with_capacity(vps.embeddings.len());
        for e in vps.embeddings.drain(..) {
            let is_centroid = !new_ids.contains(&e.id)
                && e.model == inp.model
                && e.source.kind == live.kind()
                && e.source.meeting_id.as_ref() == Some(&inp.meeting_id)
                && identity::cosine(&e.vector()?, c) > 0.9999;
            if !is_centroid {
                kept.push(e);
            }
        }
        vps.embeddings = kept;
    }
    // Samples already stored keep their record (and creation time).
    for p in prints {
        if !vps.embeddings.iter().any(|e| e.id == p.id) {
            vps.embeddings.push(p);
        }
    }
    cap_confirmed(&mut vps, &inp.model)?;
    if vps != before {
        // The confirmed evidence changes: request reevaluation, this
        // meeting first, before the voiceprints are written. Unchanged
        // samples (a retry) request nothing.
        broscribe_core::learning::request(tx, &inp.model, Some(&inp.meeting_id))?;
        tx.save_voiceprints(&vps)?;
        let person = lib.load_person(&inp.person_id)?;
        let mut index = Index::open(tx.root())?;
        index.upsert_person(tx, &person)?;
    }
    Ok(format!("{n} voice samples confirmed"))
}

/// The voiceprint `ops::name_speaker` stores for a confirmation sample.
fn sample_voiceprint(
    person: &PersonId,
    model: &str,
    meeting: &Meeting,
    speaker: SpeakerId,
    namespace: &str,
    s: &ConfirmationSample,
    kind: VoiceprintKind,
) -> Voiceprint {
    let (start, end) = (f64::from(s.start), f64::from(s.end));
    let mut v = s.vector.clone();
    identity::l2_normalize(&mut v);
    Voiceprint {
        id: schema::voiceprint_id(
            person,
            model,
            VoiceprintOrigin::of_meeting(meeting),
            start,
            end,
            kind,
        ),
        model: model.to_owned(),
        dim: v.len(),
        vector_b64: schema::encode_vector(&v),
        source: VoiceprintSource {
            kind,
            meeting_id: Some(meeting.id.clone()),
            sha256: meeting.recording.as_ref().map(|r| r.sha256.clone()),
            start,
            end,
            ranges: Vec::new(),
            speaker_id: Some(speaker),
            speaker_namespace: namespace.to_owned(),
        },
        device: None,
        strength: Strength::Confirmed,
        created_at: schema::now(),
    }
}

// ---- playback-cache ----

#[derive(Deserialize)]
struct PlaybackInput {
    meeting_id: String,
}

/// `<cache_dir>/playback/<sha256>.<token>.part`: the WAV one lease holder
/// writes before publishing it. It starts with the recording's hash, so
/// whatever deletes that recording's caches deletes it too.
fn playback_staging_path(cache_dir: &Path, sha256: &str, token: &str) -> PathBuf {
    cache_dir
        .join(PLAYBACK_CACHE_DIR)
        .join(format!("{sha256}.{token}.part"))
}

/// Whether meeting `id` still has the available recording `sha256` (read
/// in the caller's library write).
fn playback_source_current(lib: &Library, id: &MeetingId, sha256: &str) -> Result<bool> {
    let meeting = match lib.load_meeting(id) {
        Ok(m) => m,
        Err(Error::NotFound(_)) => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(meeting
        .recording
        .as_ref()
        .is_some_and(|r| r.sha256 == sha256 && r.state == RecordingState::Available))
}

/// Writes `bytes` to a new file at `path` (never over an existing one) and
/// syncs it.
fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| Error::io_at(path, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| Error::io_at(path, e))
}

/// Decodes the claimed meeting's recording into a playback WAV, staged
/// under this lease's token and renamed into place inside the lease-checked
/// commit, after checking under the library lock that the recording still
/// exists as decoded. A worker whose lease was reclaimed, or whose
/// recording was deleted (or replaced) meanwhile, publishes nothing: a
/// deleted recording's audio is never recreated in the cache.
fn playback_job(
    env: &Env<'_>,
    cache_dir: &Path,
    lease: &LeaseHandle,
    job: &Job,
    emit: EventSink<'_>,
) -> Result<String> {
    let inp: PlaybackInput = input(job)?;
    let id = MeetingId::parse(inp.meeting_id.trim())?;
    let meeting = env.lib.load_meeting(&id)?;
    let Some(rec) = meeting.recording.as_ref() else {
        return Err(Error::NotFound(format!("{id} has no recording")));
    };
    let sha256 = rec.sha256.clone();
    let target = playback_cache_path(cache_dir, &sha256);
    if target.is_file() {
        lease.commit(env.lib, JobState::Done, |_, _| Ok(()))?;
        return Ok(format!("already cached at {}", target.display()));
    }
    emit(&WorkerEvent::job(
        WorkerEventKind::Progress,
        job,
        Some(0.1),
        Some("decoding".into()),
    ));
    let samples = crate::audio::playback_mix(env.lib, env.decoders, &meeting)?;
    emit(&WorkerEvent::job(
        WorkerEventKind::Progress,
        job,
        Some(0.8),
        Some("writing the playback cache".into()),
    ));
    let bytes = wav_16k_bytes(&samples)?;
    drop(samples);
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io_at(dir, e))?;
    }
    let staged = playback_staging_path(cache_dir, &sha256, lease.token());
    let published = write_new_synced(&staged, &bytes).and_then(|()| {
        lease.commit(env.lib, JobState::Done, |tx, _| {
            if !playback_source_current(tx.library(), &id, &sha256)? {
                return Ok(false);
            }
            std::fs::rename(&staged, &target).map_err(|e| Error::io_at(&target, e))?;
            if let Some(dir) = target.parent()
                && let Ok(d) = std::fs::File::open(dir)
            {
                let _ = d.sync_all();
            }
            Ok(true)
        })
    });
    if !matches!(published, Ok(true)) {
        // Not ours to publish (anymore): nothing of it stays.
        match std::fs::remove_file(&staged) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("removing a staged playback cache: {}", e.kind()),
        }
    }
    match published? {
        true => Ok(format!("cached at {}", target.display())),
        false => Ok("skipped: the recording was deleted or replaced meanwhile".into()),
    }
}

// ---- WAV ----

/// 16-bit PCM mono WAV at 16 kHz.
pub fn wav_16k_bytes(samples: &[f32]) -> Result<Vec<u8>> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| n.checked_add(36).is_some())
        .ok_or_else(|| Error::InvalidInput("audio too long for a WAV file".into()))?;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&TARGET_RATE.to_le_bytes());
    b.extend_from_slice(&(TARGET_RATE * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for &x in samples {
        let v = (x.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    Ok(b)
}

/// Writes [`wav_16k_bytes`] atomically; the folder must exist.
pub fn write_wav_16k(path: &Path, samples: &[f32]) -> Result<()> {
    fsutil::write_atomic(path, &wav_16k_bytes(samples)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{ImportOptions, import_file};
    use crate::testutil::{Fixture, models};
    use broscribe_core::ops;
    use broscribe_models::fakes::FakeModelProvider;

    fn run(fx: &Fixture, cache: &Path) -> (WorkerSummary, Vec<WorkerEvent>) {
        let mut events = Vec::new();
        let summary = work_queue(
            &fx.env(),
            &FakeModelProvider::default(),
            &WorkerOptions {
                cache_dir: cache.to_path_buf(),
                once: false,
                encoder: Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
                data_dir: fx._tmp.path().to_owned(),
                secrets: Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
                calendar_authority: Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
            },
            &mut |e| events.push(e.clone()),
        )
        .unwrap();
        (summary, events)
    }

    #[test]
    fn empty_queue_is_idle() {
        let fx = Fixture::new();
        let cache = fx._tmp.path().join("cache");
        let (summary, events) = run(&fx, &cache);
        assert_eq!(summary, WorkerSummary::default());
        assert_eq!(events, [WorkerEvent::idle()]);
        let line = serde_json::to_string(&events[0]).unwrap();
        assert_eq!(line, r#"{"event":"idle"}"#);
    }

    #[test]
    fn playback_cache_and_confirm_samples_run_and_are_removed() {
        let fx = Fixture::new();
        let file = fx.wav("p.wav", &[(Some(0), 4.0), (Some(1), 4.0), (Some(0), 2.0)]);
        let mut m = models();
        let out = import_file(&fx.env(), &mut m, &file, &ImportOptions::default()).unwrap();
        let s1 = SpeakerId::new(1).unwrap();
        let model = fx.config.profile.speaker_embedding.clone();

        // The app's naming: centroid fallback now, samples later.
        let named = ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(
                tx,
                index,
                &out.meeting_id,
                s1,
                "Ana Example",
                Vec::new(),
                &model,
            )
        })
        .unwrap();
        let confirm = ConfirmInput {
            meeting_id: out.meeting_id.clone(),
            speaker_id: s1,
            person_id: named.person_id.clone(),
            model: model.clone(),
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        fx.lib
            .write(|tx| {
                queue.enqueue(tx, CONFIRM_KIND, &confirm.key(), confirm.to_json())?;
                queue.enqueue(
                    tx,
                    PLAYBACK_KIND,
                    &format!("{}/x", out.meeting_id),
                    serde_json::json!({ "meeting_id": out.meeting_id }),
                )
            })
            .unwrap();

        let cache = fx._tmp.path().join("cache");
        let (summary, events) = run(&fx, &cache);
        // The naming's identity reevaluation ran first; the confirmed
        // samples changed the voice, so it ran again after them.
        assert_eq!(summary.done, 4, "{events:?}");
        let status = broscribe_core::learning::status(&fx.lib, &model)
            .unwrap()
            .unwrap();
        assert_eq!(
            (status.state, status.requested, status.processed),
            (JobState::Done, 2, 2)
        );
        assert_eq!(events.last().unwrap().event, WorkerEventKind::Idle);
        assert!(queue.list(CONFIRM_KIND).unwrap().is_empty());
        assert!(queue.list(PLAYBACK_KIND).unwrap().is_empty());

        let sha = fx
            .lib
            .load_meeting(&out.meeting_id)
            .unwrap()
            .recording
            .unwrap()
            .sha256;
        assert!(playback_cache_path(&cache, &sha).is_file());

        let vps = fx.lib.load_voiceprints(&named.person_id).unwrap();
        let from_meeting: Vec<_> = vps
            .embeddings
            .iter()
            .filter(|e| e.source.meeting_id.as_ref() == Some(&out.meeting_id))
            .collect();
        assert_eq!(from_meeting.len(), 1, "centroid replaced by the samples");
        assert!(from_meeting[0].source.end > from_meeting[0].source.start + 1.5);

        // Running the same job again changes nothing (deterministic IDs).
        fx.lib
            .write(|tx| queue.enqueue(tx, CONFIRM_KIND, &confirm.key(), confirm.to_json()))
            .unwrap();
        run(&fx, &cache);
        assert_eq!(fx.lib.load_voiceprints(&named.person_id).unwrap(), vps);
    }

    #[test]
    fn refresh_vectors_jobs_run_once_per_meeting_and_are_removed() {
        let fx = Fixture::new();
        let file = fx.wav("v.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
        let mut m = models();
        let out = import_file(&fx.env(), &mut m, &file, &ImportOptions::default()).unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let (key, input) = vectors_job_input(&out.meeting_id);
        // Two writes before the worker runs: one job.
        for _ in 0..2 {
            fx.lib
                .write(|tx| queue.enqueue(tx, VECTORS_KIND, &key, input.clone()))
                .unwrap();
        }
        assert_eq!(queue.list(VECTORS_KIND).unwrap().len(), 1);

        let (summary, events) = run(&fx, &fx._tmp.path().join("cache"));
        assert_eq!(summary.done, 1, "{events:?}");
        assert_eq!(summary.refreshed, std::slice::from_ref(&out.meeting_id));
        assert!(summary.named.is_empty() && summary.imported.is_empty());
        // No text model is installed in the fixture: nothing to embed.
        assert!(events.iter().any(|e| {
            e.event == WorkerEventKind::JobDone
                && e.kind.as_deref() == Some(VECTORS_KIND)
                && e.message
                    .as_deref()
                    .is_some_and(|m| m.contains("nothing to embed"))
        }));
        assert!(queue.list(VECTORS_KIND).unwrap().is_empty());
    }

    #[test]
    fn refreshing_a_meeting_embeds_its_missing_windows_once() {
        let fx = Fixture::new();
        let mut m = models();
        let a = import_file(
            &fx.env(),
            &mut m,
            &fx.wav("a.wav", &[(Some(0), 4.0), (Some(1), 4.0)]),
            &ImportOptions::default(),
        )
        .unwrap();
        let model = crate::vectors::TextModel::Ready(Box::new(
            broscribe_models::FakeTextEmbedder::new("fake-text", 256),
        ));
        let (first, _) = refresh_meeting_vectors(&fx.lib, &model, &a.meeting_id).unwrap();
        assert_ne!(first, "0 search vector(s) embedded", "{first}");
        assert!(first.ends_with("search vector(s) embedded"), "{first}");
        // Done: a second run embeds nothing.
        assert_eq!(
            refresh_meeting_vectors(&fx.lib, &model, &a.meeting_id)
                .unwrap()
                .0,
            "0 search vector(s) embedded"
        );
        let unavailable = crate::vectors::TextModel::Unavailable {
            id: "none".into(),
            reason: "not installed".into(),
        };
        let (message, embedded) =
            refresh_meeting_vectors(&fx.lib, &unavailable, &a.meeting_id).unwrap();
        assert!(message.contains("nothing to embed"));
        assert_eq!(embedded, None);
    }

    /// A text model whose first `separations` calls each commit what the
    /// app's naming does when it separates a member: the meeting's windows
    /// change and its refresh job is enqueued, in one write.
    struct SeparatingEmbedder {
        lib: Library,
        meeting: MeetingId,
        separations: usize,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl broscribe_models::TextEmbedder for SeparatingEmbedder {
        fn model_key(&self) -> &str {
            "fake-text"
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call < self.separations {
                self.lib.write(|tx| {
                    let mut m = tx.library().load_meeting(&self.meeting)?;
                    m.segments[0].text.push_str(&format!(" edit {call}"));
                    tx.save_meeting(&m)?;
                    Index::open(tx.root())?.upsert_meeting(tx, &m)?;
                    let (key, input) = vectors_job_input(&self.meeting);
                    Queue::open(tx.root())?.enqueue(tx, VECTORS_KIND, &key, input)
                })?;
            }
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
        }
    }

    /// Runs the meeting's queued refresh job with `separations` concurrent
    /// separations during embedding.
    fn refresh_during_separations(separations: usize) -> (Fixture, MeetingId, Outcome) {
        let fx = Fixture::new();
        let mut m = models();
        let out = import_file(
            &fx.env(),
            &mut m,
            &fx.wav("c.wav", &[(Some(0), 4.0), (Some(1), 4.0)]),
            &ImportOptions::default(),
        )
        .unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let (key, input) = vectors_job_input(&out.meeting_id);
        let id = fx
            .lib
            .write(|tx| queue.enqueue(tx, VECTORS_KIND, &key, input))
            .unwrap();
        let job = queue.get(&id).unwrap();
        let embedder = SeparatingEmbedder {
            lib: fx.lib.clone(),
            meeting: out.meeting_id.clone(),
            separations,
            calls: Default::default(),
        };
        let mut claimed = None;
        let outcome = run_vectors_job(&fx.env(), &queue, &job, &mut claimed, || {
            Ok(crate::vectors::TextModel::Ready(Box::new(embedder)))
        })
        .unwrap();
        assert_eq!(claimed.as_ref(), Some(&out.meeting_id));
        (fx, out.meeting_id, outcome)
    }

    /// Window texts of `meeting` with no `fake-text` vector.
    fn missing_vectors(fx: &Fixture, meeting: &MeetingId) -> usize {
        let fake = broscribe_models::FakeTextEmbedder::new("fake-text", 4);
        crate::vectors::embed_missing(&fx.lib, &fake, Some(meeting))
            .unwrap()
            .pending
    }

    #[test]
    fn a_separation_during_embedding_is_embedded_by_the_same_job() {
        let (fx, meeting, outcome) = refresh_during_separations(1);
        let Outcome::Done(message) = outcome else {
            panic!("not done");
        };
        assert!(!message.contains("queued again"), "{message}");
        // The separation's enqueue found the running job; the job saw the
        // windows move and embedded the new ones before finishing.
        let queue = Queue::open(fx.lib.root()).unwrap();
        assert!(queue.list(VECTORS_KIND).unwrap().is_empty());
        assert_eq!(missing_vectors(&fx, &meeting), 0);
    }

    #[test]
    fn separations_outlasting_the_rounds_queue_the_job_again() {
        let (fx, _meeting, outcome) = refresh_during_separations(VECTOR_ROUNDS);
        let Outcome::Done(message) = outcome else {
            panic!("not done");
        };
        assert!(message.contains("queued again"), "{message}");
        let jobs = Queue::open(fx.lib.root())
            .unwrap()
            .list(VECTORS_KIND)
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, JobState::Queued);
    }

    #[test]
    fn confirm_after_forget_commits_nothing() {
        let fx = Fixture::new();
        let file = fx.wav("f.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
        let mut m = models();
        let out = import_file(&fx.env(), &mut m, &file, &ImportOptions::default()).unwrap();
        let s1 = SpeakerId::new(1).unwrap();
        let model = fx.config.profile.speaker_embedding.clone();
        let named = ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(
                tx,
                index,
                &out.meeting_id,
                s1,
                "Bo Example",
                Vec::new(),
                &model,
            )
        })
        .unwrap();
        ops::with_index(&fx.lib, |tx, index| {
            ops::forget_voice(tx, index, named.person_id.as_str())
        })
        .unwrap();
        let confirm = ConfirmInput {
            meeting_id: out.meeting_id.clone(),
            speaker_id: s1,
            person_id: named.person_id.clone(),
            model,
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        fx.lib
            .write(|tx| queue.enqueue(tx, CONFIRM_KIND, &confirm.key(), confirm.to_json()))
            .unwrap();
        let (summary, events) = run(&fx, &fx._tmp.path().join("cache"));
        // The reevaluation the naming and the forget requested ran too.
        assert_eq!(summary.done, 2, "{events:?}");
        let done = events
            .iter()
            .find(|e| {
                e.event == WorkerEventKind::JobDone && e.kind.as_deref() == Some(CONFIRM_KIND)
            })
            .unwrap();
        assert!(
            done.message.as_deref().unwrap().starts_with("skipped"),
            "{done:?}"
        );
        assert!(
            fx.lib
                .load_voiceprints(&named.person_id)
                .unwrap()
                .embeddings
                .is_empty()
        );
    }

    #[test]
    fn confirm_commit_rechecks_the_model_configured_at_commit_time() {
        let fx = Fixture::new();
        let file = fx.wav("m.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
        let out = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        let s1 = SpeakerId::new(1).unwrap();
        let model = fx.config.profile.speaker_embedding.clone();
        let named = ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(
                tx,
                index,
                &out.meeting_id,
                s1,
                "Cy Example",
                Vec::new(),
                &model,
            )
        })
        .unwrap();
        let before = fx.lib.load_voiceprints(&named.person_id).unwrap();
        let confirm = ConfirmInput {
            meeting_id: out.meeting_id.clone(),
            speaker_id: s1,
            person_id: named.person_id.clone(),
            model,
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        fx.lib
            .write(|tx| queue.enqueue(tx, CONFIRM_KIND, &confirm.key(), confirm.to_json()))
            .unwrap();
        // The worker started with the old configuration; the user switched
        // the embedding model since.
        let mut switched = fx.config.clone();
        switched.profile.speaker_embedding = "another-embedder".into();
        switched.save(fx._tmp.path()).unwrap();
        let (summary, events) = run(&fx, &fx._tmp.path().join("cache"));
        // The naming's reevaluation also stops: its model is no longer active.
        assert_eq!(summary.done, 2, "{events:?}");
        let done = events
            .iter()
            .find(|e| {
                e.event == WorkerEventKind::JobDone && e.kind.as_deref() == Some(CONFIRM_KIND)
            })
            .unwrap();
        assert_eq!(
            done.message.as_deref(),
            Some("skipped: the active speaker-embedding model changed")
        );
        assert_eq!(fx.lib.load_voiceprints(&named.person_id).unwrap(), before);
    }

    #[test]
    fn confirm_commit_refuses_samples_taken_before_a_reanalysis() {
        let fx = Fixture::new();
        let file = fx.wav("r.wav", &[(Some(0), 4.0), (Some(1), 4.0), (Some(0), 2.0)]);
        let mut m = models();
        let out = import_file(&fx.env(), &mut m, &file, &ImportOptions::default()).unwrap();
        let s1 = SpeakerId::new(1).unwrap();
        let model = fx.config.profile.speaker_embedding.clone();
        let named = ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(
                tx,
                index,
                &out.meeting_id,
                s1,
                "Eva Example",
                Vec::new(),
                &model,
            )
        })
        .unwrap();
        let confirm = ConfirmInput {
            meeting_id: out.meeting_id.clone(),
            speaker_id: s1,
            person_id: named.person_id.clone(),
            model: model.clone(),
        };
        let sampled_under = fx
            .lib
            .load_meeting(&out.meeting_id)
            .unwrap()
            .speaker_namespace;
        let samples =
            confirmation_samples(&fx.lib, &fx.chain, m.embedder.as_ref(), &out.meeting_id, s1)
                .unwrap();
        assert!(!samples.is_empty());
        // A reanalysis renumbered the speakers while the samples were taken.
        let reanalysed = "reanalysed";
        fx.lib
            .write(|tx| {
                let mut meeting = tx.library().load_meeting(&out.meeting_id)?;
                meeting.speaker_namespace = reanalysed.into();
                tx.save_meeting(&meeting)
            })
            .unwrap();
        let before = fx.lib.load_voiceprints(&named.person_id).unwrap();
        let message = fx
            .lib
            .write(|tx| {
                commit_confirmation(
                    tx,
                    &confirm,
                    &model,
                    &sampled_under,
                    &samples,
                    &LiveConfirmationFence::default(),
                )
            })
            .unwrap();
        assert!(message.starts_with("skipped"), "{message}");
        assert_eq!(fx.lib.load_voiceprints(&named.person_id).unwrap(), before);

        // Taken under the current analysis: stamped with its namespace.
        let message = fx
            .lib
            .write(|tx| {
                commit_confirmation(
                    tx,
                    &confirm,
                    &model,
                    reanalysed,
                    &samples,
                    &LiveConfirmationFence::default(),
                )
            })
            .unwrap();
        assert!(message.ends_with("voice samples confirmed"), "{message}");
        let stamped: Vec<_> = fx
            .lib
            .load_voiceprints(&named.person_id)
            .unwrap()
            .embeddings
            .into_iter()
            .filter(|e| e.source.meeting_id.as_ref() == Some(&out.meeting_id))
            .map(|e| (e.source.speaker_id, e.source.speaker_namespace))
            .collect();
        assert!(!stamped.is_empty());
        assert!(
            stamped
                .iter()
                .all(|(s, ns)| *s == Some(s1) && ns == reanalysed),
            "{stamped:?}"
        );
    }

    #[test]
    fn confirm_accepts_a_job_for_the_model_activated_after_the_worker_started() {
        let fx = Fixture::new();
        let file = fx.wav("n.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
        let out = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        let s1 = SpeakerId::new(1).unwrap();
        let named = ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(
                tx,
                index,
                &out.meeting_id,
                s1,
                "Di Example",
                Vec::new(),
                &fx.config.profile.speaker_embedding,
            )
        })
        .unwrap();
        // The user switched the embedding model after the worker started
        // (its `env.config` still names the old one); the app queued the
        // confirmation for the new model.
        let mut switched = fx.config.clone();
        switched.profile.speaker_embedding = "another-embedder".into();
        switched.save(fx._tmp.path()).unwrap();
        let confirm = ConfirmInput {
            meeting_id: out.meeting_id.clone(),
            speaker_id: s1,
            person_id: named.person_id.clone(),
            model: "another-embedder".into(),
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        fx.lib
            .write(|tx| queue.enqueue(tx, CONFIRM_KIND, &confirm.key(), confirm.to_json()))
            .unwrap();
        let (summary, events) = run(&fx, &fx._tmp.path().join("cache"));
        // The old model's reevaluation ends, the new samples request one
        // for the new model.
        assert_eq!(summary.done, 3, "{events:?}");
        assert_eq!(summary.named, std::slice::from_ref(&out.meeting_id));
        let done = events
            .iter()
            .find(|e| {
                e.event == WorkerEventKind::JobDone && e.kind.as_deref() == Some(CONFIRM_KIND)
            })
            .unwrap();
        assert!(
            !done.message.as_deref().unwrap().starts_with("skipped"),
            "{done:?}"
        );
    }

    /// Enqueues a setup Try-it import of a session no longer current, with
    /// the staged copy a killed child left behind.
    fn abandoned_setup_import(fx: &Fixture, name: &str) -> JobId {
        let path = fx.wav(name, &[(Some(0), 3.0)]);
        let (sha, _) = crate::audio::file_sha256(&path).unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let input = serde_json::json!({
            "path": path,
            "sha256": sha,
            "handling": "copy",
            "setup_session": (fx._tmp.path(), "ended-session"),
        });
        let id = fx
            .lib
            .write(|tx| queue.enqueue(tx, IMPORT_KIND, &sha, input))
            .unwrap();
        std::fs::copy(&path, queue.job_dir(&id).join("audio.wav")).unwrap();
        id
    }

    #[test]
    fn worker_start_discards_abandoned_setup_imports_without_another_import() {
        let fx = Fixture::new();
        let id = abandoned_setup_import(&fx, "killed-try.wav");
        let queue = Queue::open(fx.lib.root()).unwrap();
        let (summary, events) = run(&fx, &fx._tmp.path().join("cache"));
        assert_eq!(summary.failed, 0, "{events:?}");
        assert!(!queue.job_dir(&id).exists());
        assert!(fx.lib.list_meeting_dirs().unwrap().is_empty());
    }

    #[test]
    fn discarded_setup_import_is_reported_done_not_failed() {
        let fx = Fixture::new();
        let id = abandoned_setup_import(&fx, "ended-try.wav");
        let mut events = Vec::new();
        let summary = work_job(
            &fx.env(),
            &FakeModelProvider::default(),
            &WorkerOptions {
                cache_dir: fx._tmp.path().join("cache"),
                once: false,
                encoder: Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
                data_dir: fx._tmp.path().to_owned(),
                secrets: Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
                calendar_authority: Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
            },
            &id,
            &mut |e| events.push(e.clone()),
        )
        .unwrap();
        assert_eq!((summary.done, summary.failed), (1, 0), "{events:?}");
        let done = events
            .iter()
            .find(|e| e.event == WorkerEventKind::JobDone)
            .unwrap();
        assert!(
            done.message.as_deref().unwrap().starts_with("discarded"),
            "{done:?}"
        );
        assert!(!Queue::open(fx.lib.root()).unwrap().job_dir(&id).exists());
    }

    #[test]
    fn conflicted_move_survives_a_worker_run_on_an_already_imported_replacement() {
        use crate::import::{MOVE_CONFLICT, MOVE_FAIL_AT, MoveJournal, MoveStep};
        use broscribe_core::schema::Handling;
        let fx = Fixture::new();
        // Already in the library: the file that will take the original path.
        let replacement = fx.wav("replacement.wav", &[(Some(0), 5.0)]);
        import_file(
            &fx.env(),
            &mut models(),
            &replacement,
            &ImportOptions::default(),
        )
        .unwrap();
        let source = fx.wav("moved.wav", &[(Some(0), 3.0)]);
        // The move stops after the original went into the job folder.
        MOVE_FAIL_AT.set(Some(MoveStep::Verified));
        let opts = ImportOptions {
            handling: Some(Handling::Move),
            ..Default::default()
        };
        import_file(&fx.env(), &mut models(), &source, &opts).unwrap_err();
        MOVE_FAIL_AT.set(None);
        let queue = Queue::open(fx.lib.root()).unwrap();
        let job = queue
            .list(IMPORT_KIND)
            .unwrap()
            .into_iter()
            .find(|j| j.state != JobState::Done)
            .unwrap();
        let dir = queue.job_dir(&job.id);
        let journal = MoveJournal::load(fx.lib.root(), &dir, crate::import::MOVE_JOURNAL).unwrap();
        let moved = std::fs::read(&journal.staged).unwrap();
        // Another, already imported, file now holds the original path.
        std::fs::copy(&replacement, &source).unwrap();

        let cache = fx._tmp.path().join("cache");
        run(&fx, &cache);
        run(&fx, &cache);
        assert_eq!(std::fs::read(&journal.staged).unwrap(), moved);
        assert!(dir.join(MOVE_CONFLICT).is_file());
        let kept = queue.get(&job.id).unwrap();
        assert_eq!(kept.state, JobState::Failed);
        assert!(kept.keeps_file());
        assert!(
            kept.error
                .as_deref()
                .unwrap()
                .contains("out of the library folder"),
            "{kept:?}"
        );
        // Importing the kept file itself is refused and leaves it intact.
        let refused = import_file(
            &fx.env(),
            &mut models(),
            &journal.staged,
            &ImportOptions::default(),
        )
        .unwrap_err();
        assert!(
            refused.to_string().contains("inside the library folder"),
            "{refused}"
        );
        assert_eq!(std::fs::read(&journal.staged).unwrap(), moved);
    }

    #[test]
    fn processing_job_reports_references_that_need_review() {
        use broscribe_core::schema::{Enrichment, Speaker};
        let fx = Fixture::new();
        let file = fx.wav("r.wav", &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0)]);
        let id = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let ghost = SpeakerId::new(9).unwrap();
        ops::with_index(&fx.lib, |tx, index| {
            let mut m = tx.library().load_meeting(&id)?;
            // A speaker that matches no voice after reanalysis.
            m.speakers.push(Speaker::new(ghost, "t0"));
            m.enrichment = Some(Enrichment {
                provider: "fake".into(),
                model: None,
                requested_model: None,
                model_reported: false,
                created_at: schema::now(),
                stale: false,
                revision: broscribe_core::index::windows::meeting_window_revision(&m),
                attribution_revision: String::new(),
                line_people: Default::default(),
                summary: "{{S9}} agreed.".into(),
                topics: vec![],
                keywords: vec![],
                tags: vec![],
                decisions: vec![],
                actions: vec![],
                instructions: None,
            });
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)
        })
        .unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        fx.lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    REPROCESS_KIND,
                    id.as_str(),
                    serde_json::json!({ "meeting_id": id }),
                )
            })
            .unwrap();
        let (summary, events) = run(&fx, &fx._tmp.path().join("cache"));
        assert_eq!(summary.done, 1, "{events:?}");
        let done = events
            .iter()
            .find(|e| e.event == WorkerEventKind::JobDone)
            .unwrap();
        let message = done.message.as_deref().unwrap();
        assert!(
            message.starts_with("reprocess complete; 1 item(s) need review: ")
                && message.contains("S9"),
            "{message}"
        );
    }

    #[test]
    fn failed_jobs_are_retried_up_to_max_attempts() {
        let fx = Fixture::new();
        let queue = Queue::open(fx.lib.root()).unwrap();
        fx.lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    PLAYBACK_KIND,
                    "m-missing/x",
                    serde_json::json!({ "meeting_id": "m-000000" }),
                )
            })
            .unwrap();
        let cache = fx._tmp.path().join("cache");
        for _ in 0..MAX_ATTEMPTS {
            let (summary, _) = run(&fx, &cache);
            assert_eq!(summary.failed, 1);
        }
        let (summary, events) = run(&fx, &cache);
        assert_eq!(summary, WorkerSummary::default());
        assert_eq!(events, [WorkerEvent::idle()]);
        let job = &queue.list(PLAYBACK_KIND).unwrap()[0];
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.attempts, MAX_ATTEMPTS);
    }

    #[test]
    fn retention_failure_is_reported_and_the_queue_goes_on() {
        let fx = Fixture::new();
        let queue = Queue::open(fx.lib.root()).unwrap();
        for (name, seconds) in [("first.wav", 3.0), ("second.wav", 4.0)] {
            let file = fx.wav(name, &[(Some(0), seconds)]);
            let (sha, _) = crate::audio::file_sha256(&file).unwrap();
            fx.lib
                .write(|tx| {
                    queue.enqueue(
                        tx,
                        IMPORT_KIND,
                        &sha,
                        serde_json::json!({"path": file, "handling": "copy"}),
                    )
                })
                .unwrap();
        }
        RETENTION_FAILS.set(true);
        let (summary, events) = run(&fx, &fx._tmp.path().join("cache"));
        RETENTION_FAILS.set(false);
        assert_eq!(summary.done, 2, "{events:?}");
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.imported.len(), 2);
        assert_eq!(summary.retention_failures.len(), 2);
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event == WorkerEventKind::Progress
                    && e.message
                        .as_deref()
                        .is_some_and(|m| m.starts_with("retention after completion failed")))
                .count(),
            2
        );
        assert_eq!(events.last(), Some(&WorkerEvent::idle()));
        assert_eq!(fx.lib.list_meeting_dirs().unwrap().len(), 2);
    }

    #[test]
    fn import_failing_every_time_stops_at_max_attempts() {
        struct RejectDiarizer;
        impl broscribe_models::Diarizer for RejectDiarizer {
            fn diarize(&self, _: &[f32]) -> Result<Vec<broscribe_models::DiarSegment>> {
                Err(Error::Inference("synthetic diarizer failure".into()))
            }
        }
        struct BrokenProvider;
        impl ModelProvider for BrokenProvider {
            fn load(&self, profile: &broscribe_core::config::Profile) -> Result<ModelSet> {
                let mut set = FakeModelProvider::default().load(profile)?;
                set.diarizer = Box::new(RejectDiarizer);
                Ok(set)
            }
            fn load_vad(&self, model_id: &str) -> Result<Box<dyn broscribe_models::Vad + Send>> {
                FakeModelProvider::default().load_vad(model_id)
            }
            fn load_embedder(
                &self,
                model_id: &str,
            ) -> Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
                FakeModelProvider::default().load_embedder(model_id)
            }
        }
        let fx = Fixture::new();
        let file = fx.wav("always-fails.wav", &[(Some(0), 3.0)]);
        let (sha, _) = crate::audio::file_sha256(&file).unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let id = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    IMPORT_KIND,
                    &sha,
                    serde_json::json!({"path": file, "handling": "copy"}),
                )
            })
            .unwrap();
        let opts = WorkerOptions {
            cache_dir: fx._tmp.path().join("cache"),
            once: false,
            encoder: Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
            data_dir: fx._tmp.path().to_owned(),
            secrets: Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            calendar_authority: Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
        };
        for attempt in 1..=MAX_ATTEMPTS {
            let summary = work_queue(&fx.env(), &BrokenProvider, &opts, &mut |_| {}).unwrap();
            assert_eq!(summary.failed, 1, "attempt {attempt}");
            let job = queue.get(&id).unwrap();
            assert_eq!((job.state, job.attempts), (JobState::Failed, attempt));
        }
        // Exhausted: automatic retries stop; only the user can restart it.
        let mut events = Vec::new();
        let summary = work_queue(&fx.env(), &BrokenProvider, &opts, &mut |e| {
            events.push(e.clone())
        })
        .unwrap();
        assert_eq!(summary, WorkerSummary::default());
        assert_eq!(events, [WorkerEvent::idle()]);
        let job = queue.get(&id).unwrap();
        assert_eq!((job.state, job.attempts), (JobState::Failed, MAX_ATTEMPTS));
    }

    #[test]
    fn wav_header_is_16k_mono_pcm() {
        let b = wav_16k_bytes(&[0.0, 1.0, -1.0]).unwrap();
        assert_eq!(b.len(), 44 + 6);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([b[24], b[25], b[26], b[27]]), 16_000);
    }

    #[test]
    fn work_job_runs_only_that_job() {
        let fx = Fixture::new();
        let file = fx.wav("w.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
        let mut m = models();
        let out = import_file(&fx.env(), &mut m, &file, &ImportOptions::default()).unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let (first, second) = fx
            .lib
            .write(|tx| {
                let input = serde_json::json!({ "meeting_id": out.meeting_id });
                let a = queue.enqueue(tx, PLAYBACK_KIND, "a", input.clone())?;
                let b = queue.enqueue(tx, PLAYBACK_KIND, "b", input)?;
                Ok((a, b))
            })
            .unwrap();
        let opts = WorkerOptions {
            cache_dir: fx._tmp.path().join("cache"),
            once: false,
            encoder: Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
            data_dir: fx._tmp.path().to_owned(),
            secrets: Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            calendar_authority: Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
        };
        let mut events = Vec::new();
        let summary = work_job(
            &fx.env(),
            &FakeModelProvider::default(),
            &opts,
            &second,
            &mut |e| events.push(e.clone()),
        )
        .unwrap();
        assert_eq!(summary.done, 1, "{events:?}");
        assert!(
            events
                .iter()
                .all(|e| e.job_id.as_deref() == Some(second.as_str()))
        );
        assert!(matches!(queue.get(&second), Err(Error::NotFound(_))));
        assert_eq!(queue.get(&first).unwrap().state, JobState::Queued);

        let missing = JobId::for_key(PLAYBACK_KIND, "nope");
        let err = work_job(
            &fx.env(),
            &FakeModelProvider::default(),
            &opts,
            &missing,
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn import_and_reprocess_workers_apply_retention_after_completion() {
        for queue_work in [false, true] {
            for reprocess in [false, true] {
                let mut fx = Fixture::new();
                let file = fx.wav("retained.wav", &[(Some(0), 4.0)]);
                let queue = Queue::open(fx.lib.root()).unwrap();
                let id =
                    if reprocess {
                        let out =
                            import_file(&fx.env(), &mut models(), &file, &ImportOptions::default())
                                .unwrap();
                        fx.lib
                            .write(|tx| {
                                queue.enqueue(tx, REPROCESS_KIND, out.meeting_id.as_str(),
                        serde_json::json!({"meeting_id": out.meeting_id, "asr_only": true}))
                            })
                            .unwrap()
                    } else {
                        let (sha, _) = crate::audio::file_sha256(&file).unwrap();
                        fx.lib
                            .write(|tx| {
                                queue.enqueue(
                                    tx,
                                    IMPORT_KIND,
                                    &sha,
                                    serde_json::json!({"path": file, "handling": "copy"}),
                                )
                            })
                            .unwrap()
                    };
                fx.config.retention = broscribe_core::config::Retention::DeleteImmediately {
                    wait_until_named: false,
                };
                fx.config.save(fx._tmp.path()).unwrap();
                let opts = WorkerOptions::new(
                    fx._tmp.path().join("cache"),
                    Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
                    fx._tmp.path().to_owned(),
                    Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
                    Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
                );
                let summary = if queue_work {
                    work_queue(&fx.env(), &FakeModelProvider::default(), &opts, &mut |_| {})
                } else {
                    work_job(
                        &fx.env(),
                        &FakeModelProvider::default(),
                        &opts,
                        &id,
                        &mut |_| {},
                    )
                }
                .unwrap();
                assert_eq!(summary.done, 1);
                assert_eq!(summary.imported.len(), 1);
                let meeting = fx.lib.load_meeting(&summary.imported[0]).unwrap();
                assert_eq!(
                    meeting.recording.unwrap().state,
                    schema::RecordingState::Deleted
                );
            }
        }
    }

    #[test]
    fn queue_retention_runs_before_following_enrichment() {
        let mut fx = Fixture::new();
        let file = fx.wav("before-enrich.wav", &[(Some(0), 4.0)]);
        let queue = Queue::open(fx.lib.root()).unwrap();
        let (sha, _) = crate::audio::file_sha256(&file).unwrap();
        fx.lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    IMPORT_KIND,
                    &sha,
                    serde_json::json!({"path": file, "handling": "copy"}),
                )
            })
            .unwrap();
        fx.config.retention = broscribe_core::config::Retention::DeleteImmediately {
            wait_until_named: false,
        };
        fx.config.save(fx._tmp.path()).unwrap();
        let opts = WorkerOptions::new(
            fx._tmp.path().join("cache"),
            Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
            fx._tmp.path().to_owned(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
        );
        let mut checked = false;
        work_queue(
            &fx.env(),
            &FakeModelProvider::default(),
            &opts,
            &mut |event| {
                if event.kind.as_deref() == Some(IMPORT_KIND)
                    && event.event == WorkerEventKind::JobDone
                {
                    let dirs = fx.lib.list_meeting_dirs().unwrap();
                    let meeting = fx.lib.load_meeting_at(&dirs[0]).unwrap();
                    fx.lib
                        .write(|tx| crate::enrich::enqueue_enrich(tx, &meeting.id))
                        .unwrap();
                } else if event.kind.as_deref() == Some(ENRICH_KIND)
                    && event.event == WorkerEventKind::JobStarted
                {
                    let dirs = fx.lib.list_meeting_dirs().unwrap();
                    assert_eq!(
                        fx.lib
                            .load_meeting_at(&dirs[0])
                            .unwrap()
                            .recording
                            .unwrap()
                            .state,
                        schema::RecordingState::Deleted
                    );
                    checked = true;
                }
            },
        )
        .unwrap();
        assert!(checked);
    }

    #[test]
    fn resumed_manifest_workers_apply_retention_after_resolution() {
        for queue_work in [false, true] {
            let mut fx = Fixture::new();
            let wav = fx.wav("manifest-retention.wav", &[(Some(0), 4.0)]);
            let path = fx.input.join("manifest.json");
            fsutil::write_json_atomic(
                &path,
                &serde_json::json!({
                    "format": "broscribe-import-manifest", "version": 1,
                    "meetings": [{"audio_path": wav}]
                }),
            )
            .unwrap();
            let manifest = crate::manifest::read(&path).unwrap();
            crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::AfterPhase1));
            assert!(
                crate::batch::import_manifest(
                    &fx.env(),
                    &mut models(),
                    &manifest,
                    &crate::batch::BatchOptions::default()
                )
                .is_err()
            );
            let queue = Queue::open(fx.lib.root()).unwrap();
            let id = queue.list(MANIFEST_KIND).unwrap()[0].id.clone();
            fx.config.retention = broscribe_core::config::Retention::DeleteImmediately {
                wait_until_named: false,
            };
            fx.config.save(fx._tmp.path()).unwrap();
            let opts = WorkerOptions::new(
                fx._tmp.path().join("cache"),
                Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
                fx._tmp.path().to_owned(),
                Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
                Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
            );
            let summary = if queue_work {
                work_queue(&fx.env(), &FakeModelProvider::default(), &opts, &mut |_| {})
            } else {
                work_job(
                    &fx.env(),
                    &FakeModelProvider::default(),
                    &opts,
                    &id,
                    &mut |_| {},
                )
            }
            .unwrap();
            assert!(summary.batches_resolved);
            assert_eq!(summary.done, 1);
            let dirs = fx.lib.list_meeting_dirs().unwrap();
            let meeting = fx.lib.load_meeting_at(&dirs[0]).unwrap();
            assert_eq!(
                meeting.recording.unwrap().state,
                schema::RecordingState::Deleted
            );
        }
    }

    #[test]
    fn confirmation_worker_applies_retention_after_samples_commit() {
        let mut fx = Fixture::new();
        let file = fx.wav("named-retention.wav", &[(Some(0), 4.0)]);
        let out = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        let model = fx.config.profile.speaker_embedding.clone();
        let speaker = fx.lib.load_meeting(&out.meeting_id).unwrap().speakers[0].id;
        let named = ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(
                tx,
                index,
                &out.meeting_id,
                speaker,
                "Ari Example",
                Vec::new(),
                &model,
            )
        })
        .unwrap();
        let input = ConfirmInput {
            meeting_id: out.meeting_id.clone(),
            speaker_id: speaker,
            person_id: named.person_id.clone(),
            model,
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        fx.lib
            .write(|tx| queue.enqueue(tx, CONFIRM_KIND, &input.key(), input.to_json()))
            .unwrap();
        fx.config.retention = broscribe_core::config::Retention::DeleteImmediately {
            wait_until_named: true,
        };
        fx.config.save(fx._tmp.path()).unwrap();
        let (summary, _) = run(&fx, &fx._tmp.path().join("cache"));
        assert_eq!(summary.named, std::slice::from_ref(&out.meeting_id));
        assert_eq!(
            fx.lib
                .load_meeting(&out.meeting_id)
                .unwrap()
                .recording
                .unwrap()
                .state,
            schema::RecordingState::Deleted
        );
        let prints = fx.lib.load_voiceprints(&named.person_id).unwrap();
        assert!(
            prints
                .embeddings
                .iter()
                .any(|v| v.source.kind == VoiceprintKind::Confirmation)
        );
    }

    #[test]
    fn recording_time_name_uses_clean_sample_worker_and_undo_withdraws_its_voice() {
        use broscribe_core::live_identity::{
            self, LiveAction, LiveAttribution, LiveOutcome, LiveRange,
        };
        let fx = Fixture::new();
        let file = fx.wav("synthetic-live-name.wav", &[(Some(0), 6.0)]);
        let out = import_file(&fx.env(), &mut models(), &file, &ImportOptions::default()).unwrap();
        let model = fx.config.profile.speaker_embedding.clone();
        let profile = schema::Person::new(fx.lib.new_person_id(), "Synthetic Rowan");
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&profile)?;
            index.upsert_person(tx, &profile)?;
            let mut meeting = tx.library().load_meeting(&out.meeting_id)?;
            let base = meeting.clone();
            let mut second = meeting.segments[0].clone();
            let middle = (second.start + second.end) / 2.0;
            meeting.segments[0].end = middle;
            meeting.segments[0].words.clear();
            second.start = middle;
            second.id = 1;
            second.words.clear();
            meeting.segments.push(second);
            for segment in &meeting.segments {
                let fp = ops::segment_fingerprint(segment);
                let decision = u64::from(segment.id) + 1;
                meeting.live_attributions.push(LiveAttribution {
                    turn_id: format!("synthetic-{decision}"),
                    decision,
                    action: LiveAction::Name,
                    person_id: Some(profile.id.clone()),
                    range: LiveRange {
                        track: fp.track.clone(),
                        from_sample: (fp.start * 16_000.0) as u64,
                        to_sample: (fp.end * 16_000.0) as u64,
                        from: None,
                        to: None,
                    },
                    from_s: Some(fp.start),
                    to_s: Some(fp.end),
                    supported: vec![[fp.start, fp.end]],
                    outcome: LiveOutcome::Applied,
                    at: schema::now(),
                    user_override: false,
                    forget_fence: None,
                });
                meeting.utterance_people.push(schema::UtterancePerson {
                    track: fp.track,
                    start: fp.start,
                    end: fp.end,
                    text_sha256: fp.text_sha256,
                    person_id: profile.id.clone(),
                    at: schema::now(),
                    live_decision: Some(decision),
                });
            }
            ops::reconcile_live_speakers(tx.library(), &mut meeting, &fx.config.live)?;
            assert_eq!(live_identity::speaker_namings(&meeting).len(), 1);
            ops::sync_live_promotions(tx, Some(&base), &meeting, &model)?;
            tx.save_meeting(&meeting)?;
            index.upsert_meeting(tx, &meeting)
        })
        .unwrap();
        let (_summary, events) = run(&fx, &fx._tmp.path().join("cache"));
        assert!(
            events
                .iter()
                .any(|event| event.event == WorkerEventKind::JobDone
                    && event.kind.as_deref() == Some(CONFIRM_KIND))
        );
        let prints = fx.lib.load_voiceprints(&profile.id).unwrap();
        assert!(!prints.embeddings.is_empty());
        assert!(prints.embeddings.iter().all(|print| print.source.kind
            == VoiceprintKind::LiveConfirmation
            && print.strength == Strength::Confirmed));
        let meeting = fx.lib.load_meeting(&out.meeting_id).unwrap();
        let naming = &live_identity::speaker_namings(&meeting)[0];
        ops::with_index(&fx.lib, |tx, index| {
            ops::undo_live_promotion(tx, index, &meeting.id, naming)
        })
        .unwrap();
        run(&fx, &fx._tmp.path().join("cache"));
        assert!(
            fx.lib
                .load_voiceprints(&profile.id)
                .unwrap()
                .embeddings
                .is_empty()
        );
        let saved = fx.lib.load_meeting(&meeting.id).unwrap();
        assert!(live_identity::speaker_namings(&saved).is_empty());
        assert_eq!(saved.utterance_people.len(), 2);
    }
}

#[cfg(test)]
mod record_worker_tests {
    use super::*;
    use crate::testutil::Fixture;
    use broscribe_capture::journal::{JournalWriter, SessionMeta, Track};
    use broscribe_models::fakes::{FakeModelProvider, sine};
    use std::collections::BTreeMap;

    #[test]
    fn worker_dispatches_canonical_provisional_job_without_final_completion() {
        let fixture = Fixture::new();
        let mut writer = JournalWriter::create(
            &fixture._tmp.path().join("journal"),
            "worker-session",
            SessionMeta {
                library_root: Some(fixture.lib.root().canonicalize().unwrap()),
                setup: Some("headphones".into()),
                ..Default::default()
            },
            2.0,
            std::process::id(),
        )
        .unwrap();
        for track in Track::ALL {
            writer.begin_segment(track, 0, None, 16000, 0).unwrap();
            writer.append(track, 0, &sine(200.0, 2.0, 0.2)).unwrap();
        }
        let input = broscribe_ipc::RecordJobInput {
            session_id: "worker-session".into(),
            journal_dir: writer.dir().to_owned(),
            cutoff: Some(broscribe_ipc::ChunkCutoff {
                chunks: BTreeMap::from([
                    (broscribe_ipc::Track::Mic, 0),
                    (broscribe_ipc::Track::System, 0),
                ]),
                recording_s: 2.0,
            }),
        };
        let queue = Queue::open(fixture.lib.root()).unwrap();
        let id = fixture
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    PROVISIONAL_KIND,
                    "worker-session@mic0.system0",
                    serde_json::to_value(&input).unwrap(),
                )
            })
            .unwrap();
        let options = WorkerOptions::new(
            fixture._tmp.path().join("cache"),
            Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
            fixture._tmp.path().to_owned(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
        );
        let mut events = Vec::new();
        let summary = work_job(
            &fixture.env(),
            &FakeModelProvider::default(),
            &options,
            &id,
            &mut |event| events.push(event.clone()),
        )
        .unwrap();
        assert_eq!(summary.done, 1, "{events:?}");
        assert!(summary.imported.is_empty());
        assert!(writer.dir().exists());
        let dirs = fixture.lib.list_meeting_dirs().unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(
            fixture
                .lib
                .load_meeting_at(&dirs[0])
                .unwrap()
                .processing
                .status,
            schema::ProcessingStatus::Provisional
        );
        assert_eq!(queue.get(&id).unwrap().state, JobState::Done);
    }

    #[test]
    fn daemon_final_job_applies_retention_after_recording_completion() {
        let mut fx = Fixture::new();
        fx.chain = broscribe_platform::DecoderChain::new(vec![Box::new(
            broscribe_platform::encoder::FakeEncodedDecoder,
        )]);
        fx.config.retention = broscribe_core::config::Retention::DeleteImmediately {
            wait_until_named: false,
        };
        fx.config.save(fx._tmp.path()).unwrap();
        let mut writer = JournalWriter::create(
            &fx._tmp.path().join("journal"),
            "final-worker",
            SessionMeta {
                library_root: Some(fx.lib.root().canonicalize().unwrap()),
                setup: Some("headphones".into()),
                ..Default::default()
            },
            2.0,
            std::process::id(),
        )
        .unwrap();
        for track in Track::ALL {
            writer.begin_segment(track, 0, None, 16000, 0).unwrap();
            writer.append(track, 0, &sine(200.0, 2.0, 0.2)).unwrap();
        }
        let dir = writer.dir().to_owned();
        writer.finish(2_000_000_000, "user").unwrap();
        let input = broscribe_ipc::RecordJobInput {
            session_id: "final-worker".into(),
            journal_dir: dir.clone(),
            cutoff: None,
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        let id = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    broscribe_core::queue::RECORD_FINAL_KIND,
                    "final-worker",
                    serde_json::to_value(&input).unwrap(),
                )
            })
            .unwrap();
        let opts = WorkerOptions::new(
            fx._tmp.path().join("cache"),
            Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
            fx._tmp.path().to_owned(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
        );
        let summary = work_job(
            &fx.env(),
            &FakeModelProvider::default(),
            &opts,
            &id,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(summary.done, 1);
        assert_eq!(summary.imported.len(), 1);
        let meeting = fx.lib.load_meeting(&summary.imported[0]).unwrap();
        assert_eq!(meeting.processing.status, schema::ProcessingStatus::Done);
        assert_eq!(
            meeting.recording.unwrap().state,
            schema::RecordingState::Deleted
        );
        assert!(!dir.exists());
    }

    #[test]
    fn empty_final_job_reports_discard_without_retaining_a_missing_meeting() {
        let mut fx = Fixture::new();
        fx.config.retention = broscribe_core::config::Retention::DeleteImmediately {
            wait_until_named: false,
        };
        fx.config.save(fx._tmp.path()).unwrap();
        let writer = JournalWriter::create(
            &fx._tmp.path().join("journal"),
            "empty-worker",
            SessionMeta {
                library_root: Some(fx.lib.root().canonicalize().unwrap()),
                setup: Some("headphones".into()),
                ..Default::default()
            },
            2.0,
            std::process::id(),
        )
        .unwrap();
        let dir = writer.dir().to_owned();
        writer.finish(0, "user").unwrap();
        let input = broscribe_ipc::RecordJobInput {
            session_id: "empty-worker".into(),
            journal_dir: dir.clone(),
            cutoff: None,
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        let id = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    broscribe_core::queue::RECORD_FINAL_KIND,
                    "empty-worker",
                    serde_json::to_value(&input).unwrap(),
                )
            })
            .unwrap();
        let opts = WorkerOptions::new(
            fx._tmp.path().join("cache"),
            Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
            fx._tmp.path().to_owned(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
        );
        let mut events = Vec::new();
        let summary = work_job(
            &fx.env(),
            &FakeModelProvider::default(),
            &opts,
            &id,
            &mut |event| events.push(event.clone()),
        )
        .unwrap();
        assert_eq!(summary.done, 1);
        assert!(summary.imported.is_empty());
        assert!(fx.lib.list_meeting_dirs().unwrap().is_empty());
        assert!(!dir.exists());
        assert!(
            events
                .iter()
                .any(|e| e.message.as_deref() == Some("empty recording discarded"))
        );
    }
}

#[cfg(test)]
mod follow_up_tests {
    use broscribe_core::config::{ProviderConfig, Retention};
    use broscribe_models::fakes::FakeModelProvider;
    use broscribe_platform::fakes::FakeProcessProbe;

    use super::*;
    use crate::testutil::Fixture;

    fn options(fx: &Fixture) -> WorkerOptions {
        WorkerOptions::new(
            fx._tmp.path().join("cache"),
            Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
            fx._tmp.path().to_owned(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
        )
    }

    fn run_job(fx: &Fixture, id: &JobId) -> (WorkerSummary, Vec<WorkerEvent>) {
        let mut events = Vec::new();
        let summary = work_job(
            &fx.env(),
            &FakeModelProvider::default(),
            &options(fx),
            id,
            &mut |e| events.push(e.clone()),
        )
        .unwrap();
        (summary, events)
    }

    /// A queued import of `file`, as an interrupted import leaves it.
    fn queued_import(fx: &Fixture, file: &Path, extra: serde_json::Value) -> JobId {
        let (sha, _) = crate::audio::file_sha256(file).unwrap();
        let mut input = serde_json::json!({"path": file, "handling": "copy"});
        if let (Some(input), Some(extra)) = (input.as_object_mut(), extra.as_object()) {
            input.extend(extra.clone());
        }
        let queue = Queue::open(fx.lib.root()).unwrap();
        fx.lib
            .write(|tx| queue.enqueue(tx, IMPORT_KIND, &sha, input))
            .unwrap()
    }

    #[test]
    fn recovered_single_imports_follow_auto_enrich_but_folder_files_do_not() {
        for folder in [false, true] {
            let mut fx = Fixture::new();
            fx.config.providers.insert(
                "openai-http".into(),
                ProviderConfig {
                    enabled: true,
                    consented_at: Some(schema::now()),
                    model: Some("synthetic-model".into()),
                    base_url: Some("http://127.0.0.1:9/v1".into()),
                },
            );
            fx.config.save(fx._tmp.path()).unwrap();
            let file = fx.wav("recovered.wav", &[(Some(0), 4.0)]);
            let id = queued_import(&fx, &file, serde_json::json!({ "folder": folder }));
            let (summary, _) = run_job(&fx, &id);
            assert_eq!(summary.imported.len(), 1, "folder: {folder}");
            let queue = Queue::open(fx.lib.root()).unwrap();
            let enrich = queue.list(ENRICH_KIND).unwrap();
            assert_eq!(enrich.len(), usize::from(!folder), "folder: {folder}");
        }
    }

    #[test]
    fn a_move_published_before_the_interruption_gets_its_follow_ups() {
        use crate::import::{ImportOptions, MOVE_FAIL_AT, MoveStep, import_file};
        use crate::testutil::models;
        for step in [MoveStep::Committed, MoveStep::SourceDeleted] {
            let mut fx = Fixture::new();
            fx.config.providers.insert(
                "openai-http".into(),
                ProviderConfig {
                    enabled: true,
                    consented_at: Some(schema::now()),
                    model: Some("synthetic-model".into()),
                    base_url: Some("http://127.0.0.1:9/v1".into()),
                },
            );
            fx.config.save(fx._tmp.path()).unwrap();
            let file = fx.wav("moved.wav", &[(Some(0), 4.0)]);
            let opts = ImportOptions {
                handling: Some(schema::Handling::Move),
                ..Default::default()
            };
            MOVE_FAIL_AT.set(Some(step));
            import_file(&fx.env(), &mut models(), &file, &opts).unwrap_err();
            MOVE_FAIL_AT.set(None);
            let queue = Queue::open(fx.lib.root()).unwrap();
            let job = queue
                .list(IMPORT_KIND)
                .unwrap()
                .into_iter()
                .find(|j| runnable(&queue, j, &fx.probe))
                .unwrap_or_else(|| panic!("{step:?}: no runnable import job"));
            let (summary, events) = run_job(&fx, &job.id);
            assert_eq!(summary.imported.len(), 1, "{step:?}: {events:?}");
            assert_eq!(summary.failed, 0, "{step:?}");
            assert_eq!(queue.list(ENRICH_KIND).unwrap().len(), 1, "{step:?}");
            assert!(queue.list(IMPORT_KIND).unwrap().is_empty(), "{step:?}");
        }
    }

    #[test]
    fn an_unrelated_import_gives_the_moves_its_recovery_completes_their_follow_ups() {
        use crate::import::{ImportOptions, MOVE_FAIL_AT, MoveStep, import_file, import_folder};
        use crate::testutil::models;
        for folder in [false, true] {
            let mut fx = Fixture::new();
            fx.config.providers.insert(
                "openai-http".into(),
                ProviderConfig {
                    enabled: true,
                    consented_at: Some(schema::now()),
                    model: Some("synthetic-model".into()),
                    base_url: Some("http://127.0.0.1:9/v1".into()),
                },
            );
            fx.config.save(fx._tmp.path()).unwrap();
            let opts = ImportOptions {
                handling: Some(schema::Handling::Move),
                ..Default::default()
            };
            // A move import published its meeting, then crashed.
            MOVE_FAIL_AT.set(Some(MoveStep::Committed));
            if folder {
                let dir = fx.input.join("batch");
                std::fs::create_dir_all(&dir).unwrap();
                fx.wav("batch/moved.wav", &[(Some(0), 4.0)]);
                let report = import_folder(&fx.env(), &mut models(), &dir, &opts).unwrap();
                assert_eq!(report.failed.len(), 1, "folder: {folder}");
            } else {
                let file = fx.wav("moved.wav", &[(Some(0), 4.0)]);
                import_file(&fx.env(), &mut models(), &file, &opts).unwrap_err();
            }
            MOVE_FAIL_AT.set(None);
            let other = fx.wav("unrelated.wav", &[(Some(1), 5.0)]);
            let id = queued_import(&fx, &other, serde_json::json!({}));
            let (summary, events) = run_job(&fx, &id);
            assert_eq!(summary.failed, 0, "folder: {folder}: {events:?}");
            // Both meetings get vectors and retention after completion…
            assert_eq!(summary.imported.len(), 2, "folder: {folder}: {events:?}");
            let handlings: Vec<_> = summary
                .imported
                .iter()
                .map(|m| fx.lib.load_meeting(m).unwrap().recording.unwrap().handling)
                .collect();
            assert!(
                handlings.contains(&schema::Handling::Move)
                    && handlings.contains(&schema::Handling::Copy),
                "folder: {folder}: {handlings:?}"
            );
            // …and enrichment by their own job's classification.
            let queue = Queue::open(fx.lib.root()).unwrap();
            let enrich = queue.list(ENRICH_KIND).unwrap();
            assert_eq!(enrich.len(), 1 + usize::from(!folder), "folder: {folder}");
            assert!(
                queue.list(IMPORT_KIND).unwrap().is_empty(),
                "folder: {folder}"
            );
        }
    }

    #[test]
    fn retention_follows_the_policy_configured_when_the_job_completes() {
        let mut fx = Fixture::new();
        // The worker started under immediate deletion; the user has since
        // switched to keeping recordings.
        fx.config.retention = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        let mut current = fx.config.clone();
        current.retention = Retention::KeepForever;
        current.save(fx._tmp.path()).unwrap();
        let file = fx.wav("kept.wav", &[(Some(0), 4.0)]);
        let id = queued_import(&fx, &file, serde_json::json!({}));
        let (summary, _) = run_job(&fx, &id);
        assert_eq!(summary.imported.len(), 1);
        let meeting = fx.lib.load_meeting(&summary.imported[0]).unwrap();
        assert_ne!(
            meeting.recording.unwrap().state,
            schema::RecordingState::Deleted
        );
    }

    #[test]
    fn a_claim_that_fails_a_crashed_job_is_reported_as_failed() {
        let fx = Fixture::new();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let id = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    ENRICH_KIND,
                    "crashed",
                    serde_json::json!({ "meeting_id": "m-0000a1" }),
                )
            })
            .unwrap();
        // Dead workers took it over until one attempt is left, then died
        // holding it.
        let mut pid = 4242;
        while queue.get(&id).unwrap().attempts + 1 < MAX_ATTEMPTS
            || queue.get(&id).unwrap().lease.is_none()
        {
            drop(
                queue
                    .claim(&fx.lib, &id, &FakeProcessProbe::new(pid))
                    .unwrap()
                    .unwrap(),
            );
            pid += 1;
        }
        let (summary, events) = run_job(&fx, &id);
        assert_eq!((summary.failed, summary.busy), (1, 0));
        assert!(events.iter().any(|e| {
            e.event == WorkerEventKind::JobFailed
                && e.message
                    .as_deref()
                    .is_some_and(|m| m.contains("worker crashed"))
        }));
    }

    #[test]
    fn entries_of_a_running_manifest_import_wait_without_failing() {
        let fx = Fixture::new();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let id = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    MANIFEST_KIND,
                    "waiting",
                    serde_json::json!({ "batch": "running-batch", "index": 1 }),
                )
            })
            .unwrap();
        let importer = 4242;
        let batch_file = queue.dir().join("batch-running-batch.json");
        let write_batch = || {
            fsutil::write_json_atomic(
                &batch_file,
                &serde_json::json!({
                    "id": "running-batch",
                    "created_at": schema::now(),
                    "jobs": [id],
                    "sealed": true,
                    "owner": importer,
                }),
            )
            .unwrap();
        };
        write_batch();
        // The importer is alive: the entry waits for its turn.
        fx.probe.set_alive(importer, true);
        for _ in 0..MAX_ATTEMPTS + 1 {
            let (summary, _) = run_job(&fx, &id);
            assert_eq!((summary.failed, summary.busy), (0, 1));
        }
        let job = queue.get(&id).unwrap();
        assert_eq!((job.state, job.attempts), (JobState::Queued, 0));
        // The importer died before the entry's checkpoint: interrupted.
        fx.probe.set_alive(importer, false);
        let (summary, _) = run_job(&fx, &id);
        assert_eq!(summary.failed, 1);
        assert_eq!(queue.get(&id).unwrap().state, JobState::Failed);
    }

    #[test]
    fn entries_reused_by_a_running_batch_wait_after_their_first_batch_died() {
        let fx = Fixture::new();
        let queue = Queue::open(fx.lib.root()).unwrap();
        // Batch A enqueued the entry, then died before its checkpoint.
        let id = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    MANIFEST_KIND,
                    "shared",
                    serde_json::json!({ "batch": "batch-a", "index": 0 }),
                )
            })
            .unwrap();
        let (dead, importer) = (4241, 4242);
        for (batch, owner) in [("batch-a", dead), ("batch-b", importer)] {
            fsutil::write_json_atomic(
                &queue.dir().join(format!("batch-{batch}.json")),
                &serde_json::json!({
                    "id": batch,
                    "created_at": schema::now(),
                    "jobs": [id],
                    "sealed": true,
                    "owner": owner,
                }),
            )
            .unwrap();
        }
        // Batch B, running now, lists the same job: it waits for B.
        fx.probe.set_alive(importer, true);
        for _ in 0..MAX_ATTEMPTS + 1 {
            let (summary, _) = run_job(&fx, &id);
            assert_eq!((summary.failed, summary.busy), (0, 1));
        }
        let job = queue.get(&id).unwrap();
        assert_eq!((job.state, job.attempts), (JobState::Queued, 0));
    }

    /// Models that never load.
    struct BrokenModels;

    impl ModelProvider for BrokenModels {
        fn load(&self, _: &broscribe_core::config::Profile) -> Result<ModelSet> {
            Err(Error::Inference("synthetic models are missing".into()))
        }
        fn load_vad(&self, _: &str) -> Result<Box<dyn broscribe_models::Vad + Send>> {
            Err(Error::Inference("synthetic models are missing".into()))
        }
        fn load_embedder(
            &self,
            _: &str,
        ) -> Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
            Err(Error::Inference("synthetic models are missing".into()))
        }
    }

    fn run_broken(fx: &Fixture, id: &JobId) -> WorkerSummary {
        work_job(&fx.env(), &BrokenModels, &options(fx), id, &mut |_| {}).unwrap()
    }

    /// Standalone `queue work` with models that fail to load: the import's
    /// failure is recorded on its job (an attempt each, so it stops at the
    /// limit), and an interrupted move of another file is still completed
    /// and followed up.
    #[test]
    fn a_model_load_failure_is_recorded_and_moves_still_recover() {
        use crate::import::{ImportOptions, MOVE_FAIL_AT, MoveStep, import_file};
        use crate::testutil::models;
        let fx = Fixture::new();
        let moved = fx.wav("moved.wav", &[(Some(0), 4.0)]);
        MOVE_FAIL_AT.set(Some(MoveStep::Committed));
        import_file(
            &fx.env(),
            &mut models(),
            &moved,
            &ImportOptions {
                handling: Some(schema::Handling::Move),
                ..Default::default()
            },
        )
        .unwrap_err();
        MOVE_FAIL_AT.set(None);
        let other = fx.wav("other.wav", &[(Some(1), 4.0)]);
        let id = queued_import(&fx, &other, serde_json::json!({}));
        let summary = run_broken(&fx, &id);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.imported.len(), 1, "the interrupted move completed");
        let queue = Queue::open(fx.lib.root()).unwrap();
        // The interrupted move's own job is settled (done or gone).
        assert!(
            queue
                .list(IMPORT_KIND)
                .unwrap()
                .iter()
                .all(|j| j.id == id || j.state == JobState::Done)
        );
        let job = queue.get(&id).unwrap();
        assert_eq!((job.state, job.attempts), (JobState::Failed, 1));
        assert!(
            job.error
                .as_deref()
                .is_some_and(|e| e.contains("loading the models failed"))
        );
        for _ in 1..MAX_ATTEMPTS {
            run_broken(&fx, &id);
        }
        let job = queue.get(&id).unwrap();
        assert_eq!(job.attempts, MAX_ATTEMPTS);
        assert!(!job.is_claimable(), "no endless retries");
    }

    /// A checkpointed manifest entry whose resolve keeps failing (here the
    /// models never load) counts an attempt per run and stops at the limit.
    #[test]
    fn a_checkpointed_manifest_entry_failing_to_resolve_stops_at_the_limit() {
        let fx = Fixture::new();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let id = fx
            .lib
            .write(|tx| queue.enqueue(tx, MANIFEST_KIND, "synthetic-entry", serde_json::json!({})))
            .unwrap();
        std::fs::write(queue.job_dir(&id).join(HINTS_JSON), b"{}").unwrap();
        for attempt in 1..=MAX_ATTEMPTS {
            let summary = run_broken(&fx, &id);
            assert_eq!(summary.failed, 1);
            let job = queue.get(&id).unwrap();
            assert_eq!((job.state, job.attempts), (JobState::Failed, attempt));
        }
        assert!(!queue.get(&id).unwrap().is_claimable());
        assert!(!runnable(&queue, &queue.get(&id).unwrap(), &fx.probe));
    }

    /// The worker processes the input its claim captured: a request that
    /// replaced the input after the listing that picked the job wins.
    #[test]
    fn a_job_runs_the_input_it_claimed_not_the_one_it_was_listed_with() {
        let fx = Fixture::new();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let a = MeetingId::parse("m-0000a1").unwrap();
        let b = MeetingId::parse("m-0000b2").unwrap();
        let id = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    VECTORS_KIND,
                    "synthetic-key",
                    serde_json::json!({ "meeting_id": a }),
                )
            })
            .unwrap();
        let listed = queue.get(&id).unwrap();
        fx.lib
            .write(|tx| {
                queue
                    .request(tx, VECTORS_KIND, "synthetic-key", |_| {
                        Ok(serde_json::json!({ "meeting_id": b }))
                    })
                    .map(|_| ())
            })
            .unwrap();
        let mut claimed = None;
        let outcome = run_vectors_job(&fx.env(), &queue, &listed, &mut claimed, || {
            Ok(crate::vectors::TextModel::Unavailable {
                id: "synthetic-text".into(),
                reason: "synthetic".into(),
            })
        })
        .unwrap();
        assert!(matches!(outcome, Outcome::Done(_)));
        assert_eq!(claimed, Some(b));
    }

    /// An imported meeting and its queued playback-cache job.
    fn playback_setup(fx: &Fixture) -> (MeetingId, JobId, PathBuf) {
        use crate::import::{ImportOptions, import_file};
        let meeting = import_file(
            &fx.env(),
            &mut crate::testutil::models(),
            &fx.wav("playback.wav", &[(Some(0), 2.0)]),
            &ImportOptions::default(),
        )
        .unwrap()
        .meeting_id;
        let queue = Queue::open(fx.lib.root()).unwrap();
        let job = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    PLAYBACK_KIND,
                    meeting.as_str(),
                    serde_json::json!({ "meeting_id": meeting }),
                )
            })
            .unwrap();
        (meeting, job, options(fx).cache_dir)
    }

    fn playback_files(cache: &Path) -> Vec<PathBuf> {
        match std::fs::read_dir(cache.join(PLAYBACK_CACHE_DIR)) {
            Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// A worker whose lease was reclaimed while it decoded publishes no
    /// playback cache and leaves no staged file behind.
    #[test]
    fn a_playback_cache_whose_lease_was_lost_publishes_nothing() {
        let fx = Fixture::new();
        let (_, job, cache) = playback_setup(&fx);
        let queue = Queue::open(fx.lib.root()).unwrap();
        let holder = queue
            .claim(&fx.lib, &job, &FakeProcessProbe::new(100))
            .unwrap()
            .unwrap();
        let _taker = queue
            .claim(&fx.lib, &job, &FakeProcessProbe::new(200))
            .unwrap()
            .unwrap();
        let err = playback_job(
            &fx.env(),
            &cache,
            &holder,
            holder.claimed_job(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(err.code(), "lease_lost");
        assert!(
            playback_files(&cache).is_empty(),
            "{:?}",
            playback_files(&cache)
        );
    }

    /// A recording deleted while its playback cache was being prepared is
    /// never recreated in the cache; the job ends without it.
    #[test]
    fn a_deleted_recording_never_gets_its_playback_cache_back() {
        let fx = Fixture::new();
        let (meeting, job, cache) = playback_setup(&fx);
        fx.lib
            .write(|tx| {
                let mut m = tx.library().load_meeting(&meeting)?;
                m.recording.as_mut().unwrap().state = RecordingState::Deleted;
                tx.save_meeting(&m)
            })
            .unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let lease = queue.claim(&fx.lib, &job, &fx.probe).unwrap().unwrap();
        let message =
            playback_job(&fx.env(), &cache, &lease, lease.claimed_job(), &mut |_| {}).unwrap();
        assert!(message.starts_with("skipped"), "{message}");
        assert!(
            playback_files(&cache).is_empty(),
            "{:?}",
            playback_files(&cache)
        );
        lease.release(&fx.lib).unwrap();
        assert_eq!(queue.get(&job).unwrap().state, JobState::Done);
    }
}
