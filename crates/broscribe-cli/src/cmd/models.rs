//! `models list | pull | use | remove | doctor` (design §8.3).

use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use broscribe_core::config::{Config, Profile, ProviderConfig};
use broscribe_core::{Error, Library, Result};
use broscribe_models::llm::{ProviderPurpose, ProviderRegistry};
use broscribe_models::preflight::synthetic_fixture;
use broscribe_models::text_embedding::MODEL2VEC_RUNTIME;
use broscribe_models::{
    Health, ModelEntry, ModelStore, PullOptions, PullOutcome, Registry, Role, SherpaProvider,
    SpeakerEmbedder, pull,
};
use serde_json::{Value, json};

use crate::cli::ModelsCmd;
use crate::{Ctx, to_value};

/// What the first Ctrl-C does to `models pull`.
pub(super) const PULL_STOP_NOTICE: &str = "Stopping the download before its next read: what it staged is \
     removed and an earlier install is kept; press Ctrl-C again to quit now.";

/// Sets `cancel` once stdin reaches end of file or cannot be read (`models
/// pull --cancel-on-stdin-eof`: the app keeps the pipe open while the pull
/// runs and closes it to cancel). Anything written to stdin is ignored.
pub(super) fn cancel_at_stdin_eof(cancel: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 256];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        cancel.store(true, Ordering::SeqCst);
    });
}

/// Whether `entry` lists this operating system (no list: any).
fn supports_this_platform(entry: &ModelEntry) -> bool {
    entry.platforms.is_empty() || entry.platforms.iter().any(|p| p == std::env::consts::OS)
}

fn error_value(e: &Error) -> Value {
    json!({ "code": e.code(), "message": e.to_string() })
}

/// `models doctor --live`: validates `profile`'s models (the registry's
/// declared dependencies, live model included), checks each runs on this
/// platform and is installed, then loads its VAD and live model and runs
/// them over the synthetic fixture ([`SherpaProvider::live_check`]).
/// Nothing is downloaded and no microphone or recording is used; a model
/// that is missing, unsupported or fails to load or run makes `ok` false
/// with the reason, never a pass.
fn live_doctor(
    ctx: &mut Ctx<'_>,
    registry: &Registry,
    store: &ModelStore,
    data_dir: &Path,
    name: &str,
    profile: &Profile,
) -> Result<Value> {
    let needed = match profile_models(registry, profile) {
        Ok(needed) => needed,
        Err(e) => {
            return Ok(json!({
                "profile": name,
                "ok": false,
                "stage": "profile",
                "error": error_value(&e),
            }));
        }
    };
    let mut preflight = Vec::with_capacity(needed.len());
    let mut ready = true;
    let mut live_ready = true;
    for (entry, roles) in &needed {
        let supported = supports_this_platform(entry);
        let health = store.doctor(entry, false)?;
        let usable = supported && health == Health::Installed;
        ready &= usable;
        if roles.iter().any(|r| matches!(r, Role::Vad | Role::Live)) {
            live_ready &= usable;
        }
        preflight.push(json!({
            "id": entry.id,
            "roles": roles.iter().map(|r| role_name(*r)).collect::<Vec<_>>(),
            "platform_supported": supported,
            "state": health_str(&health),
        }));
    }
    let mut value = json!({ "profile": name, "preflight": preflight });
    if !live_ready {
        value["ok"] = json!(false);
        value["stage"] = json!("preflight");
        value["error"] = error_value(&Error::ModelMissing(
            "the VAD or live model is not installed for this platform; run `broscribe models pull <id>`".into(),
        ));
        return Ok(value);
    }
    ctx.note("Running the live models over the synthetic fixture…");
    let provider = SherpaProvider::new(registry.clone(), data_dir);
    match provider.live_check(profile, &synthetic_fixture()) {
        Ok(check) => {
            value["ok"] = json!(ready);
            value["check"] = to_value(&check)?;
            if !ready {
                value["stage"] = json!("preflight");
                value["error"] = error_value(&Error::ModelMissing(
                    "another model of the profile is not installed for this platform".into(),
                ));
            }
        }
        Err(e) => {
            value["ok"] = json!(false);
            value["stage"] = json!("live_check");
            value["error"] = error_value(&e);
        }
    }
    Ok(value)
}

/// Roles a profile assigns, with the active model ID (the settings `models
/// use` changes).
fn profile_roles(config: &Config) -> [(Role, &str); 4] {
    let p = &config.profile;
    [
        (Role::Vad, p.vad.as_str()),
        (Role::Asr, p.asr.as_str()),
        (Role::Diarization, p.diarization.as_str()),
        (Role::SpeakerEmbedding, p.speaker_embedding.as_str()),
    ]
}

/// Every model `profile` needs ([`Registry::profile_models`]: VAD, ASR, the
/// live-preview model, diarization and speaker embedding, validated for
/// their roles, each once), with the roles it fills there. The live model
/// is the ASR model or the one it declares; an invalid profile is an error,
/// never replaced by another or an installed model.
pub(crate) fn profile_models<'r>(
    registry: &'r Registry,
    profile: &Profile,
) -> Result<Vec<(&'r ModelEntry, Vec<Role>)>> {
    let live = registry.live_model(profile)?.id.as_str();
    let slots = [
        (Role::Vad, profile.vad.as_str()),
        (Role::Asr, profile.asr.as_str()),
        (Role::Live, live),
        (Role::Diarization, profile.diarization.as_str()),
        (Role::SpeakerEmbedding, profile.speaker_embedding.as_str()),
    ];
    Ok(registry
        .profile_models(profile)?
        .into_iter()
        .map(|entry| {
            let roles = slots
                .iter()
                .filter(|(_, id)| *id == entry.id)
                .map(|(role, _)| *role)
                .collect();
            (entry, roles)
        })
        .collect())
}

/// `roles` as their names joined with `/` (`asr/live`).
pub(crate) fn role_names(roles: &[Role]) -> String {
    roles
        .iter()
        .map(|r| role_name(*r))
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn role_name(role: Role) -> String {
    match to_value(&role) {
        Ok(Value::String(s)) => s,
        _ => format!("{role:?}"),
    }
}

fn parse_role(s: &str) -> Result<Role> {
    serde_json::from_value(json!(s.trim().replace('-', "_"))).map_err(|_| {
        Error::InvalidInput(format!(
            "unknown role {s:?} (expected vad, asr, diarization, speaker_embedding or text_embedding)"
        ))
    })
}

pub(crate) fn health_str(h: &Health) -> String {
    match h {
        Health::Installed => "installed".into(),
        Health::Missing => "missing".into(),
        Health::Corrupt { reason } => format!("corrupt: {reason}"),
    }
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// The roles `entry` fills for `config`: its settings, plus `live` when it
/// is the profile's live-preview model (`live`: the valid one, if any).
fn active_roles(config: &Config, live: Option<&str>, entry: &ModelEntry) -> Vec<String> {
    let mut roles: Vec<String> = profile_roles(config)
        .iter()
        .filter(|(_, id)| *id == entry.id)
        .map(|(r, _)| role_name(*r))
        .collect();
    if entry.id == config.profile.text_embedding && entry.has_role(Role::TextEmbedding) {
        roles.push(role_name(Role::TextEmbedding));
    }
    if live == Some(entry.id.as_str()) {
        roles.push(role_name(Role::Live));
    }
    roles
}

fn check_model_removable(config: &Config, registry: &Registry, id: &str) -> Result<()> {
    // Keep protecting a configured dependency even if a user registry edit
    // made the live profile invalid. Win32 also aliases trailing periods.
    let live = registry
        .get(&config.profile.asr)
        .filter(|asr| !asr.has_role(Role::Live))
        .and_then(|asr| asr.live_model.as_deref());
    let matches = |active: &str| {
        active == id || (std::env::consts::OS == "windows" && active.trim_end_matches('.') == id)
    };
    if profile_roles(config)
        .iter()
        .any(|(_, candidate)| matches(candidate))
        || live.is_some_and(matches)
        || matches(&config.profile.text_embedding)
    {
        return Err(Error::Service {
            code: "model_in_use",
            message: format!("Model {id} is in use. Switch to another model before deleting it."),
        });
    }
    Ok(())
}

/// Whether any speaker centroid or person voiceprint of the library was
/// computed with a model other than `model`.
fn has_other_embeddings(lib: &Library, model: &str) -> Result<bool> {
    for dir in lib.list_meeting_dirs()? {
        let m = lib.load_meeting_at(&dir)?;
        if m.speakers
            .iter()
            .filter_map(|s| s.centroid.as_ref())
            .any(|c| c.model != model)
        {
            return Ok(true);
        }
    }
    for p in lib.list_people()? {
        if lib
            .load_voiceprints(&p.id)?
            .embeddings
            .iter()
            .any(|v| v.model != model)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The embedder to re-embed the library with after `profile.speaker_embedding`
/// was set (in `config`, not saved yet) from `previous`: when the setting
/// changes, or any centroid or voiceprint is of another model (an interrupted
/// switch, or files written by an older config). Loaded before anything is
/// saved, so a missing model leaves the config untouched. `None`: nothing to do.
pub(super) fn speaker_switch_embedder(
    ctx: &Ctx<'_>,
    config: &Config,
    previous: &str,
) -> Result<Option<Box<dyn SpeakerEmbedder + Send>>> {
    let id = config.profile.speaker_embedding.as_str();
    let stale = previous != id
        || match Library::open_existing(ctx.library_path(config).0) {
            Ok(lib) => has_other_embeddings(&lib, id)?,
            Err(Error::NotFound(_)) => false,
            Err(e) => return Err(e),
        };
    if !stale {
        return Ok(None);
    }
    ctx.deps.models.load_embedder(id).map(Some)
}

/// Re-embeds every centroid and voiceprint of the library with `embedder`
/// (the model `config` now names). Idempotent.
pub(super) fn reembed_library(
    ctx: &mut Ctx<'_>,
    config: &Config,
    embedder: &dyn SpeakerEmbedder,
) -> Result<broscribe_pipeline::ReembedReport> {
    let lib = ctx.create_library(config)?;
    reembed_in(ctx, &lib, config, embedder)
}

/// [`reembed_library`] on an opened library.
fn reembed_in(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    embedder: &dyn SpeakerEmbedder,
) -> Result<broscribe_pipeline::ReembedReport> {
    ctx.note(format!(
        "Re-embedding the library with {}…",
        config.profile.speaker_embedding
    ));
    broscribe_pipeline::reembed(lib, &ctx.deps.decoder, embedder)
}

#[derive(Clone, Copy)]
struct UseOptions {
    allow_unverified_license: bool,
    require_installed: bool,
}

impl UseOptions {
    fn check_install(self, store: &ModelStore, entry: &ModelEntry) -> Result<()> {
        if self.require_installed && store.doctor(entry, false)? != Health::Installed {
            return Err(Error::Service {
                code: "model_missing",
                message: format!("Download {} before switching to it.", entry.id),
            });
        }
        Ok(())
    }
}

/// `models use text_embedding <id>`: a registry text-embedding model of the
/// model2vec runtime, or a text-embedding provider, checked as
/// `broscribe_pipeline::vectors::text_model` checks them. Nothing is loaded
/// or sent; a provider is used only once enabled with consent.
fn use_text_embedding(
    ctx: &mut Ctx<'_>,
    config: &Config,
    registry: &Registry,
    store: &ModelStore,
    data_dir: &Path,
    id: &str,
    options: UseOptions,
) -> Result<()> {
    let local = registry.get(id);
    let (installed, enabled) = if let Some(entry) = local {
        if !entry.has_role(Role::TextEmbedding) {
            return Err(Error::InvalidInput(format!(
                "model {id} is not a text-embedding model"
            )));
        }
        if entry.runtime != MODEL2VEC_RUNTIME {
            return Err(Error::Unsupported(format!(
                "text-embedding runtime {} (model {id})",
                entry.runtime
            )));
        }
        entry.check_license(options.allow_unverified_license)?;
        (
            Some(options.require_installed || store.doctor(entry, false)? == Health::Installed),
            None,
        )
    } else {
        let providers = ProviderRegistry::load(data_dir)?;
        let Some(entry) = providers.get(id) else {
            return Err(Error::InvalidInput(format!(
                "text_embedding {id} is neither a registry model nor a provider"
            )));
        };
        if entry.purpose != ProviderPurpose::TextEmbedding {
            return Err(Error::InvalidInput(format!(
                "provider {id} is not a text-embedding provider"
            )));
        }
        let enabled = config
            .provider(id)
            .is_some_and(ProviderConfig::is_consented);
        (None, Some(enabled))
    };
    let previous = config.profile.text_embedding.clone();
    let changed = previous != id;
    if changed || options.require_installed {
        // Only this role, under the data directory's config lock: settings
        // another process saved meanwhile survive. Config only: no library
        // is opened or created.
        Config::update(data_dir, |c| {
            if let Some(entry) = local {
                options.check_install(store, entry)?;
            }
            c.profile.text_embedding = id.to_owned();
            Ok(())
        })?;
    }
    // Refresh each meeting off the CLI/UI thread. A missing local model is
    // also selectable through the CLI, so it can be downloaded first later.
    let refresh_queued = match (installed, enabled) {
        (Some(true), _) | (_, Some(true)) => match ctx.library_existing(config) {
            Ok(library) => {
                use broscribe_core::queue::{Queue, VECTORS_KIND};
                let queue = Queue::open(library.root())?;
                library.write(|tx| {
                    let meetings = library.list_meeting_ids()?;
                    for meeting in &meetings {
                        let (_, input) = broscribe_pipeline::worker::vectors_job_input(meeting);
                        // Each selected model has its own request key so an
                        // older embedding job cannot swallow a new choice.
                        let key = format!("{meeting}:{id}");
                        queue.request(tx, VECTORS_KIND, &key, |_| Ok(input))?;
                    }
                    Ok(meetings.len())
                })?
            }
            Err(Error::NotFound(_)) => 0,
            Err(error) => return Err(error),
        },
        _ => 0,
    };
    let mut value = json!({
        "role": role_name(Role::TextEmbedding),
        "id": id,
        "previous": previous,
        "changed": changed,
        "kind": if installed.is_some() { "model" } else { "provider" },
        "refresh_queued": refresh_queued,
    });
    if let Some(installed) = installed {
        value["installed"] = json!(installed);
    }
    if let Some(enabled) = enabled {
        value["enabled"] = json!(enabled);
    }
    ctx.emit(value, |out| {
        if changed {
            writeln!(out, "text_embedding is now {id} (was {previous}).")?;
        } else {
            writeln!(out, "text_embedding is already {id}.")?;
        }
        if installed == Some(false) {
            writeln!(out, "Not installed yet: run `broscribe models pull {id}`.")?;
        }
        if enabled == Some(false) {
            writeln!(
                out,
                "Semantic search uses it only after `broscribe providers enable {id}`."
            )?;
        }
        Ok(())
    })
}

/// The profile setting of `role`, for the roles `models use` sets there.
fn role_slot(config: &mut Config, role: Role) -> Result<&mut String> {
    let p = &mut config.profile;
    match role {
        Role::Vad => Ok(&mut p.vad),
        Role::Asr => Ok(&mut p.asr),
        Role::Diarization => Ok(&mut p.diarization),
        Role::SpeakerEmbedding => Ok(&mut p.speaker_embedding),
        other => Err(Error::Unsupported(format!(
            "the {} role is not used in this version",
            role_name(other)
        ))),
    }
}

pub(super) fn models(ctx: &mut Ctx<'_>, action: ModelsCmd) -> Result<()> {
    let data_dir = ctx.data_dir().to_path_buf();
    let mut config = ctx.config()?;
    let registry = Registry::load(&data_dir)?;
    let store = ModelStore::new(&data_dir);
    match action {
        ModelsCmd::List => {
            // The models the active profile needs, live preview included
            // (what setup installs); an invalid profile is reported, never
            // replaced by a guessed list.
            let needed = profile_models(&registry, &config.profile);
            let live = registry
                .live_model(&config.profile)
                .ok()
                .map(|e| e.id.as_str());
            let mut rows = Vec::new();
            for e in &registry.models {
                let health = store.doctor(e, false)?;
                rows.push(json!({
                    "id": e.id,
                    "display_name": e.display_name,
                    "roles": e.roles,
                    "runtime": e.runtime,
                    "license": e.license,
                    "license_verified": e.license_verified,
                    "bytes": e.total_bytes(),
                    "download_hosts": e.files.iter().filter_map(|file| {
                        let (_, rest) = file.url.split_once("://")?;
                        let host = rest.split(['/', '?', '#']).next()?;
                        (!host.is_empty() && !host.contains('@')).then_some(host)
                    }).collect::<std::collections::BTreeSet<_>>(),
                    "platforms": e.platforms,
                    "platform_supported": supports_this_platform(e),
                    "languages": e.languages,
                    "state": health_str(&health),
                    "has_model_files": store.has_model_files(e),
                    "active_for": active_roles(&config, live, e),
                    "accepts_hotwords": e.accepts_hotwords,
                }));
            }
            // Profiles: the speech-model choices of setup step 3; `languages`
            // are the ASR model's registry languages (`*`: any).
            let profiles: Vec<Value> = registry
                .profiles
                .iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "description": p.description,
                        "asr": p.asr,
                        "languages": registry.get(&p.asr).map(|m| m.languages.clone()).unwrap_or_default(),
                        "active": p.is_active(&config.profile),
                    })
                })
                .collect();
            let mut value = json!({ "models": rows, "profiles": profiles });
            let needed = match needed {
                Ok(needed) => {
                    let ids: Vec<&str> = needed.iter().map(|(e, _)| e.id.as_str()).collect();
                    value["profile_models"] = json!(ids);
                    Ok(ids.join(", "))
                }
                Err(e) => {
                    value["profile_models_error"] = json!(e.to_string());
                    Err(e.to_string())
                }
            };
            ctx.emit(value, |out| {
                for (e, row) in registry.models.iter().zip(&rows) {
                    let roles: Vec<String> = e.roles.iter().map(|r| role_name(*r)).collect();
                    let active = active_roles(&config, live, e);
                    let active = if active.is_empty() {
                        String::new()
                    } else {
                        format!("  active: {}", active.join(", "))
                    };
                    let verify = if e.license_verified { "" } else { " (verify)" };
                    writeln!(
                        out,
                        "{}  [{}]  {}{verify}  {}  {}{active}",
                        e.id,
                        roles.join(", "),
                        e.license,
                        mib(e.total_bytes()),
                        row["state"].as_str().unwrap_or("")
                    )?;
                }
                for p in &registry.profiles {
                    let active = if p.is_active(&config.profile) {
                        "  active"
                    } else {
                        ""
                    };
                    writeln!(out, "profile {}  {}{active}", p.id, p.description)?;
                }
                match &needed {
                    Ok(ids) => writeln!(out, "active profile needs: {ids}")?,
                    Err(e) => writeln!(out, "active profile: {e}")?,
                }
                Ok(())
            })
        }
        ModelsCmd::Pull {
            id,
            allow_unverified_license,
            force,
            cancel_on_stdin_eof,
        } => {
            let entry = registry.require(&id)?;
            entry.check_license(allow_unverified_license)?;
            super::setup::model_space(&data_dir, [entry], force)?.require_disk()?;
            let mut opts = PullOptions::from_env(allow_unverified_license);
            opts.force = force;
            // `from_env` carries the pull's finite default deadline. The
            // cancel flag is checked while waiting for the model's lock,
            // before every read and before the install; a read already
            // blocked ends at the deadline or the per-read timeout. A stopped
            // pull removes what it staged and keeps an earlier install.
            let cancel = match &ctx.request_cancel {
                Some(cancel) => Arc::clone(cancel),
                None => {
                    let cancel = Arc::new(AtomicBool::new(false));
                    if cancel_on_stdin_eof {
                        cancel_at_stdin_eof(Arc::clone(&cancel));
                    }
                    super::enrich::watch_interrupts(
                        Arc::clone(&cancel),
                        PULL_STOP_NOTICE,
                        ctx.background_notes(),
                    );
                    cancel
                }
            };
            opts.cancel = Some(Arc::clone(&cancel));
            let progress_updates = ctx.request_progress.clone();
            let err = &mut *ctx.err;
            let mut last_pct = u64::MAX;
            let mut progress = |done: u64, total: u64| {
                let pct = (done * 100).checked_div(total).unwrap_or(100);
                if pct / 5 != last_pct / 5 || last_pct == u64::MAX {
                    last_pct = pct;
                    if let Some(updates) = &progress_updates {
                        updates.send_replace(Some((pct as f64 / 100.0, 1.0)));
                    }
                    let _ = writeln!(err, "{id}: {pct}% of {}", mib(total));
                }
            };
            let outcome = match pull(entry, &data_dir, &opts, &mut progress, &ctx.deps.execution) {
                Ok(outcome) => outcome,
                Err(_) if cancel.load(Ordering::SeqCst) => {
                    let kept = if store.is_installed(&id) {
                        "; the earlier install is kept"
                    } else {
                        ""
                    };
                    return Err(Error::Service {
                        code: "cancelled",
                        message: format!(
                            "the download of {id} was cancelled: nothing new was installed{kept}"
                        ),
                    });
                }
                Err(e) => return Err(e),
            };
            let state = match outcome {
                PullOutcome::AlreadyInstalled => "already_installed",
                PullOutcome::Installed => "installed",
            };
            ctx.emit(json!({ "id": id, "outcome": state }), |out| match outcome {
                PullOutcome::AlreadyInstalled => writeln!(out, "{id} is already installed."),
                PullOutcome::Installed => writeln!(out, "Installed {id}."),
            })
        }
        ModelsCmd::Use {
            role,
            id,
            allow_unverified_license,
            require_installed,
        } => {
            let options = UseOptions {
                allow_unverified_license,
                require_installed,
            };
            let role = parse_role(&role)?;
            if role == Role::TextEmbedding {
                return use_text_embedding(
                    ctx, &config, &registry, &store, &data_dir, &id, options,
                );
            }
            let entry = registry.require(&id)?;
            if !entry.has_role(role) {
                return Err(Error::InvalidInput(format!(
                    "model {id} does not have the {} role",
                    role_name(role)
                )));
            }
            entry.check_license(allow_unverified_license)?;
            let mut next = config.clone();
            let slot = role_slot(&mut next, role)?;
            let previous = std::mem::replace(slot, id.clone());
            let changed = previous != id;
            // The new embedder loads, and the install state is read, before
            // anything is saved: once the role is saved the command reports
            // it.
            let embedder = if role == Role::SpeakerEmbedding {
                speaker_switch_embedder(ctx, &next, &previous)?
            } else {
                None
            };
            let installed = require_installed || store.doctor(entry, false)? == Health::Installed;
            // The live preview runs the profile's declared live model, the
            // ASR model itself or the one it names: resolved before anything
            // is saved (a profile without a valid one is refused, never
            // saved to restart a preview that cannot load) and reported with
            // its preflight state.
            let live = if role == Role::Asr {
                let live = registry.live_model(&next.profile)?;
                if ctx.request_library.is_some() {
                    live.check_license(false)?;
                }
                Some((
                    live.id.as_str(),
                    store.doctor(live, false)? == Health::Installed,
                    supports_this_platform(live),
                ))
            } else {
                None
            };
            // Only a library that exists is re-embedded; none is created.
            let library = if embedder.is_some() {
                match ctx.library_existing(&config) {
                    Ok(lib) => Some(lib),
                    Err(Error::NotFound(_)) => None,
                    Err(e) => return Err(e),
                }
            } else {
                None
            };
            if changed || library.is_some() || require_installed {
                // Only this role, under the data directory's config lock:
                // settings another process saved meanwhile survive. Config
                // only: re-embedding below takes the library's own locks.
                config = Config::update(&data_dir, |c| {
                    options.check_install(&store, entry)?;
                    role_slot(c, role).map(|slot| *slot = id.clone())
                })?
                .0;
            }
            let reembed = embedder
                .zip(library)
                .map(|(e, lib)| reembed_in(ctx, &lib, &config, e.as_ref()))
                .transpose()
                .map_err(|e| Error::Service {
                    code: e.code(),
                    message: format!(
                        "{} is now {id}, but re-embedding the library failed ({e}); run \
                         `broscribe models use {} {id}` again to finish it",
                        role_name(role),
                        role_name(role)
                    ),
                })?;
            let thresholds = config
                .thresholds(&config.profile.speaker_embedding)
                .is_some();
            let mut value = json!({
                "role": role_name(role),
                "id": id,
                "previous": previous,
                "changed": changed,
                "installed": installed,
            });
            if role == Role::SpeakerEmbedding {
                value["thresholds_calibrated"] = json!(thresholds);
            }
            if let Some(r) = &reembed {
                value["reembed"] = to_value(r)?;
            }
            if let Some((live_id, live_installed, supported)) = live {
                value["live_model"] = json!({
                    "id": live_id,
                    "installed": live_installed,
                    "platform_supported": supported,
                });
            }
            ctx.emit(value.clone(), |out| {
                if changed {
                    writeln!(out, "{} is now {id} (was {previous}).", role_name(role))?;
                } else {
                    writeln!(out, "{} is already {id}.", role_name(role))?;
                }
                if !installed {
                    writeln!(out, "Not installed yet: run `broscribe models pull {id}`.")?;
                }
                if let Some((live_id, live_installed, supported)) = live {
                    if !supported {
                        writeln!(
                            out,
                            "The live preview needs {live_id}, which does not run on this platform: it stays unavailable."
                        )?;
                    } else if !live_installed && live_id != id {
                        writeln!(
                            out,
                            "The live preview needs {live_id}, not installed yet: run `broscribe models pull {live_id}`."
                        )?;
                    }
                }
                if let Some(r) = value.get("reembed") {
                    writeln!(out, "Re-embedding:")?;
                    super::write_value(out, r, 1)?;
                }
                if role == Role::SpeakerEmbedding && !thresholds {
                    writeln!(
                        out,
                        "No naming thresholds for {id}: speakers are only suggested until you run `broscribe identity calibrate`."
                    )?;
                }
                Ok(())
            })
        }
        ModelsCmd::Remove { id, yes } => {
            if !yes {
                return Err(Error::InvalidInput(
                    "confirm model removal with --yes".into(),
                ));
            }
            let entry = registry.require(&id)?;
            check_model_removable(&config, &registry, &id)?;
            let lock = store.lock_for_removal(entry)?;
            let (_, removed) = Config::update(&data_dir, |current| {
                check_model_removable(current, &registry, &id)?;
                match &lock {
                    Some(_) => store.remove_model_files(entry),
                    None => Ok(false),
                }
            })?;
            ctx.emit(json!({ "id": id, "removed": removed }), |out| {
                if removed {
                    writeln!(out, "Deleted the downloaded files for {id}.")
                } else {
                    writeln!(out, "{id} has no downloaded files.")
                }
            })
        }
        ModelsCmd::Doctor { live, profile } => {
            // An invalid profile (no valid live model, a model of the wrong
            // role) is a failure of its own.
            let invalid = profile_models(&registry, &config.profile)
                .err()
                .map(|e| e.to_string());
            let live_id = registry
                .live_model(&config.profile)
                .ok()
                .map(|e| e.id.as_str());
            let mut rows = Vec::new();
            let mut ok = invalid.is_none();
            for e in &registry.models {
                let active = active_roles(&config, live_id, e);
                let installed = store.is_installed(&e.id);
                if active.is_empty() && !installed {
                    continue;
                }
                if installed {
                    ctx.note(format!("Verifying {}…", e.id));
                }
                let health = store.doctor(e, true)?;
                if !active.is_empty() && health != Health::Installed {
                    ok = false;
                }
                rows.push(json!({
                    "id": e.id,
                    "active_for": active,
                    "state": health_str(&health),
                    "license_verified": e.license_verified,
                }));
            }
            let live = if live {
                // `--profile`: a registry profile applied over the active
                // settings; an unknown one is an error, never replaced.
                let (name, target) = match profile.as_deref() {
                    Some(p) => (p.to_owned(), registry.profile(p)?.apply(&config.profile)),
                    None => ("active".to_owned(), config.profile.clone()),
                };
                Some(live_doctor(
                    ctx, &registry, &store, &data_dir, &name, &target,
                )?)
            } else {
                None
            };
            if let Some(l) = &live {
                ok &= l["ok"] == json!(true);
            }
            let mut value = json!({ "ok": ok, "models": rows });
            if let Some(e) = &invalid {
                value["profile_models_error"] = json!(e);
            }
            if let Some(l) = &live {
                value["live"] = l.clone();
            }
            ctx.emit(value, |out| {
                if let Some(e) = &invalid {
                    writeln!(out, "active profile: {e}")?;
                }
                for r in &rows {
                    let active: Vec<&str> = r["active_for"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    let active = if active.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", active.join(", "))
                    };
                    writeln!(
                        out,
                        "{}{active}: {}",
                        r["id"].as_str().unwrap_or(""),
                        r["state"].as_str().unwrap_or("")
                    )?;
                }
                if let Some(l) = &live {
                    write_live(out, l)?;
                }
                if ok {
                    writeln!(out, "All active models are installed and verified.")
                } else {
                    writeln!(
                        out,
                        "Some active models are missing, corrupt or failed the live check."
                    )
                }
            })
        }
    }
}

/// Human lines of a [`live_doctor`] result.
fn write_live(out: &mut dyn std::io::Write, live: &Value) -> std::io::Result<()> {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    writeln!(out, "Live check of profile {}:", text(&live["profile"]))?;
    for m in live["preflight"].as_array().into_iter().flatten() {
        let roles: Vec<&str> = m["roles"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let platform = if m["platform_supported"] == json!(true) {
            ""
        } else {
            ", not available on this platform"
        };
        writeln!(
            out,
            "  {} ({}): {}{platform}",
            text(&m["id"]),
            roles.join(", "),
            text(&m["state"])
        )?;
    }
    if let Some(check) = live.get("check") {
        writeln!(
            out,
            "  {} and {} ran over {:.1} s of synthetic audio: loaded in {} ms, ran in {} ms",
            text(&check["vad"]),
            text(&check["live_model"]),
            check["fixture_s"].as_f64().unwrap_or_default(),
            check["load_ms"],
            check["run_ms"]
        )?;
    }
    if let Some(e) = live.get("error") {
        writeln!(out, "  failed: {}", text(&e["message"]))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use broscribe_models::fakes::FakeModelProvider;
    use broscribe_platform::fakes::{
        FakeAutostart, FakeBrowserLauncher, FakeMicCapture, FakePaths, FakeProcessProbe,
        FakeProcessTreeControl, FakeSecretStore, FakeTrustStore,
    };
    use broscribe_platform::{DecoderChain, resolve_paths};

    use super::*;
    use crate::Deps;

    fn deps(root: &Path) -> Deps {
        Deps {
            paths: Box::new(FakePaths::new(root.join("root"))),
            models: Box::new(FakeModelProvider::default()),
            decoder: DecoderChain::default(),
            process: Box::new(FakeProcessProbe::new(1)),
            mic: Box::new(FakeMicCapture::new(16_000)),
            secrets: Arc::new(FakeSecretStore::default()),
            browser: Arc::new(FakeBrowserLauncher::default()),
            agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
            autostart: Box::new(FakeAutostart::default()),
            agents: crate::AgentEnv::isolated(root),
            updates: crate::UpdateDeps::with_transport(
                Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                    root.join("install"),
                    "synthetic-test-target",
                )),
                Arc::new(broscribe_updates::FakeTransport::default()),
            ),
            // A live check starts no process and makes no request.
            execution: broscribe_models::ExecutionContext::new(
                Arc::new(FakeProcessTreeControl::default()),
                Arc::new(FakeTrustStore::default()),
            ),
            clock: crate::WallClock::System,
        }
    }

    fn live(root: &Path, registry: &Registry, profile: &Profile) -> Value {
        let deps = deps(root);
        let data = root.join("data");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let paths = resolve_paths(deps.paths.as_ref()).unwrap();
        Ctx::scoped(&deps, true, None, paths, None, &mut out, &mut err, |ctx| {
            let store = ModelStore::new(&data);
            live_doctor(ctx, registry, &store, &data, "active", profile).unwrap()
        })
    }

    #[test]
    fn live_doctor_without_installed_models_fails_in_preflight_and_downloads_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Registry::builtin().unwrap();
        let profile = Config::default().profile;
        let v = live(tmp.path(), &registry, &profile);
        assert_eq!(v["ok"], json!(false), "{v}");
        assert_eq!(v["stage"], json!("preflight"));
        assert_eq!(v["error"]["code"], json!("model_missing"));
        assert!(v.get("check").is_none());
        // Every model the profile needs is listed with its roles, from the
        // registry's own dependencies (the live model included).
        let ids: Vec<&str> = v["preflight"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        let expected: Vec<&str> = registry
            .profile_models(&profile)
            .unwrap()
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, expected);
        let live_id = registry.live_model(&profile).unwrap().id.clone();
        assert!(v["preflight"].as_array().unwrap().iter().any(|m| {
            m["id"] == json!(live_id)
                && m["roles"].as_array().unwrap().contains(&json!("live"))
                && m["state"] == json!("missing")
        }));
        assert!(!tmp.path().join("data/models").join(&live_id).exists());
    }

    #[test]
    fn live_doctor_reports_an_invalid_profile_without_loading() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Registry::builtin().unwrap();
        let profile = Profile {
            asr: "not-a-model".into(),
            ..Config::default().profile
        };
        let v = live(tmp.path(), &registry, &profile);
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["stage"], json!("profile"));
        assert!(v.get("preflight").is_none() && v.get("check").is_none());
    }

    /// `models use <role> <id>` as the CLI runs it, with `--library` on a
    /// path that does not exist; its JSON output.
    fn use_model(root: &Path, role: &str, id: &str) -> Value {
        let deps = deps(root);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let paths = resolve_paths(deps.paths.as_ref()).unwrap();
        Ctx::scoped(
            &deps,
            true,
            Some(root.join("typo")),
            paths,
            None,
            &mut out,
            &mut err,
            |ctx| {
                models(
                    ctx,
                    ModelsCmd::Use {
                        role: role.into(),
                        id: id.into(),
                        allow_unverified_license: true,
                        require_installed: false,
                    },
                )
            },
        )
        .unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    /// Choosing an ASR model that runs its live preview on another model
    /// reports that dependency and its install state, so an upgraded
    /// multilingual install learns what the preview still needs.
    #[test]
    fn models_use_asr_reports_its_uninstalled_live_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = Registry::builtin().unwrap();
        let asr = "whisper-large-v3-turbo";
        let next = Profile {
            asr: asr.into(),
            ..Config::default().profile
        };
        let live_id = registry.live_model(&next).unwrap().id.clone();
        assert_ne!(live_id, asr);
        let v = use_model(tmp.path(), "asr", asr);
        assert_eq!(v["changed"], json!(true), "{v}");
        assert_eq!(v["live_model"]["id"], json!(live_id), "{v}");
        assert_eq!(v["live_model"]["installed"], json!(false), "{v}");
        assert!(v["live_model"]["platform_supported"].is_boolean(), "{v}");
    }

    /// `models use` only edits the config: a wrong `--library` stays absent,
    /// for a config-only role and for a speaker-model switch alike.
    #[test]
    fn models_use_never_creates_a_library_on_a_wrong_path() {
        let tmp = tempfile::tempdir().unwrap();
        let v = use_model(tmp.path(), "asr", "whisper-large-v3-turbo");
        assert_eq!(v["changed"], json!(true), "{v}");
        let v = use_model(tmp.path(), "speaker_embedding", "nemo-titanet-small");
        assert_eq!(v["changed"], json!(true), "{v}");
        assert!(v.get("reembed").is_none(), "{v}");
        assert!(!tmp.path().join("typo").exists());
        let data = resolve_paths(&FakePaths::new(tmp.path().join("root")))
            .unwrap()
            .data_dir;
        let saved = Config::load(&data).unwrap().profile;
        assert_eq!(
            (saved.asr.as_str(), saved.speaker_embedding.as_str()),
            ("whisper-large-v3-turbo", "nemo-titanet-small")
        );
    }
}
