//! Whole-installation removal. Discovery and safety checks precede all writes.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use broscribe_core::{
    Error, Result, config::Config, lock::DaemonStartupLock, maintenance::profile_id, queue::Queue,
};
use broscribe_integrations::{IntegrationId, Integrations, IntegrationsConfig, RevocationOutcome};
use broscribe_ipc::{Call, DaemonState};
use broscribe_platform::uninstall::{
    CredentialProfile, DeferredDefault, DeferredProfile, DeferredUpdateCleanup, DirectoryIdentity,
    Layout, ReleaseRemovalCheck, RemovalProtection, path_within,
};
use broscribe_platform::{
    paths::{ENV_DATA_DIR, ENV_LIBRARY},
    profile_cleanup, same_path, stable_path,
};
use broscribe_updates::RemovalInventory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{agents, daemon, integrations};
use crate::{Ctx, cli::UninstallArgs};

const UNINSTALL_RETRY_DETAIL: &str = "Kept so you can correct the failed steps and retry.";

#[derive(Serialize)]
struct Step {
    action: String,
    path: Option<String>,
    status: &'static str,
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
}

#[derive(Default)]
struct Report(Vec<Step>);

impl Report {
    fn add(
        &mut self,
        action: &str,
        path: Option<&Path>,
        status: &'static str,
        detail: Option<String>,
    ) {
        self.0.push(Step {
            action: action.into(),
            path: path.map(|path| path.display().to_string()),
            status,
            detail,
            code: None,
        });
    }

    fn outcome(&mut self, action: &str, path: Option<&Path>, result: Result<bool>) {
        match result {
            Ok(true) => self.add(action, path, "done", None),
            Ok(false) => self.add(action, path, "skipped", Some("Nothing to remove.".into())),
            Err(error) => {
                self.add(action, path, "failed", Some(error.to_string()));
                self.0.last_mut().expect("failed step").code = Some(error.code());
            }
        }
    }

    fn list_kept(&mut self, root: &Path, entries: &[PathBuf]) {
        let mut parents = std::collections::BTreeSet::new();
        for path in entries {
            if !self.0.iter().any(|step| {
                step.action == "Keep unrecognized entry" && step.path.as_deref() == path.to_str()
            }) {
                self.add(
                    "Keep unrecognized entry",
                    Some(path),
                    "kept",
                    Some("not created by BroScribe".into()),
                );
            }
            parents.extend(
                path.ancestors()
                    .skip(1)
                    .take_while(|parent| parent.starts_with(root)),
            );
        }
        for path in parents {
            if !self.0.iter().any(|step| {
                step.action == "Keep directory containing an unrecognized entry"
                    && step.path.as_deref() == path.to_str()
            }) {
                self.add(
                    "Keep directory containing an unrecognized entry",
                    Some(path),
                    "kept",
                    None,
                );
            }
        }
    }

    fn warn_update_state(&mut self, path: &Path, error: Error) {
        if !self.update_state_unusable(path) {
            self.add(
                "Read secondary update state",
                Some(path),
                "warning",
                Some(error.to_string()),
            );
        }
    }

    fn update_state_unusable(&self, path: &Path) -> bool {
        self.0.iter().any(|step| {
            step.action == "Read secondary update state" && step.path.as_deref() == path.to_str()
        })
    }

    fn blocked(&self) -> bool {
        self.0.iter().any(|step| step.status == "failed")
    }
}

struct Profile {
    data: PathBuf,
    pending: PendingProfile,
    settled_receipt: Option<PathBuf>,
    skip_disk: bool,
    library: PathBuf,
    relative_config_library: bool,
    libraries: Vec<PathBuf>,
    caches: Vec<PathBuf>,
    runtime: Vec<PathBuf>,
    select_library: bool,
    exists: bool,
    agent: Option<DaemonState>,
    job: broscribe_platform::LoginJob,
}

fn error(message: impl Into<String>) -> Error {
    Error::InvalidInput(message.into())
}

fn present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

const PENDING_PROFILES: &str = "uninstall-pending.json";

#[derive(Clone, Default, Deserialize, Serialize)]
struct PendingUninstall {
    created_default: Option<DirectoryIdentity>,
    #[serde(default)]
    default_libraries: Vec<PathBuf>,
    profiles: Vec<PendingProfile>,
}

#[derive(Clone, Deserialize, Serialize)]
struct PendingProfile {
    data: PathBuf,
    identity: Option<DirectoryIdentity>,
    #[serde(default)]
    marker: Option<String>,
    namespace: Option<String>,
    receipt: Option<PathBuf>,
    #[serde(default, alias = "registered_libraries")]
    libraries: Vec<PathBuf>,
}

impl PendingUninstall {
    fn read(default: &Path) -> Result<Self> {
        let path = default.join(PENDING_PROFILES);
        let Some(bytes) = bounded_file(&path, 1024 * 1024)? else {
            return Ok(Self::default());
        };
        // Older builds only stored path names. They can still guide credential
        // cleanup, but cannot authorize writes to any existing directory.
        let state = serde_json::from_slice::<Self>(&bytes)
            .or_else(|_| {
                serde_json::from_slice::<Vec<PathBuf>>(&bytes).map(|paths| Self {
                    created_default: None,
                    default_libraries: Vec::new(),
                    profiles: paths
                        .into_iter()
                        .map(|data| PendingProfile {
                            data,
                            identity: None,
                            marker: None,
                            namespace: None,
                            receipt: None,
                            libraries: Vec::new(),
                        })
                        .collect(),
                })
            })
            .map_err(|_| error("The pending uninstall profile list is invalid."))?;
        if state
            .default_libraries
            .iter()
            .any(|path| !path.is_absolute())
            || state.profiles.iter().any(|profile| {
                !profile.data.is_absolute()
                    || profile
                        .receipt
                        .as_ref()
                        .is_some_and(|path| !path.is_absolute())
                    || profile.libraries.iter().any(|path| !path.is_absolute())
            })
        {
            return Err(error(
                "The pending uninstall profile list contains a relative path.",
            ));
        }
        Ok(state)
    }

    fn save(&self, default: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)
            .map_err(|_| error("Could not encode the pending uninstall profile list."))?;
        if bytes.len() > 1024 * 1024 {
            return Err(error("The pending uninstall profile list is too large."));
        }
        broscribe_core::fsutil::write_atomic_with_mode(
            &default.join(PENDING_PROFILES),
            &bytes,
            Some(0o600),
        )
    }
}

fn bounded_file(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io_at(path, e)),
    };
    if !metadata.is_file()
        || broscribe_platform::release_layout::is_reparse(&metadata)
        || metadata.len() > limit
    {
        return Err(error(format!(
            "{} is not a regular, bounded file.",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|e| Error::io_at(path, e))?;
    if bytes.len() as u64 > limit {
        return Err(error(format!("{} is too large.", path.display())));
    }
    Ok(Some(bytes))
}

pub(super) struct PendingProtection {
    pub dirs: Vec<PathBuf>,
    pub libraries: Vec<PathBuf>,
}

pub(super) fn pending_protection(default: &Path) -> Result<PendingProtection> {
    let state = PendingUninstall::read(default)?;
    let mut protection = PendingProtection {
        dirs: Vec::new(),
        libraries: state.default_libraries,
    };
    for profile in state.profiles {
        protection.dirs.push(profile.data);
        protection.libraries.extend(profile.libraries);
    }
    Ok(protection)
}

fn paths_for(
    ctx: &Ctx<'_>,
    args: &UninstallArgs,
    state: &PendingUninstall,
    report: &mut Report,
) -> Vec<Profile> {
    let platform = &ctx.deps.agents.uninstall;
    let default = stable_path(&ctx.deps.paths.data_dir());
    let current = stable_path(ctx.data_dir());
    let mut paths = BTreeMap::from([
        (default.clone(), default == current),
        (current.clone(), true),
    ]);
    for path in &args.data_dir {
        paths.insert(stable_path(path), true);
    }
    let mut registered = BTreeMap::<PathBuf, Vec<PathBuf>>::new();
    match ctx.deps.autostart.registered_profiles() {
        Ok(discovered) => {
            for profile in discovered {
                let path = stable_path(&profile.data_dir);
                let libraries = registered.entry(path.clone()).or_default();
                if let Some(library) = profile.library {
                    libraries.push(stable_path(&library));
                    libraries.push(library);
                }
                paths.entry(path).or_insert(false);
            }
        }
        Err(error) => report.outcome("Discover login profiles", None, Err(error)),
    }
    match platform.shortcut_profiles() {
        Ok(discovered) => {
            for path in discovered {
                paths.entry(stable_path(&path)).or_insert(false);
            }
        }
        Err(error) => report.outcome("Discover shortcut profiles", None, Err(error)),
    }
    let pending: BTreeMap<_, _> = state.profiles.iter().map(|p| (p.data.clone(), p)).collect();
    for path in pending.keys() {
        let resolved = stable_path(path);
        if resolved != *path && resolved != default {
            paths.remove(&resolved);
        }
        paths.entry(path.clone()).or_insert(false);
    }
    let mut profiles = Vec::new();
    for (data, selected) in paths {
        let retry = pending.get(&data).copied();
        if retry.is_none() && protected_user_root(ctx, &data) {
            report.outcome(
                "Inspect profile",
                Some(&data),
                Err(error(
                    "The profile path contains a protected user location.",
                )),
            );
            continue;
        }
        let mut libraries = registered.get(&data).cloned().unwrap_or_default();
        if data == default {
            libraries.extend(state.default_libraries.iter().cloned());
        }
        let options = ProfileOptions {
            selected,
            explicit: data == current || args.data_dir.iter().any(|path| stable_path(path) == data),
            removes_data_or_library: args.remove_data || args.remove_library,
            registered_libraries: libraries,
            created_default: state.created_default,
            retry,
        };
        match profile(ctx, data.clone(), &default, &current, options, report) {
            Ok(profile) => profiles.push(profile),
            Err(error) => report.outcome("Inspect profile", Some(&data), Err(error)),
        }
    }
    // Stop the live shared job first. The guarded stop otherwise refuses to
    // stop a direct agent while a different profile owns the login job.
    profiles.sort_by_key(|profile| !registered.contains_key(&profile.data));
    profiles
}

struct ProfileOptions<'a> {
    selected: bool,
    explicit: bool,
    removes_data_or_library: bool,
    registered_libraries: Vec<PathBuf>,
    created_default: Option<DirectoryIdentity>,
    retry: Option<&'a PendingProfile>,
}

enum Receipt {
    Done,
    Waiting,
    Retry,
}

enum PreviousHelper {
    Settled(PathBuf),
    Obsolete(PathBuf),
}

fn deferred_receipt(path: &Path) -> Result<Receipt> {
    let Some(bytes) = bounded_file(path, 32 * 1024)? else {
        return Ok(Receipt::Retry);
    };
    if bytes.starts_with(b"Done. Removed ") {
        return Ok(Receipt::Done);
    }
    if bytes.starts_with(b"Failed. ") {
        return Ok(Receipt::Retry);
    }
    let modified = std::fs::metadata(path)?.modified()?;
    let stale = modified
        .elapsed()
        .is_ok_and(|age| age > std::time::Duration::from_secs(16 * 60));
    Ok(if stale {
        Receipt::Retry
    } else {
        Receipt::Waiting
    })
}

// Keep the reference beside the native default data folder so an after-exit
// helper can remove the entire folder without losing its own receipt.
fn helper_reference(ctx: &Ctx<'_>) -> PathBuf {
    ctx.deps
        .paths
        .data_dir()
        .with_extension("uninstall-helper.json")
}

fn save_helper_reference(reference: &Path, receipt: &Path) -> Result<PathBuf> {
    // Resolve the parent without following the reference itself.
    let parent = stable_path(reference.parent().expect("the reference has a parent"));
    let reference = parent.join(reference.file_name().expect("the reference has a name"));
    std::fs::create_dir_all(&parent).map_err(|e| Error::io_at(&parent, e))?;
    let mut file =
        tempfile::NamedTempFile::new_in(&parent).map_err(|e| Error::io_at(&parent, e))?;
    serde_json::to_writer(file.as_file_mut(), &stable_path(receipt)).map_err(|e| {
        error(format!(
            "Could not encode the uninstall helper receipt: {e}"
        ))
    })?;
    file.as_file()
        .sync_all()
        .map_err(|e| Error::io_at(&reference, e))?;
    file.persist_noclobber(&reference)
        .map_err(|e| Error::io_at(&reference, e.error))?;
    Ok(reference)
}

fn read_helper_reference(reference: &Path) -> Result<Option<PathBuf>> {
    let Some(bytes) = bounded_file(reference, 256 * 1024)? else {
        return Ok(None);
    };
    let receipt = serde_json::from_slice::<PathBuf>(&bytes)
        .map_err(|_| error("The uninstall helper receipt reference is invalid."))?;
    Ok(Some(receipt))
}

fn inspect_previous_helper(
    temp: &Path,
    reference: &Path,
    report: &mut Report,
) -> Result<Option<PreviousHelper>> {
    let check_reference = |reason: &str| {
        error(format!(
            "{reason} After checking that the after-exit helper has finished, delete {} and retry.",
            reference.display()
        ))
    };
    let Some(receipt) =
        read_helper_reference(reference).map_err(|e| check_reference(&e.to_string()))?
    else {
        return Ok(None);
    };
    let run = recorded_helper_run(&receipt).ok_or_else(|| {
        check_reference(&format!(
            "Invalid uninstall helper receipt path {}.",
            receipt.display()
        ))
    })?;
    if !present(run).map_err(|e| check_reference(&e.to_string()))? {
        report.add(
            "Inspect previous after-exit helper",
            Some(reference),
            "warning",
            Some(format!("The helper folder {} is missing. Applying this plan clears the obsolete reference.", run.display())),
        );
        return Ok(Some(PreviousHelper::Obsolete(receipt)));
    }
    if helper_run(temp, &receipt).is_none() {
        return Err(check_reference(&format!(
            "The uninstall helper folder {} is outside this user's temporary folder.",
            run.display()
        )));
    }
    if !std::fs::symlink_metadata(run)
        .is_ok_and(|meta| meta.is_dir() && !broscribe_platform::release_layout::is_reparse(&meta))
    {
        return Err(check_reference(&format!(
            "The uninstall helper folder {} changed.",
            run.display()
        )));
    }
    inspect_helper_receipt(&receipt, report)?;
    Ok(Some(PreviousHelper::Settled(receipt)))
}

fn clear_helper_reference(reference: &Path, receipt: &Path) -> Result<bool> {
    if read_helper_reference(reference)?.as_deref() == Some(receipt) {
        remove_file(reference)
    } else {
        Ok(false)
    }
}

fn inspect_helper_receipt(receipt: &Path, report: &mut Report) -> Result<Receipt> {
    let status = deferred_receipt(receipt)?;
    if matches!(status, Receipt::Waiting) {
        return Err(error(format!(
            "The after-exit helper is still running. See {} and retry when it finishes.",
            receipt.display()
        )));
    }
    if !report.0.iter().any(|step| {
        step.action == "Read after-exit cleanup details" && step.path.as_deref() == receipt.to_str()
    }) && let Ok(Some(detail)) = bounded_file(&receipt_details_path(receipt), 64 * 1024)
    {
        report.add(
            "Read after-exit cleanup details",
            Some(receipt),
            "done",
            Some(String::from_utf8_lossy(&detail).into_owned()),
        );
    }
    Ok(status)
}

fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_file() && !broscribe_platform::release_layout::is_reparse(&meta))
}

fn profile_evidence(data: &Path) -> Result<bool> {
    let config = Config::path(data);
    let written_config = if let Some(bytes) = bounded_file(&config, 1024 * 1024)? {
        let keys = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| toml::from_str::<toml::Table>(text).ok());
        keys.is_some_and(|keys| {
            [
                "consent_reminder",
                "audio_setup_overrides",
                "speaker_grouping_presets",
                "auto_enrich",
            ]
            .iter()
            .any(|name| keys.contains_key(*name))
                || keys
                    .get("profile")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|profile| {
                        ["asr", "diarization", "speaker_embedding"]
                            .iter()
                            .any(|key| profile.contains_key(*key))
                    })
        })
    } else {
        false
    };
    let known_run = std::fs::symlink_metadata(data.join("run"))
        .is_ok_and(|meta| meta.is_dir() && !broscribe_platform::release_layout::is_reparse(&meta))
        && ["broscribed.pid", "app.lock"]
            .iter()
            .any(|name| regular_file(&data.join("run").join(name)));
    Ok(
        written_config || known_run || regular_file(&data.join("ui-state.json")) || {
            let (pending, orphan) = journal_work(data, true)?;
            pending || orphan.is_some()
        },
    )
}

fn profile(
    ctx: &Ctx<'_>,
    data: PathBuf,
    default: &Path,
    current: &Path,
    options: ProfileOptions<'_>,
    report: &mut Report,
) -> Result<Profile> {
    let platform = ctx.deps.agents.uninstall.as_ref();
    let config_base = if case_insensitive(ctx) {
        super::maintenance_config_base()
    } else {
        None
    };
    let config_path = |path: &Path| match &config_base {
        Some(base) => base.join(path),
        None => path.to_path_buf(),
    };
    let mut pending = options.retry.cloned().unwrap_or_else(|| PendingProfile {
        data: data.clone(),
        identity: None,
        marker: None,
        namespace: None,
        receipt: None,
        libraries: Vec::new(),
    });
    pending.libraries.extend(options.registered_libraries);
    if data == current {
        pending.libraries.extend(
            ctx.paths
                .library_from_env
                .iter()
                .map(|path| stable_path(&config_path(path))),
        );
    }
    pending.libraries.sort_unstable();
    pending.libraries.dedup();
    if pending.namespace.is_none() {
        pending.namespace = Some(platform.credential_namespace(&data));
    }
    let identity = platform.directory_identity(&data)?;
    let marker = if identity.is_some() {
        profile_id(&data)?
    } else {
        None
    };
    let exists = identity.is_some()
        && !(data == default && options.created_default == identity && marker.is_none());
    let mut skip = None;
    let mut settled_receipt = None;
    if options.retry.is_some() {
        if let Some(receipt) = &pending.receipt {
            if matches!(inspect_helper_receipt(receipt, report)?, Receipt::Done) {
                skip = Some("The after-exit helper finished profile cleanup.");
            }
            settled_receipt = pending.receipt.take();
        }
        if skip.is_none()
            && (!exists
                || identity != pending.identity
                || marker != pending.marker
                || protected_user_root(ctx, &data))
        {
            skip = Some("The recorded profile directory or marker is missing or has changed.");
        }
    } else {
        if identity.is_none() && present(&data)? {
            return Err(error("The profile path is not a regular directory."));
        }
        pending.identity = identity;
        pending.marker = marker.clone();
    }
    if skip.is_none() && exists && marker.is_none() {
        if !options.explicit {
            skip = Some(
                "No BroScribe profile marker was found. Only credentials are eligible for cleanup.",
            );
        } else if data != default && !profile_evidence(&data)? {
            return Err(error(
                "This folder has no BroScribe configuration or profile files.",
            ));
        }
    }
    if let Some(detail) = skip {
        let config_library = if marker.is_none()
            && exists
            && let Ok(Some(bytes)) = bounded_file(&Config::path(&data), 1024 * 1024)
        {
            std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| toml::from_str::<Config>(text).ok())
                .and_then(|config| config.library)
        } else {
            None
        };
        if let Some(library) = &config_library {
            pending.libraries.push(stable_path(&config_path(library)));
        }
        report.add(
            "Clean up profile files",
            Some(&data),
            "skipped",
            Some(detail.into()),
        );
        return Ok(Profile {
            skip_disk: true,
            libraries: pending.libraries.clone(),
            pending,
            settled_receipt,
            data,
            library: ctx.paths.default_library_dir.clone(),
            relative_config_library: config_library.is_some_and(|path| !path.is_absolute()),
            caches: Vec::new(),
            runtime: Vec::new(),
            select_library: false,
            exists,
            agent: None,
            job: broscribe_platform::LoginJob::Absent,
        });
    }
    let cfg = if exists {
        let config = Config::path(&data);
        if present(&config)? && !regular_file(&config) {
            return Err(error("The profile configuration is not a regular file."));
        }
        Config::load(&data)?
    } else {
        Config::default()
    };
    let (library, source) = if data == current {
        ctx.configured_library_path(&cfg)
    } else if let Some(library) = &cfg.library {
        (library.clone(), crate::LibrarySource::Config)
    } else {
        (
            ctx.paths.default_library_dir.clone(),
            crate::LibrarySource::Default,
        )
    };
    if options.removes_data_or_library
        && source == crate::LibrarySource::Default
        && !library.is_absolute()
    {
        return Err(error(format!(
            "The default library path ({}) is relative. Correct the platform's home folder or select an absolute library path before removing data or libraries and preview again with --dry-run.",
            library.display(),
        )));
    }
    let relative_config_library = cfg.library.as_ref().is_some_and(|path| !path.is_absolute());
    let library = config_path(&library);
    let agent = daemon::optional_call(&data, Call::Status)?
        .map(serde_json::from_value::<DaemonState>)
        .transpose()
        .map_err(|_| error("BroScribe Agent returned an unreadable status"))?;
    if agent.is_none() && daemon::agent_instance_held(&data, ctx.deps.agent_instance.as_ref())? {
        return Err(error(
            "BroScribe Agent holds this profile and did not answer",
        ));
    }
    let mut libraries = vec![library.clone()];
    libraries.extend(cfg.library.as_deref().map(config_path));
    libraries.extend(pending.libraries.iter().cloned());
    if let Some(state) = &agent {
        let live = Path::new(&state.library);
        if !live.is_absolute() {
            return Err(error(
                "BroScribe Agent did not report an absolute library path",
            ));
        }
        libraries.push(live.to_path_buf());
    }
    // Retain the original spelling and its current target for the safety checks.
    libraries.extend(libraries.iter().map(|p| stable_path(p)).collect::<Vec<_>>());
    // Keep every absolute spelling or resolved target across retries.
    pending
        .libraries
        .extend(libraries.iter().filter(|path| path.is_absolute()).cloned());
    pending.libraries.sort_unstable();
    pending.libraries.dedup();
    let library = stable_path(&library);
    let mut caches = Vec::new();
    if exists {
        caches.push(data.join("cache"));
    }
    if data == default {
        caches.push(ctx.deps.paths.cache_dir());
    }
    let runtime = ctx.deps.agents.uninstall.runtime_paths(&data)?;
    let job = ctx.deps.autostart.login_job(&data);
    Ok(Profile {
        skip_disk: false,
        pending,
        settled_receipt,
        data,
        library,
        relative_config_library,
        libraries,
        caches,
        runtime,
        select_library: options.selected,
        exists,
        agent,
        job,
    })
}

fn journal_entries(path: &Path) -> Result<Option<std::fs::ReadDir>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if broscribe_platform::release_layout::is_reparse(&meta) => Err(error(format!(
            "Cannot verify journals at {} because the path is a link.",
            path.display()
        ))),
        Ok(meta) if meta.is_dir() => std::fs::read_dir(path)
            .map(Some)
            .map_err(|e| Error::io_at(path, e)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

fn journal_work(data: &Path, inspect_chunks: bool) -> Result<(bool, Option<PathBuf>)> {
    use broscribe_core::lifecycle;
    let root = lifecycle::journal_root(data);
    let Some(sessions) = journal_entries(&root)? else {
        return Ok((false, None));
    };
    let discard = root.join(".discard");
    let mut pending =
        journal_entries(&discard)?.is_some() && !lifecycle::discard_intents(&root)?.is_empty();
    let mut orphan = None;
    for entry in sessions {
        let entry = entry.map_err(|e| Error::io_at(&root, e))?;
        let session = entry.path();
        if session == discard || journal_entries(&session)?.is_none() {
            continue;
        }
        if regular_file(&session.join(lifecycle::SESSION_MANIFEST)) {
            pending = true;
            continue;
        }
        if !inspect_chunks || orphan.is_some() {
            continue;
        }
        for track in ["mic", "system"] {
            let track = session.join(track);
            for entry in journal_entries(&track)?.into_iter().flatten() {
                let chunk = entry.map_err(|e| Error::io_at(&track, e))?.path();
                if chunk
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| profile_cleanup::is_journal_chunk(name, cfg!(windows)))
                    && regular_file(&chunk)
                {
                    orphan = Some(session.clone());
                    break;
                }
            }
        }
    }
    Ok((pending, orphan))
}

fn check_journals(data: &Path, args: &UninstallArgs) -> Result<()> {
    let (pending, orphan) = journal_work(data, args.remove_data && !args.discard_unsaved)?;
    if args.discard_unsaved {
        return Ok(());
    }
    if let Some(folder) = orphan {
        return Err(error(format!(
            "Unsaved or processing recordings may remain at {}. Move this folder away, or use --remove-data --discard-unsaved to delete its BroScribe journal files.",
            folder.display()
        )));
    }
    if pending {
        return Err(error(
            "This profile holds unsaved or processing recordings. Save them first, or use --remove-data --discard-unsaved to permanently delete those journals.",
        ));
    }
    Ok(())
}

fn check_profile(ctx: &Ctx<'_>, args: &UninstallArgs, profile: &Profile) -> Result<()> {
    use broscribe_platform::LoginJob;
    if matches!(
        profile.job,
        LoginJob::Unknown
            | LoginJob::Registered {
                serves_profile: None
            }
            | LoginJob::Idle {
                serves_profile: None
            }
    ) {
        return Err(Error::Service {
            code: "login_job_unverified",
            message: "The BroScribe Agent login registration could not be verified.".into(),
        });
    }
    if let Some(state) = &profile.agent
        && (state.recording.is_some() || state.jobs.running.is_some() || state.install.is_some())
    {
        return Err(error(
            "Recording, processing or update work is active. Let it finish before uninstalling.",
        ));
    }
    check_journals(&profile.data, args)?;
    if let Some(queue) = Queue::open_existing(&profile.library)?
        && queue
            .snapshot()?
            .jobs
            .iter()
            .any(|job| queue.is_held(job, ctx.deps.process.as_ref()))
    {
        return Err(error(
            "The library has a running job. Let it finish before uninstalling.",
        ));
    }
    Ok(())
}

fn case_insensitive(ctx: &Ctx<'_>) -> bool {
    ctx.deps.agents.uninstall.defers_settings_cleanup()
}

fn protected_user_root(ctx: &Ctx<'_>, path: &Path) -> bool {
    let ignore_case = case_insensitive(ctx);
    path.parent().is_none()
        || ctx
            .deps
            .agents
            .home
            .as_deref()
            .is_some_and(|home| path_within(home, path, ignore_case))
        || path_within(&ctx.deps.paths.documents_dir(), path, ignore_case)
}

#[derive(Clone, Copy)]
enum Removal<'a> {
    Installation,
    Data(&'a Profile),
    Library(&'a Profile),
    Cache(&'a Profile),
    Runtime(&'a Profile),
}

impl Removal<'_> {
    fn protection(
        self,
        ctx: &Ctx<'_>,
        args: &UninstallArgs,
        profiles: &[Profile],
        target: &Path,
    ) -> RemovalProtection {
        let ignore_case = case_insensitive(ctx);
        let within = |path: &Path, parent: &Path| path_within(path, parent, ignore_case);
        let same_path = |a, b| within(a, b) && within(b, a);
        let selected = |profile: &Profile| args.remove_library && profile.select_library;
        let default = &ctx.paths.default_library_dir;
        let mut protection = RemovalProtection {
            contains: std::iter::once(ctx.deps.paths.documents_dir())
                .chain(ctx.deps.agents.home.iter().cloned())
                .collect(),
            overlap: Vec::new(),
        };

        let libraries = std::iter::once(default).filter(|path| {
            !profiles.iter().any(|p| {
                selected(p)
                    && same_path(path, p.library.as_path())
                    && match self {
                        Self::Library(_) => same_path(path, target),
                        Self::Data(_) => within(path, target) && !same_path(path, target),
                        _ => false,
                    }
            })
        });
        protection.overlap.extend(libraries.cloned());

        for profile in profiles {
            let owns = |owner: &Profile| owner.data == profile.data;
            if !matches!(self, Self::Data(owner) if owns(owner)) {
                let paths = if matches!(self, Self::Cache(owner) | Self::Runtime(owner) | Self::Library(owner) if owns(owner))
                {
                    &mut protection.contains
                } else {
                    &mut protection.overlap
                };
                paths.push(profile.data.clone());
            }

            for library in &profile.libraries {
                let explicitly_removed = selected(profile) && same_path(library, &profile.library);
                let permitted = match self {
                    Self::Library(_) => {
                        same_path(library, target)
                            && (explicitly_removed || !profile.exists && profile.agent.is_none())
                    }
                    Self::Data(_) => {
                        explicitly_removed && within(library, target) && !same_path(library, target)
                    }
                    _ => false,
                };
                if !permitted {
                    protection.overlap.push(library.clone());
                }
            }

            for cache in &profile.caches {
                let permitted = match self {
                    Self::Cache(owner) => owns(owner) && same_path(cache, target),
                    Self::Data(owner) => {
                        owns(owner) && within(cache, target) && !same_path(cache, target)
                    }
                    _ => false,
                };
                if !permitted {
                    protection.overlap.push(cache.clone());
                }
            }
        }
        protection
    }

    fn validate(
        self,
        ctx: &Ctx<'_>,
        args: &UninstallArgs,
        profiles: &[Profile],
        target: &Path,
    ) -> Result<()> {
        if protected_user_root(ctx, &stable_path(target))
            || self
                .protection(ctx, args, profiles, target)
                .conflicts(target, case_insensitive(ctx))
        {
            return Err(error(match self {
                Self::Library(_) => {
                    "Another profile uses this library, or removal overlaps a protected BroScribe or user location."
                }
                _ => {
                    "The removal path overlaps BroScribe data, a library, a cache or a protected user location."
                }
            }));
        }
        Ok(())
    }
}

const DATA_SEPARATE: &[&str] = &["cache", "run", "update-runner", "app-backups", "updates"];
const DATA_WITH_STATE: &[&str] = &[
    "cache",
    "run",
    "update-runner",
    "app-backups",
    "updates",
    ".broscribe-profile",
    "uninstall-pending.json",
    "config.toml",
];

fn cleanup_report(
    report: &mut Report,
    action: &str,
    path: &Path,
    result: Result<profile_cleanup::Cleanup>,
) {
    match result {
        Ok(cleanup) => {
            if !cleanup.kept.is_empty() {
                report.list_kept(path, &cleanup.kept);
                report.add(
                    action,
                    Some(path),
                    "partial",
                    Some("Entries remain at this path.".into()),
                );
            } else {
                report.outcome(action, Some(path), Ok(cleanup.changed));
            }
        }
        Err(error) => report.outcome(action, Some(path), Err(error)),
    }
}

fn clean_runtime(
    kind: Removal<'_>,
    path: &Path,
    apply: bool,
    ignore_case: bool,
) -> Result<profile_cleanup::Cleanup> {
    if matches!(kind, Removal::Cache(_)) {
        return profile_cleanup::clean(path, "cache", apply, &[], ignore_case);
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("known runtime path");
    if matches!(name, "run" | "update-runner" | "app-backups" | "updates") {
        return profile_cleanup::clean(path, name, apply, &[], ignore_case);
    }
    let mut cleanup = profile_cleanup::Cleanup::default();
    if regular_file(path) {
        if apply {
            cleanup.changed = remove_file(path)?;
        }
    } else if present(path)? {
        cleanup.kept.push(path.to_path_buf());
    }
    Ok(cleanup)
}

fn clean_data(
    ctx: &Ctx<'_>,
    args: &UninstallArgs,
    profiles: &[Profile],
    profile: &Profile,
    retain_state: bool,
) -> Result<profile_cleanup::Cleanup> {
    Removal::Data(profile).validate(ctx, args, profiles, &profile.data)?;
    recheck_profile(ctx, profile)?;
    check_journals(&profile.data, args)?;
    profile_cleanup::clean(
        &profile.data,
        "data",
        true,
        if retain_state {
            DATA_WITH_STATE
        } else {
            DATA_SEPARATE
        },
        case_insensitive(ctx),
    )
}

fn validate_installation(
    ctx: &Ctx<'_>,
    args: &UninstallArgs,
    profiles: &[Profile],
    root: &Path,
    inventory: Option<&RemovalInventory>,
) -> Result<()> {
    Removal::Installation.validate(ctx, args, profiles, root)?;
    if let Some(inventory) = inventory {
        inventory.validate(root, ctx.deps.updates.platform.as_ref())?;
    }
    ctx.deps
        .agents
        .uninstall
        .validate_installation(root, inventory.is_some())
}

fn update_state_dirs(profiles: &[Profile]) -> Vec<PathBuf> {
    profiles
        .iter()
        .filter(|profile| !profile.skip_disk)
        .map(|profile| broscribe_update_runner::state_dir(&profile.data))
        .collect()
}

fn check_profile_selectors(ctx: &Ctx<'_>, args: &UninstallArgs) -> Result<()> {
    let selectors = args.data_dir.iter().map(|path| ("--data-dir", path)).chain(
        ctx.paths
            .data_dir_from_env
            .iter()
            .map(|path| (ENV_DATA_DIR, path)),
    );
    for (source, path) in selectors {
        if !path.is_absolute() {
            return Err(error(format!(
                "The path from {source} ({}) is relative. Use an absolute path in {source} before uninstalling and preview again with --dry-run.",
                path.display(),
            )));
        }
    }
    if let Some(detail) = crate::relative_default_data_dir(ctx.deps.paths.as_ref()) {
        return Err(error(format!(
            "{detail} before uninstalling and preview again with --dry-run.",
        )));
    }
    Ok(())
}

fn check_removal_selectors(
    ctx: &Ctx<'_>,
    args: &UninstallArgs,
    profiles: &[Profile],
) -> Result<()> {
    if !args.remove_data && !args.remove_library {
        return Ok(());
    }
    let selectors = ctx
        .library_flag
        .iter()
        .map(|path| ("--library", path))
        .chain(
            ctx.paths
                .library_from_env
                .iter()
                .map(|path| (ENV_LIBRARY, path)),
        );
    for (source, path) in selectors {
        if !path.is_absolute() {
            return Err(error(format!(
                "The path from {source} ({}) is relative. Use an absolute path in {source} before removing data or libraries and preview again with --dry-run.",
                path.display(),
            )));
        }
    }
    if let Some(profile) = profiles
        .iter()
        .find(|profile| profile.relative_config_library)
    {
        return Err(error(format!(
            "The config `library` path for the profile at {} is relative. Set an absolute library path in that config before removing data or libraries and preview again with --dry-run.",
            broscribe_platform::plain_verbatim(&profile.data).display(),
        )));
    }
    Ok(())
}

fn check_targets(
    ctx: &Ctx<'_>,
    args: &UninstallArgs,
    profiles: &[Profile],
    layout: &Layout,
    inventory: Option<&RemovalInventory>,
    report: &mut Report,
) {
    for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
        if args.remove_data && profile.exists {
            let result = Removal::Data(profile)
                .validate(ctx, args, profiles, &profile.data)
                .and_then(|()| {
                    profile_cleanup::clean(
                        &profile.data,
                        "data",
                        false,
                        DATA_SEPARATE,
                        case_insensitive(ctx),
                    )
                });
            match result {
                Ok(cleanup) if args.dry_run || !args.yes => {
                    report.list_kept(&profile.data, &cleanup.kept);
                }
                Ok(_) => {}
                Err(error) => {
                    report.outcome("Validate data removal", Some(&profile.data), Err(error))
                }
            }
        }
        if args.remove_library && profile.select_library {
            let result = Removal::Library(profile)
                .validate(ctx, args, profiles, &profile.library)
                .and_then(|()| clean_library(&profile.library, false, case_insensitive(ctx)));
            match result {
                Ok(cleanup) if args.dry_run || !args.yes => {
                    report.list_kept(&profile.library, &cleanup.kept);
                }
                Ok(_) => {}
                Err(error) => report.outcome(
                    "Validate library removal",
                    Some(&profile.library),
                    Err(error),
                ),
            }
        }
        let updates = profile.data.join("updates");
        let check_updates = profile.exists && !report.update_state_unusable(&updates);
        for (kind, target) in cleanup_paths(profile)
            .chain(check_updates.then_some((Removal::Runtime(profile), updates)))
        {
            let result = kind
                .validate(ctx, args, profiles, &target)
                .and_then(|()| clean_runtime(kind, &target, false, case_insensitive(ctx)));
            match result {
                Ok(cleanup) if args.dry_run || !args.yes => {
                    report.list_kept(&target, &cleanup.kept)
                }
                Ok(_) => {}
                Err(error) => report.outcome(
                    "Validate cache and runtime removal",
                    Some(&target),
                    Err(error),
                ),
            }
        }
    }
    if let Some(root) = &layout.installation
        && !args.keep_app
        && let Err(error) = validate_installation(ctx, args, profiles, root, inventory)
    {
        report.outcome("Validate app removal", Some(root), Err(error));
    }
}

fn release_refusal(step: &Step) -> bool {
    step.status == "failed"
        && matches!(
            step.action.as_str(),
            "Validate app removal" | "Read release inventory"
        )
        && matches!(
            step.code,
            Some(
                broscribe_updates::codes::RELEASE_TREE_UNSAFE
                    | broscribe_updates::codes::INSTALLATION_CHANGED_OUTSIDE
            )
        )
}

fn retry_command(
    ctx: &Ctx<'_>,
    args: &UninstallArgs,
    profiles: &[Profile],
    layout: &Layout,
) -> std::result::Result<Option<String>, String> {
    use super::agents::shell_quote;
    let Some(root) = layout
        .installation
        .as_deref()
        .filter(|_| layout.shell_retry)
    else {
        return Ok(None);
    };
    let omit = |reason: &str| format!("Retry command omitted because {reason}.");
    let cli = root.join("broscribe");
    if !regular_file(&cli) {
        return Err(omit("the installed CLI could not be verified"));
    }
    let current = stable_path(ctx.data_dir());
    let additional = args
        .data_dir
        .iter()
        .map(|path| stable_path(path))
        .filter(|path| *path != current);
    let selected = std::iter::once(current.clone())
        .chain(additional)
        .collect::<Vec<_>>();
    let inspected = |path: &Path| {
        profiles
            .iter()
            .find(|profile| profile.data == path && !profile.skip_disk)
            .ok_or_else(|| {
                omit(&format!(
                    "the profile at {} could not be inspected",
                    path.display()
                ))
            })
    };
    let primary = inspected(&current)?;
    for path in selected.iter().skip(1) {
        let profile = inspected(path)?;
        if profile
            .libraries
            .iter()
            .any(|library| !library.is_absolute())
        {
            // One --library flag cannot pin another selected profile's relative library.
            return Err(omit(&format!(
                "the profile at {} has a relative library path",
                path.display()
            )));
        }
    }
    let quote_path = |path: &Path| {
        path.to_str()
            .filter(|_| path.is_absolute())
            .map(shell_quote)
            .ok_or_else(|| {
                omit(&format!(
                    "the absolute path at {} could not be pinned",
                    path.display()
                ))
            })
    };
    let mut command = String::new();
    for (key, path) in ctx.deps.paths.uninstall_retry_env() {
        let value = match key {
            // Linux rejects a symlink at the runtime folder itself.
            "XDG_RUNTIME_DIR" => quote_path(path)?,
            // A missing component before `..` can make this config root
            // unreachable. A lexical normalization could expose another unit.
            "XDG_CONFIG_HOME" => match path.canonicalize() {
                Ok(canonical) => quote_path(&canonical)?,
                Err(_) => quote_path(path)?,
            },
            _ => quote_path(&stable_path(path))?,
        };
        command.push_str(&format!("{key}={value} "));
    }
    // The global library override belongs to the primary profile, not
    // to an additional profile named by uninstall's --data-dir option.
    let protected_env_library = ctx
        .paths
        .library_from_env
        .as_ref()
        .map(|path| quote_path(&stable_path(path)))
        .transpose()?
        .unwrap_or_else(|| shell_quote(""));
    command.push_str(&format!(
        "BROSCRIBE_DATA_DIR={} BROSCRIBE_LIBRARY={protected_env_library} {} --library {}",
        quote_path(&current)?,
        quote_path(&cli)?,
        quote_path(&primary.library)?
    ));
    command.push_str(" uninstall");
    for path in selected {
        command.push_str(" --data-dir ");
        command.push_str(&quote_path(&path)?);
    }
    for (set, flag) in [
        (args.remove_data, " --remove-data"),
        (args.remove_library, " --remove-library"),
        (args.discard_unsaved, " --discard-unsaved"),
        (args.all_profiles_secrets, " --all-profiles-secrets"),
        (args.skip_credentials, " --skip-credentials"),
        (args.revoke_at_google, " --revoke-at-google"),
    ] {
        if set {
            command.push_str(flag);
        }
    }
    Ok(Some(format!(
        "Run {command} to preview, then add --yes to apply after checking the plan."
    )))
}

fn remove_installation_launchers(ctx: &Ctx<'_>, layout: &Layout, root: &Path, report: &mut Report) {
    for path in &layout.desktop_paths {
        report.outcome(
            "Remove installation launcher",
            Some(path),
            ctx.deps
                .agents
                .uninstall
                .remove_desktop_entry(&ctx.deps.agents.cli, path, root),
        );
    }
}

fn clean_library(root: &Path, apply: bool, ignore_case: bool) -> Result<profile_cleanup::Cleanup> {
    if !present(root)? {
        return Ok(profile_cleanup::Cleanup::default());
    }
    if !profile_cleanup::library_evidence(root, true)?
        && !profile_cleanup::empty_library_skeleton(root)?
    {
        return Err(error(format!(
            "BroScribe could not verify a meeting library at {}. Check the library path before retrying.",
            root.display()
        )));
    }
    profile_cleanup::clean(root, "library", apply, &[], ignore_case)
}

fn cleanup_paths(profile: &Profile) -> impl Iterator<Item = (Removal<'_>, PathBuf)> {
    profile
        .caches
        .iter()
        .cloned()
        .map(|path| (Removal::Cache(profile), path))
        .chain(
            DATA_RUNTIME
                .iter()
                .filter(move |_| profile.exists)
                .map(|name| (Removal::Runtime(profile), profile.data.join(name))),
        )
}

const DATA_RUNTIME: &[&str] = &[
    "run",
    "config.lock",
    "daemon-start.lock",
    "update-runner",
    "app-backups",
];

fn install_artifacts(root: &Path) -> Vec<PathBuf> {
    let Some(parent) = root.parent() else {
        return Vec::new();
    };
    let Some(name) = root.file_name() else {
        return Vec::new();
    };
    [".update", ".update.lock"]
        .map(|suffix| {
            let mut leaf = std::ffi::OsString::from(".");
            leaf.push(name);
            leaf.push(suffix);
            parent.join(leaf)
        })
        .to_vec()
}

fn kept_update_stages(root: &Path) -> Result<Vec<PathBuf>> {
    let Some(parent) = root.parent() else {
        return Ok(Vec::new());
    };
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return Ok(Vec::new());
    };
    let prefix = format!(".{name}.update-kept-");
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(Error::io_at(parent, error)),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| Error::io_at(parent, error))?;
        let leaf = entry.file_name();
        if leaf
            .to_str()
            .and_then(|leaf| leaf.strip_prefix(&prefix))
            .and_then(|suffix| suffix.split_once('-'))
            .is_some_and(|(pid, stamp)| {
                pid.parse::<u32>().is_ok_and(|n| n.to_string() == pid)
                    && stamp.parse::<i64>().is_ok_and(|n| n.to_string() == stamp)
            })
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn plan(args: &UninstallArgs, profiles: &[Profile], layout: &Layout, report: &mut Report) {
    for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
        report.add(
            "Stop BroScribe Agent and remove its login registration",
            Some(&profile.data),
            "planned",
            None,
        );
    }
    report.add(
        "Sign out of integrations and remove BroScribe credentials",
        None,
        if args.skip_credentials {
            "skipped"
        } else {
            "planned"
        },
        if args.skip_credentials {
            Some("Requested with --skip-credentials. Any stored credentials will remain.".into())
        } else {
            args.all_profiles_secrets
                .then(|| "Includes undiscovered credential namespaces.".into())
        },
    );
    if args.revoke_at_google {
        report.add(
            "Revoke Google access",
            None,
            "planned",
            Some("Google revokes the entire OAuth project grant across all its clients.".into()),
        );
    }
    report.add(
        "Remove AI tool registrations, CLI link and skills",
        None,
        "planned",
        None,
    );
    report.add(
        "Remove desktop registrations and runtime files",
        None,
        "planned",
        None,
    );
    for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
        for cache in &profile.caches {
            report.add("Remove cache", Some(cache), "planned", None);
        }
        report.add(
            "Remove data folder",
            Some(&profile.data),
            if args.remove_data {
                "planned"
            } else {
                "skipped"
            },
            args.discard_unsaved.then(|| {
                "Unsaved and processing recordings in the journal will be permanently deleted."
                    .into()
            }),
        );
        report.add(
            "Remove library",
            Some(&profile.library),
            if args.remove_library && profile.select_library {
                "planned"
            } else {
                "skipped"
            },
            None,
        );
    }
    report.add(
        "Remove BroScribe app",
        layout.installation.as_deref(),
        if !args.keep_app && layout.installation.is_some() {
            "planned"
        } else {
            "skipped"
        },
        None,
    );
}

fn stop_profile(ctx: &Ctx<'_>, profile: &Profile) -> Result<bool> {
    use broscribe_platform::LoginJob;
    if profile.agent.is_none()
        && matches!(
            profile.job,
            LoginJob::Absent
                | LoginJob::Unsupported
                | LoginJob::Running(_)
                | LoginJob::Registered {
                    serves_profile: Some(_)
                }
                | LoginJob::Idle {
                    serves_profile: Some(false)
                }
        )
    {
        return Ok(false);
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut paths = ctx.paths.clone();
    paths.data_dir.clone_from(&profile.data);
    Ctx::scoped(
        ctx.deps,
        true,
        None,
        paths,
        None,
        &mut out,
        &mut err,
        daemon::stop,
    )?;
    let status: Value = serde_json::from_slice(&out)
        .map_err(|_| error("BroScribe Agent stop reported an unreadable result"))?;
    Ok(status["stopped"] == true || status["login_job_unloaded"] == true)
}

fn sign_out(ctx: &Ctx<'_>, profile: &Profile, revoke: bool, report: &mut Report) -> Result<()> {
    if !present(&profile.data.join("integrations"))?
        && !(revoke && Config::path(&profile.data).is_file())
    {
        return Ok(());
    }
    let (config, _) =
        IntegrationsConfig::load_isolated(&profile.data).map_err(integrations::core)?;
    let service = Integrations::new(
        &profile.data,
        config,
        ctx.deps.agents.uninstall.secret_store(&profile.data),
        ctx.deps.browser.clone(),
        std::sync::Arc::new(|_| {}),
    )
    .map_err(integrations::core)?
    .with_trust_store(ctx.deps.execution.trust_store())
    .with_reauth_notification_delivery(false);
    for id in IntegrationId::ALL {
        if id == IntegrationId::MicrosoftGraph {
            match service.list_accounts(Some(id)) {
                Ok(accounts) => {
                    for account in accounts {
                        report.outcome(
                            "Forget Microsoft account",
                            Some(&profile.data),
                            service
                                .remove_account(&account.account, account.generation)
                                .map(|()| true)
                                .map_err(integrations::core),
                        );
                    }
                }
                Err(error) => report.outcome(
                    "List Microsoft accounts",
                    Some(&profile.data),
                    Err(integrations::core(error)),
                ),
            }
        } else if id == IntegrationId::Google && revoke {
            match service.logout_and_revoke(id) {
                Ok(done) => {
                    report.outcome(
                        "Sign out of Google",
                        Some(&profile.data),
                        done.local.map(|()| true).map_err(|e| error(e.message)),
                    );
                    let outcome = match done.remote_revocation {
                        RevocationOutcome::RevocationRequested => Ok(true),
                        RevocationOutcome::NoStoredGrant => Ok(false),
                        RevocationOutcome::Failed { code } => Err(error(format!(
                            "Google revocation failed ({code}). Review the grant at https://myaccount.google.com/permissions."
                        ))),
                    };
                    report.outcome("Revoke Google access", Some(&profile.data), outcome);
                }
                Err(error) => report.outcome(
                    "Revoke Google access",
                    Some(&profile.data),
                    Err(integrations::core(error)),
                ),
            }
        } else {
            report.outcome(
                &format!("Sign out of {id}"),
                Some(&profile.data),
                service
                    .logout(id)
                    .map(|()| true)
                    .map_err(integrations::core),
            );
        }
    }
    Ok(())
}

fn recheck_profile(ctx: &Ctx<'_>, profile: &Profile) -> Result<()> {
    if ctx
        .deps
        .agents
        .uninstall
        .directory_identity(&profile.data)?
        != profile.pending.identity
        || profile_id(&profile.data)? != profile.pending.marker
    {
        return Err(error(
            "The profile directory changed after discovery. Retry to finish credential cleanup.",
        ));
    }
    Ok(())
}

fn check_pending_owner(
    ctx: &Ctx<'_>,
    default: &Path,
    owner: &(DirectoryIdentity, Option<String>),
) -> Result<()> {
    let (identity, marker) = owner;
    if ctx.deps.agents.uninstall.directory_identity(default)? != Some(*identity)
        || profile_id(default)? != *marker
    {
        return Err(error(
            "The default folder holding the pending uninstall list changed.",
        ));
    }
    Ok(())
}

fn clear_pending(
    ctx: &Ctx<'_>,
    default: &Path,
    owner: &(DirectoryIdentity, Option<String>),
    created: Option<DirectoryIdentity>,
) -> Result<bool> {
    check_pending_owner(ctx, default, owner)?;
    let removed = remove_file(&default.join(PENDING_PROFILES))?;
    if created == Some(owner.0) {
        check_pending_owner(ctx, default, owner)?;
        match std::fs::remove_dir(default) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(Error::io_at(default, error)),
        }
    }
    Ok(removed)
}

struct InstallCleanup<'a> {
    inventory: Option<&'a RemovalInventory>,
    has_update_lock: bool,
    previous_helper: Option<PreviousHelper>,
    retry: Option<&'a str>,
}

fn apply(
    ctx: &Ctx<'_>,
    args: &UninstallArgs,
    profiles: &[Profile],
    layout: &Layout,
    mut state: PendingUninstall,
    update: InstallCleanup<'_>,
    report: &mut Report,
) {
    let InstallCleanup {
        inventory,
        has_update_lock,
        previous_helper,
        retry,
    } = update;
    let platform = &ctx.deps.agents.uninstall;
    let default = stable_path(&ctx.deps.paths.data_dir());
    let pending = default.join(PENDING_PROFILES);
    // Stopping can unload the only registration that names a profile.
    // Save all secondary folders before any discovery records disappear.
    state.profiles = profiles
        .iter()
        .filter(|profile| profile.data != default)
        .map(|profile| profile.pending.clone())
        .collect();
    let default_profile = profiles.iter().find(|profile| profile.data == default);
    if let Some(profile) = default_profile.filter(|profile| profile.exists && !profile.skip_disk) {
        state.default_libraries = profile.pending.libraries.clone();
    }
    let default_untouched =
        default_profile.is_some_and(|profile| profile.exists && profile.skip_disk);
    let pending_owner = if !state.profiles.is_empty() || !state.default_libraries.is_empty() {
        let saved = (|| {
            if default_untouched {
                return Err(error(
                    "The default data folder is unverified and cannot hold the pending list. Explicitly select it or open its BroScribe profile before retrying.",
                ));
            }
            if let Some(profile) = default_profile.filter(|profile| profile.exists) {
                recheck_profile(ctx, profile)?;
            }
            if let Some(parent) = default.parent() {
                std::fs::create_dir_all(parent).map_err(|e| Error::io_at(parent, e))?;
            }
            match std::fs::create_dir(&default) {
                Ok(()) => state.created_default = platform.directory_identity(&default)?,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(Error::io_at(&default, error)),
            }
            let identity = platform
                .directory_identity(&default)?
                .ok_or_else(|| error("The default folder holding the pending list changed."))?;
            let owner = (identity, profile_id(&default)?);
            state.save(&default)?;
            Ok(owner)
        })();
        match saved {
            Ok(owner) => Some(owner),
            Err(error) => {
                report.outcome(
                    "Save pending uninstall profiles",
                    Some(&pending),
                    Err(error),
                );
                return;
            }
        }
    } else {
        None
    };
    // Preflight has passed and the pending profile list no longer needs
    // these receipts to decide which profile work can be retried.
    if platform.defers_settings_cleanup() {
        let temp = stable_path(&std::env::temp_dir());
        let reference = helper_reference(ctx);
        let settled = match &previous_helper {
            Some(PreviousHelper::Settled(receipt)) => Some(receipt.as_path()),
            _ => None,
        };
        let receipts: std::collections::BTreeSet<_> = settled
            .into_iter()
            .chain(profiles.iter().filter_map(|p| p.settled_receipt.as_deref()))
            .collect();
        for receipt in receipts {
            let cleanup = cleanup_helper_run(&temp, receipt).and_then(|cleanup| {
                if settled == Some(receipt) {
                    clear_helper_reference(&reference, receipt)?;
                }
                Ok(cleanup)
            });
            cleanup_report(
                report,
                "Remove previous after-exit helper files",
                receipt,
                cleanup,
            );
        }
        if let Some(PreviousHelper::Obsolete(receipt)) = previous_helper {
            let run = recorded_helper_run(&receipt).expect("validated helper run");
            let result = present(run).and_then(|exists| {
                if exists {
                    return Err(error(format!(
                        "The uninstall helper folder {} reappeared. Preview the uninstall again.",
                        run.display()
                    )));
                }
                clear_helper_reference(&reference, &receipt)
            });
            report.outcome(
                "Clear obsolete after-exit helper reference",
                Some(&reference),
                result,
            );
        }
        if report.blocked() {
            return;
        }
    }
    let mut reservations = Vec::new();
    for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
        report.outcome(
            "Stop BroScribe Agent",
            Some(&profile.data),
            if profile.exists {
                recheck_profile(ctx, profile).and_then(|()| stop_profile(ctx, profile))
            } else {
                stop_profile(ctx, profile)
            },
        );
    }
    if report.blocked() {
        report.add(
            "Remove BroScribe",
            None,
            "skipped",
            Some("All profiles must stop safely first.".into()),
        );
        return;
    }
    for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
        let reserve = (|| {
            if !profile.exists {
                return Ok(false);
            }
            recheck_profile(ctx, profile)?;
            let lock = DaemonStartupLock::try_acquire(&profile.data)?
                .ok_or_else(|| error("BroScribe Agent is starting. Try again after it stops."))?;
            if daemon::agent_instance_held(&profile.data, ctx.deps.agent_instance.as_ref())? {
                return Err(error(
                    "An agent started again. Try uninstalling after it stops.",
                ));
            }
            check_journals(&profile.data, args)?;
            reservations.push(lock);
            Ok(true)
        })();
        if let Err(error) = reserve {
            report.outcome("Reserve agent shutdown", Some(&profile.data), Err(error));
        }
    }
    if report.blocked() {
        return;
    }

    for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
        report.outcome(
            "Remove login registration",
            Some(&profile.data),
            daemon::unregister(ctx.deps.autostart.as_ref(), &profile.data, None, true),
        );
    }
    if report.blocked() {
        report.add(
            "Remove BroScribe",
            None,
            "skipped",
            Some("Login registrations must be removed safely first.".into()),
        );
        return;
    }
    for profile in profiles
        .iter()
        .filter(|profile| !profile.skip_disk && profile.exists)
    {
        if let Err(error) = recheck_profile(ctx, profile) {
            report.outcome("Check profile identity", Some(&profile.data), Err(error));
            continue;
        }
        if !args.skip_credentials
            && let Err(error) = sign_out(ctx, profile, args.revoke_at_google, report)
        {
            report.outcome("Sign out of integrations", Some(&profile.data), Err(error));
        }
        if regular_file(&Config::path(&profile.data)) {
            report.outcome(
                "Disable profile MCP access",
                Some(&profile.data),
                Config::update(&profile.data, |config| {
                    config.mcp.enabled = Some(false);
                    Ok(())
                })
                .map(|_| true),
            );
        }
    }
    let credentials: Vec<_> = profiles
        .iter()
        .map(|profile| CredentialProfile {
            data_dir: profile.data.clone(),
            namespace: profile
                .pending
                .namespace
                .clone()
                .expect("discovered credential namespace"),
        })
        .collect();
    if args.skip_credentials {
        report.add(
            "Remove BroScribe credentials",
            None,
            "skipped",
            Some("Requested with --skip-credentials.".into()),
        );
    } else {
        report_count(
            report,
            "Remove BroScribe credentials",
            platform.remove_credentials(&credentials, args.all_profiles_secrets),
        );
    }
    match ctx.deps.autostart.registered_profiles() {
        Ok(profiles) if profiles.is_empty() => {}
        Ok(_) => {
            report.outcome("Verify login registrations are gone", None,
                Err(error("A BroScribe login registration is still installed. Retry after stopping the registered profile.")));
        }
        Err(error) => {
            report.outcome("Verify login registrations are gone", None, Err(error));
        }
    }
    if report.blocked() {
        report.add(
            "Remove BroScribe app",
            layout.installation.as_deref(),
            "skipped",
            Some(UNINSTALL_RETRY_DETAIL.into()),
        );
        return;
    }
    for (agent, result) in agents::unregister_all(ctx.deps) {
        report.outcome(
            &format!("Remove MCP registration from {agent}"),
            None,
            result.map(|()| true).map_err(error),
        );
    }
    match agents::change_agent_tools(&ctx.deps.agents, false) {
        Ok(items) => {
            for item in items {
                let name = item["item"].as_str().unwrap_or("AI tool");
                let message = item["error"].as_str().or_else(|| item["skipped"].as_str());
                let status = if item["ok"] == false {
                    "failed"
                } else if item["skipped"].is_string() {
                    "skipped"
                } else {
                    "done"
                };
                report.add(
                    &format!("Remove {name}"),
                    item["path"].as_str().map(Path::new),
                    status,
                    message.map(str::to_owned),
                );
            }
        }
        Err(error) => report.outcome("Remove AI tools", None, Err(error)),
    }
    report_count(
        report,
        "Remove Windows desktop registrations",
        platform.remove_shortcuts(
            &profiles
                .iter()
                .filter(|p| !p.skip_disk)
                .map(|profile| profile.data.clone())
                .collect::<Vec<_>>(),
        ),
    );
    if let Some(root) = &layout.installation {
        remove_installation_launchers(ctx, layout, root, report);
    }
    let after_exit = platform.defers_settings_cleanup();
    let staging_removed = !args.keep_app
        && layout
            .installation
            .as_ref()
            .is_some_and(|root| remove_staging(ctx, args, profiles, root, has_update_lock, report));
    // Windows cannot remove the startup lock file while it is held.
    // Registrations are gone and all profiles proved the agent was stopped.
    if after_exit {
        reservations.clear();
    }
    let active = stable_path(ctx.data_dir());
    let mut deferred = None;
    let mut remove_default = None;
    for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
        if profile.exists
            && let Err(error) = recheck_profile(ctx, profile)
        {
            report.outcome("Check profile identity", Some(&profile.data), Err(error));
            continue;
        }
        let defer = after_exit && args.from_settings && profile.exists && profile.data == active;
        let mut defer_runner = false;
        for path in &profile.runtime {
            cleanup_report(
                report,
                "Remove profile socket",
                path,
                profile_cleanup::clean_socket(path),
            );
            if let Some(parent) = path.parent() {
                remove_empty_runtime_dir(parent, after_exit);
            }
        }
        cleanup_report(
            report,
            "Remove regular profile runtime files",
            &profile.data,
            platform.remove_runtime_files(&profile.data),
        );
        for (kind, path) in cleanup_paths(profile) {
            if defer && same_path(&path, &profile.data.join("run")) {
                continue;
            }
            let action = if matches!(kind, Removal::Cache(_)) {
                "Remove cache"
            } else {
                "Remove profile runtime files"
            };
            if let Err(error) = kind.validate(ctx, args, profiles, &path) {
                report.outcome(action, Some(&path), Err(error));
                continue;
            }
            match clean_runtime(kind, &path, true, case_insensitive(ctx)) {
                Err(_) if defer && same_path(&path, &profile.data.join("update-runner")) => {
                    defer_runner = true;
                }
                result => cleanup_report(report, action, &path, result),
            }
        }
        if defer {
            let (kind, target) = if args.remove_data {
                (Removal::Data(profile), profile.data.clone())
            } else {
                (Removal::Runtime(profile), profile.data.join("run"))
            };
            match kind.validate(ctx, args, profiles, &target) {
                Ok(()) => {
                    deferred = Some(DeferredProfile {
                        data_dir: profile.data.clone(),
                        identity: profile.pending.identity.expect("deferred profile exists"),
                        marker: profile.pending.marker.clone(),
                        remove_data: args.remove_data,
                        discard_unsaved: args.discard_unsaved,
                        remove_update_runner: defer_runner,
                        protection: kind.protection(ctx, args, profiles, &target),
                        default_cleanup: None,
                    })
                }
                Err(error) => report.outcome(
                    "Validate profile removal after exit",
                    Some(&target),
                    Err(error),
                ),
            }
        } else if args.remove_data && profile.exists {
            if profile.data == default {
                remove_default = Some(profile);
            } else {
                cleanup_report(
                    report,
                    "Remove data folder",
                    &profile.data,
                    clean_data(
                        ctx,
                        args,
                        profiles,
                        profile,
                        profile.data.join("updates").exists(),
                    ),
                );
            }
        }
        if !args.remove_data {
            report.add(
                "Remove data folder",
                Some(&profile.data),
                "skipped",
                Some("Kept by request.".into()),
            );
        }
        if args.remove_library && profile.select_library {
            cleanup_report(
                report,
                "Remove library",
                &profile.library,
                Removal::Library(profile)
                    .validate(ctx, args, profiles, &profile.library)
                    .and_then(|()| clean_library(&profile.library, true, case_insensitive(ctx))),
            );
        }
    }
    drop(reservations);
    match platform.registration_lock_paths() {
        Ok(paths) => {
            for path in paths {
                report.outcome(
                    "Remove login registration lock",
                    Some(&path),
                    lock_existing(&path).and_then(|guard| match guard {
                        Some(_guard) => remove_file(&path),
                        None => Ok(false),
                    }),
                );
                if let Some(parent) = path.parent() {
                    remove_empty_runtime_dir(parent, after_exit);
                }
            }
        }
        Err(error) => report.outcome("Locate login registration lock", None, Err(error)),
    }
    // The receipt must outlive a failed/timed-out helper. The default
    // directory also holds the retry list, so wait to remove it.
    let deferred_secondary = deferred
        .as_ref()
        .is_some_and(|profile| profile.data_dir != default);
    if !report.blocked() && deferred_secondary {
        let prepare = || {
            let owner = pending_owner
                .as_ref()
                .expect("secondary profiles have a pending list");
            check_pending_owner(ctx, &default, owner)?;
            let (identity, marker) = owner;
            let remove_data = if let Some(profile) = remove_default {
                Removal::Data(profile).validate(ctx, args, profiles, &default)?;
                recheck_profile(ctx, profile)?;
                Some(Removal::Data(profile).protection(ctx, args, profiles, &default))
            } else {
                None
            };
            Ok(DeferredDefault {
                data_dir: default.clone(),
                identity: *identity,
                marker: marker.clone(),
                remove_data,
                created_for_pending_list: state.created_default == Some(*identity),
            })
        };
        match prepare() {
            Ok(cleanup) => {
                deferred
                    .as_mut()
                    .expect("secondary cleanup")
                    .default_cleanup = Some(cleanup)
            }
            Err(error) => report.outcome(
                "Prepare default data cleanup after exit",
                Some(&default),
                Err(error),
            ),
        }
    }
    if !report.blocked() && !deferred_secondary {
        if let Some(profile) = remove_default {
            cleanup_report(
                report,
                "Remove data folder",
                &default,
                clean_data(
                    ctx,
                    args,
                    profiles,
                    profile,
                    profile.data.join("updates").exists(),
                ),
            );
        } else if let Some(owner) = &pending_owner
            && !deferred.as_ref().is_some_and(|profile| profile.remove_data)
        {
            report.outcome(
                "Clear pending uninstall profiles",
                Some(&pending),
                clear_pending(ctx, &default, owner, state.created_default),
            );
        }
    }
    // The handle keeps update state available through staging and installation removal.
    let mut update_cleanup = Vec::new();
    for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
        let updates = profile.data.join("updates");
        match present(&updates) {
            Ok(false) => continue,
            Err(error) => report.outcome("Inspect update state", Some(&updates), Err(error)),
            Ok(true)
                if staging_removed
                    && !args.keep_app
                    && !report.blocked()
                    && !report.update_state_unusable(&updates) =>
            {
                let result = Removal::Runtime(profile)
                    .validate(ctx, args, profiles, &updates)
                    .and_then(|()| recheck_profile(ctx, profile))
                    .and_then(|()| {
                        clean_runtime(
                            Removal::Runtime(profile),
                            &updates,
                            false,
                            case_insensitive(ctx),
                        )
                    });
                match result {
                    Ok(_) => update_cleanup.push(DeferredUpdateCleanup {
                        data_dir: profile.data.clone(),
                        identity: profile
                            .pending
                            .identity
                            .expect("existing profile has identity"),
                        marker: profile.pending.marker.clone(),
                        remove_data: args.remove_data,
                        discard_unsaved: args.discard_unsaved,
                    }),
                    Err(error) => {
                        report.outcome("Validate update state cleanup", Some(&updates), Err(error))
                    }
                }
            }
            Ok(true) => report.add(
                "Remove update state",
                Some(&updates),
                "kept",
                Some("Kept until update staging and the installation have been removed.".into()),
            ),
        }
    }
    let windows_plan = if after_exit && !args.keep_app && !report.blocked() {
        match inventory.map(RemovalInventory::windows_plan).transpose() {
            Ok(plan) => plan,
            Err(error) => {
                report.outcome(
                    "Prepare release inventory for after-exit removal",
                    layout.installation.as_deref(),
                    Err(error),
                );
                None
            }
        }
    } else {
        None
    };
    let reference = helper_reference(ctx);
    let record_helper = |receipt: &Path| save_helper_reference(&reference, receipt);
    let check = ReleaseRemovalCheck {
        validate: &|root| validate_installation(ctx, args, profiles, root, inventory),
        windows_inventory: windows_plan.as_deref(),
        windows_update_cleanup: &update_cleanup,
        windows_before_launch: &record_helper,
    };
    let installation = layout
        .installation
        .as_deref()
        .filter(|_| !args.keep_app && !report.blocked());
    let installation = installation.filter(|root| {
        match validate_installation(ctx, args, profiles, root, inventory) {
            Ok(()) => true,
            Err(error) => {
                report.outcome("Validate app removal", Some(root), Err(error));
                false
            }
        }
    });
    if let Some(profile) = deferred.filter(|_| !report.blocked()) {
        let runtime = profile.data_dir.join("run");
        let (action, target) = if profile.remove_data {
            ("Remove data folder after exit", profile.data_dir.as_path())
        } else {
            ("Remove profile runtime files after exit", runtime.as_path())
        };
        let mut record_receipt = |receipt: &Path| {
            if let Some(entry) = state
                .profiles
                .iter_mut()
                .find(|entry| entry.data == profile.data_dir)
            {
                entry.receipt = Some(stable_path(receipt));
                let owner = pending_owner
                    .as_ref()
                    .expect("secondary profiles have a pending list");
                check_pending_owner(ctx, &default, owner)?;
                state.save(&default)?;
            }
            record_helper(receipt)
        };
        match platform.remove_after_exit(
            &ctx.deps.agents.cli,
            installation,
            installation.map(|_| &check),
            &profile,
            &mut record_receipt,
        ) {
            Ok(receipt) => {
                report.add(
                    action,
                    Some(target),
                    "pending",
                    Some(receipt_detail(&receipt)),
                );
                if profile.remove_update_runner {
                    report.add(
                        "Remove update runner after exit",
                        Some(&profile.data_dir.join("update-runner")),
                        "pending",
                        Some(receipt_detail(&receipt)),
                    );
                }
                if let Some(cleanup) = &profile.default_cleanup {
                    if cleanup.remove_data.is_some() {
                        report.add(
                            "Remove default data folder after exit",
                            Some(&default),
                            "pending",
                            Some(receipt_detail(&receipt)),
                        );
                    }
                    report.add(
                        "Clear pending uninstall profiles after exit",
                        Some(&pending),
                        "pending",
                        Some(receipt_detail(&receipt)),
                    );
                }
                if let Some(root) = installation {
                    report.add(
                        "Remove BroScribe app after exit",
                        Some(root),
                        "pending",
                        Some(receipt_detail(&receipt)),
                    );
                    for cleanup in &update_cleanup {
                        report.add(
                            "Remove update state after exit",
                            Some(&cleanup.data_dir.join("updates")),
                            "pending",
                            Some(receipt_detail(&receipt)),
                        );
                    }
                }
                report_helper_reference(report, &reference, &receipt);
                if installation.is_some() {
                    return;
                }
            }
            Err(error) => report.outcome(action, Some(target), Err(error)),
        }
    } else if let Some(root) = installation {
        match platform.remove_installation(&ctx.deps.agents.cli, root, &check) {
            Ok(Some(receipt)) => {
                report.add(
                    "Remove BroScribe app after exit",
                    Some(root),
                    "pending",
                    Some(receipt_detail(&receipt)),
                );
                for cleanup in &update_cleanup {
                    report.add(
                        "Remove update state after exit",
                        Some(&cleanup.data_dir.join("updates")),
                        "pending",
                        Some(receipt_detail(&receipt)),
                    );
                }
                report_helper_reference(report, &reference, &receipt);
            }

            Ok(None) => {
                report.outcome("Remove BroScribe app", Some(root), Ok(true));
                for cleanup in update_cleanup {
                    let profile = profiles
                        .iter()
                        .find(|profile| profile.data == cleanup.data_dir)
                        .expect("cleanup names a discovered profile");
                    let updates = profile.data.join("updates");
                    cleanup_report(
                        report,
                        "Remove update state",
                        &updates,
                        recheck_profile(ctx, profile).and_then(|()| {
                            clean_runtime(
                                Removal::Runtime(profile),
                                &updates,
                                true,
                                case_insensitive(ctx),
                            )
                        }),
                    );
                    if args.remove_data && !report.blocked() && !updates.exists() {
                        cleanup_report(
                            report,
                            "Finish data folder cleanup",
                            &profile.data,
                            clean_data(ctx, args, profiles, profile, false),
                        );
                    }
                }
            }
            Err(error) => report.outcome("Remove BroScribe app", Some(root), Err(error)),
        }
        return;
    }
    let detail = if args.keep_app {
        "Kept by request.".into()
    } else if report.blocked() {
        retry
            .map(str::to_owned)
            .unwrap_or_else(|| UNINSTALL_RETRY_DETAIL.into())
    } else {
        "No verified app installation was found.".into()
    };
    report.add(
        "Remove BroScribe app",
        layout.installation.as_deref(),
        "skipped",
        Some(detail),
    );
}

/// The updater can be staging from another profile or a running CLI. Take
/// the existing lock before removing staging, without creating it on a dry run.
fn lock_existing(path: &Path) -> Result<Option<std::fs::File>> {
    if !present(path)? {
        return Ok(None);
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(error("The lock path is not a regular file."));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| Error::io_at(path, error))?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => {
            error("Another BroScribe operation still holds this lock.")
        }
        std::fs::TryLockError::Error(e) => Error::io_at(path, e),
    })?;
    Ok(Some(file))
}

fn receipt_details_path(receipt: &Path) -> PathBuf {
    let mut path = receipt.as_os_str().to_os_string();
    path.push(".details");
    path.into()
}

/// Retire only the known files of a completed helper under the current
/// user's temp folder. Keep unexpected entries and never follow a run link.
fn recorded_helper_run(receipt: &Path) -> Option<&Path> {
    receipt.parent().filter(|run| {
        receipt.is_absolute()
            && receipt
                .file_name()
                .is_some_and(|name| name == "receipt.txt")
            && run
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.strip_prefix("broscribe-uninstall-").is_some_and(|id| {
                        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                })
    })
}

fn helper_run<'a>(temp: &Path, receipt: &'a Path) -> Option<&'a Path> {
    recorded_helper_run(receipt)
        .filter(|run| run.parent().is_some_and(|parent| same_path(parent, temp)))
}

fn cleanup_helper_run(temp: &Path, receipt: &Path) -> Result<profile_cleanup::Cleanup> {
    let mut cleanup = profile_cleanup::Cleanup::default();
    let Some(run) = helper_run(temp, receipt) else {
        return Ok(cleanup);
    };
    let entries = match std::fs::symlink_metadata(run) {
        Ok(meta) if meta.is_dir() && !broscribe_platform::release_layout::is_reparse(&meta) => {
            std::fs::read_dir(run).map_err(|e| Error::io_at(run, e))?
        }
        Ok(_) => {
            cleanup.kept.push(run.to_path_buf());
            return Ok(cleanup);
        }
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(cleanup),
        Err(e) => return Err(Error::io_at(run, e)),
    };
    for entry in entries {
        let path = entry.map_err(|e| Error::io_at(run, e))?.path();
        if matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(
                "uninstall-after-exit.ps1"
                    | "release-inventory.json"
                    | "receipt.txt"
                    | "receipt.txt.details"
            )
        ) && regular_file(&path)
        {
            match std::fs::remove_file(&path) {
                Ok(()) => cleanup.changed = true,
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io_at(&path, e)),
            }
        } else if present(&path)? {
            cleanup.kept.push(path);
        }
    }
    match std::fs::remove_dir(run) {
        Ok(()) => cleanup.changed = true,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty) => {}
        Err(e) => return Err(Error::io_at(run, e)),
    }
    Ok(cleanup)
}

fn receipt_detail(receipt: &Path) -> String {
    format!(
        "The helper waits up to 15 minutes for BroScribe processes to exit. Status is written to {}. Paths and other details are written to {}.",
        receipt.display(),
        receipt_details_path(receipt).display(),
    )
}

fn report_helper_reference(report: &mut Report, reference: &Path, receipt: &Path) {
    report.add(
        "Clear after-exit helper reference",
        Some(reference),
        "pending",
        Some(receipt_detail(receipt)),
    );
}

fn remove_staging(
    ctx: &Ctx<'_>,
    args: &UninstallArgs,
    profiles: &[Profile],
    root: &Path,
    has_update_lock: bool,
    report: &mut Report,
) -> bool {
    let paths = install_artifacts(root);
    let [staging, _] = paths.as_slice() else {
        return false;
    };
    let skipped = |report: &mut Report, detail: &str| {
        report.add(
            "Remove update staging",
            Some(staging),
            "skipped",
            Some(detail.into()),
        );
    };
    match present(staging) {
        Ok(false) => return true,
        Ok(true) => {}
        Err(error) => {
            report.outcome("Inspect update staging", Some(staging), Err(error));
            return false;
        }
    }
    if Removal::Installation
        .validate(ctx, args, profiles, staging)
        .is_err()
    {
        skipped(report, "The staging path overlaps a protected location.");
        return false;
    }
    if !has_update_lock {
        skipped(
            report,
            "The updater lock is missing; could not verify staging ownership.",
        );
        return false;
    }
    let mut owners = update_state_dirs(profiles);
    owners.retain(|dir| !report.update_state_unusable(dir));
    let name = root.file_name().expect("verified installation has a name");
    let result = broscribe_updates::remove_owned_stage(
        staging,
        name,
        &owners,
        ctx.deps.updates.platform.as_ref(),
    );
    let removed = matches!(result, Ok(true));
    match result {
        Ok(false) => skipped(report, "Could not verify ownership of this staging folder."),
        result => report.outcome("Remove update staging", Some(staging), result),
    }
    removed
}

fn report_count(report: &mut Report, action: &str, result: Result<usize>) {
    match result {
        Ok(count) => report.add(
            action,
            None,
            if count == 0 { "skipped" } else { "done" },
            Some(format!("Removed {count} entries.")),
        ),
        Err(error) => report.outcome(action, None, Err(error)),
    }
}

fn remove_file(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

fn remove_empty_runtime_dir(path: &Path, ignore_case: bool) {
    if path.file_name().is_some_and(|name| {
        name == "broscribe"
            || (ignore_case && name.eq_ignore_ascii_case("broscribe"))
            || name.to_string_lossy().starts_with("broscribe-")
    }) {
        let _ = std::fs::remove_dir(path);
    }
}

pub(super) fn run(ctx: &mut Ctx<'_>, args: UninstallArgs) -> Result<()> {
    let dry_run = args.dry_run || !args.yes;
    let mut report = Report::default();
    if let Err(error) = check_profile_selectors(ctx, &args) {
        report.outcome("Validate removal selectors", None, Err(error));
        return finish_report(ctx, dry_run, &report, &[]);
    }
    let layout = match ctx.deps.agents.uninstall.layout(&ctx.deps.agents.cli) {
        Ok(layout) => layout,
        Err(error) => {
            report.outcome("Inspect installation and launchers", None, Err(error));
            Layout::default()
        }
    };
    if let Some(root) = &layout.installation {
        match kept_update_stages(root) {
            Ok(paths) => {
                for path in paths {
                    report.add("Keep quarantined update staging", Some(&path), "kept", None);
                }
            }
            Err(error) => report.add(
                "List quarantined update staging",
                Some(root),
                "warning",
                Some(error.to_string()),
            ),
        }
    }

    let default = stable_path(&ctx.deps.paths.data_dir());
    let state = match PendingUninstall::read(&default) {
        Ok(state) => state,
        Err(error) => {
            report.outcome(
                "Read pending uninstall profiles",
                Some(&default.join(PENDING_PROFILES)),
                Err(error),
            );
            PendingUninstall::default()
        }
    };
    let mut previous_helper = None;
    if case_insensitive(ctx) {
        let reference = helper_reference(ctx);
        match inspect_previous_helper(&std::env::temp_dir(), &reference, &mut report) {
            Ok(receipt) => previous_helper = receipt,
            Err(error) => report.outcome(
                "Inspect previous after-exit helper",
                Some(&reference),
                Err(error),
            ),
        }
    }
    let profiles = paths_for(ctx, &args, &state, &mut report);
    let selectors = check_removal_selectors(ctx, &args, &profiles);
    let selectors_valid = selectors.is_ok();
    if let Err(error) = selectors {
        report.outcome("Validate removal selectors", None, Err(error));
    }
    if let Some(root) = &layout.installation
        && !args.keep_app
        && case_insensitive(ctx)
        && let Err(error) = super::require_working_directory_outside_installation(root, || {
            use std::fmt::Write as _;
            let mut guidance = String::new();
            for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
                if args.remove_data && profile.exists {
                    let data = broscribe_platform::plain_verbatim(&profile.data);
                    let _ = write!(guidance, "\nData folder: {}", data.display());
                }
                if args.remove_library && profile.select_library {
                    let library = broscribe_platform::plain_verbatim(&profile.library);
                    if library.is_absolute() {
                        let _ = write!(guidance, "\nLibrary: {}", library.display());
                    }
                }
            }
            let has_paths = !guidance.is_empty();
            guidance.push_str("\nChange to another folder and run a fresh uninstall --dry-run. ");
            guidance.push_str(if has_paths {
                "Check the data and library paths before applying with --yes."
            } else {
                "Check the plan before applying with --yes."
            });
            guidance
        })
    {
        report.outcome(
            "Check the terminal's current folder",
            Some(root),
            Err(error),
        );
    }
    let current_state = broscribe_update_runner::state_dir(&stable_path(ctx.data_dir()));
    if let Err(error) = broscribe_updates::inspect_update_directory(&current_state) {
        report.outcome(
            "Read current update state",
            Some(&current_state),
            Err(error),
        );
    }
    for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
        let updates = broscribe_update_runner::state_dir(&profile.data);
        match broscribe_update_runner::InstallRecord::load(&profile.data) {
            Ok(Some(record))
                if record
                    .runner_pid
                    .is_some_and(|pid| ctx.deps.process.is_alive(pid)) =>
            {
                report.outcome(
                    "Check for an active update runner",
                    Some(&updates),
                    Err(error(
                        "An update runner is active. Let it finish before uninstalling.",
                    )),
                );
            }
            Err(error) if same_path(&updates, &current_state) => {
                report.outcome(
                    "Check for an active update runner",
                    Some(&updates),
                    Err(error),
                );
            }
            Err(error) => report.warn_update_state(&updates, error),
            Ok(_) => {}
        }
        if let Err(error) = check_profile(ctx, &args, profile) {
            report.outcome(
                "Check for recording and processing work",
                Some(&profile.data),
                Err(error),
            );
        }
        if profile.exists
            && !(args.from_settings && stable_path(ctx.data_dir()) == profile.data)
            && let Err(error) = check_app(&profile.data)
        {
            report.outcome("Check desktop app", Some(&profile.data), Err(error));
        }
    }
    let update_lock = layout.installation.as_ref().and_then(|root| {
        let paths = install_artifacts(root);
        let lock = paths.last()?;
        match lock_existing(lock) {
            Ok(lock) => lock,
            Err(error) => {
                report.outcome("Check for an update in progress", Some(lock), Err(error));
                None
            }
        }
    });
    let inventory = if let Some(root) = layout.installation.as_deref().filter(|_| !args.keep_app) {
        let mut warnings = Vec::new();
        let mut sources = update_state_dirs(&profiles);
        sources.retain(|dir| !report.update_state_unusable(dir));
        let inventory = broscribe_updates::removal_inventory_with_warnings(
            root,
            &current_state,
            &sources,
            ctx.deps.updates.platform.as_ref(),
            &mut warnings,
        );
        for warning in warnings {
            report.warn_update_state(&warning.state_dir, warning.error);
        }
        match inventory {
            Ok(inventory) => inventory,
            Err(error) => {
                report.outcome("Read release inventory", Some(root), Err(error));
                None
            }
        }
    } else {
        None
    };
    check_targets(
        ctx,
        &args,
        &profiles,
        &layout,
        inventory.as_ref(),
        &mut report,
    );
    if !report.blocked()
        && !dry_run
        && let Err(error) = broscribe_updates::tighten_update_directory(&current_state)
    {
        report.outcome(
            "Prepare current update state",
            Some(&current_state),
            Err(error),
        );
    }
    let retry = selectors_valid
        .then(|| retry_command(ctx, &args, &profiles, &layout).unwrap_or_else(Some))
        .flatten();
    if dry_run {
        plan(&args, &profiles, &layout, &mut report);
    } else if report.blocked() {
        if let Some(root) = layout.installation.as_deref()
            && let Some(refusal) = report.0.iter().find(|step| release_refusal(step))
        {
            let mut detail = refusal.detail.clone().unwrap_or_default();
            if !detail.is_empty() {
                detail.push(' ');
            }
            detail.push_str(retry.as_deref().unwrap_or(UNINSTALL_RETRY_DETAIL));
            if report
                .0
                .iter()
                .filter(|step| step.status == "failed")
                .all(release_refusal)
            {
                remove_installation_launchers(ctx, &layout, root, &mut report);
            }
            report.add("Keep BroScribe app", Some(root), "kept", Some(detail));
        }
        let first_skipped = report.0.len();
        plan(&args, &profiles, &layout, &mut report);
        for step in &mut report.0[first_skipped..] {
            if step.status == "planned" {
                step.status = "skipped";
                step.detail = Some("A safety check failed before removal.".into());
            }
        }
    } else {
        apply(
            ctx,
            &args,
            &profiles,
            &layout,
            state,
            InstallCleanup {
                inventory: inventory.as_ref(),
                has_update_lock: update_lock.is_some(),
                previous_helper,
                retry: retry.as_deref(),
            },
            &mut report,
        );
    }
    drop(update_lock);
    if !dry_run && report.blocked() {
        for dir in update_state_dirs(&profiles) {
            if present(&dir).unwrap_or(false)
                && !report.0.iter().any(|step| {
                    step.action == "Remove update state"
                        && step.status == "kept"
                        && step.path.as_deref() == dir.to_str()
                })
            {
                report.add(
                    "Remove update state",
                    Some(&dir),
                    "kept",
                    Some(
                        "Kept because an earlier uninstall step failed. Retry after correcting it."
                            .into(),
                    ),
                );
            }
        }
    }
    if !dry_run && args.remove_data {
        for profile in profiles.iter().filter(|profile| !profile.skip_disk) {
            let updates = profile.data.join("updates");
            let waiting = report.0.iter().any(|step| {
                step.status == "pending"
                    && step.action == "Remove update state after exit"
                    && step.path.as_deref() == updates.to_str()
            });
            if waiting || !updates.exists() {
                continue;
            }
            let detail = "Kept for update and uninstall recovery.";
            for name in [
                "config.toml",
                ".broscribe-profile",
                PENDING_PROFILES,
                "updates",
            ] {
                let path = profile.data.join(name);
                if present(&path).unwrap_or(false) {
                    report.add(
                        "Retain update recovery entry",
                        Some(&path),
                        "kept",
                        Some(detail.into()),
                    );
                }
            }
            for step in &mut report.0 {
                if step.action == "Remove data folder"
                    && step.path.as_deref() == profile.data.to_str()
                    && (matches!(step.status, "done" | "partial")
                        || step.status == "skipped"
                            && step.detail.as_deref() == Some("Nothing to remove."))
                {
                    step.status = "partial";
                    step.detail = Some(detail.into());
                }
            }
        }
    }
    finish_report(ctx, dry_run, &report, &layout.manual_steps)
}

fn finish_report(
    ctx: &mut Ctx<'_>,
    dry_run: bool,
    report: &Report,
    manual_steps: &[&str],
) -> Result<()> {
    if report.blocked() {
        ctx.exit_code = Some(if dry_run { 2 } else { 1 });
    }
    ctx.emit(
        json!({
            "uninstall_report": true,
            "dry_run": dry_run,
            "ready": !report.blocked(),
            "steps": report.0,
            "manual_steps": manual_steps,
        }),
        |out| {
            for step in &report.0 {
                write!(out, "{}: {}", step.status, step.action)?;
                if let Some(path) = &step.path {
                    write!(out, " ({path})")?;
                }
                if let Some(detail) = &step.detail {
                    write!(out, " — {detail}")?;
                }
                writeln!(out)?;
            }
            for line in manual_steps {
                writeln!(out, "{line}")?;
            }
            if dry_run {
                writeln!(
                    out,
                    "Run broscribe uninstall --yes with the same options to apply this plan."
                )?;
            }
            Ok(())
        },
    )
}

/// Observe an existing instance lock without creating any files.
fn check_app(data_dir: &Path) -> Result<()> {
    let path = data_dir.join("run/app.lock");
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io_at(&path, e)),
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(error(
            "Quit the BroScribe window for this profile before uninstalling.",
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(Error::io_at(&path, e)),
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;

    #[test]
    fn oversized_receipt_details_cannot_block_retry_or_completion() {
        let root = tempfile::tempdir().unwrap();
        let receipt = root.path().join("receipt.txt");
        let detail = receipt_details_path(&receipt);
        std::fs::write(&detail, vec![b'x'; 64 * 1024 + 1]).unwrap();
        std::fs::write(&receipt, "Done. Removed 1 entries.").unwrap();
        assert!(matches!(deferred_receipt(&receipt).unwrap(), Receipt::Done));
        std::fs::write(&receipt, "Failed. See the uninstall details.").unwrap();
        assert!(matches!(
            deferred_receipt(&receipt).unwrap(),
            Receipt::Retry
        ));
    }

    #[test]
    fn uninstall_counts_session_manifests_or_track_chunks_and_pending_discards() {
        let data = tempfile::tempdir().unwrap();
        let journal = broscribe_core::lifecycle::journal_root(data.path());
        std::fs::create_dir_all(journal.join(".discard")).unwrap();
        let session = journal.join("session-1");
        std::fs::create_dir(&session).unwrap();
        assert_eq!(journal_work(data.path(), true).unwrap(), (false, None));

        let manifest = session.join(broscribe_core::lifecycle::SESSION_MANIFEST);
        std::fs::write(&manifest, b"unreadable manifest").unwrap();
        assert_eq!(journal_work(data.path(), false).unwrap(), (true, None));
        std::fs::remove_file(manifest).unwrap();

        for track in ["mic", "system"] {
            let track = session.join(track);
            std::fs::create_dir(&track).unwrap();
            std::fs::write(track.join("notes.f32"), b"foreign file").unwrap();
            std::fs::write(track.join("0000001.f32"), b"noncanonical").unwrap();
            assert_eq!(journal_work(data.path(), true).unwrap(), (false, None));
            let chunk = track.join("000000.f32");
            std::fs::write(&chunk, 0.25_f32.to_le_bytes()).unwrap();
            assert_eq!(
                journal_work(data.path(), true).unwrap(),
                (false, Some(session.clone()))
            );
            assert_eq!(journal_work(data.path(), false).unwrap(), (false, None));
            std::fs::remove_file(chunk).unwrap();
        }

        broscribe_core::lifecycle::write_discard_intent(&journal, "session-1").unwrap();
        assert_eq!(journal_work(data.path(), false).unwrap(), (true, None));
    }

    #[test]
    fn retry_removes_the_old_helper_run_and_keeps_unknown_entries() {
        let root = tempfile::tempdir().unwrap();
        let unrelated = root.path().join("receipt.txt");
        std::fs::write(&unrelated, b"unrelated").unwrap();
        assert!(!cleanup_helper_run(root.path(), &unrelated).unwrap().changed);
        for with_unknown in [false, true] {
            let run = root
                .path()
                .join("broscribe-uninstall-1234567890abcdef1234567890abcdef");
            std::fs::create_dir(&run).unwrap();
            let receipt = run.join("receipt.txt");
            std::fs::write(&receipt, "Done. Removed 1 entries.").unwrap();
            std::fs::write(receipt_details_path(&receipt), "synthetic detail").unwrap();
            for name in ["uninstall-after-exit.ps1", "release-inventory.json"] {
                std::fs::write(run.join(name), b"synthetic").unwrap();
            }
            let unknown = run.join("keep.txt");
            if with_unknown {
                std::fs::write(&unknown, b"keep me").unwrap();
            }
            let cleanup = cleanup_helper_run(root.path(), &receipt).unwrap();
            assert!(cleanup.changed);
            assert!(!receipt.exists());
            assert_eq!(run.exists(), with_unknown);
            if with_unknown {
                assert_eq!(cleanup.kept, vec![unknown.clone()]);
                assert_eq!(std::fs::read(unknown).unwrap(), b"keep me");
                assert_eq!(std::fs::read_dir(&run).unwrap().count(), 1);
            }
        }
        assert_eq!(std::fs::read(unrelated).unwrap(), b"unrelated");
    }

    #[test]
    fn windows_cleans_up_the_empty_mixed_case_runtime_parent() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("BroScribe");
        std::fs::create_dir(&runtime).unwrap();
        remove_empty_runtime_dir(&runtime, true);
        assert!(!runtime.exists());
    }
}
