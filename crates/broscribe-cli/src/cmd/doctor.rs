//! `doctor`: configuration, library, models, Me enrollment, decoders and
//! what this platform can do (design §4.1); `library check` and `library
//! repair`. Reports problems; exits 0 unless the checks themselves fail.
//!
//! Every read here changes nothing: no library, queue or index is created,
//! no retention, prune or repair runs. A capability the CLI cannot observe
//! is reported unknown, never as passing. `library repair` is the only
//! writer, and only in the scopes named with `--only` and `--yes`.

use std::collections::BTreeMap;

use broscribe_core::config::Config;
use broscribe_core::index::{Index, IndexDatabase};
use broscribe_core::lifecycle;
use broscribe_core::ops;
use broscribe_core::queue::{PruneCriteria, PruneKeep, Queue};
use broscribe_core::{Error, Library, Result};
use broscribe_models::{Health, ModelStore, Registry};
use broscribe_platform::ProcessProbe;
use broscribe_platform::ffmpeg_decoder::FfmpegDecoder;
use broscribe_platform_native::NativePlatform;
use serde::Serialize;
use serde_json::{Value, json};

use super::diagnostics::{
    Agent, Availability, Capability, CapabilityName, Grant, Permission, Permissions, Runtime,
};
use super::models::{health_str, profile_models, role_names};
use crate::Ctx;
use crate::cli::RepairScopeArg;

/// What a check found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ok,
    Problem,
    /// Not observable from here: never counted as passing.
    Unknown,
}

impl State {
    fn of(ok: bool) -> Self {
        if ok { Self::Ok } else { Self::Problem }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Problem => "problem",
            Self::Unknown => "unknown",
        }
    }
}

struct Check {
    name: &'static str,
    state: State,
    /// Optional checks (ffmpeg, capabilities) never make the report fail.
    required: bool,
    detail: String,
}

impl Check {
    fn new(name: &'static str, ok: bool, detail: impl Into<String>) -> Self {
        Self {
            name,
            state: State::of(ok),
            required: true,
            detail: detail.into(),
        }
    }

    fn optional(name: &'static str, state: State, detail: impl Into<String>) -> Self {
        Self {
            name,
            state,
            required: false,
            detail: detail.into(),
        }
    }
}

/// A serializable enum's snake_case name (`unknown` if it has none).
fn name_of(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn count_by<T: Serialize>(items: impl IntoIterator<Item = T>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for item in items {
        *counts.entry(name_of(item)).or_insert(0) += 1;
    }
    counts
}

/// [`check_library`]'s findings: `clean` when nothing is pending or
/// unreadable (due deletions and finished jobs are not problems).
struct LibraryCheck {
    value: Value,
    clean: bool,
}

/// The read-only integrity check behind `doctor` (counts only) and
/// `library check` (`ids`: also the identifiers). Never names, titles or
/// paths; a part that cannot be read reports its error code.
fn check_library(
    lib: &Library,
    config: &Config,
    data_dir: &std::path::Path,
    probe: &dyn ProcessProbe,
    ids: bool,
) -> LibraryCheck {
    let root = lib.root();
    let mut clean = true;
    let index = Index::health(root).map(|health| {
        let mut value = json!({
            "database": health.database,
            "rebuild": health.rebuild,
            "meetings": health.meetings.len(),
            "people": health.people.len(),
        });
        if ids {
            value["meeting_ids"] = json!(health.meetings);
            value["person_ids"] = json!(health.people);
        }
        let ok = health.is_current();
        if !ok {
            value["repair"] = "broscribe library repair --only index --yes".into();
        }
        (value, ok)
    });
    let lifecycle = lifecycle::diagnose(lib, data_dir, probe).map(|d| {
        let pending = !d.pending_deletions.is_empty()
            || d.unreadable_deletion_markers > 0
            || d.unaccounted_staged > 0
            || !d.pending_discards.is_empty()
            || !d.unpublished_meetings.is_empty()
            || d.unidentified_folders > 0
            || d.orphan_temp_files > 0;
        let mut value = json!({
            "pending_deletions": d.pending_deletions.len(),
            "deletion_stages": count_by(d.pending_deletions.iter().map(|p| p.stage)),
            "unreadable_deletion_markers": d.unreadable_deletion_markers,
            "unaccounted_staged": d.unaccounted_staged,
            "pending_discards": d.pending_discards.len(),
            "unpublished_meetings": d.unpublished_meetings.len(),
            "unidentified_folders": d.unidentified_folders,
            "orphan_temp_files": d.orphan_temp_files,
        });
        if ids {
            value["deletion_meeting_ids"] = json!(
                d.pending_deletions
                    .iter()
                    .map(|p| &p.meeting_id)
                    .collect::<Vec<_>>()
            );
            value["discard_session_ids"] = json!(d.pending_discards);
            value["unpublished_meeting_ids"] = json!(d.unpublished_meetings);
        }
        (value, !pending)
    });
    let identity = ops::identity_check(lib).map(|c| {
        let mut value = json!({
            "people": c.people,
            "samples": c.samples,
            "people_with_unusable_samples": c.unusable.len(),
            "unusable_samples": c.unusable.iter().map(|u| u.undecodable + u.non_finite).sum::<usize>(),
            "unreadable_voiceprints": c.unreadable_voiceprints.len(),
            "unreadable_live_sidecars": c.unreadable_live_sidecars.len(),
        });
        if ids {
            value["unusable"] = json!(c.unusable);
            value["unreadable_voiceprint_person_ids"] = json!(c.unreadable_voiceprints);
            value["unreadable_live_sidecar_session_ids"] = json!(c.unreadable_live_sidecars);
        }
        (value, c.is_clean())
    });
    let queue = Queue::open_existing(root).and_then(|queue| match queue {
        None => Ok((json!({ "present": false }), true)),
        Some(queue) => {
            let criteria = PruneCriteria {
                failed: true,
                ..PruneCriteria::default()
            };
            queue.prune_plan(probe, &criteria).map(|plan| {
                let unknown = |reason: PruneKeep| {
                    matches!(reason, PruneKeep::Unreadable | PruneKeep::NoRecord)
                };
                let mut value = json!({
                    "present": true,
                    "checked": plan.checked,
                    "removable_finished_jobs": plan.removed.len() + plan.remaining,
                    "empty_folders": plan.empty_folders,
                    "kept": count_by(plan.kept.iter().map(|k| k.reason)),
                });
                if ids {
                    value["unreadable_job_ids"] = json!(
                        plan.kept
                            .iter()
                            .filter(|k| unknown(k.reason))
                            .map(|k| &k.id)
                            .collect::<Vec<_>>()
                    );
                }
                (value, !plan.kept.iter().any(|k| unknown(k.reason)))
            })
        }
    });
    let retention =
        broscribe_pipeline::retention::explain(lib, config, probe, None, None).map(|r| {
            let due = r
                .meetings
                .iter()
                .filter(|m| m.status.verdict.is_delete())
                .count();
            let mut verdicts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
            for m in &r.meetings {
                let v = serde_json::to_value(m.status.verdict).unwrap_or(Value::Null);
                let verdict = v["verdict"].as_str().unwrap_or("unknown").to_owned();
                let reason = v["reason"].as_str().unwrap_or("unknown").to_owned();
                *verdicts
                    .entry(verdict)
                    .or_default()
                    .entry(reason)
                    .or_insert(0) += 1;
            }
            let mut value = json!({
                "due_deletions": due,
                "verdicts": verdicts,
                "unreadable_meetings": r.unreadable,
                "unreadable_jobs": r.unreadable_jobs,
            });
            if ids {
                value["due_meeting_ids"] = json!(
                    r.meetings
                        .iter()
                        .filter(|m| m.status.verdict.is_delete())
                        .map(|m| &m.meeting_id)
                        .collect::<Vec<_>>()
                );
            }
            (value, r.unreadable == 0 && r.unreadable_jobs == 0)
        });
    let mut part = |result: Result<(Value, bool)>| match result {
        Ok((value, ok)) => {
            clean &= ok;
            value
        }
        Err(e) => {
            clean = false;
            json!({ "error": e.code() })
        }
    };
    let value = json!({
        "index": part(index),
        "lifecycle": part(lifecycle),
        "identity": part(identity),
        "queue": part(queue),
        "retention": part(retention),
    });
    LibraryCheck { value, clean }
}

/// The detail line of a library check in `doctor`.
fn library_summary(check: &LibraryCheck) -> String {
    if check.clean {
        return "nothing pending or unreadable".into();
    }
    let v = &check.value;
    let mut parts = Vec::new();
    let mut add = |label: &str, n: &Value| {
        if n.as_u64().is_some_and(|n| n > 0) || n.as_bool() == Some(true) {
            parts.push(format!("{label}: {n}"));
        }
    };
    add("index rebuild needed", &v["index"]["rebuild"]);
    add("index meetings pending", &v["index"]["meetings"]);
    add("index people pending", &v["index"]["people"]);
    add("deletions pending", &v["lifecycle"]["pending_deletions"]);
    add(
        "orphaned temporary files",
        &v["lifecycle"]["orphan_temp_files"],
    );
    add("discards pending", &v["lifecycle"]["pending_discards"]);
    add(
        "unpublished meetings",
        &v["lifecycle"]["unpublished_meetings"],
    );
    add("unusable voice samples", &v["identity"]["unusable_samples"]);
    add(
        "unreadable voiceprints",
        &v["identity"]["unreadable_voiceprints"],
    );
    add(
        "unreadable meetings",
        &v["retention"]["unreadable_meetings"],
    );
    add("unreadable jobs", &v["retention"]["unreadable_jobs"]);
    for part in ["index", "lifecycle", "identity", "queue", "retention"] {
        if let Some(code) = v[part]["error"].as_str() {
            parts.push(format!("{part} unreadable ({code})"));
        }
    }
    if parts.is_empty() {
        parts.push("see `broscribe library check`".into());
    }
    format!("{}; `broscribe library check` lists them", parts.join(", "))
}

/// What capturing, encoding, playback, meeting readers and window titles
/// can do here, worded from the probes `diagnostics` reports (one source,
/// no side effects). A check passes only on an observed fact: a facility
/// found and, where it has one, its permission granted; whatever the CLI
/// cannot observe stays unknown.
fn capability_checks(agent: &Agent, capabilities: &[Capability]) -> Vec<Check> {
    let probe = |name: CapabilityName| capabilities.iter().find(|c| c.name == name).copied();
    let mut checks = Vec::new();
    if let Some(encoder) = probe(CapabilityName::Encoder) {
        checks.push(Check::optional(
            "encoder",
            capability_state(&encoder),
            match encoder.availability {
                Availability::BuiltIn => "built into the system",
                Availability::Found => "ffmpeg (AAC): found",
                Availability::Unknown => {
                    "ffmpeg is not on PATH; only the agent sees one installed beside BroScribe"
                }
                _ => "ffmpeg not found: recordings stay uncompressed until it is installed",
            },
        ));
    }
    if let Some(playback) = probe(CapabilityName::Playback) {
        checks.push(Check::optional(
            "playback",
            capability_state(&playback),
            match playback.availability {
                Availability::BuiltIn => {
                    "built into the system; the player itself is checked when the app plays"
                }
                Availability::Found => {
                    "ffplay and ffprobe found; the player itself is checked when the app plays"
                }
                Availability::Unknown => {
                    "ffplay is not on PATH; only the app sees one installed beside BroScribe"
                }
                _ => "ffplay or ffprobe not found: the app cannot play recordings until they are installed",
            },
        ));
    }
    if let Some(readers) = probe(CapabilityName::MeetingReaders) {
        let base = match readers.availability {
            Availability::BuiltIn => {
                "readers run in the app under its own Accessibility grant, which the CLI cannot read"
            }
            Availability::Reachable => {
                "the accessibility service answers; reading a call is proven only during one"
            }
            _ => "the accessibility service does not answer: meeting readers cannot run",
        };
        let runtime = match readers.runtime {
            Runtime::Observed => "; the current recording's reader is reading",
            Runtime::Degraded => "; the current recording's reader is degraded",
            Runtime::NotChecked => "",
        };
        checks.push(Check::optional(
            "meeting_readers",
            capability_state(&readers),
            format!("{base}{runtime}"),
        ));
    }
    checks.push(match agent {
        Agent::Running {
            permissions:
                Permissions::Answered {
                    microphone,
                    system_audio,
                },
            ..
        } => {
            let state = match (microphone, system_audio) {
                (Grant::Granted, Grant::Granted) => State::Ok,
                (Grant::Denied, _) | (_, Grant::Denied) => State::Problem,
                // Not granted yet, or a system without a grant state: only
                // a recording proves capture.
                _ => State::Unknown,
            };
            Check::optional(
                "capture",
                state,
                format!(
                    "microphone: {}; system audio: {} (the agent's grants)",
                    microphone.as_str(),
                    system_audio.as_str()
                ),
            )
        }
        Agent::Running {
            permissions: Permissions::Stopped,
            ..
        } => Check::optional(
            "capture",
            State::Unknown,
            "BroScribe Agent stopped before it answered",
        ),
        Agent::Running {
            permissions: Permissions::Failed(e),
            ..
        }
        | Agent::Unreachable(e) => Check::optional("capture", State::Unknown, e.to_string()),
        Agent::NotRunning => Check::optional(
            "capture",
            State::Unknown,
            "capture permissions belong to BroScribe Agent, which is not running",
        ),
    });
    if let Some(titles) = probe(CapabilityName::WindowTitles) {
        let available = titles.availability == Availability::Reachable;
        checks.push(Check::optional(
            "window_titles",
            State::of(available),
            format!("available: {available}; detection still works without titles"),
        ));
    }
    checks
}

/// A capability passes when its facility is there and its permission, if
/// any, granted; missing or denied is a problem; anything else unknown.
fn capability_state(capability: &Capability) -> State {
    match capability.availability {
        Availability::NotFound | Availability::Unreachable => State::Problem,
        Availability::Unknown | Availability::NotRunning => State::Unknown,
        Availability::BuiltIn | Availability::Found | Availability::Reachable => {
            match capability.permission {
                Permission::Granted | Permission::NotApplicable => State::Ok,
                Permission::Denied => State::Problem,
                Permission::NotDetermined | Permission::NotExposed | Permission::Unknown => {
                    State::Unknown
                }
            }
        }
    }
}

pub(super) fn doctor(ctx: &mut Ctx<'_>) -> Result<()> {
    let data_dir = ctx.data_dir().to_path_buf();
    let mut checks = Vec::new();

    let config_path = Config::path(&data_dir);
    let config = match ctx.config() {
        Ok(c) => {
            let detail = if config_path.is_file() {
                config_path.display().to_string()
            } else {
                format!("{} (not written yet; defaults)", config_path.display())
            };
            checks.push(Check::new("config", true, detail));
            c
        }
        Err(e) => {
            checks.push(Check::new("config", false, e.to_string()));
            Config::default()
        }
    };
    if config.setup.defer_assistant {
        checks.push(Check::new("setup", true, "Setup assistant deferred."));
    } else if !config.setup.completed_steps.is_empty() {
        checks.push(Check::new(
            "setup",
            true,
            format!("completed: {}", config.setup.completed_steps.join(", ")),
        ));
    } else {
        checks.push(Check::new("setup", false, "not run yet: `broscribe setup`"));
    }

    let (lib_path, source) = ctx.library_path(&config);
    let not_created = || {
        Check::new(
            "library",
            true,
            format!(
                "{} (from {}), not created yet",
                lib_path.display(),
                source.as_str()
            ),
        )
    };
    let mut lib = None;
    if lib_path.is_dir() {
        match Library::open_existing(&lib_path)
            .and_then(|l| l.list_meeting_dirs().map(|d| (l, d.len())))
        {
            Ok((l, n)) => {
                checks.push(Check::new(
                    "library",
                    true,
                    format!(
                        "{} (from {}), {n} meetings",
                        lib_path.display(),
                        source.as_str()
                    ),
                ));
                // Whether the database answers searches; what is pending is
                // part of `library_integrity` below.
                checks.push(match Index::health(l.root()).map(|h| h.database) {
                    Ok(IndexDatabase::Ready) => Check::new("index", true, "ready"),
                    Ok(IndexDatabase::NotNeeded) => {
                        Check::new("index", true, "not needed yet: nothing to index")
                    }
                    Ok(database) => Check::new(
                        "index",
                        false,
                        format!(
                            "{}: run `broscribe library repair --only index --yes`",
                            name_of(database)
                        ),
                    ),
                    Err(e) => Check::new("index", false, e.to_string()),
                });
                lib = Some(l);
            }
            Err(Error::NotFound(_)) => checks.push(not_created()),
            Err(e) => checks.push(Check::new("library", false, e.to_string())),
        }
    } else {
        checks.push(not_created());
    }

    // Every model the active profile needs, the live preview's included; an
    // invalid profile (unknown model, wrong role, no live model) fails.
    let registry = Registry::load(&data_dir);
    match registry
        .as_ref()
        .map_err(ToString::to_string)
        .and_then(|r| profile_models(r, &config.profile).map_err(|e| e.to_string()))
    {
        Ok(needed) => {
            let store = ModelStore::new(&data_dir);
            for (entry, roles) in needed {
                let id = &entry.id;
                let health = store.doctor(entry, false)?;
                let mut detail = format!("{}: {id} {}", role_names(&roles), health_str(&health));
                if health == Health::Missing {
                    detail.push_str(&format!(" (run `broscribe models pull {id}`)"));
                }
                if !entry.license_verified {
                    detail.push_str("; licence not verified");
                }
                checks.push(Check::new("model", health == Health::Installed, detail));
            }
        }
        Err(e) => checks.push(Check::new("models", false, e)),
    }

    let model = config.profile.speaker_embedding.as_str();
    let me = match &lib {
        Some(l) => match l.self_person()? {
            Some(p) => {
                let n = l
                    .load_voiceprints(&p.id)?
                    .embeddings
                    .iter()
                    .filter(|v| v.model == model)
                    .count();
                if n > 0 {
                    Check::new(
                        "me",
                        true,
                        format!("{} ({}), {n} voiceprints for {model}", p.name, p.id),
                    )
                } else {
                    Check::new(
                        "me",
                        false,
                        format!(
                            "{} ({}) has no voiceprint for {model}: `broscribe setup --enroll-me <meeting|file> --range a-b`",
                            p.name, p.id
                        ),
                    )
                }
            }
            None => Check::new("me", false, "no Me person: `broscribe setup --name …`"),
        },
        None => Check::new("me", false, "no library yet"),
    };
    checks.push(me);

    let ffmpeg = FfmpegDecoder::locate();
    checks.push(match ffmpeg.ffmpeg_path() {
        Some(p) if ffmpeg.is_available() => {
            Check::optional("ffmpeg", State::Ok, p.display().to_string())
        }
        _ => Check::optional(
            "ffmpeg",
            State::Problem,
            "not found: reduced format support (install ffmpeg for more formats)",
        ),
    });
    checks.push(Check::new(
        "decoders",
        !ctx.deps.decoder.is_empty(),
        ctx.deps.decoder.names().join(" → "),
    ));

    let agent = super::diagnostics::ask_agent(&data_dir);
    if agent
        .error()
        .is_some_and(|error| error.code() == broscribe_core::maintenance::ERROR_CODE)
    {
        return Err(super::maintenance::denied());
    }
    let running = agent.is_running();
    let installed = super::daemon::installed(ctx);
    let protocol = agent.protocol();
    let daemon = json!({
        "installed": installed.as_ref().ok().copied(),
        "running": running,
        "protocol": protocol,
        "error": agent.error().map(ToString::to_string).or_else(|| installed.as_ref().err().map(ToString::to_string)),
    });
    checks.push(Check::optional(
        "daemon",
        match &agent {
            Agent::Running { .. } => State::Ok,
            Agent::NotRunning => State::Problem,
            // An agent that cannot be asked (an older protocol, a transport
            // this platform does not wire yet) is not a passing agent.
            Agent::Unreachable(_) => State::Unknown,
        },
        match agent.error() {
            Some(e) => format!("not reachable: {e}"),
            None => format!(
                "installed: {}; running: {running}",
                installed
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_else(|e| e.to_string())
            ),
        },
    ));
    let capabilities = capability_checks(&agent, &super::diagnostics::capabilities(&agent));
    let titles = NativePlatform::window_titles_available();
    checks.extend(capabilities);

    let journal_bytes = directory_bytes(&lifecycle::journal_root(&data_dir));
    checks.push(Check::optional(
        "pending_journal",
        State::of(journal_bytes.is_ok()),
        journal_bytes
            .as_ref()
            .map(|n| format!("{n} bytes"))
            .unwrap_or_else(ToString::to_string),
    ));

    if let Some(state) = agent.state() {
        for code in [
            broscribe_ipc::recording_words::JOURNAL_LIBRARY_MISMATCH,
            broscribe_ipc::recording_words::JOURNAL_LIBRARY_UNKNOWN,
        ] {
            let count = state
                .journal_issues
                .iter()
                .filter(|issue| issue.code == code)
                .count();
            if count > 0
                && let Some(words) = broscribe_ipc::recording_words::journal_library_issue(code)
            {
                checks.push(Check::optional(
                    code,
                    State::Problem,
                    format!("{count} affected. {words}"),
                ));
            }
        }
    }

    let library_check = lib
        .as_ref()
        .map(|lib| check_library(lib, &config, &data_dir, ctx.deps.process.as_ref(), false));
    let due_deletions = match &library_check {
        Some(check) => match check.value["retention"]["due_deletions"].as_u64() {
            Some(count) => json!({"count": count, "dry_run": true}),
            None => json!({"error": check.value["retention"]["error"], "dry_run": true}),
        },
        None => json!({"count": 0, "dry_run": true}),
    };
    if let Some(check) = &library_check {
        checks.push(Check::optional(
            "library_integrity",
            State::of(check.clean),
            library_summary(check),
        ));
    }
    checks.push(Check::optional(
        "due_deletions",
        State::of(due_deletions["error"].is_null()),
        due_deletions.to_string(),
    ));

    let integrations = match super::integrations::service(ctx)
        .and_then(|s| s.statuses().map_err(super::integrations::core))
    {
        Ok(statuses) => json!(statuses),
        Err(e) => json!({"error":e.to_string()}),
    };
    checks.push(Check::optional(
        "integrations",
        State::of(integrations.is_array()),
        integrations.to_string(),
    ));

    let ok = checks.iter().all(|c| c.state == State::Ok || !c.required);
    let rows: Vec<Value> = checks
        .iter()
        .map(|c| {
            json!({
                "check": c.name,
                "ok": c.state == State::Ok,
                "state": c.state.as_str(),
                "required": c.required,
                "detail": c.detail,
            })
        })
        .collect();
    let value = json!({
        "ok": ok,
        "data_dir": data_dir,
        "library": lib_path,
        "library_source": source.as_str(),
        "checks": rows,
        "setup": {
            "deferred": config.setup.defer_assistant,
            "completed_steps": config.setup.completed_steps,
        },
        "daemon": daemon,
        "window_titles_available": titles,
        "pending_journal_bytes": journal_bytes.ok(),
        "due_deletions": due_deletions,
        "library_check": library_check.as_ref().map(|c| &c.value),
        "integrations": integrations,
    });
    ctx.emit(value, |out| {
        for c in &checks {
            let mark = match (c.state, c.required) {
                (State::Ok, _) => "ok  ",
                (State::Problem, true) | (State::Unknown, true) => "FAIL",
                (State::Problem, false) => "warn",
                (State::Unknown, false) => "?   ",
            };
            writeln!(out, "[{mark}] {}: {}", c.name, c.detail)?;
        }
        if ok {
            writeln!(out, "Everything needed is in place.")
        } else {
            writeln!(out, "Some checks failed.")
        }
    })
}

/// `library check`: the read-only integrity check with identifiers.
pub(super) fn library_check(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let check = check_library(
        &lib,
        &config,
        ctx.data_dir(),
        ctx.deps.process.as_ref(),
        true,
    );
    let mut value = check.value;
    value["clean"] = check.clean.into();
    super::daemon::emit(ctx, value)
}

/// Repair scopes in the order they run: orphaned temporaries, finished
/// deletions and removed samples first, so the index repair that follows
/// sees their files.
const REPAIR_ORDER: [RepairScopeArg; 4] = [
    RepairScopeArg::Temps,
    RepairScopeArg::Deletions,
    RepairScopeArg::Identity,
    RepairScopeArg::Index,
];

/// `library repair --only <scopes> --dry-run|--yes`: what `library check`
/// reports, in the named scopes only; `dry_run` reports what would change.
pub(super) fn library_repair(
    ctx: &mut Ctx<'_>,
    only: &[RepairScopeArg],
    dry_run: bool,
) -> Result<()> {
    if only.is_empty() {
        return Err(Error::InvalidInput(
            "name the scopes to repair with --only (index, deletions, identity, temps)".into(),
        ));
    }
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let data_dir = ctx.data_dir().to_path_buf();
    let probe = ctx.deps.process.as_ref();
    let mut scopes = serde_json::Map::new();
    let mut rechecks = false;
    for scope in REPAIR_ORDER.into_iter().filter(|s| only.contains(s)) {
        let (name, value) = match scope {
            RepairScopeArg::Temps => {
                let removed = if dry_run {
                    None
                } else {
                    Some(lifecycle::sweep_orphan_temps(&lib, &data_dir, probe)?)
                };
                let left = lifecycle::diagnose(&lib, &data_dir, probe)?.orphan_temp_files;
                (
                    "temps",
                    json!({ "removed": removed, "orphan_temp_files": left }),
                )
            }
            RepairScopeArg::Deletions => {
                let finished = if dry_run {
                    None
                } else {
                    Some(lifecycle::finish_pending_deletions(&lib, probe)?)
                };
                let left = lifecycle::diagnose(&lib, &data_dir, probe)?.pending_deletions;
                (
                    "deletions",
                    json!({
                        "finished": finished,
                        "pending": left.len(),
                        "meeting_ids": left.iter().map(|d| &d.meeting_id).collect::<Vec<_>>(),
                    }),
                )
            }
            RepairScopeArg::Identity => {
                if dry_run {
                    let check = ops::identity_check(&lib)?;
                    (
                        "identity",
                        json!({
                            "samples_to_remove": check.unusable.iter().map(|u| u.undecodable + u.non_finite).sum::<usize>(),
                            "person_ids": check.unusable.iter().map(|u| &u.person_id).collect::<Vec<_>>(),
                            "unreadable_voiceprints_left_alone": check.unreadable_voiceprints.len(),
                        }),
                    )
                } else {
                    let repair = lib.write(ops::repair_identity)?;
                    rechecks |= !repair.learning.is_empty();
                    (
                        "identity",
                        json!({
                            "samples_removed": repair.samples_removed,
                            "person_ids": repair.people,
                            "rechecks_requested": repair.learning.len(),
                        }),
                    )
                }
            }
            RepairScopeArg::Index => {
                let repaired = if dry_run {
                    None
                } else {
                    let report = lib.write(|tx| Index::repair_pending(tx))?;
                    Some(json!({
                        "rebuilt": report.rebuilt.as_ref().map(|r| json!({
                            "meetings": r.meetings,
                            "people": r.people,
                            "skipped": r.skipped.len(),
                        })),
                        "meetings": report.meetings,
                        "people": report.people,
                        "removed": report.removed,
                        // Unreadable files stay pending; counted, never named.
                        "skipped": report.skipped.len(),
                    }))
                };
                let health = Index::health(lib.root())?;
                (
                    "index",
                    json!({
                        "repaired": repaired,
                        "database": health.database,
                        "pending": {
                            "rebuild": health.rebuild,
                            "meetings": health.meetings,
                            "people": health.people,
                        },
                    }),
                )
            }
        };
        scopes.insert(name.into(), value);
    }
    let agent_notified = rechecks.then(|| super::kick_recheck(ctx));
    super::daemon::emit(
        ctx,
        json!({
            "dry_run": dry_run,
            "scopes": scopes,
            "agent_notified": agent_notified,
        }),
    )
}

fn directory_bytes(path: &std::path::Path) -> std::io::Result<u64> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut total = 0u64;
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            total = total.saturating_add(directory_bytes(&entry.path())?);
        } else if kind.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::fakes::FakeProcessProbe;

    #[test]
    fn an_unknown_check_never_passes() {
        let unknown = Check::optional("playback", State::Unknown, "synthetic");
        assert_ne!(unknown.state, State::Ok);
        assert_eq!(unknown.state.as_str(), "unknown");
        assert_eq!(State::of(true), State::Ok);
        assert_eq!(State::of(false), State::Problem);
    }

    fn capability(availability: Availability, permission: Permission) -> Capability {
        Capability {
            name: CapabilityName::Microphone,
            availability,
            permission,
            runtime: Runtime::NotChecked,
        }
    }

    #[test]
    fn a_capability_passes_only_when_present_and_permitted() {
        let state = |a, p| capability_state(&capability(a, p));
        assert_eq!(
            state(Availability::Found, Permission::NotApplicable),
            State::Ok
        );
        assert_eq!(
            state(Availability::Reachable, Permission::Granted),
            State::Ok
        );
        assert_eq!(
            state(Availability::NotFound, Permission::NotApplicable),
            State::Problem
        );
        assert_eq!(
            state(Availability::BuiltIn, Permission::Denied),
            State::Problem
        );
        // A system without a grant state, or another process's grant, is
        // never proof.
        assert_eq!(
            state(Availability::Reachable, Permission::NotExposed),
            State::Unknown
        );
        assert_eq!(
            state(Availability::BuiltIn, Permission::Unknown),
            State::Unknown
        );
        assert_eq!(
            state(Availability::Unknown, Permission::Granted),
            State::Unknown
        );

        // Capture without an agent: unknown, never passing.
        let checks = capability_checks(&Agent::NotRunning, &[]);
        let capture = checks.iter().find(|c| c.name == "capture").unwrap();
        assert_eq!(capture.state, State::Unknown);
    }

    #[test]
    fn checking_a_library_creates_nothing_and_finds_nothing_pending() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        std::fs::create_dir_all(root.join("people")).unwrap();
        let data = dir.path().join("data");
        let lib = Library::open_existing(&root).unwrap();
        let probe = FakeProcessProbe::new(std::process::id());
        let before = std::fs::read_dir(&root).unwrap().count();
        let check = check_library(&lib, &Config::default(), &data, &probe, true);
        assert!(check.clean, "{}", check.value);
        assert_eq!(check.value["queue"]["present"], false);
        assert_eq!(check.value["retention"]["due_deletions"], 0);
        assert_eq!(check.value["identity"]["unusable_samples"], 0);
        // No queue, index or lock was created by the read.
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), before);
        assert!(!data.exists());
    }

    /// A library with a meeting whose index was deleted, or recreated empty
    /// and incomplete, is not clean: the check names the database state and
    /// creates nothing, and the index repair rebuilds it so search finds the
    /// meeting again (never a no-op with nothing pending).
    #[test]
    fn a_missing_or_incomplete_index_is_reported_and_rebuilt_for_search() {
        use broscribe_core::MeetingId;
        use broscribe_core::index::SearchQuery;
        use broscribe_core::schema::{Meeting, Source};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        let data = dir.path().join("data");
        let lib = Library::open(&root).unwrap();
        let started = chrono::DateTime::parse_from_rfc3339("2026-11-02T10:00:00Z").unwrap();
        let mut meeting = Meeting::new(
            MeetingId::parse("m-000001").unwrap(),
            Source::Import,
            started,
        );
        meeting.title = Some("Synthetic zebra review".into());
        lib.write(|tx| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)?;
            Index::open(tx.root())?.upsert_meeting(tx, &meeting)
        })
        .unwrap();
        let probe = FakeProcessProbe::new(std::process::id());
        let check = || check_library(&lib, &Config::default(), &data, &probe, true);
        let found = || {
            Index::open_readonly(&root)
                .unwrap()
                .search_meetings(&SearchQuery {
                    text: Some("zebra".into()),
                    ..SearchQuery::default()
                })
                .unwrap()
                .into_iter()
                .map(|hit| hit.meeting_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(found(), vec![meeting.id.clone()]);
        assert!(check().clean, "{}", check().value);

        let index_dir = root.join(broscribe_core::paths::INDEX_DIR);
        for incomplete in [false, true] {
            std::fs::remove_dir_all(&index_dir).unwrap();
            if incomplete {
                Index::open(&root).unwrap();
            }
            let broken = check();
            assert!(!broken.clean, "{}", broken.value);
            let state = if incomplete { "incomplete" } else { "missing" };
            assert_eq!(broken.value["index"]["database"], state);
            assert_eq!(broken.value["index"]["rebuild"], true);
            assert!(broken.value["index"]["repair"].is_string());
            assert_eq!(index_dir.exists(), incomplete, "the check creates nothing");

            let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
            assert_eq!(report.rebuilt.map(|r| r.meetings), Some(1));
            assert_eq!(found(), vec![meeting.id.clone()]);
            let repaired = check();
            assert!(repaired.clean, "{}", repaired.value);
            assert_eq!(repaired.value["index"]["database"], "ready");
        }
    }

    #[test]
    fn enum_names_are_counted_by_their_serialized_form() {
        let counts = count_by([PruneKeep::Held, PruneKeep::Held, PruneKeep::Unreadable]);
        assert_eq!(counts["held"], 2);
        assert_eq!(counts["unreadable"], 1);
    }
}
