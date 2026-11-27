//! `templates list|show|new|edit|delete|reset|auto|run|result` (design
//! §8.4, §11): meeting templates are instructions run on a meeting's
//! transcript through a consented provider, producing a Markdown result kept
//! in the meeting. Definitions live in `<data_dir>/templates/` (built-ins ship
//! in the binary; a user file with a built-in's ID overrides it); which ones
//! run after each summary is `[templates] auto` in the config.
//!
//! Definitions and per-run instructions come from stdin, never argv. A run
//! holds the `(meeting, template)` queue job exactly like `enrich`: `--queue`
//! leaves it to the background worker, the synchronous form claims and runs
//! it here, and Ctrl-C stops it before the next provider call.
//!
//! Every definition change (`new`, `edit`, `delete`, `reset`, `auto add`)
//! decides and writes under the data directory's config lock
//! ([`Config::update`]): definitions and `[templates] auto` live in that
//! data directory, so BroScribe writers in every process never interleave a
//! check with another's save, whatever library they name, and no library
//! is opened or created. `edit` with a `baseline` (the definition an editor
//! opened, as `Template` serializes) saves only while the definition is
//! still exactly that one; otherwise nothing is written and the error code
//! is `template_changed`. `new` only creates (`already_exists` otherwise).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use broscribe_core::config::{Config, validate_model_name};
use broscribe_core::templates::Template;
use broscribe_core::{Error, Result, templates};
use broscribe_models::llm::ProviderRegistry;
use broscribe_pipeline::enrich::llm_provider_ids;
use broscribe_pipeline::template::{
    InteractiveTemplate, TemplateOutcome, enqueue_template_request, run_template_interactive,
};
use serde_json::json;

use super::enrich::{PROVIDER_STOP_NOTICE, env, watch_interrupts};
use super::meeting_id;
use crate::cli::{TemplatesAutoCmd, TemplatesCmd};
use crate::query::{read_stdin_text, template_result_view, template_row, templates_view};
use crate::{Ctx, to_value};

/// Error code of an `edit` whose `baseline` no longer matches (the
/// definition was changed or deleted since it was opened).
pub(crate) const TEMPLATE_CHANGED: &str = "template_changed";

/// `templates new|edit --stdin` input. `new` needs `name` and `body`; for
/// `edit`, a field left out keeps its value and `baseline`, when given,
/// makes the save conditional on the definition being unchanged.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateInput {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    baseline: Option<Baseline>,
}

/// The definition an editor opened: [`Template`] exactly as it serializes
/// (`show`, `list` and `templates::get`).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    id: String,
    name: String,
    description: String,
    body: String,
    builtin: bool,
    overridden: bool,
}

impl Baseline {
    /// The baseline of template `id`; `invalid_input` for another ID.
    fn of(self, id: &str) -> Result<Template> {
        if self.id != id {
            return Err(Error::InvalidInput(format!(
                "templates edit {id}: the baseline is template {:?}",
                self.id
            )));
        }
        Ok(Template {
            id: self.id,
            name: self.name,
            description: self.description,
            body: self.body,
            builtin: self.builtin,
            overridden: self.overridden,
        })
    }
}

fn template_changed(id: &str, what: &str) -> Error {
    Error::Service {
        code: TEMPLATE_CHANGED,
        message: format!("template {id} {what} since it was opened; nothing was saved"),
    }
}

pub(crate) fn run(ctx: &mut Ctx<'_>, action: TemplatesCmd) -> Result<()> {
    match action {
        TemplatesCmd::Restore {
            meeting,
            id,
            expect,
        } => super::meeting_edits::restore(ctx, &meeting, Some(&id), expect),
        TemplatesCmd::List { meeting } => list(ctx, meeting.as_deref()),
        TemplatesCmd::Show { id } => show(ctx, &id),
        TemplatesCmd::New { id, .. } => new(ctx, &id),
        TemplatesCmd::Edit { id, .. } => edit(ctx, &id),
        TemplatesCmd::Delete { id } => delete(ctx, &id),
        TemplatesCmd::Reset { id } => reset(ctx, &id),
        TemplatesCmd::Auto { action } => auto(ctx, action),
        TemplatesCmd::Run {
            meeting,
            id,
            provider,
            model,
            queue,
            instructions_stdin,
        } => run_template(
            ctx,
            &meeting,
            &id,
            provider.as_deref(),
            model.as_deref(),
            queue,
            instructions_stdin,
        ),
        TemplatesCmd::Result { meeting, id } => result(ctx, &meeting, &id),
    }
}

fn kind(t: &serde_json::Value) -> &'static str {
    match (t["builtin"] == true, t["overridden"] == true) {
        (true, true) => "built-in, edited",
        (true, false) => "built-in",
        _ => "yours",
    }
}

fn list(ctx: &mut Ctx<'_>, meeting: Option<&str>) -> Result<()> {
    let config = ctx.config()?;
    let loaded = match meeting {
        Some(m) => Some(
            ctx.library_existing(&config)?
                .load_meeting(&meeting_id(m)?)?,
        ),
        None => None,
    };
    let value = templates_view(ctx.data_dir(), &config, loaded.as_ref())?;
    ctx.emit(value.clone(), |out| {
        for t in value["templates"].as_array().into_iter().flatten() {
            writeln!(
                out,
                "{:<20} {} ({}{})",
                t["id"].as_str().unwrap_or_default(),
                t["name"].as_str().unwrap_or_default(),
                kind(t),
                if t["auto"] == true { ", automatic" } else { "" }
            )?;
            if let Some(d) = t["description"].as_str().filter(|d| !d.is_empty()) {
                writeln!(out, "    {d}")?;
            }
        }
        if let Some(results) = value["results"].as_array() {
            if results.is_empty() {
                writeln!(out, "No template results in this meeting.")?;
            } else {
                writeln!(out, "Results in this meeting:")?;
            }
            for r in results {
                let mut notes = Vec::new();
                if r["stale"] == true {
                    notes.push("predates transcript changes");
                }
                if r["defined"] == false {
                    notes.push("template deleted");
                }
                writeln!(
                    out,
                    "  {:<18} {}{}",
                    r["template_id"].as_str().unwrap_or_default(),
                    r["template_name"].as_str().unwrap_or_default(),
                    if notes.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", notes.join(", "))
                    }
                )?;
            }
        }
        Ok(())
    })
}

/// `{"template": row}`, the definition with its instructions, as `show`,
/// `new`, `edit` and `reset` answer, with `config` as it now stands.
fn emit_template(ctx: &mut Ctx<'_>, t: &templates::Template, config: &Config) -> Result<()> {
    let row = template_row(t, config)?;
    ctx.emit(json!({ "template": row }), |out| {
        writeln!(out, "{} ({}, {})", t.name, t.id, kind(&row))?;
        if !t.description.is_empty() {
            writeln!(out, "{}", t.description)?;
        }
        writeln!(out)?;
        writeln!(out, "{}", t.body)
    })
}

fn show(ctx: &mut Ctx<'_>, id: &str) -> Result<()> {
    let t = templates::get(ctx.data_dir(), id)?;
    let config = ctx.config()?;
    emit_template(ctx, &t, &config)
}

fn new(ctx: &mut Ctx<'_>, id: &str) -> Result<()> {
    let input: TemplateInput = ctx.input_json("templates new")?;
    let (config, t) = create_locked(ctx.data_dir(), id, input, || ctx.check_request_cancelled())?;
    emit_template(ctx, &t, &config)
}

/// Creates user template `id` from `input` under the data directory's
/// config lock: `already_exists` when it is defined (a built-in included),
/// and nothing is written then. Returns the config as it stands and the
/// template.
fn create_locked(
    data_dir: &Path,
    id: &str,
    input: TemplateInput,
    check: impl FnOnce() -> Result<()>,
) -> Result<(Config, Template)> {
    if input.baseline.is_some() {
        return Err(Error::InvalidInput(
            "templates new only creates: \"baseline\" belongs to templates edit".into(),
        ));
    }
    let (Some(name), Some(body)) = (input.name, input.body) else {
        return Err(Error::InvalidInput(
            "templates new input needs \"name\" and \"body\"".into(),
        ));
    };
    let description = input.description.unwrap_or_default();
    templates::validate_id(id)?;
    Config::update(data_dir, |_| {
        check()?;
        match templates::get(data_dir, id) {
            Ok(_) => {
                return Err(Error::AlreadyExists(format!(
                    "template {id:?} (change it with `templates edit`)"
                )));
            }
            Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        templates::save_user(data_dir, id, &name, &description, &body)
    })
}

fn edit(ctx: &mut Ctx<'_>, id: &str) -> Result<()> {
    let input: TemplateInput = ctx.input_json("templates edit")?;
    let (config, t) = edit_locked(ctx.data_dir(), id, input, || ctx.check_request_cancelled())?;
    emit_template(ctx, &t, &config)
}

/// Applies `input` to template `id` under the data directory's config
/// lock. With a baseline, the definition read there must equal it
/// (`template_changed` otherwise, also when it was deleted); an unreadable
/// file is never overwritten. Returns the config as it stands and the
/// template.
fn edit_locked(
    data_dir: &Path,
    id: &str,
    input: TemplateInput,
    check: impl FnOnce() -> Result<()>,
) -> Result<(Config, Template)> {
    let baseline = input.baseline.map(|b| b.of(id)).transpose()?;
    Config::update(data_dir, |_| {
        check()?;
        let current = match templates::get(data_dir, id) {
            Ok(t) => t,
            Err(Error::NotFound(_)) if baseline.is_some() => {
                return Err(template_changed(id, "was deleted"));
            }
            Err(e) => return Err(e),
        };
        if baseline.as_ref().is_some_and(|b| *b != current) {
            return Err(template_changed(id, "was changed"));
        }
        templates::save_user(
            data_dir,
            id,
            input.name.as_deref().unwrap_or(&current.name),
            input.description.as_deref().unwrap_or(&current.description),
            input.body.as_deref().unwrap_or(&current.body),
        )
    })
}

/// Deletes a user template (built-ins are reset instead); results already
/// generated stay in their meetings. The template also leaves
/// `[templates] auto`, under the same config lock.
fn delete(ctx: &mut Ctx<'_>, id: &str) -> Result<()> {
    let data_dir = ctx.data_dir();
    Config::update(data_dir, |c| {
        templates::delete_user(data_dir, id)?;
        c.templates.auto.retain(|a| a != id);
        Ok(())
    })?;
    ctx.emit(json!({ "deleted": id }), |out| {
        writeln!(out, "Deleted template {id}.")
    })
}

fn reset(ctx: &mut Ctx<'_>, id: &str) -> Result<()> {
    let data_dir = ctx.data_dir();
    let (config, t) = Config::update(data_dir, |_| {
        templates::reset(data_dir, id)?;
        templates::get(data_dir, id)
    })?;
    emit_template(ctx, &t, &config)
}

/// `templates auto add|remove <id>`: `[templates] auto`, saved under the
/// data directory's config lock. Only a template defined under that lock
/// can be added (a concurrent delete cannot leave it listed); any ID can be
/// removed.
fn auto(ctx: &mut Ctx<'_>, action: TemplatesAutoCmd) -> Result<()> {
    let data_dir = ctx.data_dir();
    let (saved, ()) = Config::update(data_dir, |c| {
        ctx.check_request_cancelled()?;
        match &action {
            TemplatesAutoCmd::Add { id } => {
                let t = templates::get(data_dir, id)?;
                if !c.templates.auto.contains(&t.id) {
                    c.templates.auto.push(t.id);
                }
            }
            TemplatesAutoCmd::Remove { id } => c.templates.auto.retain(|a| a != id),
        }
        Ok(())
    })?;
    let auto = saved.templates.auto;
    ctx.emit(json!({ "auto": auto }), |out| {
        if auto.is_empty() {
            writeln!(out, "No template runs automatically.")
        } else {
            writeln!(out, "Run after each summary: {}", auto.join(", "))
        }
    })
}

fn outcome_line(o: &TemplateOutcome) -> String {
    let mut line = format!(
        "Ran {} on {} with {}",
        o.template_name, o.meeting_id, o.provider
    );
    if let Some(m) = &o.model {
        line.push_str(&format!(" ({m})"));
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

fn run_template(
    ctx: &mut Ctx<'_>,
    meeting: &str,
    template: &str,
    provider: Option<&str>,
    model: Option<&str>,
    queue: bool,
    instructions_stdin: bool,
) -> Result<()> {
    if let Some(m) = model {
        validate_model_name(m)?;
    }
    let id = meeting_id(meeting)?;
    let instructions = if instructions_stdin {
        Some(read_stdin_text("templates run --instructions-stdin")?)
    } else {
        None
    };
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    // Nothing goes anywhere without consent; the run checks it again.
    let registry = ProviderRegistry::load(ctx.data_dir())?;
    llm_provider_ids(&config, &registry, provider)?;
    if queue {
        let data_dir = ctx.data_dir().to_path_buf();
        let (job, baseline, run) = lib.write(|tx| {
            let baseline = tx
                .library()
                .load_meeting(&id)?
                .template_result(template)
                .map(|result| result.created_at);
            let job = enqueue_template_request(
                tx,
                &data_dir,
                &id,
                template,
                provider,
                model,
                instructions.as_deref(),
            )?;
            let run = broscribe_core::queue::Queue::open(tx.root())?
                .get(&job)?
                .generation_run;
            Ok((job, baseline, run))
        })?;
        if let Err(e) = super::daemon::optional_call(ctx.data_dir(), broscribe_ipc::Call::KickQueue)
        {
            ctx.note(format!(
                "note: template run queued but the agent could not be notified: {e}"
            ));
        }
        return ctx.emit(
            json!({ "queued": true, "job_id": job, "baseline": baseline, "run": run }),
            |out| writeln!(out, "Queued {template} on {id}: job {job}"),
        );
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    watch_interrupts(
        cancelled.clone(),
        PROVIDER_STOP_NOTICE,
        ctx.background_notes(),
    );
    let ran = run_template_interactive(
        &env(ctx, &lib, &config),
        ctx.data_dir(),
        ctx.deps.secrets.as_ref(),
        &id,
        template,
        provider,
        model,
        instructions.as_deref(),
        &cancelled,
    )?;
    match ran {
        InteractiveTemplate::Ran(o) => {
            let value = json!({ "queued": false, "outcome": to_value(&o)? });
            ctx.emit(value, |out| writeln!(out, "{}", outcome_line(&o)))
        }
        // The background worker claimed the job first: it runs it.
        InteractiveTemplate::Queued(job) => {
            ctx.emit(json!({ "queued": true, "job_id": job }), |out| {
                writeln!(
                    out,
                    "The background worker is running {template} on {id}: job {job}"
                )
            })
        }
    }
}

fn result(ctx: &mut Ctx<'_>, meeting: &str, template: &str) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let value = template_result_view(&lib, &meeting_id(meeting)?, template)?;
    ctx.emit(value.clone(), |out| {
        let model = value["model"].as_str().unwrap_or("provider default");
        writeln!(
            out,
            "{} · {} · {} · {}{}",
            value["template_name"].as_str().unwrap_or_default(),
            value["provider"].as_str().unwrap_or_default(),
            model,
            value["created_at"].as_str().unwrap_or_default(),
            if value["stale"] == true {
                " (predates transcript changes)"
            } else {
                ""
            }
        )?;
        writeln!(out)?;
        writeln!(out, "{}", value["markdown"].as_str().unwrap_or_default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(value: serde_json::Value) -> TemplateInput {
        serde_json::from_value(value).unwrap()
    }

    fn create(data: &Path, id: &str, value: serde_json::Value) -> Result<Template> {
        create_locked(data, id, input(value), || Ok(())).map(|(_, t)| t)
    }

    fn edit(data: &Path, id: &str, value: serde_json::Value) -> Result<Template> {
        edit_locked(data, id, input(value), || Ok(())).map(|(_, t)| t)
    }

    #[test]
    fn edit_saves_only_over_the_baseline_it_was_opened_on() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let opened = create(
            data,
            "follow-up",
            json!({ "name": "Follow-up", "body": "Draft the e-mail." }),
        )
        .unwrap();
        // Someone else saves first.
        let theirs = edit(
            data,
            "follow-up",
            json!({ "body": "Theirs.", "baseline": opened }),
        )
        .unwrap();
        assert_eq!(theirs.body, "Theirs.");
        let err = edit(
            data,
            "follow-up",
            json!({ "body": "Mine.", "baseline": opened }),
        )
        .unwrap_err();
        assert_eq!(err.code(), TEMPLATE_CHANGED);
        assert_eq!(
            templates::get(data, "follow-up").unwrap(),
            theirs,
            "a stale baseline writes nothing"
        );
        // Saving over what is defined now is explicit: that version is the
        // baseline.
        let mine = edit(
            data,
            "follow-up",
            json!({ "body": "Mine.", "baseline": theirs }),
        )
        .unwrap();
        assert_eq!(mine.body, "Mine.");
        assert_eq!(mine.name, "Follow-up", "fields left out keep their value");
    }

    #[test]
    fn a_deleted_template_is_a_conflict_not_a_recreation() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let opened = create(data, "notes-x", json!({ "name": "X", "body": "Body." })).unwrap();
        templates::delete_user(data, "notes-x").unwrap();
        let err = edit(
            data,
            "notes-x",
            json!({ "body": "Mine.", "baseline": opened }),
        )
        .unwrap_err();
        assert_eq!(err.code(), TEMPLATE_CHANGED);
        assert_eq!(
            templates::get(data, "notes-x").unwrap_err().code(),
            "not_found"
        );
    }

    #[test]
    fn a_built_in_baseline_detects_an_override_saved_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let builtin = templates::get(data, "meeting-notes").unwrap();
        assert!(builtin.builtin && !builtin.overridden);
        edit(
            data,
            "meeting-notes",
            json!({ "body": "Theirs.", "baseline": builtin }),
        )
        .unwrap();
        let err = edit(
            data,
            "meeting-notes",
            json!({ "body": "Mine.", "baseline": builtin }),
        )
        .unwrap_err();
        assert_eq!(err.code(), TEMPLATE_CHANGED);
    }

    #[test]
    fn new_only_creates_and_refuses_a_baseline_or_another_id() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        let err = create(
            data,
            "meeting-notes",
            json!({ "name": "Mine", "body": "Mine." }),
        )
        .unwrap_err();
        assert_eq!(err.code(), "already_exists");
        assert!(!templates::get(data, "meeting-notes").unwrap().overridden);
        let builtin = templates::get(data, "meeting-notes").unwrap();
        let err = create(
            data,
            "fresh",
            json!({ "name": "Fresh", "body": "B.", "baseline": builtin }),
        )
        .unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        let err = edit(
            data,
            "meeting-outline",
            json!({ "body": "B.", "baseline": builtin }),
        )
        .unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        assert!(!templates::get(data, "meeting-outline").unwrap().overridden);
    }
}
