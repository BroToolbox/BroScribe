//! Command implementations.
//!
//! `--spec-stdin` inputs that only replace argv values (`import`, `export`,
//! `setup`, `setup voice`, `people lookup --query-stdin`) are read here, so
//! a command implementation sees one set of values whichever transport
//! carried them, and refuses the same combinations.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use broscribe_core::config::Config;
use broscribe_core::{Error, Library, MeetingId, Result, SpeakerId};
use broscribe_pipeline::retention::{RetentionOptions, apply_reloading};
use broscribe_pipeline::vectors;

use crate::Ctx;
use crate::cli::{
    ActionsCmd, AgentsCmd, AppCmd, AudioCmd, Command, EnrichCmd, ExportSpec, IdentityCmd,
    IdentitySpec, ImportArgs, ImportSpec, IndexCmd, LibraryCmd, LookupSpec, McpCmd, MeetingCmd,
    PeopleCmd, ProvidersCmd, QueueCmd, RetentionCmd, SetupCmd, SetupSpec, TranscriptCmd,
};
use crate::query::read_stdin;

pub(crate) mod actions;
mod agents;
mod app;
mod audio;
mod audio_check;
pub(crate) mod bench;
pub(crate) mod daemon;
mod detect;
pub(crate) mod diagnostics;
mod doctor;
mod enrich;
mod export;
pub(crate) mod identity_checks;
pub(crate) mod identity_review;
mod import;
pub(crate) mod integrations;
pub(crate) mod lifecycle;
pub(crate) mod maintenance;
mod meeting_edits;
mod models;
mod providers;
mod queue;
pub(crate) mod read;
pub(crate) mod readers;
pub(crate) mod record;
mod reprocess;
pub(crate) mod retention;
mod settings;
mod setup;
pub(crate) mod templates;
mod timeline;
mod uninstall;
pub(crate) mod update;
mod voice;
mod write;
pub(crate) use write::undo_recording_name;

fn maintenance_config_base() -> Option<PathBuf> {
    std::env::var_os(broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn require_working_directory_outside_installation(
    root: &Path,
    retry_guidance: impl FnOnce() -> String,
) -> Result<()> {
    let cwd = std::env::current_dir()
        .map_err(|error| Error::io("resolving the working directory", error))?;
    if broscribe_platform::uninstall::path_within(&cwd, root, true) {
        return Err(Error::Service {
            code: "installation_working_directory",
            message: format!(
                "Your terminal's current folder is inside the BroScribe installation ({}).{}",
                broscribe_platform::plain_verbatim(root).display(),
                retry_guidance(),
            ),
        });
    }
    Ok(())
}

pub(crate) use write::{forget_preview, forget_voice};

fn write_new_private_file(parent: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| Error::io_at(parent, e))?;
    temp.write_all(bytes)
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|e| Error::io_at(path, e))?;
    temp.persist_noclobber(path)
        .map_err(|e| Error::io_at(path, e.error))?;
    broscribe_core::fsutil::sync_dir(parent)
}

pub(crate) fn dispatch(ctx: &mut Ctx<'_>, command: Command) -> Result<()> {
    match command {
        Command::Maintenance { action } => maintenance::run(ctx, action),
        Command::Version => {
            let version = env!("CARGO_PKG_VERSION");
            ctx.emit(serde_json::json!({ "version": version }), |out| {
                writeln!(out, "broscribe {version}")
            })
        }
        Command::Diagnostics => diagnostics::run(ctx),
        Command::Uninstall(args) => uninstall::run(ctx, args),
        Command::Import(mut a) => {
            if a.spec_stdin {
                import_spec(&mut a, ctx.input_json("import")?)?;
            }
            import::import(ctx, a)
        }
        Command::Search(a) => read::search(ctx, a),
        Command::Meetings(a) => read::meetings(ctx, a),
        Command::Meeting { action } => match action {
            MeetingCmd::ReviewNotes {
                meeting,
                dismiss,
                expect,
            } => meeting_edits::review_notes(ctx, &meeting, dismiss, expect.as_deref()),
            MeetingCmd::CheckRecording { meeting } => timeline::check_recording(ctx, &meeting),
            MeetingCmd::PrepareAudio { meeting } => timeline::prepare_audio(ctx, &meeting),
            MeetingCmd::Clip {
                meeting,
                from,
                to,
                out,
            } => timeline::meeting_clip(ctx, &meeting, &from, &to, &out),
            MeetingCmd::Timeline { meeting } => timeline::timeline(ctx, &meeting),
        },
        Command::Show { meeting } => read::show(ctx, &meeting),
        Command::Transcript { action, args } => match action {
            None => read::transcript(ctx, args),
            Some(TranscriptCmd::Attribute {
                meeting,
                segments,
                fingerprints_stdin,
                person,
                clear,
            }) => write::attribute(ctx, &meeting, &segments, fingerprints_stdin, person, clear),
        },
        Command::People { action } => match action {
            None => read::people(ctx, None),
            Some(PeopleCmd::List { query }) => read::people(ctx, query.as_deref()),
            Some(PeopleCmd::Samples { person }) => timeline::samples(ctx, &person),
            Some(PeopleCmd::Clip {
                person,
                sample,
                out,
            }) => timeline::person_clip(ctx, &person, &sample, &out),
            Some(PeopleCmd::Recheck { person }) => identity_checks::recheck(ctx, person.as_deref()),
            Some(PeopleCmd::KeepIdentification { .. }) => {
                identity_checks::keep(ctx, read_stdin("people keep-identification")?)
            }
            Some(PeopleCmd::Review) => identity_review::review(ctx),
            Some(PeopleCmd::Identify { .. }) => {
                identity_review::identify(ctx, read_stdin("people identify")?)
            }
            Some(PeopleCmd::Merge { resume, .. }) => identity_review::merge(ctx, resume),
            Some(PeopleCmd::NotSame { a, b }) => identity_review::not_same(ctx, &a, &b),
            Some(PeopleCmd::Add { .. }) => {
                meeting_edits::add_person(ctx, read_stdin("people add")?)
            }
            Some(PeopleCmd::Show { person }) => read::person(ctx, &person),
            Some(PeopleCmd::Lookup {
                query,
                query_stdin: _,
                integration,
                account,
            }) => {
                // clap gives exactly one of the argument and `--query-stdin`.
                let query = match query {
                    Some(query) => query,
                    None => read_stdin::<LookupSpec>("people lookup")?.query,
                };
                read::lookup_people(ctx, &query, integration, account.as_deref())
            }
            Some(PeopleCmd::Photo { person, account }) => {
                read::person_photo(ctx, &person, account.as_deref())
            }
            Some(PeopleCmd::Forget(args)) => write::forget(ctx, args, false),
            Some(PeopleCmd::ForgetVoice(args)) => write::forget(ctx, args, true),
            Some(PeopleCmd::Edit(a)) => write::edit_person(ctx, a),
            Some(PeopleCmd::UndoEdit { person, undo_id }) => {
                write::undo_edit(ctx, &person, &undo_id)
            }
        },
        Command::Speakers { action } => write::speakers(ctx, action),
        Command::Tag { meeting, changes } => write::tag(ctx, &meeting, &changes),
        Command::Title {
            meeting,
            title,
            stdin,
        } => lifecycle::title(ctx, &meeting, title, stdin),
        Command::Delete {
            meeting, dry_run, ..
        } => lifecycle::delete(ctx, &meeting, dry_run),
        Command::Identity { action } => match action {
            IdentityCmd::EditSession => identity_review::edit_session(ctx),
            IdentityCmd::Calibrate { manifest } => import::calibrate_cmd(ctx, &manifest),
            IdentityCmd::ReResolve { dry_run } => write::re_resolve(ctx, dry_run),
            IdentityCmd::Status => queue::identity_status(ctx),
        },
        Command::Models { action } => models::models(ctx, action),
        Command::Index {
            action: IndexCmd::Rebuild,
        } => write::rebuild(ctx),
        Command::Setup { action, args } => match action.map(|action| *action) {
            None => {
                let mut args = args;
                if args.spec_stdin {
                    let spec: SetupSpec = read_stdin("setup")?;
                    (args.name, args.emails, args.people_file) =
                        (spec.name, spec.emails, spec.people_file);
                }
                setup::setup(ctx, args)
            }
            Some(SetupCmd::RecordingAccess { request }) => setup::recording_access(ctx, request),
            Some(SetupCmd::AudioCheck { echo_handling }) => {
                audio_check::run(ctx, true, echo_handling)
            }
            Some(SetupCmd::Summaries) => setup::complete_step(ctx, "summaries"),
            Some(SetupCmd::MeetingApps { app, policy }) => {
                if let Some((app, policy)) = app.zip(policy) {
                    detect::set_policy(ctx, app, policy)?;
                }
                setup::complete_step(ctx, "meeting_apps")
            }
            Some(SetupCmd::Finish) => setup::complete_step(ctx, "assistant"),
            Some(SetupCmd::Voice(mut a)) => {
                if a.spec_stdin {
                    let spec: IdentitySpec = read_stdin("setup voice")?;
                    (a.name, a.emails) = (spec.name, spec.emails);
                }
                voice::setup_voice(ctx, a)
            }
            Some(SetupCmd::Try(a)) => voice::setup_try(ctx, a),
            Some(SetupCmd::MicTest) => voice::mic_test(ctx),
            Some(SetupCmd::VoiceStatus) => voice::voice_status(ctx),
            Some(SetupCmd::ModelCheck) => setup::model_check(ctx),
            Some(SetupCmd::Defer) => setup::set_deferred(ctx, true),
            Some(SetupCmd::Resume) => setup::set_deferred(ctx, false),
        },
        Command::Doctor => doctor::doctor(ctx),
        Command::Settings { action } => settings::run(ctx, action),
        Command::Library { action } => match action {
            LibraryCmd::Switch { path, timeout } => settings::switch_library(ctx, &path, timeout),
            LibraryCmd::Filters => read::filters(ctx),
            LibraryCmd::DiskUsage => settings::disk_usage(ctx),
            LibraryCmd::Check => doctor::library_check(ctx),
            LibraryCmd::Repair { only, dry_run, .. } => doctor::library_repair(ctx, &only, dry_run),
        },
        Command::Daemon { action } => daemon::run(ctx, action),
        Command::Record { action } => record::run(ctx, action),
        Command::Detect { action } => detect::run(ctx, action),
        Command::Readers { action } => readers::run(ctx, action),
        Command::Integrations { action } => integrations::run(ctx, action),
        Command::AudioCheck {
            save,
            echo_handling,
        } => audio_check::run(ctx, save, echo_handling),
        Command::Mcp { action } => match action {
            None => {
                let config = ctx.config()?;
                let (library, _) = ctx.library_path(&config);
                let admission = ctx.admission.ok_or_else(|| {
                    Error::Internal("MCP invocation has no mode admission".into())
                })?;
                crate::mcp::serve_admitted(ctx.deps, library, admission)
            }
            Some(McpCmd::Enable) => agents::mcp_switch(ctx, true),
            Some(McpCmd::Disable) => agents::mcp_switch(ctx, false),
            Some(McpCmd::Status) => agents::mcp_status(ctx),
        },
        Command::Agents { action } => match action {
            AgentsCmd::Status => agents::status(ctx),
            AgentsCmd::Install => agents::install(ctx, true),
            AgentsCmd::Uninstall => agents::install(ctx, false),
        },
        Command::App { action } => match action {
            AppCmd::Show => {
                let value = daemon::required_call(ctx, broscribe_ipc::Call::AppShow)?;
                daemon::emit(ctx, value)
            }
            AppCmd::OpenAtLogin { state } => app::open_at_login(ctx, state),
            AppCmd::StatusItem { state } => app::status_item(ctx, state),
            AppCmd::PanelCompact { state } => app::panel_compact(ctx, state),
            AppCmd::HideControlsFromScreenSharing { state } => {
                app::hide_controls_from_screen_sharing(ctx, state)
            }
            AppCmd::About => app::about(ctx),
            AppCmd::Permissions { request } => app::permissions(ctx, request),
            AppCmd::ReadingSize { size } => app::reading_size(ctx, size),
            AppCmd::LiveIdentity { state } => app::live_identity(ctx, state),
            AppCmd::UncalibratedMeSuggestions { state } => {
                app::uncalibrated_me_suggestions(ctx, state)
            }
        },
        Command::Update { action } => update::run(ctx, action),
        Command::Queue { action } => match action {
            QueueCmd::Work {
                json_progress,
                once,
            } => queue::work(ctx, json_progress, once),
            QueueCmd::Status => queue::status(ctx),
            QueueCmd::Cancel { job, run } => meeting_edits::cancel_generation(ctx, &job, run),
            QueueCmd::DiscardSetup { session } => queue::discard_setup(ctx, session.as_deref()),
            QueueCmd::Prune(args) => queue::prune(ctx, &args),
        },
        Command::Audio { action } => match action {
            AudioCmd::PinStatus => audio::pin_status(ctx),
            AudioCmd::CheckStatus => audio::check_status(ctx),
            AudioCmd::Devices => voice::audio_devices(ctx),
            AudioCmd::Pin { device, clear } => audio::pin(ctx, device.filter(|_| !clear)),
        },
        Command::Retention { action } => match action {
            RetentionCmd::Apply { dry_run, policy } => retention::apply(ctx, dry_run, policy),
            RetentionCmd::Policy {
                policy,
                dry_run,
                yes,
            } => retention::policy(ctx, policy, dry_run, yes),
            RetentionCmd::Keep { meeting } => retention::keep(ctx, &meeting),
            RetentionCmd::DeleteNow {
                meeting, dry_run, ..
            } => retention::delete_now(ctx, &meeting, dry_run),
            RetentionCmd::Explain { meetings } => retention::explain(ctx, &meetings),
        },
        Command::Enrich { action, args } => match action {
            None => enrich::run(ctx, args),
            Some(EnrichCmd::Instructions { action }) => enrich::instructions(ctx, action),
            Some(EnrichCmd::Restore { meeting, expect }) => {
                meeting_edits::restore(ctx, &meeting, None, expect)
            }
        },
        Command::Templates { action } => templates::run(ctx, action),
        Command::Export(mut a) => {
            if a.spec_stdin {
                a.out = Some(read_stdin::<ExportSpec>("export")?.out);
            }
            export::run(ctx, a)
        }
        Command::Actions { action, args } => match action {
            None => actions::list(ctx, args),
            Some(ActionsCmd::Set {
                meeting,
                action,
                owner,
                clear_owner,
                due,
                clear_due,
            }) => meeting_edits::edit_action(
                ctx,
                &meeting,
                &action,
                broscribe_core::ops::ActionEdit {
                    owner,
                    clear_owner,
                    due,
                    clear_due,
                },
            ),
            Some(ActionsCmd::SetStatus {
                meeting,
                action,
                status,
            }) => actions::set_status(ctx, &meeting, &action, status),
        },
        Command::Decisions(a) => actions::decisions(ctx, a),
        Command::Providers { action } => match action {
            ProvidersCmd::List => providers::list(ctx),
            ProvidersCmd::Configure {
                id,
                base_url,
                model,
            } => providers::configure(ctx, &id, &base_url, &model),
            ProvidersCmd::Prefer { id } => providers::prefer(ctx, &id),
            ProvidersCmd::Enable { id, accept } => providers::enable(ctx, &id, accept),
            ProvidersCmd::Disable { id } => providers::disable(ctx, &id),
            ProvidersCmd::Test { id } => providers::test(ctx, &id),
            ProvidersCmd::SetKey { id } => providers::set_key(ctx, &id),
            ProvidersCmd::DeleteKey { id } => providers::delete_key(ctx, &id),
            ProvidersCmd::Models { id } => providers::models(ctx, &id),
        },
        Command::Reprocess(a) => reprocess::run(ctx, a),
        Command::Bench { action } => bench::dispatch(ctx, action),
    }
}

pub(crate) fn meeting_id(s: &str) -> Result<MeetingId> {
    MeetingId::parse(s.trim())
        .map_err(|_| Error::InvalidInput(format!("not a meeting ID (expected m-…): {s:?}")))
}

/// Puts an `import --spec-stdin` input in the argv fields it replaces,
/// refusing what clap refuses beside `--manifest`.
fn import_spec(a: &mut ImportArgs, spec: ImportSpec) -> Result<()> {
    match spec {
        ImportSpec::Recording {
            path,
            title,
            participants,
        } => (a.path, a.title, a.participants) = (Some(path), title, participants),
        ImportSpec::Manifest(_) if a.lang.is_some() || a.auto_language || a.date.is_some() => {
            return Err(Error::InvalidInput(
                "--lang, --auto-language and --date do not apply to a manifest: each meeting carries its own".into(),
            ));
        }
        ImportSpec::Manifest(manifest) => a.manifest = Some(manifest),
    }
    Ok(())
}

pub(crate) fn speaker_id(s: &str) -> Result<SpeakerId> {
    SpeakerId::parse(s.trim().to_ascii_uppercase().as_str())
}

/// Asks `question` on the terminal (stderr) and reads one answer line from
/// stdin: `true` only for an explicit `y` or `yes`. Callers ask only on an
/// interactive terminal without `--json`; otherwise they require `--yes`.
pub(crate) fn confirm_on_terminal(ctx: &mut Ctx<'_>, question: &str) -> Result<bool> {
    ctx.note(question);
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| Error::io("reading the answer", e))?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// After identity completes (import, batch): search vectors for the new
/// transcripts (outside the lock), then retention, for `meetings` (`None`:
/// the whole library). Best effort: problems become notes on stderr and the
/// command still succeeds.
pub(crate) fn after_identity(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    meetings: Option<&[MeetingId]>,
) {
    if let Err(e) = refresh_vectors(
        lib,
        config,
        ctx.data_dir(),
        ctx.deps.secrets.as_ref(),
        &ctx.deps.execution,
        meetings,
    ) {
        ctx.note(format!("note: search vectors not updated: {e}"));
    }
    after_naming(ctx, lib, config, meetings);
}

/// Embeds the window texts of `meetings` (`None`: the whole library) that
/// have no vector for the configured text model yet, outside the lock;
/// nothing when semantic search is unavailable. Used after any write that
/// can change windows (identity, line corrections, forgetting a person).
pub(crate) fn refresh_vectors(
    lib: &Library,
    config: &Config,
    data_dir: &std::path::Path,
    secrets: &dyn broscribe_platform::SecretStore,
    execution: &broscribe_models::ExecutionContext,
    meetings: Option<&[MeetingId]>,
) -> Result<()> {
    let model = vectors::text_model(config, data_dir, secrets, execution)?;
    let Some(embedder) = model.embedder() else {
        return Ok(());
    };
    match meetings {
        None => vectors::embed_missing(lib, embedder, None).map(|_| ()),
        Some(ids) => ids
            .iter()
            .try_for_each(|id| vectors::embed_missing(lib, embedder, Some(id)).map(|_| ())),
    }
}

/// Retention for `meetings` (`None`: every meeting) after identity
/// completes or a naming operation. Each deletion decides under the
/// library lock with the policy configured now, not `config`'s copy: the
/// user may switch it while the command runs. Best effort, like
/// [`after_identity`].
pub(crate) fn after_naming(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    meetings: Option<&[MeetingId]>,
) {
    let opts = RetentionOptions {
        dry_run: false,
        meetings: meetings.map(<[MeetingId]>::to_vec),
        cache_dir: Some(ctx.paths.cache_dir.clone()),
        now: None,
    };
    let data_dir = ctx.data_dir().to_path_buf();
    match apply_reloading(lib, config, &data_dir, ctx.deps.process.as_ref(), &opts) {
        Ok(r) if !r.deleted.is_empty() => ctx.note(format!(
            "retention: deleted {} recording(s) as configured",
            r.deleted.len()
        )),
        Ok(_) => {}
        Err(e) => ctx.note(format!("note: retention not applied: {e}")),
    }
}

/// Asks the running agent to re-scan its queue now: `Ok(false)` when no
/// agent is running.
pub(crate) fn kick_agent(ctx: &Ctx<'_>) -> Result<bool> {
    if let Some(library) = &ctx.request_library {
        return crate::mcp::kick_library_agent(ctx.data_dir(), library);
    }
    Ok(daemon::optional_call(ctx.data_dir(), broscribe_ipc::Call::KickQueue)?.is_some())
}

/// Asks the running agent to run the speaker recheck core requested with a
/// naming, correction or forget (`learning` in its outcome): the command
/// itself never waits for it. Without an agent the request stays queued and
/// the next `broscribe queue work` (or the app's worker) runs it; a note
/// says so. Returns whether an agent took the kick.
pub(crate) fn kick_recheck(ctx: &mut Ctx<'_>) -> bool {
    match kick_agent(ctx) {
        Ok(true) => true,
        Ok(false) => {
            ctx.note(
                "note: other speakers are rechecked in the background; no agent is running, \
                 so run `broscribe queue work` (or open the app) to do it now",
            );
            false
        }
        Err(e) => {
            ctx.note(format!(
                "note: the speaker recheck is queued but the agent could not be notified: {e}"
            ));
            false
        }
    }
}

/// Writes `label: value` lines for every field of a JSON object, nesting
/// objects and arrays with indentation (human output of pipeline reports).
pub(crate) fn write_value(
    out: &mut dyn Write,
    value: &serde_json::Value,
    indent: usize,
) -> std::io::Result<()> {
    use serde_json::Value;
    let pad = "  ".repeat(indent);
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let label = k.replace('_', " ");
                match v {
                    Value::Null => {}
                    Value::Array(a) if a.is_empty() => writeln!(out, "{pad}{label}: none")?,
                    Value::Object(_) | Value::Array(_) => {
                        writeln!(out, "{pad}{label}:")?;
                        write_value(out, v, indent + 1)?;
                    }
                    _ => writeln!(out, "{pad}{label}: {}", scalar(v))?,
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::Object(_) => {
                        writeln!(out, "{pad}-")?;
                        write_value(out, item, indent + 1)?;
                    }
                    _ => writeln!(out, "{pad}- {}", scalar(item))?,
                }
            }
        }
        _ => writeln!(out, "{pad}{}", scalar(value))?,
    }
    Ok(())
}

fn scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => format!("{f:.3}"),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn import_args(args: &[&str]) -> ImportArgs {
        let argv = ["broscribe", "import", "--spec-stdin"].iter().chain(args);
        match crate::cli::Cli::try_parse_from(argv).unwrap().command {
            Command::Import(a) => a,
            other => panic!("{other:?}"),
        }
    }

    /// A spec fills exactly the argv fields of the form it replaces, so the
    /// import sees one request whichever transport carried it.
    #[test]
    fn an_import_spec_fills_exactly_the_fields_it_replaces() {
        let mut a = import_args(&["--handling", "reference", "--enrich"]);
        import_spec(&mut a, ImportSpec::Manifest("relative/batch.json".into())).unwrap();
        assert_eq!(a.manifest.as_deref(), Some("relative/batch.json".as_ref()));
        assert!(a.path.is_none() && a.title.is_none() && a.participants.is_empty());
        assert!(a.enrich && a.handling.is_some());

        let mut a = import_args(&["--lang", "pt-BR", "--date", "2026-11-10"]);
        import_spec(
            &mut a,
            ImportSpec::Recording {
                path: "/synthetic/a.m4a".into(),
                title: Some("Synthetic".into()),
                participants: vec!["Ana <ana@example.invalid>".into()],
            },
        )
        .unwrap();
        assert_eq!(a.path.as_deref(), Some("/synthetic/a.m4a".as_ref()));
        assert_eq!(a.title.as_deref(), Some("Synthetic"));
        assert_eq!(a.participants, ["Ana <ana@example.invalid>"]);
        assert!(a.manifest.is_none());
        assert_eq!(a.lang.as_deref(), Some("pt-BR"));

        // As `--manifest` with `--lang` or `--date` in argv.
        for extra in [["--lang", "en"], ["--date", "2026-11-10"]] {
            let mut a = import_args(&extra);
            let e = import_spec(&mut a, ImportSpec::Manifest("batch.json".into())).unwrap_err();
            assert_eq!(e.code(), "invalid_input");
            assert!(a.manifest.is_none());
        }
    }
}
