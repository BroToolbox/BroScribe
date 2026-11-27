//! `import` (file, folder, manifest) and `identity calibrate`, through the
//! pipeline. Progress goes to stderr; results to stdout.

use std::path::Path;

use broscribe_core::config::Config;
use broscribe_core::render::format_hms;
use broscribe_core::schema::{Handling, Participant};
use broscribe_core::{Error, Library, MeetingId, Result, ops};
use broscribe_models::Registry;
use broscribe_models::llm::ProviderRegistry;
use broscribe_pipeline::import::{
    MoveRecovery, import_file_with_recoveries, import_media_folder_without_moves,
    import_media_without_moves, is_media_source,
};
use broscribe_pipeline::{
    BatchOptions, CalibrateOptions, Env, ImportOptions, ImportOutcome, calibrate, import_folder,
    import_manifest, manifest,
};
use serde_json::{Value, json};

use super::write_value;
use crate::args::parse_start;
use crate::cli::{HandlingArg, ImportArgs};
use crate::{Ctx, Deps, to_value};

/// A `--participant` value: `Name <email>`, `<email>`, an e-mail or a name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParticipantArg {
    name: Option<String>,
    email: Option<String>,
}

fn parse_participant(s: &str) -> Result<ParticipantArg> {
    let invalid = || {
        Error::InvalidInput(format!(
            "not a participant (expected \"Name <email>\", a name or an e-mail): {s:?}"
        ))
    };
    let s = s.trim();
    if let Some((name, rest)) = s.split_once('<') {
        let email = rest.trim().strip_suffix('>').ok_or_else(invalid)?.trim();
        if !email.contains('@') || email.contains(char::is_whitespace) {
            return Err(invalid());
        }
        let name = name.trim();
        return Ok(ParticipantArg {
            name: (!name.is_empty()).then(|| name.to_owned()),
            email: Some(email.to_owned()),
        });
    }
    if s.is_empty() || s.contains('>') {
        return Err(invalid());
    }
    if s.contains('@') && !s.contains(char::is_whitespace) {
        return Ok(ParticipantArg {
            name: None,
            email: Some(s.to_owned()),
        });
    }
    Ok(ParticipantArg {
        name: Some(s.to_owned()),
        email: None,
    })
}

/// The same participant: equal folded e-mails when both have one, else
/// equal folded names, or the same known person (the user included).
fn same_participant(a: &Participant, b: &Participant) -> bool {
    let key = |v: Option<&str>| v.map(manifest::fold).filter(|s| !s.is_empty());
    let same_key = match (key(a.email.as_deref()), key(b.email.as_deref())) {
        (Some(x), Some(y)) => x == y,
        _ => key(a.name.as_deref()).is_some_and(|n| Some(n) == key(b.name.as_deref())),
    };
    same_key || (a.is_self && b.is_self) || (a.person_id.is_some() && a.person_id == b.person_id)
}

/// `--participant` values as meeting participants, matched to known people
/// by e-mail or name (the user by the setup name and e-mails), without
/// duplicates; a repeated person fills in the e-mail or name it lacked.
fn participants_from_args(
    lib: &Library,
    config: &Config,
    args: &[String],
) -> Result<Vec<Participant>> {
    let people = lib.list_people()?;
    let mut out: Vec<Participant> = Vec::new();
    for arg in args {
        let p = parse_participant(arg)?;
        let matched = manifest::match_person(&people, p.name.as_deref(), p.email.as_deref());
        let is_self = manifest::is_user(&config.setup, p.name.as_deref(), p.email.as_deref())
            || matched.is_some_and(|person| person.is_self);
        let participant = Participant {
            name: p.name,
            email: p.email,
            is_self,
            person_id: if is_self {
                people
                    .iter()
                    .find(|person| person.is_self)
                    .map(|person| person.id.clone())
            } else {
                matched.map(|person| person.id.clone())
            },
            evidence: Vec::new(),
        };
        match out.iter_mut().find(|q| same_participant(q, &participant)) {
            Some(known) => {
                if known.email.is_none() {
                    known.email = participant.email;
                }
                if known.name.is_none() {
                    known.name = participant.name;
                }
            }
            None => out.push(participant),
        }
    }
    Ok(out)
}

/// Adds `participants` to meetings that were imported before (a new import
/// takes them through its options), keeping the entries already there.
fn add_participants(
    lib: &Library,
    meetings: &[MeetingId],
    participants: &[Participant],
) -> Result<()> {
    if participants.is_empty() || meetings.is_empty() {
        return Ok(());
    }
    ops::with_index(lib, |tx, index| {
        for id in meetings {
            let mut m = tx.library().load_meeting(id)?;
            let before = m.participants.len();
            for p in participants {
                if !m.participants.iter().any(|q| same_participant(q, p)) {
                    m.participants.push(p.clone());
                }
            }
            if m.participants.len() != before {
                m.updated_at = broscribe_core::schema::now();
                tx.save_meeting(&m)?;
                index.upsert_meeting(tx, &m)?;
            }
        }
        Ok(())
    })
}

fn env<'a>(
    deps: &'a Deps,
    lib: &'a Library,
    config: &'a Config,
    progress: &'a dyn broscribe_pipeline::ProgressSink,
) -> Env<'a> {
    Env {
        lib,
        config,
        decoders: &deps.decoder,
        probe: deps.process.as_ref(),
        execution: &deps.execution,
        progress: Some(progress),
    }
}

/// Whether the active ASR model takes a language. Models with automatic
/// language detection (Parakeet TDT v3) do not: their registry entry has no
/// `language` parameter (design §6.3).
fn asr_takes_language(data_dir: &Path, config: &Config) -> Result<bool> {
    let registry = Registry::load(data_dir)?;
    Ok(registry
        .get(&config.profile.asr)
        .is_some_and(|e| e.params.contains_key("language")))
}

fn outcome_line(o: &ImportOutcome) -> String {
    if o.already_exists {
        return format!(
            "Already imported: {} ({})",
            o.meeting_id,
            o.source_path.display()
        );
    }
    format!(
        "Imported {}  {}  {}: {} segments, {} speakers ({} named automatically, {} suggested)",
        o.meeting_id,
        o.title.as_deref().unwrap_or("(untitled)"),
        format_hms(o.duration_s),
        o.segments,
        o.speakers,
        o.auto_named,
        o.suggested
    )
}

fn completed_move_line(r: &MoveRecovery) -> Option<String> {
    r.meeting_id.as_ref().map(|id| {
        format!(
            "Completed the interrupted move of {}: imported as {id}",
            r.source.display()
        )
    })
}

/// Follow-ups of other files' interrupted `move` imports that an import's
/// recovery completed (their jobs are gone, so these are the only ones they
/// get): search vectors and retention, and the enrichment `auto_enrich`
/// gives a single-file import (never a file of a folder import). Returns
/// the enrichment result, as [`super::enrich::queue_after_import`].
pub(super) fn follow_up_completed_moves(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    moves: &[MoveRecovery],
) -> Value {
    let ids: Vec<MeetingId> = moves.iter().filter_map(|r| r.meeting_id.clone()).collect();
    if ids.is_empty() {
        return Value::Null;
    }
    super::after_identity(ctx, lib, config, Some(&ids));
    let single: Vec<MeetingId> = moves
        .iter()
        .filter(|r| !r.folder)
        .filter_map(|r| r.meeting_id.clone())
        .collect();
    super::enrich::queue_after_import(ctx, lib, config, &single, false)
}

/// Both enrichment results of one import (`Null`: none tried), each as
/// [`super::enrich::queue_after_import`] gives it: every queued job, refused
/// meeting and transaction error of either is kept, `a`'s first.
fn merge_enrichment(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Null, v) | (v, Value::Null) => v,
        (a, b) => {
            let both = |key: &str| -> Vec<Value> {
                [&a, &b]
                    .into_iter()
                    .filter_map(|v| v[key].as_array())
                    .flatten()
                    .cloned()
                    .collect()
            };
            json!({
                "queued": both("queued"),
                "failed": both("failed"),
                "errors": both("errors"),
            })
        }
    }
}

fn import_manifest_input(args: &ImportArgs) -> Result<Option<manifest::Manifest>> {
    if let Some(content) = &args.manifest_content {
        return manifest::parse(&content.to_string()).map(Some);
    }
    args.manifest.as_deref().map(manifest::read).transpose()
}

fn manifest_has_audio(entry: &manifest::ManifestMeeting, file_ok: impl Fn(&Path) -> bool) -> bool {
    if entry.sources.is_empty() {
        file_ok(&entry.audio_path)
    } else {
        file_ok(&entry.audio_path) && entry.sources.iter().all(|s| file_ok(&s.path))
    }
}

fn preview(ctx: &mut Ctx<'_>, a: &ImportArgs) -> Result<()> {
    use broscribe_core::import_files::{folder_media, is_media};

    let mcp = ctx.request_library.is_some();
    let eligible = |path: &Path| {
        if mcp {
            is_media_source(path)
        } else {
            path.is_file()
        }
    };
    let (kind, recordings, skipped, issues) = if let Some(manifest) = import_manifest_input(a)? {
        let mut recordings = Vec::new();
        let mut skipped = Vec::new();
        for m in &manifest.meetings {
            let has_audio = if !mcp && !m.sources.is_empty() {
                m.sources.iter().all(|source| source.path.is_file())
            } else {
                manifest_has_audio(m, eligible)
            };
            if has_audio {
                recordings.push(m.audio_path.clone());
            } else {
                skipped.push(m.audio_path.clone());
            }
        }
        ("manifest", recordings, skipped, to_value(&manifest.issues)?)
    } else {
        let path = a
            .path
            .as_deref()
            .ok_or_else(|| Error::InvalidInput("give a file or folder to inspect".into()))?;
        a.participants
            .iter()
            .try_for_each(|value| parse_participant(value).map(|_| ()))?;
        a.date.as_deref().map(parse_start).transpose()?;
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => {
                let (mut recordings, mut skipped) =
                    folder_media(path).map_err(|e| Error::io_at(path, e))?;
                if mcp {
                    recordings.retain(|path| {
                        if eligible(path) {
                            return true;
                        }
                        skipped.push(path.clone());
                        false
                    });
                }
                ("folder", recordings, skipped, json!([]))
            }
            Ok(meta) if meta.is_file() && is_media(path) && eligible(path) => {
                ("file", vec![path.into()], Vec::new(), json!([]))
            }
            Ok(_) => ("file", Vec::new(), vec![path.into()], json!([])),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(path.display().to_string()));
            }
            Err(e) => return Err(Error::io_at(path, e)),
        }
    };
    let count = recordings.len();
    if mcp {
        return crate::print_json(
            ctx.out,
            &json!({
                "dry_run": true, "kind": kind, "count": count, "recordings": recordings,
                "skipped_count": skipped.len(), "issues": issues,
            }),
        );
    }
    let value = json!({"dry_run": true, "kind": kind, "count": count, "recordings": recordings, "skipped": skipped, "issues": issues});
    ctx.emit(value, |out| {
        writeln!(out, "{kind}: {count} recording(s)")?;
        for path in skipped {
            writeln!(out, "Skipped: {}", path.display())?;
        }
        if let Some(issues) = issues.as_array() {
            for issue in issues {
                super::write_value(out, issue, 0)?;
            }
        }
        Ok(())
    })
}

pub(super) fn import(ctx: &mut Ctx<'_>, a: ImportArgs) -> Result<()> {
    let mcp = ctx.request_library.is_some();
    let mut handling = a.handling.map(|h| match h {
        HandlingArg::Copy => Handling::Copy,
        HandlingArg::Reference => Handling::Reference,
        HandlingArg::Move => Handling::Move,
    });
    if a.dry_run && !mcp {
        return preview(ctx, &a);
    }
    let config = ctx.config()?;
    if mcp {
        let resolved = handling.unwrap_or(config.import_handling);
        if resolved == Handling::Move {
            return Err(Error::InvalidInput("MCP imports require Copy or Reference. Choose a handling explicitly or change the default in BroScribe.".into()));
        }
        handling = Some(resolved);
    }
    if a.dry_run {
        return preview(ctx, &a);
    }
    let deps = ctx.deps;
    if a.enrich {
        // Refuse before importing: without an LLM provider enabled with
        // consent the queued enrich jobs could only fail later.
        let providers = ProviderRegistry::load(ctx.data_dir())?;
        broscribe_pipeline::enrich::llm_provider_ids(&config, &providers, None)?;
    }
    let lib = if mcp {
        ctx.library_existing(&config)?
    } else {
        ctx.create_library(&config)?
    };

    if let Some(manifest) = import_manifest_input(&a)? {
        if mcp
            && manifest.meetings.iter().any(|entry| {
                !manifest_has_audio(entry, |path| {
                    broscribe_core::import_files::is_media(path)
                        && (!path.exists() || is_media_source(path))
                })
            })
        {
            return Err(Error::InvalidInput(
                "manifest audio paths must name supported audio or video files".into(),
            ));
        }
        let mut models = deps.models.load(&config.profile)?;
        let run_manifest = if mcp {
            broscribe_pipeline::batch::import_manifest_without_decode_details
        } else {
            import_manifest
        };
        let summary = run_manifest(
            &env(deps, &lib, &config, &ctx.background_notes()),
            &mut models,
            &manifest,
            &BatchOptions {
                handling,
                encoder: broscribe_platform_native::NativePlatform::audio_encoder(),
            },
        )?;
        drop(models);
        for n in &summary.notes {
            ctx.note(n);
        }
        // Any manifest meeting may have changed: vectors and retention for
        // the whole library.
        super::after_identity(ctx, &lib, &config, None);
        let mut value = to_value(&summary)?;
        if a.enrich {
            let ids = super::enrich::unenriched_manifest_meetings(&lib, &manifest)?;
            value["enrichment"] = super::enrich::queue_after_import(ctx, &lib, &config, &ids, true);
        }
        return ctx.emit(value.clone(), |out| write_value(out, &value, 0));
    }

    let Some(path) = a.path.as_deref() else {
        return Err(Error::InvalidInput(
            "give a file or folder to import, or --manifest <file>".into(),
        ));
    };
    let mut notes = Vec::new();
    if let Some(lang) = &a.lang
        && !asr_takes_language(ctx.data_dir(), &config)?
    {
        notes.push(format!(
            "--lang {lang} is stored on the meeting but has no effect on recognition: {} detects the language automatically",
            config.profile.asr
        ));
    }
    let participants = participants_from_args(&lib, &config, &a.participants)?;
    let opts = ImportOptions {
        handling,
        language: a.lang.clone(),
        auto_language: a.auto_language,
        title: a.title.clone(),
        started_at: a.date.as_deref().map(parse_start).transpose()?,
        participants: participants.clone(),
        setup_session: None,
    };
    if !path.exists() {
        return Err(Error::NotFound(path.display().to_string()));
    }
    if mcp && !path.is_dir() && !is_media_source(path) {
        return Err(Error::InvalidInput(
            "choose a supported audio or video file".into(),
        ));
    }
    let mut models = deps.models.load(&config.profile)?;
    let progress = ctx.background_notes();
    let env = env(deps, &lib, &config, &progress);
    for n in &notes {
        ctx.note(format!("note: {n}"));
    }
    if path.is_dir() {
        let report = if mcp {
            import_media_folder_without_moves(&env, &mut models, path, &opts)?
        } else {
            import_folder(&env, &mut models, path, &opts)?
        };
        let ids: Vec<MeetingId> = report
            .imported
            .iter()
            .map(|o| o.meeting_id.clone())
            .collect();
        let existing: Vec<MeetingId> = report
            .imported
            .iter()
            .filter(|o| o.already_exists)
            .map(|o| o.meeting_id.clone())
            .collect();
        add_participants(&lib, &existing, &participants)?;
        super::after_identity(ctx, &lib, &config, Some(&ids));
        let moved = follow_up_completed_moves(ctx, &lib, &config, &report.completed_moves);
        let mut value = to_value(&report)?;
        let mut enrichment = Value::Null;
        if a.enrich {
            let new: Vec<MeetingId> = report
                .imported
                .iter()
                .filter(|o| !o.already_exists)
                .map(|o| o.meeting_id.clone())
                .collect();
            enrichment = super::enrich::queue_after_import(ctx, &lib, &config, &new, true);
        }
        let enrichment = merge_enrichment(enrichment, moved);
        if a.enrich || !enrichment.is_null() {
            value["enrichment"] = enrichment;
        }
        value["notes"] = json!(notes);
        value["participants"] = json!(participants.len());
        ctx.emit(value, |out| {
            for o in &report.imported {
                writeln!(out, "{}", outcome_line(o))?;
            }
            for line in report
                .completed_moves
                .iter()
                .filter_map(completed_move_line)
            {
                writeln!(out, "{line}")?;
            }
            for f in &report.failed {
                writeln!(
                    out,
                    "Failed {}: [{}] {}",
                    f.path.display(),
                    f.code,
                    f.message
                )?;
            }
            writeln!(
                out,
                "{} imported, {} failed.",
                report.imported.len(),
                report.failed.len()
            )
        })
    } else {
        let mut completed = Vec::new();
        let result = if mcp {
            import_media_without_moves(&env, &mut models, path, &opts)
        } else {
            import_file_with_recoveries(&env, &mut models, path, &opts, &mut completed)
        };
        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                follow_up_completed_moves(ctx, &lib, &config, &completed);
                return Err(e);
            }
        };
        if outcome.already_exists {
            add_participants(
                &lib,
                std::slice::from_ref(&outcome.meeting_id),
                &participants,
            )?;
        }
        super::after_identity(
            ctx,
            &lib,
            &config,
            Some(std::slice::from_ref(&outcome.meeting_id)),
        );
        let moved = follow_up_completed_moves(ctx, &lib, &config, &completed);
        let mut value = to_value(&outcome)?;
        let enrichment = if outcome.already_exists {
            Value::Null
        } else {
            super::enrich::queue_after_import(
                ctx,
                &lib,
                &config,
                std::slice::from_ref(&outcome.meeting_id),
                a.enrich,
            )
        };
        if !outcome.already_exists || !moved.is_null() {
            value["enrichment"] = merge_enrichment(enrichment, moved);
        }
        value["completed_moves"] = to_value(&completed)?;
        value["notes"] = json!(notes);
        value["participants"] = json!(participants.len());
        ctx.emit(value, |out| {
            writeln!(out, "{}", outcome_line(&outcome))?;
            for line in completed.iter().filter_map(completed_move_line) {
                writeln!(out, "{line}")?;
            }
            Ok(())
        })
    }
}

pub(super) fn calibrate_cmd(ctx: &mut Ctx<'_>, manifest_path: &Path) -> Result<()> {
    let deps = ctx.deps;
    let config = ctx.config()?;
    let lib = ctx.create_library(&config)?;
    let manifest = manifest::read(manifest_path)?;
    let mut models = deps.models.load(&config.profile)?;
    let report = calibrate(
        &env(deps, &lib, &config, &ctx.background_notes()),
        &mut models,
        &manifest,
        ctx.paths.data_dir.as_path(),
        &CalibrateOptions {
            include_names: false,
        },
    )?;
    let value: Value = to_value(&report)?;
    ctx.emit(value.clone(), |out| {
        writeln!(out, "Calibration for {}:", config.profile.speaker_embedding)?;
        write_value(out, &value, 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn participant_forms() {
        let p = parse_participant(" Ana Example <ana@example.com> ").unwrap();
        assert_eq!(p.name.as_deref(), Some("Ana Example"));
        assert_eq!(p.email.as_deref(), Some("ana@example.com"));
        let p = parse_participant("<bo@example.com>").unwrap();
        assert_eq!((p.name, p.email.as_deref()), (None, Some("bo@example.com")));
        let p = parse_participant("bo@example.com").unwrap();
        assert_eq!(p.email.as_deref(), Some("bo@example.com"));
        let p = parse_participant("Bo Test").unwrap();
        assert_eq!((p.name.as_deref(), p.email), (Some("Bo Test"), None));
        assert!(parse_participant("Ana <not-an-email>").is_err());
        assert!(parse_participant("Ana <ana@example.com").is_err());
        assert!(parse_participant("  ").is_err());
    }

    #[test]
    fn merged_enrichment_keeps_every_outcome_of_both_results_in_either_order() {
        let current = json!({
            "queued": [],
            "failed": [{"meeting_id": "m-current", "code": "schema", "message": "unreadable record"}],
            "errors": [{"code": "io", "message": "commit failed"}],
        });
        let recovered = json!({
            "queued": ["job-recovered"],
            "failed": [],
            "errors": [{"code": "library_busy", "message": "queue lock held"}],
        });
        assert_eq!(
            merge_enrichment(current.clone(), recovered.clone()),
            json!({
                "queued": ["job-recovered"],
                "failed": [{"meeting_id": "m-current", "code": "schema", "message": "unreadable record"}],
                "errors": [
                    {"code": "io", "message": "commit failed"},
                    {"code": "library_busy", "message": "queue lock held"},
                ],
            })
        );
        assert_eq!(
            merge_enrichment(recovered.clone(), current.clone()),
            json!({
                "queued": ["job-recovered"],
                "failed": [{"meeting_id": "m-current", "code": "schema", "message": "unreadable record"}],
                "errors": [
                    {"code": "library_busy", "message": "queue lock held"},
                    {"code": "io", "message": "commit failed"},
                ],
            })
        );
        assert_eq!(merge_enrichment(Value::Null, recovered.clone()), recovered);
        assert_eq!(merge_enrichment(current.clone(), Value::Null), current);
        assert_eq!(merge_enrichment(Value::Null, Value::Null), Value::Null);
    }

    #[test]
    fn repeated_participants_are_stored_once() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("library")).unwrap();
        let args: Vec<String> = [
            "Ana Example <ana@example.com>",
            "<ANA@example.com>",
            "Bo Test",
            " bo test ",
            "Bo Test <bo@example.com>",
        ]
        .map(String::from)
        .to_vec();
        let out = participants_from_args(&lib, &Config::default(), &args).unwrap();
        let keys: Vec<_> = out
            .iter()
            .map(|p| (p.name.as_deref(), p.email.as_deref()))
            .collect();
        assert_eq!(
            keys,
            [
                (Some("Ana Example"), Some("ana@example.com")),
                (Some("Bo Test"), Some("bo@example.com")),
            ]
        );
    }
}
