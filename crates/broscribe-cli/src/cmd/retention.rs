//! `retention apply [--dry-run [--policy <VALUE>]]`,
//! `retention policy <VALUE> [--dry-run | --yes]`, `retention keep <meeting>`,
//! `retention delete-now <meeting> (--dry-run | --yes)` and
//! `retention explain [<meeting>…]` (design §7.2).
//!
//! `delete-now` plans with the same rules a deletion decides by
//! (`broscribe_core::retention::explain` with the override applied and the
//! live-job rule of `broscribe_pipeline::retention::explain`): a dry run
//! reports that plan, and `--yes` refuses while an import job holds the
//! only copy of a moved original (`only_copy_in_flight`). A recording moved
//! into the library is the user's only copy: the plan says so and, on an
//! interactive terminal, `--yes` asks once more before deleting it.

use std::io::IsTerminal;

use broscribe_core::config::{Config, Retention};
use broscribe_core::lifecycle::ONLY_COPY_IN_FLIGHT;
use broscribe_core::queue::{JobId, Queue, job_refers_to};
use broscribe_core::retention::{
    self as core_retention, KeepReason, PreviewFailure, RetentionStatus, Verdict,
};
use broscribe_core::schema::{self, Handling, Meeting, RecordingState, RetentionOverride};
use broscribe_core::{Error, Library, MeetingId, Result, ops};
use broscribe_pipeline::retention::{
    self, RetentionOptions, RetentionReport, apply as apply_fixed, apply_reloading,
};
use serde::Serialize;
use serde_json::{Value, json};

use super::meeting_id;
use crate::cli::{Command, RetentionCmd};
use crate::{Ctx, to_value};

pub(super) fn apply(ctx: &mut Ctx<'_>, dry_run: bool, policy: Option<Retention>) -> Result<()> {
    if policy.is_some() && !dry_run {
        return Err(Error::InvalidInput(
            "--policy requires --dry-run; use `broscribe retention policy <value>` to save a policy"
                .into(),
        ));
    }
    if let Some(proposed) = policy {
        return preview(ctx, proposed);
    }
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    // Each deletion decides under the lock with the policy configured then:
    // the user may switch it during a long pass.
    let report = apply_reloading(
        &lib,
        &config,
        ctx.data_dir(),
        ctx.deps.process.as_ref(),
        &RetentionOptions {
            dry_run,
            meetings: None,
            cache_dir: Some(ctx.paths.cache_dir.clone()),
            now: None,
        },
    )?;
    let recordings = report.deleted.len();
    let total_bytes: u64 = report.deleted.iter().map(|d| d.bytes).sum();
    let mut value = to_value(&report)?;
    value["policy"] = config.retention.cli_value().into();
    value["recordings"] = recordings.into();
    value["bytes"] = total_bytes.into();
    ctx.emit(value, |out| {
        let verb = if dry_run { "Would delete" } else { "Deleted" };
        writeln!(
            out,
            "{verb} {recordings} recording(s) ({total_bytes} bytes) of {} meetings checked.",
            report.checked
        )?;
        for d in &report.deleted {
            writeln!(
                out,
                "  {} ({}, {} bytes)",
                d.meeting_id,
                to_value(&d.reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                d.bytes
            )?;
        }
        if report.kept_for_live_jobs > 0 {
            writeln!(
                out,
                "{} due recording(s) kept while jobs work on them.",
                report.kept_for_live_jobs
            )?;
        }
        if report.unreadable_people > 0 {
            let left = if dry_run {
                "would be left alone"
            } else {
                "was left alone"
            };
            writeln!(
                out,
                "{} person profile(s) with a kept voice reading couldn't be read, so the reading \
                 {left}.",
                report.unreadable_people
            )?;
        }
        Ok(())
    })
}

/// `retention apply --dry-run --policy <VALUE>`: what `proposed` would delete
/// if it were saved now, as counts and bytes only (no titles, paths or meeting
/// IDs; SET-01). Changes no library file or setting and creates no queue
/// folder. A failure keeps the error's machine code with a path-free
/// [`PreviewFailure`] cause as its message.
fn preview(ctx: &mut Ctx<'_>, proposed: Retention) -> Result<()> {
    let config = ctx.config().map_err(|e| preview_error(&e, true))?;
    let report = preview_report(ctx, config, proposed)?;
    ctx.emit(preview_value(&report, proposed), |out| {
        write_preview(out, &report, proposed)
    })
}

fn preview_report(
    ctx: &Ctx<'_>,
    mut config: Config,
    proposed: Retention,
) -> Result<RetentionReport> {
    let lib = ctx
        .library_existing(&config)
        .map_err(|e| preview_error(&e, false))?;
    config.retention = proposed;
    apply_fixed(
        &lib,
        &config,
        ctx.deps.process.as_ref(),
        &RetentionOptions {
            dry_run: true,
            meetings: None,
            cache_dir: Some(ctx.paths.cache_dir.clone()),
            now: None,
        },
    )
    .map_err(|e| preview_error(&e, false))
}

fn preview_value(report: &RetentionReport, proposed: Retention) -> Value {
    let recordings = report.deleted.len();
    let bytes: u64 = report.deleted.iter().map(|d| d.bytes).sum();
    json!({
        "dry_run": true,
        "policy": proposed.cli_value(),
        "checked": report.checked,
        "recordings": recordings,
        "bytes": bytes,
        "kept_for_live_jobs": report.kept_for_live_jobs,
        "kept_for_live_jobs_bytes": report.kept_for_live_jobs_bytes,
        "due_dates_updated": report.due_dates_updated,
        "leftovers_removed": report.leftovers_removed,
        "enrollment_files_removed": report.enrollment_files_removed,
        "unreadable": report.unreadable,
        "unreadable_people": report.unreadable_people,
        "unreadable_jobs": report.unreadable_jobs,
    })
}

fn write_preview(
    out: &mut dyn std::io::Write,
    report: &RetentionReport,
    proposed: Retention,
) -> std::io::Result<()> {
    let name = proposed.cli_value();
    let recordings = report.deleted.len();
    let bytes: u64 = report.deleted.iter().map(|d| d.bytes).sum();
    writeln!(
        out,
        "Would delete {recordings} recording(s) ({bytes} bytes) of {} meetings checked (policy: {name}).",
        report.checked
    )?;
    if report.kept_for_live_jobs > 0 {
        writeln!(
            out,
            "Would delete {} more recording(s) ({} bytes) once the jobs working on them finish.",
            report.kept_for_live_jobs, report.kept_for_live_jobs_bytes
        )?;
    }
    if report.enrollment_files_removed > 0 {
        writeln!(
            out,
            "Would delete {} kept voice reading(s).",
            report.enrollment_files_removed
        )?;
    }
    if report.unreadable > 0 {
        writeln!(
            out,
            "{} meeting(s) couldn't be read, so this count is incomplete.",
            report.unreadable
        )?;
    }
    if report.unreadable_people > 0 {
        writeln!(
            out,
            "{} person profile(s) with a kept voice reading couldn't be read, so the voice \
                 reading count is incomplete.",
            report.unreadable_people
        )?;
    }
    Ok(())
}

/// Whether `command` can check a proposed policy. Its errors before the
/// command runs name no paths.
pub(crate) fn is_policy_preview(command: &Command) -> bool {
    matches!(
        command,
        Command::Retention {
            action: RetentionCmd::Policy { .. }
                | RetentionCmd::Apply {
                    policy: Some(_),
                    ..
                }
        }
    )
}

/// A failure of a proposed-policy check met before the command ran
/// (resolving the data folder, taking its admission lock): its machine code
/// with a path-free cause. A maintenance refusal names no path and keeps its
/// own message.
pub(crate) fn preview_admission_error(error: Error) -> Error {
    match error.code() {
        broscribe_core::maintenance::ERROR_CODE | broscribe_core::maintenance::BUSY_CODE => error,
        code => Error::Service {
            code,
            message: PreviewFailure::from_data_folder_code(code)
                .text()
                .to_owned(),
        },
    }
}

/// `error` with its machine code and a path-free cause as its message.
/// `settings`: it came from reading `config.toml`.
fn preview_error(error: &Error, settings: bool) -> Error {
    Error::Service {
        code: error.code(),
        message: PreviewFailure::from_code(error.code(), settings)
            .text()
            .to_owned(),
    }
}

/// The saved policy affects the configured library and the agent's open
/// library. Both must be the library the command has checked.
fn check_policy_library(ctx: &Ctx<'_>, config: &Config) -> Result<()> {
    let open = std::path::absolute(ctx.library_path(config).0)?;
    let configured = std::path::absolute(
        config
            .library
            .as_deref()
            .unwrap_or(&ctx.paths.default_library_dir),
    )?;
    if open != configured {
        return Err(Error::Service {
            code: "retention_library_mismatch",
            message: "Settings name a different library. Run the command on the configured library before changing retention.".into(),
        });
    }
    if let Some(status) = super::daemon::optional_call(ctx.data_dir(), broscribe_ipc::Call::Status)?
    {
        let matches = status["library"].as_str().is_some_and(|library| {
            !library.contains(char::REPLACEMENT_CHARACTER)
                && std::path::Path::new(library).is_absolute()
                && std::path::absolute(library).is_ok_and(|agent| agent == open)
        });
        if !matches {
            return Err(Error::Service {
                code: "retention_agent_library_mismatch",
                message: "The recording agent's library does not match Settings. Start the agent on the configured library before changing retention.".into(),
            });
        }
    }
    Ok(())
}

/// Previews stricter policies and saves only against the settings checked.
pub(super) fn policy(
    ctx: &mut Ctx<'_>,
    retention: Retention,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let baseline = ctx.config().map_err(|e| preview_error(&e, true))?;
    let stricter = retention.may_delete_more_than(baseline.retention);
    let preview = if stricter || dry_run {
        check_policy_library(ctx, &baseline)?;
        let report = preview_report(ctx, baseline.clone(), retention)?;
        if stricter && !dry_run && (report.unreadable > 0 || report.unreadable_people > 0) {
            return Err(Error::Service {
                code: "retention_preview_incomplete",
                message: "Couldn't check all meetings and profiles with kept voice readings. The retention policy is unchanged.".into(),
            });
        }
        let confirmation_required = stricter && !yes;
        if dry_run || confirmation_required {
            let mut value = preview_value(&report, retention);
            value["changed"] = false.into();
            value["confirmation_required"] = confirmation_required.into();
            value["revision"] = baseline.revision().into();
            value["retention"] = to_value(&baseline.retention)?;
            return ctx.emit(value, |out| {
                write_preview(out, &report, retention)?;
                if confirmation_required && !dry_run {
                    writeln!(
                        out,
                        "Policy unchanged. Use --yes to save this stricter policy."
                    )?;
                }
                Ok(())
            });
        }
        check_policy_library(ctx, &baseline)?;
        Some(report)
    } else {
        None
    };
    let (config, changed) = Config::update(ctx.data_dir(), |config| {
        if config.retention != baseline.retention
            || (stricter && config.library != baseline.library)
        {
            return Err(Error::Service {
                code: "retention_settings_changed",
                message: "The retention policy or its library changed during this command. Review the policy and try again.".into(),
            });
        }
        let changed = config.retention != retention;
        config.retention = retention;
        Ok(changed)
    })?;
    let name = config.retention.cli_value();
    let mut value = json!({
        "policy": name,
        "retention": config.retention,
        "changed": changed,
        "revision": config.revision(),
    });
    if let Some(report) = &preview {
        value["preview"] = preview_value(report, retention);
    }
    ctx.emit(value, |out| {
        if let Some(report) = &preview {
            write_preview(out, report, retention)?;
        }
        let description = match config.retention {
            Retention::KeepForever => "forever".to_owned(),
            Retention::DeleteImmediately {
                wait_until_named: false,
            } => "delete once processed".to_owned(),
            Retention::DeleteImmediately {
                wait_until_named: true,
            } => "delete once processed and named".to_owned(),
            Retention::DeleteAfterDays { days: 1 } => "delete after 1 day".to_owned(),
            Retention::DeleteAfterDays { days } => format!("delete after {days} days"),
        };
        writeln!(out, "Retention policy: {name} ({description})")
    })
}

/// `retention explain [<meeting>…]`: every meeting's (or these meetings')
/// verdict with its reasons, the shown speakers a `wait_until_named`
/// recording waits for and the queue jobs holding it. Reads only.
pub(super) fn explain(ctx: &mut Ctx<'_>, meetings: &[String]) -> Result<()> {
    let ids = meetings
        .iter()
        .map(|m| meeting_id(m))
        .collect::<Result<Vec<_>>>()?;
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let report = retention::explain(
        &lib,
        &config,
        ctx.deps.process.as_ref(),
        (!ids.is_empty()).then_some(ids.as_slice()),
        None,
    )?;
    ctx.emit(to_value(&report)?, |out| {
        if report.meetings.is_empty() {
            writeln!(out, "No meetings.")?;
        }
        for m in &report.meetings {
            let mut line = format!("{}  {}", m.meeting_id, verdict_str(m.status.verdict));
            if let Some(due) = m.status.due_at {
                line.push_str(&format!("  due {}", due.to_rfc3339()));
            }
            if !m.status.blocking_speakers.is_empty() {
                let speakers: Vec<String> = m
                    .status
                    .blocking_speakers
                    .iter()
                    .map(|s| match s.dismissed_at {
                        Some(at) => format!(
                            "{} (can't tell, settles {})",
                            s.speaker_id,
                            (at + core_retention::DISMISS_GRACE).to_rfc3339()
                        ),
                        None => format!("{} (unnamed)", s.speaker_id),
                    })
                    .collect();
                line.push_str(&format!("  waiting for: {}", speakers.join(", ")));
            }
            if !m.live_jobs.is_empty() {
                let jobs: Vec<&str> = m.live_jobs.iter().map(JobId::as_str).collect();
                line.push_str(&format!("  jobs: {}", jobs.join(", ")));
            }
            writeln!(out, "{line}")?;
        }
        if report.unreadable > 0 {
            writeln!(out, "{} meeting(s) could not be read.", report.unreadable)?;
        }
        if report.unreadable_jobs > 0 {
            writeln!(
                out,
                "{} queue record(s) could not be read (they hold nothing back).",
                report.unreadable_jobs
            )?;
        }
        Ok(())
    })
}

/// `keep 2026-…`/`delete 2026-…`: the verdict as one phrase.
fn verdict_str(v: Verdict) -> String {
    let reason = |value: Result<Value>| {
        value
            .ok()
            .and_then(|v| v["reason"].as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    match v {
        Verdict::Delete(_) => format!("due ({})", reason(to_value(&v))),
        Verdict::Keep(_) => format!("kept ({})", reason(to_value(&v))),
    }
}

/// `retention keep <meeting>`: keeps the recording whatever the policy
/// says, then reapplies retention to the meeting (which clears its due
/// date).
pub(super) fn keep(ctx: &mut Ctx<'_>, meeting: &str) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let id = meeting_id(meeting)?;
    set_override(&lib, &id, RetentionOverride::Keep)?;
    let (report, retention_error) = apply_after_override(ctx, &lib, &config, &id);
    let mut value = json!({
        "meeting_id": id,
        "retention_override": RetentionOverride::Keep,
        "recording_state": recording_state(&lib, &id),
        "due_dates_updated": report.as_ref().map_or(0, |r| r.due_dates_updated),
    });
    if let Some(e) = &retention_error {
        value["retention_error"] = e.clone();
    }
    ctx.emit(value, |out| {
        writeln!(
            out,
            "{id}: the recording is kept whatever the retention policy."
        )
    })
}

/// What `retention delete-now` would do now (the `--dry-run` output).
#[derive(Serialize)]
struct DeleteNowPlan {
    meeting_id: MeetingId,
    dry_run: bool,
    /// The verdict with the delete-now override applied, and its reasons.
    #[serde(flatten)]
    status: RetentionStatus,
    /// The recording's recorded size.
    bytes: u64,
    /// The recording was moved into the library on import: deleting it
    /// deletes the user's only copy.
    moved_original: bool,
    /// An import job whose folder holds the only copy of the moved
    /// original: `--yes` is refused (`only_copy_in_flight`).
    #[serde(skip_serializing_if = "Option::is_none")]
    only_copy_in_flight: Option<JobId>,
    /// Queue jobs working on the meeting (the recording goes once they end).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    live_jobs: Vec<JobId>,
}

fn plan_delete_now(
    lib: &Library,
    config: &Config,
    probe: &dyn broscribe_core::process::ProcessProbe,
    id: &MeetingId,
) -> Result<DeleteNowPlan> {
    let mut m = lib.load_meeting(id)?;
    if m.recording.is_none() {
        return Err(Error::NotFound(format!("{id} has no recording")));
    }
    // The live-job rule of the deletion itself: whether a job holds the
    // meeting now (retention's verdict is `live_jobs` exactly then, unless
    // an earlier reason keeps the recording anyway).
    let current = retention::explain(lib, config, probe, Some(std::slice::from_ref(id)), None)?
        .meetings
        .pop()
        .ok_or_else(|| Error::NotFound(format!("meeting {id}")))?;
    let live = current.status.verdict == Verdict::Keep(KeepReason::LiveJobs);
    let only_copy_in_flight = only_copy_job(lib, &m)?;
    let Some(rec) = m.recording.as_mut() else {
        return Err(Error::NotFound(format!("{id} has no recording")));
    };
    rec.retention_override = Some(RetentionOverride::DeleteNow);
    let moved_original = rec.handling == Handling::Move && rec.state == RecordingState::Available;
    let bytes = rec.bytes;
    let status = core_retention::explain(&m, config, schema::now(), live);
    Ok(DeleteNowPlan {
        meeting_id: m.id,
        dry_run: true,
        status,
        bytes,
        moved_original,
        only_copy_in_flight,
        live_jobs: current.live_jobs,
    })
}

/// The import job whose folder holds the only copy of `m`'s moved original.
fn only_copy_job(lib: &Library, m: &Meeting) -> Result<Option<JobId>> {
    let Some(queue) = Queue::open_existing(lib.root())? else {
        return Ok(None);
    };
    Ok(queue
        .jobs()?
        .into_iter()
        .find(|job| job_refers_to(job, m) && queue.holds_only_copy(job))
        .map(|job| job.id))
}

/// `retention delete-now <meeting> --dry-run | --yes`: the plan, or the
/// deletion. The override is saved first; once it is, a failed retention
/// pass is reported (`retention_error`) and never undoes or hides it: the
/// next pass deletes the recording.
pub(super) fn delete_now(ctx: &mut Ctx<'_>, meeting: &str, dry_run: bool) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let id = meeting_id(meeting)?;
    let plan = plan_delete_now(&lib, &config, ctx.deps.process.as_ref(), &id)?;
    if dry_run {
        return emit_plan(ctx, &plan);
    }
    if let Some(job) = &plan.only_copy_in_flight {
        return Err(Error::Service {
            code: ONLY_COPY_IN_FLIGHT,
            message: format!(
                "an import job ({job}) holds the only copy of this meeting's moved original; \
                 let it finish first"
            ),
        });
    }
    match plan.status.verdict {
        Verdict::Keep(KeepReason::Reference) => {
            return Err(Error::InvalidInput(format!(
                "the recording of {id} is a referenced original outside the library; \
                 BroScribe never deletes it"
            )));
        }
        Verdict::Keep(KeepReason::NotAvailable) => {
            return ctx.emit(
                json!({
                    "meeting_id": id,
                    "deleted": false,
                    "recording_state": recording_state(&lib, &id),
                }),
                |out| writeln!(out, "{id}: the recording is already deleted or missing."),
            );
        }
        _ => {}
    }
    if plan.moved_original {
        let warning = format!(
            "The recording of {id} was moved into the library: it is your only copy of that file."
        );
        if !ctx.json && std::io::stdin().is_terminal() {
            if !super::confirm_on_terminal(ctx, &format!("{warning}\nDelete it? [y/N]"))? {
                return Err(Error::InvalidInput("nothing was deleted".into()));
            }
        } else {
            ctx.note(format!("warning: {warning}"));
        }
    }
    set_override(&lib, &id, RetentionOverride::DeleteNow)?;
    let (report, retention_error) = apply_after_override(ctx, &lib, &config, &id);
    let deleted = report.as_ref().is_some_and(|r| !r.deleted.is_empty());
    let kept_for_live_jobs = report.as_ref().map_or(0, |r| r.kept_for_live_jobs);
    let mut value = json!({
        "meeting_id": id,
        "retention_override": RetentionOverride::DeleteNow,
        "deleted": deleted,
        "recording_state": recording_state(&lib, &id),
        "kept_for_live_jobs": kept_for_live_jobs,
        "moved_original": plan.moved_original,
    });
    if let Some(e) = &retention_error {
        value["retention_error"] = e.clone();
    }
    let waiting = plan.status.verdict;
    ctx.emit(value, |out| {
        if deleted {
            writeln!(out, "Deleted the recording of {id}; the transcript stays.")?;
            if plan.moved_original {
                writeln!(
                    out,
                    "It was the only copy of the file moved into the library."
                )?;
            }
            return Ok(());
        }
        match waiting {
            Verdict::Keep(KeepReason::NotProcessed) => writeln!(
                out,
                "{id}: the recording will be deleted once processing finishes."
            ),
            _ if kept_for_live_jobs > 0 || waiting == Verdict::Keep(KeepReason::LiveJobs) => {
                writeln!(
                    out,
                    "{id}: the recording will be deleted once its running jobs finish."
                )
            }
            _ => writeln!(
                out,
                "{id}: the recording will be deleted by the next retention pass."
            ),
        }
    })
}

fn emit_plan(ctx: &mut Ctx<'_>, plan: &DeleteNowPlan) -> Result<()> {
    ctx.emit(to_value(plan)?, |out| {
        let id = &plan.meeting_id;
        match plan.status.verdict {
            Verdict::Delete(_) => writeln!(
                out,
                "Would delete the recording of {id} ({} bytes); the transcript stays.",
                plan.bytes
            )?,
            Verdict::Keep(KeepReason::LiveJobs) => writeln!(
                out,
                "A job is working on {id}: the recording would be deleted once it ends."
            )?,
            Verdict::Keep(KeepReason::NotProcessed) => writeln!(
                out,
                "{id} is not processed yet: the recording would be deleted once it is."
            )?,
            Verdict::Keep(KeepReason::Reference) => writeln!(
                out,
                "The recording of {id} is a referenced original outside the library: it is \
                 never deleted."
            )?,
            Verdict::Keep(KeepReason::NotAvailable) => {
                writeln!(out, "The recording of {id} is already deleted or missing.")?
            }
            Verdict::Keep(_) => writeln!(out, "{id}: {}", verdict_str(plan.status.verdict))?,
        }
        if plan.moved_original {
            writeln!(
                out,
                "Warning: this recording was moved into the library; it is your only copy."
            )?;
        }
        if let Some(job) = &plan.only_copy_in_flight {
            writeln!(
                out,
                "Refused with --yes: import job {job} holds the only copy of the moved \
                 original; let it finish first."
            )?;
        }
        writeln!(out, "Nothing was deleted; run again with --yes to delete.")
    })
}

/// Saves the meeting's retention override under the library lock.
fn set_override(lib: &Library, id: &MeetingId, value: RetentionOverride) -> Result<()> {
    ops::with_index(lib, |tx, index| {
        let mut m = tx.library().load_meeting(id)?;
        let Some(rec) = m.recording.as_mut() else {
            return Err(Error::NotFound(format!("{id} has no recording")));
        };
        if rec.retention_override != Some(value) {
            rec.retention_override = Some(value);
            m.updated_at = schema::now();
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)?;
        }
        Ok(())
    })
}

/// Retention for `id` after its override was saved. The override is
/// durable by then: a failure (a policy that cannot be reloaded, a busy
/// library) is returned as `{code, message}` for the output, never as the
/// command's error.
fn apply_after_override(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    id: &MeetingId,
) -> (Option<RetentionReport>, Option<Value>) {
    let result = apply_reloading(
        lib,
        config,
        ctx.data_dir(),
        ctx.deps.process.as_ref(),
        &RetentionOptions {
            dry_run: false,
            meetings: Some(vec![id.clone()]),
            cache_dir: Some(ctx.paths.cache_dir.clone()),
            now: None,
        },
    );
    match result {
        Ok(report) => (Some(report), None),
        Err(e) => {
            ctx.note(format!(
                "note: the choice for {id} is saved, but retention was not applied now: {e}"
            ));
            (
                None,
                Some(json!({ "code": e.code(), "message": e.to_string() })),
            )
        }
    }
}

/// The recording's state as saved now; `null` when unreadable.
fn recording_state(lib: &Library, id: &MeetingId) -> Value {
    lib.load_meeting(id)
        .ok()
        .and_then(|m| m.recording)
        .and_then(|r| to_value(&r.state).ok())
        .unwrap_or(Value::Null)
}

/// Public CLI runs against an isolated synthetic data folder and library;
/// [`tests::run`] is shared with the other command modules' tests.
#[cfg(test)]
pub(super) mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use broscribe_core::schema::{ProcessingStatus, Recording, Source};
    use broscribe_models::ExecutionContext;
    use broscribe_models::fakes::FakeModelProvider;
    use broscribe_platform::DecoderChain;
    use broscribe_platform::fakes::{
        FakeAutostart, FakeBrowserLauncher, FakeMicCapture, FakePaths, FakeProcessProbe,
        FakeProcessTreeControl, FakeSecretStore, FakeTrustStore,
    };

    use super::*;
    use crate::{AgentEnv, Deps};

    fn deps(root: &Path) -> Deps {
        Deps {
            clock: crate::system_clock(),
            paths: Box::new(FakePaths::new(root)),
            updates: crate::UpdateDeps::with_transport(
                std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                    std::path::Path::new(&root).join("install"),
                    "synthetic-test-target",
                )),
                std::sync::Arc::new(broscribe_updates::FakeTransport::default()),
            ),
            models: Box::new(FakeModelProvider::default()),
            decoder: DecoderChain::default(),
            process: Box::new(FakeProcessProbe::new(1)),
            mic: Box::new(FakeMicCapture::new(16_000)),
            secrets: Arc::new(FakeSecretStore::default()),
            browser: Arc::new(FakeBrowserLauncher::default()),
            agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
            autostart: Box::new(FakeAutostart::default()),
            agents: AgentEnv::isolated(root),
            execution: ExecutionContext::new(
                Arc::new(FakeProcessTreeControl::default()),
                Arc::new(FakeTrustStore::default()),
            ),
        }
    }

    /// `broscribe --json --library <root>/library <args>`: the exit code
    /// and the JSON written (the result, or the error envelope).
    pub(in crate::cmd) fn run(root: &Path, args: &[&str]) -> (i32, Value) {
        let mut argv: Vec<OsString> = vec!["broscribe".into(), "--json".into(), "--library".into()];
        argv.push(root.join("library").into());
        argv.extend(args.iter().map(OsString::from));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = crate::run(argv, &deps(root), &mut out, &mut err);
        let value = serde_json::from_slice(&out)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out)));
        (code, value)
    }

    /// `broscribe --library <root>/library <args>` in human mode: exit code,
    /// stdout and stderr as UTF-8 strings.
    fn run_text(root: &Path, args: &[&str]) -> (i32, String, String) {
        let mut argv: Vec<OsString> = vec!["broscribe".into(), "--library".into()];
        argv.push(root.join("library").into());
        argv.extend(args.iter().map(OsString::from));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = crate::run(argv, &deps(root), &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    /// A synthetic meeting whose recording file exists in its folder
    /// (`Reference`: an absolute path outside the library).
    fn meeting(root: &Path, handling: Handling, status: ProcessingStatus) -> (MeetingId, PathBuf) {
        meeting_named(root, handling, status, None, None)
    }

    fn meeting_named(
        root: &Path,
        handling: Handling,
        status: ProcessingStatus,
        title: Option<&str>,
        started_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    ) -> (MeetingId, PathBuf) {
        let lib = Library::open(root.join("library")).unwrap();
        let mut m = Meeting::new(
            lib.new_meeting_id().unwrap(),
            Source::Import,
            started_at.unwrap_or_else(schema::now),
        );
        m.processing.status = status;
        if let Some(title) = title {
            m.title = Some(title.to_owned());
        }
        let path = if handling == Handling::Reference {
            root.join("original.m4a")
        } else {
            PathBuf::from("audio.m4a")
        };
        m.recording = Some(Recording {
            state: RecordingState::Available,
            handling,
            path: path.clone(),
            original_path: None,
            sha256: "ab".repeat(32),
            source_sha256: None,
            sources: Vec::new(),
            bytes: 9,
            retention_override: None,
            delete_after: None,
        });
        lib.write(|tx| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)
        })
        .unwrap();
        let file = lib.meeting_dir(&m.id).unwrap().join(&path);
        std::fs::write(&file, b"synthetic").unwrap();
        (m.id, file)
    }

    fn saved(root: &Path, id: &MeetingId) -> Recording {
        Library::open_existing(root.join("library"))
            .unwrap()
            .load_meeting(id)
            .unwrap()
            .recording
            .unwrap()
    }

    #[test]
    fn delete_now_dry_run_warns_about_a_moved_only_copy_and_yes_deletes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, file) = meeting(tmp.path(), Handling::Move, ProcessingStatus::Done);
        let (code, plan) = run(
            tmp.path(),
            &["retention", "delete-now", id.as_str(), "--dry-run"],
        );
        assert_eq!(code, 0, "{plan}");
        assert_eq!(plan["dry_run"], true);
        assert_eq!(plan["verdict"], "delete");
        assert_eq!(plan["reason"], "override");
        assert_eq!(plan["moved_original"], true);
        assert!(plan.get("only_copy_in_flight").is_none(), "{plan}");
        // A dry run writes nothing: no override, the file stays.
        assert_eq!(saved(tmp.path(), &id).retention_override, None);
        assert!(file.exists());

        let (code, done) = run(
            tmp.path(),
            &["retention", "delete-now", id.as_str(), "--yes"],
        );
        assert_eq!(code, 0, "{done}");
        assert_eq!(done["deleted"], true);
        assert_eq!(done["moved_original"], true);
        assert_eq!(done["recording_state"], "deleted");
        assert!(done.get("retention_error").is_none(), "{done}");
        assert!(!file.exists());
        let rec = saved(tmp.path(), &id);
        assert_eq!(rec.retention_override, Some(RetentionOverride::DeleteNow));
        assert_eq!(rec.state, RecordingState::Deleted);
    }

    #[test]
    fn delete_now_refuses_a_referenced_original_and_waits_for_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let (reference, original) =
            meeting(tmp.path(), Handling::Reference, ProcessingStatus::Done);
        let (code, plan) = run(
            tmp.path(),
            &["retention", "delete-now", reference.as_str(), "--dry-run"],
        );
        assert_eq!(code, 0, "{plan}");
        assert_eq!(
            (&plan["verdict"], &plan["reason"]),
            (&json!("keep"), &json!("reference"))
        );
        let (code, refused) = run(
            tmp.path(),
            &["retention", "delete-now", reference.as_str(), "--yes"],
        );
        assert_ne!(code, 0);
        assert_eq!(refused["error"]["code"], "invalid_input");
        assert!(original.exists());
        assert_eq!(saved(tmp.path(), &reference).retention_override, None);

        // Not processed yet: the choice is saved and the recording stays
        // until processing finishes; explain gives that reason.
        let (pending, file) = meeting(tmp.path(), Handling::Copy, ProcessingStatus::Queued);
        let (code, done) = run(
            tmp.path(),
            &["retention", "delete-now", pending.as_str(), "--yes"],
        );
        assert_eq!(code, 0, "{done}");
        assert_eq!(done["deleted"], false);
        assert_eq!(done["moved_original"], false);
        assert!(file.exists());
        assert_eq!(
            saved(tmp.path(), &pending).retention_override,
            Some(RetentionOverride::DeleteNow)
        );
        let (code, explained) = run(tmp.path(), &["retention", "explain", pending.as_str()]);
        assert_eq!(code, 0, "{explained}");
        let rows = explained["meetings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["meeting_id"], pending.as_str());
        assert_eq!(rows[0]["verdict"], "keep");
        assert_eq!(rows[0]["reason"], "not_processed");
    }

    #[test]
    fn apply_dry_run_with_policy_reports_counts_and_bytes_only_without_mutating() {
        let tmp = tempfile::tempdir().unwrap();
        let secret_title = "Confidential Synthetic Quarterly Review";
        let old = schema::now() - chrono::TimeDelta::days(45);
        let recent = schema::now() - chrono::TimeDelta::days(2);
        let (old_id, old_file) = meeting_named(
            tmp.path(),
            Handling::Copy,
            ProcessingStatus::Done,
            Some(secret_title),
            Some(old),
        );
        let (recent_id, recent_file) = meeting_named(
            tmp.path(),
            Handling::Copy,
            ProcessingStatus::Done,
            Some("Recent Synthetic Standup"),
            Some(recent),
        );
        // Remove any `.queue` created by `Library::open` during test setup so
        // we can prove `--dry-run` does not recreate it.
        let _ = std::fs::remove_dir_all(tmp.path().join("library/.queue"));

        // Saved policy stays KeepForever; simulating `processed` reports both
        // recordings and their bytes without printing titles, paths or ids.
        let (code, dry) = run(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 0, "{dry}");
        assert_eq!(dry["dry_run"], true);
        assert_eq!(dry["policy"], "processed");
        assert_eq!(dry["checked"], 2);
        assert_eq!(dry["recordings"], 2);
        assert_eq!(dry["bytes"], 18);
        assert_eq!(dry["kept_for_live_jobs"], 0);
        assert_eq!(dry["kept_for_live_jobs_bytes"], 0);
        assert_eq!(dry["enrollment_files_removed"], 0);
        assert_eq!(dry["unreadable"], 0);
        assert!(dry.get("deleted").is_none(), "counts and bytes only: {dry}");
        let json_str = dry.to_string();
        assert!(!json_str.contains(secret_title), "{json_str}");
        assert!(!json_str.contains("audio.m4a"), "{json_str}");
        assert!(!json_str.contains(old_id.as_str()), "{json_str}");

        // Simulating `days:30` catches only the 45-day-old meeting.
        let (code, dry_days) = run(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "days:30"],
        );
        assert_eq!(code, 0, "{dry_days}");
        assert_eq!(dry_days["policy"], "days:30");
        assert_eq!(dry_days["recordings"], 1);
        assert_eq!(dry_days["bytes"], 9);

        // Simulating `keep` affects zero recordings.
        let (code, dry_keep) = run(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "keep"],
        );
        assert_eq!(code, 0, "{dry_keep}");
        assert_eq!(dry_keep["recordings"], 0);
        assert_eq!(dry_keep["bytes"], 0);

        // Human output also contains only counts and bytes (no titles/paths).
        let (code, out, err) = run_text(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 0, "stderr: {err}");
        assert!(out.contains("2 recording(s)"), "{out}");
        assert!(out.contains("18 bytes"), "{out}");
        assert!(!out.contains(secret_title), "{out}");
        assert!(!out.contains("audio.m4a"), "{out}");
        assert!(!out.contains(old_id.as_str()), "{out}");

        // Non-destructive proof: files, meeting states and config stay untouched.
        assert!(old_file.exists());
        assert!(recent_file.exists());
        assert_eq!(saved(tmp.path(), &old_id).state, RecordingState::Available);
        assert_eq!(
            saved(tmp.path(), &recent_id).state,
            RecordingState::Available
        );
        assert_eq!(
            Config::load(&tmp.path().join("data")).unwrap().retention,
            Retention::KeepForever
        );
        assert!(
            !tmp.path().join("library/.queue").exists(),
            "dry-run must not create .queue"
        );
        // Every CLI run, a dry run included, takes the data folder's mode
        // lease (admission) first, which creates `maintenance.lock` there.
        assert!(
            tmp.path()
                .join("data")
                .join(broscribe_core::maintenance::LOCK_FILE)
                .is_file()
        );

        // `--policy` without `--dry-run` is rejected.
        let (code, refused) = run(tmp.path(), &["retention", "apply", "--policy", "processed"]);
        assert_eq!(code, 2, "{refused}");
        assert_eq!(refused["error"]["code"], "invalid_input");
        assert!(old_file.exists());
        assert!(recent_file.exists());

        // Invalid policy values are rejected.
        for bad in ["days:0", "days:-1", "days:abc", "forever"] {
            let (code, err) = run(
                tmp.path(),
                &["retention", "apply", "--dry-run", "--policy", bad],
            );
            assert_eq!(code, 2, "{bad}: {err}");
            assert_eq!(err["error"]["code"], "invalid_input");
        }
    }

    /// A meeting `retention apply --dry-run --policy` sees as processed, with
    /// one shown speaker: named as a synthetic person, or not.
    fn meeting_with_speaker(root: &Path, named: bool) -> MeetingId {
        use broscribe_core::schema::{Assignment, Speaker};
        use broscribe_core::{PersonId, SpeakerId};

        let (id, _) = meeting(root, Handling::Copy, ProcessingStatus::Done);
        let lib = Library::open_existing(root.join("library")).unwrap();
        let mut m = lib.load_meeting(&id).unwrap();
        let mut speaker = Speaker::new(SpeakerId::new(1).unwrap(), "mic");
        if named {
            speaker.person_id = Some(PersonId::parse("p-a1a1a1").unwrap());
            speaker.assignment = Assignment::Confirmed;
        }
        m.speakers.push(speaker);
        lib.write(|tx| tx.save_meeting(&m)).unwrap();
        id
    }

    /// A synthetic person with a kept voice reading; `voiceprints`: the
    /// `voiceprints.json` to write next to it, if any.
    fn person_with_reading(root: &Path, voiceprints: Option<&str>) -> PathBuf {
        use broscribe_core::PersonId;
        use broscribe_core::schema::Person;

        let lib = Library::open(root.join("library")).unwrap();
        let id = PersonId::parse("p-c3c3c3").unwrap();
        lib.write(|tx| tx.save_person(&Person::new(id.clone(), "Synthetic Reader")))
            .unwrap();
        let dir = lib.person_dir(&id);
        std::fs::create_dir_all(dir.join("enrollment")).unwrap();
        std::fs::write(dir.join("enrollment/reading.wav"), b"synthetic").unwrap();
        if let Some(text) = voiceprints {
            std::fs::write(dir.join(broscribe_core::paths::VOICEPRINTS_JSON), text).unwrap();
        }
        dir
    }

    /// Valid `voiceprints.json` for [`person_with_reading`]'s person, with
    /// its enrollment voiceprint: the kept reading is then due under
    /// `processed`.
    const ENROLLMENT_VOICEPRINTS: &str = r#"{"schema_version":1,"person_id":"p-c3c3c3","embeddings":[{"id":"vp-synthetic","model":"synthetic-model","dim":1,"vector_b64":"AACAPw==","source":{"kind":"enrollment","start":0.0,"end":1.0},"strength":"confirmed","created_at":"2026-11-01T10:00:00+00:00"}]}"#;

    #[test]
    fn apply_dry_run_with_policy_tells_named_and_unnamed_meetings_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let named = meeting_with_speaker(tmp.path(), true);
        let unnamed = meeting_with_speaker(tmp.path(), false);

        // `processed-and-named` waits for the unnamed speaker; `processed`
        // does not.
        let (code, and_named) = run(
            tmp.path(),
            &[
                "retention",
                "apply",
                "--dry-run",
                "--policy",
                "processed-and-named",
            ],
        );
        assert_eq!(code, 0, "{and_named}");
        assert_eq!(and_named["policy"], "processed-and-named");
        assert_eq!(and_named["checked"], 2);
        assert_eq!(and_named["recordings"], 1);
        assert_eq!(and_named["bytes"], 9);
        let (code, processed) = run(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 0, "{processed}");
        assert_eq!(processed["recordings"], 2);
        assert_eq!(processed["bytes"], 18);

        // Which one it is stays out of the answer, and nothing changed.
        assert!(!and_named.to_string().contains(named.as_str()));
        assert!(!and_named.to_string().contains(unnamed.as_str()));
        assert_eq!(saved(tmp.path(), &named).state, RecordingState::Available);
        assert_eq!(saved(tmp.path(), &unnamed).state, RecordingState::Available);
    }

    #[test]
    fn apply_dry_run_with_policy_counts_the_kept_voice_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let dry = |policy: &str| {
            let (code, value) = run(
                tmp.path(),
                &["retention", "apply", "--dry-run", "--policy", policy],
            );
            assert_eq!(code, 0, "{value}");
            value
        };

        // Without voiceprints `processed` keeps the reading.
        let dir = person_with_reading(tmp.path(), None);
        assert_eq!(dry("processed")["enrollment_files_removed"], 0);

        // Once its voiceprints exist, both immediate policies delete it, with
        // no meeting recording at all.
        std::fs::write(
            dir.join(broscribe_core::paths::VOICEPRINTS_JSON),
            ENROLLMENT_VOICEPRINTS,
        )
        .unwrap();
        for policy in ["processed", "processed-and-named"] {
            let value = dry(policy);
            assert_eq!(value["recordings"], 0, "{policy}: {value}");
            assert_eq!(value["bytes"], 0, "{policy}: {value}");
            assert_eq!(value["enrollment_files_removed"], 1, "{policy}: {value}");
        }
        assert_eq!(dry("keep")["enrollment_files_removed"], 0);
        assert!(dir.join("enrollment/reading.wav").is_file());
    }

    #[test]
    fn apply_dry_run_with_policy_reports_unreadable_meetings_and_path_free_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (broken, _) = meeting(tmp.path(), Handling::Copy, ProcessingStatus::Done);
        meeting(tmp.path(), Handling::Copy, ProcessingStatus::Done);
        let lib = Library::open_existing(tmp.path().join("library")).unwrap();
        let broken_dir = lib.meeting_dir(&broken).unwrap();
        std::fs::write(
            broken_dir.join(broscribe_core::paths::MEETING_JSON),
            b"{ not json",
        )
        .unwrap();

        // A meeting that can't be read is counted, not guessed at.
        let (code, dry) = run(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 0, "{dry}");
        assert_eq!(dry["unreadable"], 1);
        assert_eq!(dry["recordings"], 1);
        let (code, text, err) = run_text(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 0, "stderr: {err}");
        assert!(text.contains("1 meeting(s) couldn't be read"), "{text}");

        // A damaged voiceprints file fails the check with its machine code and
        // a cause that names no path or person.
        let person = person_with_reading(tmp.path(), Some("{ not json"));
        let (code, failed) = run(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 1, "{failed}");
        assert_eq!(failed["error"]["code"], "schema");
        assert_eq!(
            failed["error"]["message"],
            "A file in the library folder is damaged"
        );
        let whole = failed.to_string();
        for private in [
            tmp.path().to_string_lossy().as_ref(),
            "p-c3c3c3",
            "voiceprints",
            person.to_string_lossy().as_ref(),
        ] {
            assert!(!whole.contains(private), "{private} in {whole}");
        }
        let (code, _, err) = run_text(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 1);
        assert!(
            err.contains("A file in the library folder is damaged"),
            "{err}"
        );
        assert!(!err.contains("p-c3c3c3"), "{err}");
        assert!(
            !err.contains(tmp.path().to_string_lossy().as_ref()),
            "{err}"
        );

        // A missing library folder says so, still path-free.
        let other = tempfile::tempdir().unwrap();
        let (code, missing) = run(
            other.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 1, "{missing}");
        assert_eq!(missing["error"]["message"], "Library folder wasn't found");
        assert!(
            !missing
                .to_string()
                .contains(other.path().to_string_lossy().as_ref())
        );
    }

    #[test]
    fn apply_dry_run_with_policy_counts_an_unreadable_profile_holding_a_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = person_with_reading(tmp.path(), Some(ENROLLMENT_VOICEPRINTS));
        let dry = |policy: &str| {
            let (code, value) = run(
                tmp.path(),
                &["retention", "apply", "--dry-run", "--policy", policy],
            );
            assert_eq!(code, 0, "{value}");
            value
        };
        let readable = dry("processed");
        assert_eq!(readable["enrollment_files_removed"], 1);
        assert_eq!(readable["unreadable_people"], 0);

        // Damaged `person.md`, valid voiceprints and the reading beside it:
        // whether the reading goes is unknown, so it is counted as
        // unreadable rather than as nothing to delete.
        std::fs::write(
            dir.join(broscribe_core::paths::PERSON_MD),
            b"\xff\xfe not a profile",
        )
        .unwrap();
        for policy in ["processed", "processed-and-named", "days:1"] {
            let value = dry(policy);
            assert_eq!(value["unreadable_people"], 1, "{policy}: {value}");
            assert_eq!(value["enrollment_files_removed"], 0, "{policy}: {value}");
            assert_eq!(value["unreadable"], 0, "{policy}: {value}");
            assert!(!value.to_string().contains("p-c3c3c3"), "{value}");
        }
        let (code, text, err) = run_text(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 0, "stderr: {err}");
        assert!(
            text.contains("1 person profile(s) with a kept voice reading couldn't be read"),
            "{text}"
        );
        assert!(
            !text.contains("p-c3c3c3") && !err.contains("p-c3c3c3"),
            "{text}{err}"
        );

        // A real run leaves the reading alone and reports the same count.
        Config::update(&tmp.path().join("data"), |config| {
            config.retention = Retention::parse_cli("processed")?;
            Ok(())
        })
        .unwrap();
        let (code, applied) = run(tmp.path(), &["retention", "apply"]);
        assert_eq!(code, 0, "{applied}");
        assert_eq!(applied["unreadable_people"], 1);
        assert_eq!(applied["enrollment_files_removed"], 0);
        assert!(dir.join("enrollment/reading.wav").is_file());

        // A damaged profile without a reading leaves nothing unknown.
        std::fs::remove_file(dir.join("enrollment/reading.wav")).unwrap();
        assert_eq!(dry("processed")["unreadable_people"], 0);
    }

    /// A person folder whose `person.md` is missing, or is a directory,
    /// beside a kept reading and valid voiceprints is counted like a damaged
    /// one: the check never reports nothing to delete, and a real run
    /// leaves the reading alone and says so.
    #[test]
    fn apply_counts_a_reading_whose_profile_is_missing_or_a_directory() {
        for what in ["missing", "directory"] {
            let tmp = tempfile::tempdir().unwrap();
            let dir = person_with_reading(tmp.path(), Some(ENROLLMENT_VOICEPRINTS));
            let profile = dir.join(broscribe_core::paths::PERSON_MD);
            std::fs::remove_file(&profile).unwrap();
            if what == "directory" {
                std::fs::create_dir(&profile).unwrap();
            }
            for policy in ["processed", "processed-and-named", "days:1"] {
                let (code, value) = run(
                    tmp.path(),
                    &["retention", "apply", "--dry-run", "--policy", policy],
                );
                assert_eq!(code, 0, "{what} {policy}: {value}");
                assert_eq!(value["unreadable_people"], 1, "{what} {policy}: {value}");
                assert_eq!(
                    value["enrollment_files_removed"], 0,
                    "{what} {policy}: {value}"
                );
                assert!(!value.to_string().contains("p-c3c3c3"), "{value}");
            }

            Config::update(&tmp.path().join("data"), |config| {
                config.retention = Retention::parse_cli("processed")?;
                Ok(())
            })
            .unwrap();
            let (code, applied) = run(tmp.path(), &["retention", "apply"]);
            assert_eq!(code, 0, "{what}: {applied}");
            assert_eq!(applied["unreadable_people"], 1, "{what}: {applied}");
            assert_eq!(applied["enrollment_files_removed"], 0, "{what}: {applied}");
            assert!(dir.join("enrollment/reading.wav").is_file(), "{what}");
            let (code, text, err) = run_text(tmp.path(), &["retention", "apply"]);
            assert_eq!(code, 0, "{what} stderr: {err}");
            assert!(
                text.contains(
                    "1 person profile(s) with a kept voice reading couldn't be read, so the \
                     reading was left alone."
                ),
                "{what}: {text}"
            );
            assert!(!text.contains("p-c3c3c3"), "{text}");
            let (code, text, err) = run_text(tmp.path(), &["retention", "apply", "--dry-run"]);
            assert_eq!(code, 0, "{what} stderr: {err}");
            assert!(
                text.contains("so the reading would be left alone."),
                "{text}"
            );
            assert!(dir.join("enrollment/reading.wav").is_file(), "{what}");
        }
    }

    #[test]
    fn apply_dry_run_with_policy_failing_before_it_runs_names_no_path() {
        let tmp = tempfile::tempdir().unwrap();
        // The data folder is a regular file: admission to it fails before
        // the command runs.
        std::fs::write(tmp.path().join("data"), b"synthetic").unwrap();
        let (code, failed) = run(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 1, "{failed}");
        assert_eq!(failed["error"]["code"], "io");
        assert_eq!(
            failed["error"]["message"],
            "BroScribe's data folder is unreadable"
        );
        let whole = failed.to_string();
        for private in [
            tmp.path().to_string_lossy().as_ref(),
            broscribe_core::maintenance::LOCK_FILE,
        ] {
            assert!(!whole.contains(private), "{private} in {whole}");
        }
        let (code, _, err) = run_text(
            tmp.path(),
            &["retention", "apply", "--dry-run", "--policy", "processed"],
        );
        assert_eq!(code, 1);
        assert!(
            err.contains("BroScribe's data folder is unreadable"),
            "{err}"
        );
        assert!(
            !err.contains(tmp.path().to_string_lossy().as_ref()),
            "{err}"
        );

        // Other commands keep their own message.
        let (code, other) = run(tmp.path(), &["retention", "apply", "--dry-run"]);
        assert_eq!(code, 1, "{other}");
        assert_eq!(other["error"]["code"], "io");
        assert_ne!(
            other["error"]["message"],
            "BroScribe's data folder is unreadable"
        );
    }

    #[test]
    fn stricter_policy_requires_yes_even_after_a_read_only_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, audio) = meeting(tmp.path(), Handling::Copy, ProcessingStatus::Done);
        let data = tmp.path().join("data");
        let original = Config::load(&data).unwrap();
        for args in [
            &["retention", "policy", "processed"][..],
            &["retention", "policy", "processed", "--dry-run"][..],
        ] {
            let (code, plan) = run(tmp.path(), args);
            assert_eq!(code, 0, "{plan}");
            assert_eq!(plan["recordings"], 1, "{plan}");
            assert_eq!(plan["changed"], false, "{plan}");
            assert_eq!(plan["confirmation_required"], true, "{plan}");
            assert_eq!(Config::load(&data).unwrap().revision(), original.revision());
            assert_eq!(Config::load(&data).unwrap().retention, original.retention);
            assert_eq!(run(tmp.path(), &["retention", "apply"]).0, 0);
            assert!(audio.is_file());
            assert_eq!(saved(tmp.path(), &id).state, RecordingState::Available);
        }
        let (code, value) = run(tmp.path(), &["retention", "policy", "processed", "--yes"]);
        assert_eq!(code, 0, "{value}");
        assert_eq!(value["changed"], true);
        assert_eq!(value["preview"]["recordings"], 1);
        assert!(audio.is_file(), "saving a policy does not run it");
        assert_eq!(run(tmp.path(), &["retention", "apply"]).0, 0);
        assert!(!audio.exists());
    }

    #[test]
    fn stricter_policy_refuses_another_configured_library_and_unreadable_meetings() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, audio) = meeting(tmp.path(), Handling::Copy, ProcessingStatus::Done);
        let data = tmp.path().join("data");
        let save = |library| {
            Config::update(&data, |config| {
                config.library = library;
                Ok(())
            })
            .unwrap()
        };
        save(Some(tmp.path().join("another-library")));
        let args = &["retention", "policy", "processed", "--yes"];
        let (_, refused) = run(tmp.path(), args);
        assert_eq!(
            refused["error"]["code"], "retention_library_mismatch",
            "{refused}"
        );
        save(None);
        let lib = Library::open_existing(tmp.path().join("library")).unwrap();
        std::fs::write(
            lib.meeting_dir(&id)
                .unwrap()
                .join(broscribe_core::paths::MEETING_JSON),
            "damaged",
        )
        .unwrap();
        let (_, refused) = run(tmp.path(), args);
        assert_eq!(
            refused["error"]["code"], "retention_preview_incomplete",
            "{refused}"
        );
        assert_eq!(
            Config::load(&data).unwrap().retention,
            Retention::KeepForever
        );
        assert!(audio.is_file());
    }

    #[test]
    fn retention_policy_command_saves_every_variant_and_rejects_invalid_values() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        Library::open(tmp.path().join("library")).unwrap();

        let cases = [
            (
                "processed",
                Retention::DeleteImmediately {
                    wait_until_named: false,
                },
                "delete once processed",
            ),
            (
                "processed-and-named",
                Retention::DeleteImmediately {
                    wait_until_named: true,
                },
                "delete once processed and named",
            ),
            ("days:1", Retention::DeleteAfterDays { days: 1 }, "1 day"),
            (
                "days:30",
                Retention::DeleteAfterDays { days: 30 },
                "30 days",
            ),
            (
                "days:3650",
                Retention::DeleteAfterDays { days: 3650 },
                "3650 days",
            ),
            ("keep", Retention::KeepForever, "forever"),
        ];

        for (arg, expected, phrase) in cases {
            let (code, out) = run(tmp.path(), &["retention", "policy", arg, "--yes"]);
            assert_eq!(code, 0, "{arg}: {out}");
            assert_eq!(out["policy"], arg);
            assert_eq!(out["changed"], true);
            assert!(out["revision"].as_u64().is_some());
            assert_eq!(Config::load(&data_dir).unwrap().retention, expected);

            // Setting the same value again succeeds with changed=false.
            let (code, repeat) = run(tmp.path(), &["retention", "policy", arg]);
            assert_eq!(code, 0, "{repeat}");
            assert_eq!(repeat["changed"], false);

            // Human output mentions the policy name and description.
            let (code, text, err) = run_text(tmp.path(), &["retention", "policy", arg]);
            assert_eq!(code, 0, "stderr: {err}");
            assert!(text.contains(arg), "{text}");
            assert!(text.contains(phrase), "{text}");
        }

        // Invalid policy values fail and leave config unchanged.
        for bad in ["days:0", "days:3651", "days:-7", "days:ten", "never", ""] {
            let (code, err) = run(tmp.path(), &["retention", "policy", bad]);
            assert_eq!(code, 2, "{bad}: {err}");
            assert_eq!(err["error"]["code"], "invalid_input");
        }
        assert_eq!(
            Config::load(&data_dir).unwrap().retention,
            Retention::KeepForever
        );
    }
}
