//! `queue work` (the app's processing child, plan §8 item 1),
//! `queue status`, `queue prune` and `identity status` (the speaker
//! recheck).

use std::collections::BTreeMap;
use std::io::Write;

use broscribe_core::learning::{self, LearningStatus};
use broscribe_core::queue::{Job, JobState, PruneKeep, PruneReport, Queue, kind_label};
use broscribe_core::{Error, Result};
use broscribe_pipeline::Env;
use broscribe_pipeline::import::{discard_abandoned_setup_imports, discard_session_imports};
use broscribe_pipeline::worker::{
    WORKER_KINDS, WorkerEvent, WorkerEventKind, WorkerOptions, WorkerSummary, runnable, work_queue,
};
use serde_json::{Value, json};

use crate::cli::PruneArgs;
use crate::{Ctx, to_value};

fn human_line(e: &WorkerEvent) -> String {
    let job = match (&e.job_id, &e.kind) {
        (Some(id), Some(kind)) => format!("{id} {kind}"),
        (Some(id), None) => id.clone(),
        _ => String::new(),
    };
    let message = e.message.as_deref().unwrap_or("");
    match e.event {
        WorkerEventKind::JobStarted => format!("{job}: started"),
        WorkerEventKind::Progress => match e.fraction {
            Some(f) => format!("{job}: {:.0}% {message}", f * 100.0),
            None => format!("{job}: {message}"),
        },
        WorkerEventKind::JobDone => format!("{job}: done {message}"),
        WorkerEventKind::JobFailed => format!("{job}: failed {message}"),
        WorkerEventKind::Idle => "Nothing left in the queue.".to_owned(),
    }
}

/// Runs jobs until the queue has nothing runnable. With `--json-progress`
/// stdout carries one event per line and nothing else; a closed stdout (the
/// app quit) is noted on stderr once and the work goes on.
pub(super) fn work(ctx: &mut Ctx<'_>, json_progress: bool, once: bool) -> Result<()> {
    let deps = ctx.deps;
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let env = Env {
        lib: &lib,
        config: &config,
        decoders: &deps.decoder,
        probe: deps.process.as_ref(),
        execution: &deps.execution,
        progress: Some(ctx.notes.progress()),
    };
    let opts = WorkerOptions {
        cache_dir: ctx.paths.cache_dir.clone(),
        once,
        encoder: broscribe_platform_native::NativePlatform::audio_encoder(),
        // Native calendar snapshots are re-checked against the agent's live grant.
        calendar_authority: std::sync::Arc::from(
            broscribe_platform_native::NativePlatform::calendar_authority(&ctx.paths.data_dir),
        ),
        data_dir: ctx.paths.data_dir.clone(),
        secrets: deps.secrets.clone(),
    };
    let human = !json_progress && !ctx.json;
    let mut pipe_broken = false;
    let summary = {
        let out = &mut *ctx.out;
        let err = &mut *ctx.err;
        let mut emit = |e: &WorkerEvent| {
            if pipe_broken {
                return;
            }
            let written = if json_progress {
                serde_json::to_string(e)
                    .map_err(std::io::Error::other)
                    .and_then(|line| writeln!(out, "{line}"))
                    .and_then(|()| out.flush())
            } else if human {
                writeln!(out, "{}", human_line(e))
            } else {
                Ok(())
            };
            if let Err(x) = written {
                pipe_broken = true;
                let _ = writeln!(
                    err,
                    "queue work: progress output closed ({x}); finishing the work anyway"
                );
            }
        };
        work_queue(&env, deps.models.as_ref(), &opts, &mut emit)?
    };
    // Search vectors and retention for what the jobs imported or resolved
    // (notes on stderr only: stdout stays the progress stream), under the
    // consent and retention policy configured now: the work may have taken
    // long, and the settings may have changed meanwhile.
    let config = match ctx.config() {
        Ok(current) => current,
        Err(e) => {
            ctx.note(format!("note: search vectors and retention skipped: {e}"));
            return finish(ctx, &summary, json_progress, pipe_broken);
        }
    };
    if summary.batches_resolved {
        super::after_identity(ctx, &lib, &config, None);
    } else if !summary.imported.is_empty() {
        super::after_identity(ctx, &lib, &config, Some(&summary.imported));
    }
    if !summary.named.is_empty() {
        super::after_naming(ctx, &lib, &config, Some(&summary.named));
    }
    if !summary.reidentified.is_empty() {
        super::after_naming(ctx, &lib, &config, Some(&summary.reidentified));
    }
    finish(ctx, &summary, json_progress, pipe_broken)
}

/// The `--json` summary, unless progress lines already used stdout.
fn finish(
    ctx: &mut Ctx<'_>,
    summary: &WorkerSummary,
    json_progress: bool,
    pipe_broken: bool,
) -> Result<()> {
    if ctx.json && !json_progress && !pipe_broken {
        ctx.emit(to_value(summary)?, |_| Ok(()))?;
    }
    Ok(())
}

pub(super) fn status(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    // Reading only: a library without a queue folder has no jobs, and none
    // is created. One snapshot gives every row and every place in line (all
    // kinds, as the daemon's scheduler orders them), so ranks agree with
    // one another and each record is read once.
    let queue = Queue::open_existing(lib.root())?;
    let (jobs, ranks) = match &queue {
        Some(queue) => {
            let snapshot = queue.snapshot()?;
            let ranks = snapshot.ranks();
            let jobs: Vec<Job> = snapshot
                .jobs
                .into_iter()
                .filter(|j| WORKER_KINDS.contains(&j.kind.as_str()))
                .collect();
            (jobs, ranks)
        }
        None => Default::default(),
    };
    let model = config.profile.speaker_embedding.clone();
    let recheck = learning::status(&lib, &model)?;
    let probe = ctx.deps.process.as_ref();
    // The running helper owns its progress. A different library may reuse job IDs.
    let progress = match super::daemon::optional_call(ctx.data_dir(), broscribe_ipc::Call::Status) {
        Ok(Some(state)) => state["library"]
            .as_str()
            .and_then(|path| std::fs::canonicalize(path).ok())
            .filter(|path| Some(path) == std::fs::canonicalize(lib.root()).ok().as_ref())
            .and_then(|_| {
                serde_json::from_value::<broscribe_ipc::JobProgress>(
                    state["jobs"]["running"].clone(),
                )
                .ok()
            }),
        Ok(None) => None,
        Err(e) => {
            tracing::debug!(code = e.code(), "queue progress unavailable");
            None
        }
    };
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut rows = Vec::with_capacity(jobs.len());
    for job in &jobs {
        *counts.entry(job.state.as_str()).or_default() += 1;
        let pending = queue.as_ref().is_some_and(|q| runnable(q, job, probe));
        // Held or not waiting: nothing to wait for (0), as `jobs_ahead`.
        let ahead = ranks.get(&job.id).copied().unwrap_or(0);
        let progress = progress
            .as_ref()
            .filter(|p| p.job_id == job.id.as_str() && p.kind == job.kind);
        let meeting_id = job
            .meeting_id()
            .or_else(|| progress.and_then(|p| p.meeting_id.clone()));
        let live_fraction = progress.and_then(|p| p.fraction).or_else(|| {
            queue
                .as_ref()
                .filter(|q| q.is_held(job, probe))
                .filter(|_| matches!(job.state, JobState::Running | JobState::Analyzed))
                .and(job.progress)
        });
        rows.push(json!({
            "id": job.id,
            "meeting_id": meeting_id,
            "progress": progress,
            "fraction": live_fraction,
            "run": job.generation_run,
            "kind": job.kind,
            "label": kind_label(&job.kind, &job.input),
            "state": job.state.as_str(),
            "attempts": job.attempts,
            "error": job.error,
            "lease_pid": job.lease.as_ref().map(|l| l.pid),
            "runnable": pending,
            "created_at": job.created_at,
            "updated_at": job.updated_at,
            "jobs_ahead": ahead,
        }));
    }
    let value = json!({ "jobs": rows, "counts": counts, "learning": recheck });
    ctx.emit(value, |out| {
        if jobs.is_empty() {
            writeln!(out, "The queue is empty.")?;
        } else {
            for (job, row) in jobs.iter().zip(&rows) {
                let held = match job.lease.as_ref() {
                    Some(l) => format!("  held by pid {}", l.pid),
                    None => String::new(),
                };
                writeln!(
                    out,
                    "{}  {:<28} {:<8} attempts {}{held}{}",
                    job.id,
                    row["label"].as_str().unwrap_or(&job.kind),
                    job.state.as_str(),
                    job.attempts,
                    if row["runnable"] == true {
                        "  (runnable)"
                    } else {
                        ""
                    }
                )?;
                if let Some(meeting) = row["meeting_id"].as_str() {
                    writeln!(out, "    meeting: {meeting}")?;
                }
                if let Some(fraction) = row["fraction"].as_f64() {
                    writeln!(out, "    {:.0}%", fraction * 100.0)?;
                }
                if let Some(e) = &job.error {
                    writeln!(out, "    {e}")?;
                }
            }
            let summary: Vec<String> = counts.iter().map(|(s, n)| format!("{n} {s}")).collect();
            writeln!(out, "{}", summary.join(", "))?;
        }
        if let Some(recheck) = &recheck {
            write_learning(out, recheck)?;
        }
        Ok(())
    })
}

/// `identity status`: the background speaker recheck of the active
/// embedding model, `{model, learning}` (`learning` is `null` when none was
/// ever requested). Counts and meeting IDs only, never names.
pub(super) fn identity_status(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let model = config.profile.speaker_embedding.clone();
    let recheck = learning::status(&lib, &model)?;
    ctx.emit(
        json!({ "model": model, "learning": recheck }),
        |out| match &recheck {
            Some(recheck) => write_learning(out, recheck),
            None => writeln!(out, "No speaker recheck has been requested for {model}."),
        },
    )
}

/// Human lines of a speaker recheck: what is still to do, then the last
/// committed counts, the meetings named in first.
fn write_learning(out: &mut dyn Write, s: &LearningStatus) -> std::io::Result<()> {
    let ids = |ids: &[broscribe_core::MeetingId]| {
        ids.iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    if s.state == JobState::Failed {
        writeln!(
            out,
            "Checking other speakers failed (job {}): {}",
            s.job_id,
            s.error.as_deref().unwrap_or("unknown error")
        )?;
    } else if s.processed < s.requested {
        let how = if s.running { "running" } else { "queued" };
        writeln!(out, "Checking other speakers: {how} (job {}).", s.job_id)?;
        if !s.source_meetings.is_empty() {
            writeln!(out, "  first: {}", ids(&s.source_meetings))?;
        }
    } else {
        writeln!(out, "Other speakers are up to date.")?;
    }
    if !s.deferred.is_empty() {
        writeln!(
            out,
            "  waiting while busy: {} (checked once their jobs finish)",
            ids(&s.deferred)
        )?;
    }
    let Some(r) = &s.report else {
        return Ok(());
    };
    writeln!(
        out,
        "Last results: {} named automatically, {} replaced, {} automatic names withdrawn, \
         {} suggestions, {} cleared, {} meetings waiting.",
        r.newly_named, r.replaced, r.demoted, r.suggested, r.cleared, r.deferred
    )?;
    for m in &r.meetings {
        writeln!(
            out,
            "  {}{}: {} named, {} replaced, {} withdrawn, {} suggested, {} cleared",
            m.meeting_id,
            if m.source { " (named in)" } else { "" },
            m.newly_named,
            m.replaced,
            m.demoted,
            m.suggested,
            m.cleared
        )?;
    }
    for u in &r.unreadable {
        writeln!(
            out,
            "  {}: could not be read ({}); its speakers were kept",
            u.meeting_id, u.code
        )?;
    }
    Ok(())
}

/// `queue discard-setup`: removes setup Try-it imports (job and staged
/// audio) of `session`, or every one whose session ended. The app runs it
/// after killing a setup child and at launch.
pub(super) fn discard_setup(ctx: &mut Ctx<'_>, session: Option<&str>) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let discarded = if Queue::open_existing(lib.root())?.is_some() {
        let probe = ctx.deps.process.as_ref();
        match session {
            Some(session) => discard_session_imports(&lib, probe, session)?,
            None => discard_abandoned_setup_imports(&lib, probe)?,
        }
    } else {
        Vec::new()
    };
    ctx.emit(json!({ "discarded": discarded }), |out| {
        writeln!(
            out,
            "Discarded {} setup recording import(s).",
            discarded.len()
        )
    })
}

/// `queue prune --dry-run|--yes [--state …] [--older-than …] [--kind …]
/// [--quarantine-invalid] [--limit N]`. `--dry-run` reads one snapshot
/// (`Queue::prune_plan`) and changes nothing; `--yes` removes inside one
/// library write (`Queue::prune`), each folder decided again under the
/// queue lock just before it goes. Core keeps what must stay (a live or
/// revoked holder, the only copy of a moved original, a retryable failure,
/// a folder holding other files, an unreadable record unless quarantined,
/// which moves it whole and never deletes it) and reports why. A library
/// without a queue folder has nothing to prune and gets none created. The
/// JSON is the [`PruneReport`] (identifiers, kinds, states and reasons
/// only) plus `counts`.
pub(super) fn prune(ctx: &mut Ctx<'_>, a: &PruneArgs) -> Result<()> {
    let apply = match (a.dry_run, a.yes) {
        (true, false) => false,
        (false, true) => true,
        _ => {
            return Err(Error::InvalidInput(
                "queue prune needs exactly one of --dry-run or --yes".into(),
            ));
        }
    };
    let criteria = a.criteria();
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let probe = ctx.deps.process.as_ref();
    let report = match Queue::open_existing(lib.root())? {
        None => PruneReport {
            dry_run: !apply,
            ..PruneReport::default()
        },
        Some(queue) if apply => lib.write(|tx| queue.prune(tx, probe, &criteria))?,
        Some(queue) => queue.prune_plan(probe, &criteria)?,
    };
    let mut value = to_value(&report)?;
    value["counts"] = prune_counts(&report)?;
    ctx.emit(value, |out| write_prune(out, &report))
}

/// `{"removed", "quarantined", "empty_folders", "kept", "remaining",
/// "kept_by_reason": {reason: n}}`.
fn prune_counts(r: &PruneReport) -> Result<Value> {
    let mut by_reason: BTreeMap<String, usize> = BTreeMap::new();
    for k in &r.kept {
        *by_reason.entry(keep_reason(k.reason)?).or_default() += 1;
    }
    Ok(json!({
        "removed": r.removed.len(),
        "quarantined": r.quarantined.len(),
        "empty_folders": r.empty_folders,
        "kept": r.kept.len(),
        "remaining": r.remaining,
        "kept_by_reason": by_reason,
    }))
}

/// The stable name of a keep reason, as the JSON spells it.
fn keep_reason(reason: PruneKeep) -> Result<String> {
    match to_value(&reason)? {
        Value::String(s) => Ok(s),
        other => Err(Error::Internal(format!(
            "unexpected prune keep reason {other}"
        ))),
    }
}

fn write_prune(out: &mut dyn Write, r: &PruneReport) -> std::io::Result<()> {
    let (removed, quarantined) = if r.dry_run {
        ("Would remove", "Would quarantine")
    } else {
        ("Removed", "Quarantined")
    };
    for job in &r.removed {
        writeln!(
            out,
            "{removed} {}  {:<28} {}",
            job.id,
            job.kind,
            job.state.as_str()
        )?;
    }
    for id in &r.quarantined {
        writeln!(out, "{quarantined} {id} (unreadable record)")?;
    }
    for k in &r.kept {
        let reason = keep_reason(k.reason).map_err(std::io::Error::other)?;
        writeln!(
            out,
            "Kept {}  {:<28} {}",
            k.id,
            k.kind.as_deref().unwrap_or("-"),
            reason.replace('_', " ")
        )?;
    }
    writeln!(
        out,
        "{} job record(s) checked: {} {}, {} {}, {} kept, {} empty folder(s) {}.",
        r.checked,
        r.removed.len(),
        if r.dry_run { "to remove" } else { "removed" },
        r.quarantined.len(),
        if r.dry_run {
            "to quarantine"
        } else {
            "quarantined"
        },
        r.kept.len(),
        r.empty_folders,
        if r.dry_run { "to remove" } else { "removed" },
    )?;
    if r.remaining > 0 {
        writeln!(
            out,
            "{} more past --limit: run again to continue.",
            r.remaining
        )?;
    }
    if r.dry_run {
        writeln!(out, "Nothing was changed (--dry-run).")?;
    }
    Ok(())
}
