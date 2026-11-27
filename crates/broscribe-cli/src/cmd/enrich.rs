//! `enrich <meeting>|--all [--provider id] [--model m] [--yes]
//! [--instructions-stdin]`, `enrich <meeting> --queue`, `enrich instructions
//! show|set --stdin|clear` and the enrichment that follows imports (plan M2
//! §2, §9.8): single imports enrich on their own when `auto_enrich = "new"`
//! and a provider is enabled with consent; batches (folders, manifests) only
//! with `--enrich`. Transcript text goes only to providers the user enabled.
//!
//! Instructions (what summaries should focus on) come from stdin for one run
//! (empty text: none) or, without `--instructions-stdin`, from the saved
//! `[enrichment] instructions`, read when the run starts; never from argv.
//!
//! Every enrichment holds the meeting's `enrich` queue job: `--queue` leaves
//! it to the background worker; the synchronous form claims it and runs it
//! here, so two requests for one meeting never run at once (`library_busy`).
//! The first Ctrl-C (or SIGTERM) stops a synchronous run before its next
//! provider call and marks its job failed; a second one quits at once.
//! `--all` stopped that way still reports what it finished: the meetings
//! enriched (and saved) before the interruption keep their results and get
//! their search vectors and retention, the meeting in flight saved nothing
//! and the rest were not started; the report says `"status":
//! "interrupted"` and the exit code is 130.

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use broscribe_core::config::{Config, validate_model_name};
use broscribe_core::index::Index;
use broscribe_core::schema::Meeting;
use broscribe_core::{Error, Library, MeetingId, Result, ops};
use broscribe_models::llm::ProviderRegistry;
use broscribe_pipeline::Env;
use broscribe_pipeline::enrich::{
    EnrichOutcome, InteractiveEnrich, auto_enrich_applies, enqueue_enrich, enqueue_enrich_request,
    enqueue_requested_enrich, llm_provider_ids, run_enrich_interactive,
};
use serde_json::{Value, json};

use super::{confirm_on_terminal, meeting_id};
use crate::cli::{EnrichArgs, InstructionsCmd};
use crate::query::read_stdin_text;
use crate::{Ctx, to_value};

/// Exit code of an `enrich --all` the user interrupted, after it reported
/// what it finished (as a shell reports a command stopped by Ctrl-C).
const INTERRUPTED_EXIT: i32 = 130;

pub(super) fn env<'a>(ctx: &'a Ctx<'_>, lib: &'a Library, config: &'a Config) -> Env<'a> {
    Env {
        lib,
        config,
        decoders: &ctx.deps.decoder,
        probe: ctx.deps.process.as_ref(),
        execution: &ctx.deps.execution,
        progress: Some(ctx.pipeline_progress()),
    }
}

/// Sets `cancelled` on the first interrupt or termination request, telling
/// the user `notice` (what stops, and when); exits with 130 on the second
/// (the job's lease then goes stale and the queue marks it failed, never
/// re-running it). Returns once the signal handlers are installed, so a
/// Ctrl-C during the work that follows is always this watcher's.
pub(super) fn watch_interrupts(
    cancelled: Arc<AtomicBool>,
    notice: &'static str,
    notes: crate::BackgroundNotes,
) {
    use broscribe_platform_native::ipc::termination;
    use std::task::Poll;
    let (armed, listening) = std::sync::mpsc::sync_channel::<()>(1);
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async {
            // The first poll installs the handlers.
            let mut first = std::pin::pin!(termination());
            let polled = std::future::poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx))).await;
            let _ = armed.send(());
            let signalled = match polled {
                Poll::Ready(result) => result,
                Poll::Pending => first.await,
            };
            if signalled.is_err() {
                return;
            }
            cancelled.store(true, Ordering::SeqCst);
            notes.push(notice.to_owned());
            if termination().await.is_ok() {
                std::process::exit(130);
            }
        });
    });
    // Also returns when the watcher could not start (its sender dropped).
    let _ = listening.recv();
}

/// What the first Ctrl-C does to a synchronous enrichment or template run.
pub(super) const PROVIDER_STOP_NOTICE: &str = "Stopping before the next provider call; press \
     Ctrl-C again to quit now (the job is then marked failed).";

fn has_input(meeting: &Meeting) -> bool {
    meeting.segments.iter().any(|s| !s.text.trim().is_empty())
        || meeting.notes.iter().any(|n| !n.text.trim().is_empty())
        || !meeting.bookmarks.is_empty()
}

/// Meetings with transcript or user context and no current enrichment.
fn meetings_to_enrich(lib: &Library) -> Result<Vec<MeetingId>> {
    let mut ids = Vec::new();
    for id in Index::open_readonly(lib.root())?.meeting_ids()? {
        let m = lib.load_meeting(&id)?;
        let needs = m.enrichment.is_none() || ops::enrichment_is_stale(&m);
        if needs && has_input(&m) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Selects completed entries of this manifest only, including resumed
/// imports whose source was moved or deleted by retention. Existing audio
/// is matched by content, not merely by its file name.
pub(super) fn unenriched_manifest_meetings(
    lib: &Library,
    manifest: &broscribe_pipeline::manifest::Manifest,
) -> Result<Vec<MeetingId>> {
    let mut meetings = Vec::new();
    for dir in lib.list_meeting_dirs()? {
        let m = lib.load_meeting_at(&dir)?;
        if m.processing.status == broscribe_core::schema::ProcessingStatus::Done
            && (m.enrichment.is_none() || ops::enrichment_is_stale(&m))
            && has_input(&m)
        {
            meetings.push(m);
        }
    }
    let mut ids = Vec::new();
    for entry in &manifest.meetings {
        let path = entry
            .audio_path
            .canonicalize()
            .unwrap_or_else(|_| entry.audio_path.clone());
        let sources_exist = if entry.sources.is_empty() {
            entry.audio_path.is_file()
        } else {
            entry.sources.iter().all(|source| source.path.is_file())
        };
        let hash = sources_exist
            .then(|| broscribe_pipeline::batch::entry_hash(entry))
            .transpose()?;
        for m in &meetings {
            let Some(recording) = &m.recording else {
                continue;
            };
            if entry
                .source_id
                .as_ref()
                .zip(m.source_id.as_ref())
                .is_some_and(|(a, b)| a != b)
            {
                continue;
            }
            let matches = if let Some(hash) = &hash {
                hash.matches(recording)
            } else {
                // The import may have consumed its source; use its durable
                // source ID or original path in that case.
                entry
                    .source_id
                    .as_ref()
                    .is_some_and(|id| m.source_id.as_ref() == Some(id))
                    || recording
                        .original_path
                        .as_ref()
                        .is_some_and(|p| p.canonicalize().unwrap_or_else(|_| p.clone()) == path)
            };
            if matches && !ids.contains(&m.id) {
                ids.push(m.id.clone());
            }
        }
    }
    Ok(ids)
}

fn outcome_line(o: &EnrichOutcome) -> String {
    let mut line = format!(
        "Enriched {} with {}: {} decision(s), {} action(s), {} topic(s)",
        o.meeting_id, o.provider, o.decisions, o.actions, o.topics
    );
    if o.user_status_kept > 0 {
        line.push_str(&format!(
            ", {} user-set status(es) kept",
            o.user_status_kept
        ));
    }
    if o.stale_user_actions > 0 {
        line.push_str(&format!(
            ", {} user-set action(s) no longer produced (kept)",
            o.stale_user_actions
        ));
    }
    if o.stale {
        line.push_str(" (the transcript changed meanwhile: marked stale)");
    }
    if !o.needs_review.is_empty() {
        line.push_str("\nNeeds review (speaker references cleared by concurrent analysis):");
        for r in &o.needs_review {
            line.push_str(&format!("\n  {r}"));
        }
    }
    line
}

/// What the synchronous runs over `ids` gave.
#[derive(Debug, Default)]
struct Batch {
    /// Enriched and saved under their jobs' leases.
    done: Vec<EnrichOutcome>,
    /// `{meeting_id, job_id}`: a worker claimed the job first.
    queued: Vec<Value>,
    /// `{meeting_id, code, message}` (`--all` only; one meeting fails the
    /// command instead).
    failed: Vec<Value>,
    /// `{meeting_id, code, message}` of the run the interruption stopped:
    /// nothing was saved for it (`--all` only).
    interrupted: Option<Value>,
    /// Meetings never started because of the interruption (`--all` only).
    not_started: Vec<MeetingId>,
}

impl Batch {
    /// The interruption left work undone. A Ctrl-C after the last meeting
    /// was saved stopped nothing.
    fn stopped(&self) -> bool {
        self.interrupted.is_some() || !self.not_started.is_empty()
    }
}

/// Runs `run_one` for each of `ids` in order until `cancelled`. With one
/// meeting (`all == false`) any error is the command's error, as before;
/// with `--all`, an error while `cancelled` is set is the interrupted run
/// (the rest are `not_started`) and any other is listed under `failed`.
fn enrich_each(
    ids: &[MeetingId],
    all: bool,
    cancelled: &AtomicBool,
    mut run_one: impl FnMut(&MeetingId) -> Result<InteractiveEnrich>,
) -> Result<Batch> {
    let mut batch = Batch::default();
    for (i, id) in ids.iter().enumerate() {
        if cancelled.load(Ordering::SeqCst) {
            batch.not_started.extend_from_slice(&ids[i..]);
            break;
        }
        match run_one(id) {
            // Published under the job's lease: durable even when the
            // interruption came during the run.
            Ok(InteractiveEnrich::Ran(o)) => batch.done.push(o),
            Ok(InteractiveEnrich::Queued(job)) => {
                batch
                    .queued
                    .push(json!({ "meeting_id": id, "job_id": job }));
            }
            Err(e) if !all => return Err(e),
            Err(e) if cancelled.load(Ordering::SeqCst) => {
                batch.interrupted = Some(json!({
                    "meeting_id": id, "code": e.code(), "message": e.to_string()
                }));
                batch.not_started.extend_from_slice(&ids[i + 1..]);
                break;
            }
            Err(e) => batch.failed.push(json!({
                "meeting_id": id, "code": e.code(), "message": e.to_string()
            })),
        }
    }
    Ok(batch)
}

pub(super) fn run(ctx: &mut Ctx<'_>, a: EnrichArgs) -> Result<()> {
    if let Some(m) = &a.model {
        validate_model_name(m)?;
    }
    let instructions = if a.instructions_stdin {
        Some(read_stdin_text("enrich --instructions-stdin")?)
    } else {
        None
    };
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    if a.queue {
        return queue(ctx, &lib, &config, &a, instructions.as_deref());
    }
    let ids = match (&a.meeting, a.all) {
        (Some(m), false) => vec![meeting_id(m)?],
        (None, true) => meetings_to_enrich(&lib)?,
        _ => {
            return Err(Error::InvalidInput("give one meeting ID or --all".into()));
        }
    };
    // Nothing goes anywhere without consent; each run checks it again.
    let registry = ProviderRegistry::load(ctx.data_dir())?;
    let names = llm_provider_ids(&config, &registry, a.provider.as_deref())?;
    if a.all && !ids.is_empty() && !a.yes {
        let question = format!(
            "Send the transcripts of {} meeting(s) to {}? [y/N]",
            ids.len(),
            names.join(", ")
        );
        let interactive = !ctx.json && std::io::stdin().is_terminal();
        if !interactive {
            return Err(Error::InvalidInput(format!(
                "enrich --all would send {} meeting(s) to {}; add --yes to confirm",
                ids.len(),
                names.join(", ")
            )));
        }
        if !confirm_on_terminal(ctx, &question)? {
            return Err(Error::InvalidInput("nothing was enriched".into()));
        }
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    if !ids.is_empty() {
        watch_interrupts(
            cancelled.clone(),
            PROVIDER_STOP_NOTICE,
            ctx.background_notes(),
        );
    }
    let batch = enrich_each(&ids, a.all, &cancelled, |id| {
        // Consent is read again from the config file for each meeting: a
        // provider disabled meanwhile receives nothing more.
        run_enrich_interactive(
            &env(ctx, &lib, &config),
            ctx.data_dir(),
            ctx.deps.secrets.as_ref(),
            id,
            a.provider.as_deref(),
            a.model.as_deref(),
            instructions.as_deref(),
            &cancelled,
        )
    })?;
    // Follow-ups of what was saved, interrupted or not (a second Ctrl-C
    // still quits at once).
    let changed: Vec<MeetingId> = batch.done.iter().map(|o| o.meeting_id.clone()).collect();
    if !changed.is_empty() {
        super::after_identity(ctx, &lib, &config, Some(&changed));
    }
    if !a.all
        && let [only] = batch.queued.as_slice()
    {
        // The background worker claimed the job first: it runs it.
        let job = only["job_id"].clone();
        return ctx.emit(json!({ "queued": true, "job_id": job }), |out| {
            writeln!(
                out,
                "The background worker is enriching {}: job {}",
                ids[0],
                job.as_str().unwrap_or_default()
            )
        });
    }
    let stopped = batch.stopped();
    let mut value = json!({
        "status": if stopped { "interrupted" } else { "complete" },
        "enriched": to_value(&batch.done)?,
        "failed": batch.failed,
    });
    if a.all {
        value["queued"] = json!(batch.queued);
        value["interrupted"] = batch.interrupted.clone().unwrap_or(Value::Null);
        value["not_started"] = json!(batch.not_started);
    }
    ctx.emit(value, |out| write_batch(out, ids.is_empty(), &batch))?;
    if stopped {
        ctx.exit_code = Some(INTERRUPTED_EXIT);
    }
    Ok(())
}

fn write_batch(out: &mut dyn std::io::Write, none: bool, b: &Batch) -> std::io::Result<()> {
    if none {
        writeln!(out, "Nothing to enrich.")?;
    }
    for o in &b.done {
        writeln!(out, "{}", outcome_line(o))?;
    }
    for q in &b.queued {
        writeln!(
            out,
            "Left to the background worker {}: job {}",
            q["meeting_id"].as_str().unwrap_or_default(),
            q["job_id"].as_str().unwrap_or_default()
        )?;
    }
    for f in &b.failed {
        writeln!(
            out,
            "Failed {}: [{}] {}",
            f["meeting_id"].as_str().unwrap_or_default(),
            f["code"].as_str().unwrap_or_default(),
            f["message"].as_str().unwrap_or_default()
        )?;
    }
    if let Some(i) = &b.interrupted {
        writeln!(
            out,
            "Interrupted {}: nothing was saved for it.",
            i["meeting_id"].as_str().unwrap_or_default()
        )?;
    }
    if !b.not_started.is_empty() {
        let ids: Vec<String> = b.not_started.iter().map(ToString::to_string).collect();
        writeln!(
            out,
            "Not started (interrupted): {} meeting(s): {}",
            ids.len(),
            ids.join(", ")
        )?;
    }
    Ok(())
}

/// `enrich <meeting> --queue`: enqueues (or restarts a finished) `enrich`
/// job for the background worker and wakes it. `library_busy` while one
/// is queued or running. No provider is opened here; the worker checks
/// consent again when it starts.
fn queue(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    a: &EnrichArgs,
    instructions: Option<&str>,
) -> Result<()> {
    let id = meeting_id(
        a.meeting
            .as_deref()
            .ok_or_else(|| Error::InvalidInput("--queue needs one meeting ID".into()))?,
    )?;
    let registry = ProviderRegistry::load(ctx.data_dir())?;
    llm_provider_ids(config, &registry, a.provider.as_deref())?;
    let (job, baseline, run) = lib.write(|tx| {
        let baseline = tx
            .library()
            .load_meeting(&id)?
            .enrichment
            .map(|result| result.created_at);
        let job = enqueue_enrich_request(
            tx,
            &id,
            a.provider.as_deref(),
            a.model.as_deref(),
            instructions,
        )?;
        let run = broscribe_core::queue::Queue::open(tx.root())?
            .get(&job)?
            .generation_run;
        Ok((job, baseline, run))
    })?;
    if let Err(e) = super::kick_agent(ctx) {
        ctx.note(format!(
            "note: enrichment queued but the agent could not be notified: {e}"
        ));
    }
    ctx.emit(
        json!({ "queued": true, "job_id": job, "baseline": baseline, "run": run }),
        |out| writeln!(out, "Queued the enrichment of {id}: job {job}"),
    )
}

/// `enrich instructions show|set --stdin|clear`: the saved default
/// instructions, `{"instructions": string|null}`. Saved on a fresh copy of
/// the config under the data directory's config lock; no library is opened
/// or created.
pub(super) fn instructions(ctx: &mut Ctx<'_>, action: InstructionsCmd) -> Result<()> {
    let saved = match action {
        InstructionsCmd::Show => ctx.config()?.enrichment.instructions,
        InstructionsCmd::Set { .. } => {
            let text = ctx.input_text("enrich instructions set")?;
            let value = (!text.is_empty()).then_some(text);
            Config::update(ctx.data_dir(), |c| {
                ctx.check_request_cancelled()?;
                c.enrichment.instructions = value;
                Ok(())
            })?
            .0
            .enrichment
            .instructions
        }
        InstructionsCmd::Clear => {
            Config::update(ctx.data_dir(), |c| {
                ctx.check_request_cancelled()?;
                c.enrichment.instructions = None;
                Ok(())
            })?
            .0
            .enrichment
            .instructions
        }
    };
    ctx.emit(json!({ "instructions": saved }), |out| match &saved {
        Some(text) => writeln!(out, "{text}"),
        None => writeln!(
            out,
            "No saved instructions: summaries use the built-in focus."
        ),
    })
}

/// Queue enrichment after import/reprocessing. Provider discovery happens
/// before the library transaction; network/model work belongs to the worker.
/// `requested` (`import --enrich`) queues a pass the user asked for, never
/// `auto`; otherwise the pass is automatic (`auto_enrich`) and the worker
/// checks that policy again when it runs. `Null` when nothing was tried,
/// else `{queued: [job ID], failed: [{meeting_id, code, message}], errors:
/// [{code, message}]}`: `failed` holds the refused meetings and `errors`
/// the queue transaction's failure, if it did not finish.
pub(super) fn queue_after_import(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    ids: &[MeetingId],
    requested: bool,
) -> Value {
    if ids.is_empty() {
        return Value::Null;
    }
    if !requested {
        match ProviderRegistry::load(ctx.data_dir()) {
            Ok(reg) if auto_enrich_applies(config, &reg) => {}
            Ok(_) => return Value::Null,
            Err(e) => {
                ctx.note(format!("note: enrichment not queued: {e}"));
                return Value::Null;
            }
        }
    }
    // Queue writes are durable files: one refused meeting cannot roll back
    // the earlier ones. Keep their outcomes even if the transaction fails.
    let mut jobs = Vec::new();
    let mut failed = Vec::new();
    let error = lib
        .write(|tx| {
            for id in ids {
                let result = if requested {
                    enqueue_requested_enrich(tx, id)
                } else {
                    enqueue_enrich(tx, id)
                };
                match result {
                    Ok(job) => jobs.push(job),
                    Err(error) => failed.push((id, error)),
                }
            }
            Ok(())
        })
        .err();
    if !jobs.is_empty()
        && let Err(e) = super::kick_agent(ctx)
    {
        ctx.note(format!(
            "note: enrichment queued but daemon could not be notified: {e}"
        ));
    }
    let failed: Vec<_> = failed
        .into_iter()
        .map(|(id, error)| {
            ctx.note(format!("note: enrichment not queued for {id}: {error}"));
            json!({"meeting_id": id, "code": error.code(), "message": error.to_string()})
        })
        .collect();
    let errors: Vec<_> = error
        .into_iter()
        .map(|error| {
            ctx.note(format!(
                "note: enrichment queue transaction did not finish: {error}"
            ));
            json!({"code": error.code(), "message": error.to_string()})
        })
        .collect();
    json!({"queued": jobs, "failed": failed, "errors": errors})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(id: &str, needs_review: Vec<String>) -> EnrichOutcome {
        EnrichOutcome {
            meeting_id: MeetingId::parse(id).unwrap(),
            provider: "fake".into(),
            model: None,
            chunks: 1,
            topics: 1,
            keywords: 0,
            tags: 0,
            decisions: 1,
            actions: 1,
            user_status_kept: 0,
            stale_user_actions: 0,
            stale: true,
            issues: Vec::new(),
            needs_review,
            attempts: Vec::new(),
        }
    }

    fn ids(raw: &[&str]) -> Vec<MeetingId> {
        raw.iter().map(|id| MeetingId::parse(id).unwrap()).collect()
    }

    fn interrupted() -> Error {
        Error::Service {
            code: "interrupted",
            message: "enrichment was interrupted; nothing was saved".into(),
        }
    }

    #[test]
    fn human_output_lists_references_that_need_review() {
        let o = outcome("m-0000a1", vec!["action a1 owner: S2".into()]);
        let text = outcome_line(&o);
        assert!(text.contains("Needs review"), "{text}");
        assert!(text.contains("  action a1 owner: S2"), "{text}");
        assert_eq!(
            to_value(&o).unwrap()["needs_review"][0],
            "action a1 owner: S2"
        );
    }

    #[test]
    fn interrupted_all_keeps_what_was_saved_and_names_the_rest() {
        let all = ids(&["m-0000a1", "m-0000a2", "m-0000a3", "m-0000a4"]);
        let cancelled = AtomicBool::new(false);
        let mut calls = Vec::new();
        let batch = enrich_each(&all, true, &cancelled, |id| {
            calls.push(id.clone());
            match id.as_str() {
                "m-0000a1" => Ok(InteractiveEnrich::Ran(outcome("m-0000a1", Vec::new()))),
                // Ctrl-C arrives during the second run, which saves nothing.
                _ => {
                    cancelled.store(true, Ordering::SeqCst);
                    Err(interrupted())
                }
            }
        })
        .unwrap();
        assert_eq!(calls, all[..2], "nothing is sent after the interruption");
        assert!(batch.stopped());
        let done: Vec<&str> = batch.done.iter().map(|o| o.meeting_id.as_str()).collect();
        assert_eq!(
            done,
            ["m-0000a1"],
            "the saved meeting is reported, not dropped"
        );
        let stopped = batch.interrupted.as_ref().unwrap();
        assert_eq!(stopped["meeting_id"], "m-0000a2");
        assert_eq!(stopped["code"], "interrupted");
        assert!(batch.failed.is_empty(), "an interruption is not a failure");
        assert_eq!(batch.not_started, all[2..]);
        let mut text = Vec::new();
        write_batch(&mut text, false, &batch).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("Enriched m-0000a1"), "{text}");
        assert!(text.contains("Interrupted m-0000a2"), "{text}");
        assert!(text.contains("2 meeting(s): m-0000a3, m-0000a4"), "{text}");
    }

    #[test]
    fn interruption_after_the_last_save_stops_nothing() {
        let all = ids(&["m-0000a1"]);
        let cancelled = AtomicBool::new(false);
        let batch = enrich_each(&all, true, &cancelled, |id| {
            cancelled.store(true, Ordering::SeqCst);
            Ok(InteractiveEnrich::Ran(outcome(id.as_str(), Vec::new())))
        })
        .unwrap();
        assert!(!batch.stopped());
        assert_eq!(batch.done.len(), 1);
    }

    #[test]
    fn all_lists_failures_and_goes_on_but_one_meeting_fails_the_command() {
        let all = ids(&["m-0000a1", "m-0000a2"]);
        let cancelled = AtomicBool::new(false);
        let batch = enrich_each(&all, true, &cancelled, |id| match id.as_str() {
            "m-0000a1" => Err(Error::ConsentRequired("fake".into())),
            _ => Ok(InteractiveEnrich::Ran(outcome("m-0000a2", Vec::new()))),
        })
        .unwrap();
        assert_eq!(batch.failed.len(), 1);
        assert_eq!(batch.failed[0]["code"], "consent_required");
        assert_eq!(batch.done.len(), 1);
        assert!(!batch.stopped());

        let one = ids(&["m-0000a1"]);
        cancelled.store(false, Ordering::SeqCst);
        let err = enrich_each(&one, false, &cancelled, |_| {
            cancelled.store(true, Ordering::SeqCst);
            Err(interrupted())
        })
        .unwrap_err();
        assert_eq!(err.code(), "interrupted");
    }
}
