//! `reprocess <meeting> [--profile p] [--asr-only] [--queue]` (design §11,
//! plan M2 §9.6, §10.1): runs recognition (and, unless `--asr-only`,
//! diarization and identity) again on the stored recording, keeping
//! confirmed names, rejections and user-set action statuses. Afterwards:
//! search vectors and retention for the meeting, then enrichment when
//! `auto_enrich` applies and the existing enrichment became stale.
//!
//! The run holds the meeting's `reprocess` queue job under a lease, so it
//! and a queued reprocessing never run at once (`library_busy`); a worker
//! that claimed the job first runs it instead. The first Ctrl-C (or
//! SIGTERM) saves nothing more and marks the job failed (`interrupted`,
//! never re-run); a second one quits at once.
//!
//! `--queue` asks the background worker instead: the request is checked
//! and reserved in one library write (`enqueue_reprocess_request`, the
//! same reservation as the synchronous run, so each excludes the other).
//! A refused request changes nothing: no model is loaded, no job is
//! touched and the agent is not woken.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use broscribe_core::{Library, MeetingId, Result};
use broscribe_pipeline::Env;
use broscribe_pipeline::reprocess::{
    InteractiveReprocess, enqueue_reprocess_request, run_reprocess_interactive,
};
use serde_json::json;

use super::enrich::watch_interrupts;
use super::meeting_id;
use crate::cli::ReprocessArgs;
use crate::{Ctx, to_value};

/// What the first Ctrl-C does to a synchronous reprocessing.
const STOP_NOTICE: &str = "Stopping: the reprocessing saves nothing; press Ctrl-C again to quit \
     now (the job is then marked failed).";

pub(super) fn run(ctx: &mut Ctx<'_>, a: ReprocessArgs) -> Result<()> {
    let deps = ctx.deps;
    let id = meeting_id(&a.meeting)?;
    // The active config: the producer applies `--profile` to this run only
    // (the config is not saved).
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    if a.queue {
        return queue(ctx, &lib, &id, &a);
    }
    let env = Env {
        lib: &lib,
        config: &config,
        decoders: &deps.decoder,
        probe: deps.process.as_ref(),
        execution: &deps.execution,
        progress: Some(ctx.pipeline_progress()),
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    watch_interrupts(cancelled.clone(), STOP_NOTICE, ctx.background_notes());
    let ran = run_reprocess_interactive(
        &env,
        &id,
        a.profile.as_deref(),
        a.asr_only,
        deps.models.as_ref(),
        &cancelled,
    )?;
    let out = match ran {
        InteractiveReprocess::Ran(out) => out,
        InteractiveReprocess::Queued(job) => {
            // The background worker claimed the job first: it runs it, with
            // its follow-ups.
            return ctx.emit(json!({ "queued": true, "job_id": job }), |out_w| {
                writeln!(
                    out_w,
                    "The background worker is reprocessing {id}: job {job}"
                )
            });
        }
    };
    let mut enrichment = serde_json::Value::Null;
    if out.skipped.is_none() {
        super::after_identity(ctx, &lib, &config, Some(std::slice::from_ref(&id)));
        if out.enrichment_stale || lib.load_meeting(&id)?.enrichment.is_none() {
            enrichment = super::enrich::queue_after_import(
                ctx,
                &lib,
                &config,
                std::slice::from_ref(&id),
                false,
            );
        }
    }
    let mut value = to_value(&out)?;
    value["enrichment"] = enrichment;
    ctx.emit(value, |out_w| {
        if let Some(why) = &out.skipped {
            return writeln!(out_w, "{why}");
        }
        writeln!(
            out_w,
            "Reprocessed {id}: {} segments, {} speakers ({} confirmed kept, {} named automatically, {} suggested)",
            out.segments, out.speakers, out.confirmed_kept, out.auto, out.suggested
        )?;
        if out.enrichment_stale {
            writeln!(out_w, "The summary predates reprocessing (run `broscribe enrich {id}`).")?;
        }
        if !out.needs_review.is_empty() {
            writeln!(out_w, "Needs review:")?;
            for r in &out.needs_review {
                writeln!(out_w, "  {r}")?;
            }
        }
        Ok(())
    })
}

/// `reprocess <meeting> --queue`: checks and reserves the meeting's
/// reprocessing job for the background worker in one library write, then
/// wakes the agent. A queued or running reprocessing (synchronous or not)
/// is `library_busy`. A completed or failed transcription can be queued.
/// A meeting being deleted, an unfinished transcription, missing audio, an
/// unknown profile or ASR-only on a microphone + system pair is refused. `{"queued": true, "job_id",
/// "meeting_id", "agent_notified"}`.
fn queue(ctx: &mut Ctx<'_>, lib: &Library, id: &MeetingId, a: &ReprocessArgs) -> Result<()> {
    let job = lib.write_checked(
        || ctx.check_request_cancelled(),
        |tx| enqueue_reprocess_request(tx, id, a.profile.as_deref(), a.asr_only),
    )?;
    let notified = match super::kick_agent(ctx) {
        Ok(true) => true,
        Ok(false) => {
            ctx.note(
                "note: no agent is running; run `broscribe queue work` (or open the app) to \
                 reprocess now",
            );
            false
        }
        Err(e) => {
            ctx.note(format!(
                "note: reprocessing queued but the agent could not be notified: {e}"
            ));
            false
        }
    };
    ctx.emit(
        json!({ "queued": true, "job_id": job, "meeting_id": id, "agent_notified": notified }),
        |out| writeln!(out, "Queued the reprocessing of {id}: job {job}"),
    )
}
