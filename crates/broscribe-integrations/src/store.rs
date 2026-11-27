//! Native/Google provider files live in `<data_dir>/integrations/<id>/`.
//! Microsoft has a durable account registry there and account-qualified derived files
//! under `accounts/<AccountId>/`; secrets are versioned platform-store pointers.
//! Folders are 0700 and files 0600 on Unix.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::IntegrationId;
use crate::{IntegrationError, Result};

pub const INTEGRATIONS_DIR: &str = "integrations";
pub const STATE_FILE: &str = "state.json";
pub const CALENDAR_FILE: &str = "calendar.json";
pub const PHOTOS_DIR: &str = "photos";
pub(crate) const STATE_LOCK: &str = "state.lock";
pub(crate) const REFRESH_LOCK: &str = "refresh.lock";

/// `<data_dir>/integrations/<id>`.
pub fn account_dir(data_dir: &Path, id: IntegrationId) -> PathBuf {
    data_dir.join(INTEGRATIONS_DIR).join(id.as_str())
}

/// Google single-slot credentials, or the quarantined pre-registry Microsoft slot.
pub fn tokens_secret_key(id: IntegrationId) -> String {
    format!("integration/{id}/tokens")
}

/// `SecretStore` key of a configured client secret (Google desktop clients).
pub fn client_secret_key(id: IntegrationId) -> String {
    format!("integration/{id}/client_secret")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    #[default]
    SignedOut,
    Active,
    /// The refresh token was rejected or interaction is required; only a
    /// user-started sign-in clears it.
    ReauthRequired,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    #[default]
    Unknown,
    Available,
    /// The service refused it (permission or policy); other capabilities keep working.
    Denied,
    /// Not requested in the configured scopes.
    NotRequested,
    /// The last check failed for another reason.
    Error,
    /// Some selected Microsoft calendar targets could not be read; see the account's
    /// per-target outcomes.
    Partial,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub calendar: Capability,
    pub directory: Capability,
}

/// The signed-in account, as the provider reports it for the granted read scope (Google:
/// the primary calendar's identifier, which is the account address). Never decoded from a
/// token and never read through e-mail access.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// `state.json`. The generation survives sign-out so fencing stays monotonic.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountState {
    /// Google: advanced only by sign-in, sign-out and client retirement; credentials,
    /// directory results and photos belong to it. Native calendars: the opt-in generation.
    pub generation: u64,
    /// Google only: advanced by every change or withdrawal of the explicit calendar
    /// selection (and by calendar denial), never by sign-in lifecycle. Calendar copies
    /// and priors carry it beside `generation`; photos and directory work do not.
    pub selection_generation: u64,
    pub status: AccountStatus,
    /// The one re-authentication notification was sent for this generation.
    pub reauth_notified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountInfo>,
    pub capabilities: Capabilities,
    /// Scopes the provider granted at sign-in (as reported with the tokens, else those
    /// requested). Refreshes never ask for more than this stored grant.
    pub scopes: Vec<String>,
    /// The OAuth client ID the stored tokens were issued to (public, not a secret). Tokens
    /// are never sent with another client ID; `None` for sign-ins predating this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signed_in_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refresh_at: Option<DateTime<Utc>>,
    /// Native calendars and Google: the calendars the user explicitly selected. Empty
    /// (the default) queries nothing; never interpreted as "all" or "primary".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub native_calendar_ids: Vec<String>,
    /// Google only: the `SecretStore` key of the committed credential version. `None`
    /// reads the pre-versioning single slot ([`tokens_secret_key`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    /// Google only: staged or superseded credential versions still to delete. A key equal
    /// to `secret_key` is committed and kept; every other one is removed on recovery.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending_secrets: Vec<String>,
    /// Advanced by every observed calendar denial. A calendar read started before a
    /// denial never publishes after it, even when the generation itself stays.
    #[serde(skip_serializing_if = "is_zero")]
    pub calendar_fence: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// A new versioned Google credential key under its provider namespace.
pub(crate) fn google_secret_version() -> String {
    format!(
        "{}/{:032x}",
        tokens_secret_key(IntegrationId::Google),
        rand::random::<u128>()
    )
}

/// Only Google's single legacy slot or one of its versions; never a key from elsewhere.
pub(crate) fn validate_google_secret_key(key: &str) -> Result<()> {
    let legacy = tokens_secret_key(IntegrationId::Google);
    let valid = key == legacy
        || key
            .strip_prefix(&legacy)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|version| {
                version.len() == 32
                    && version
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            });
    if valid {
        Ok(())
    } else {
        Err(crate::service::service_error(
            "invalid_secret_reference",
            "credential recovery reference is outside its provider namespace",
        ))
    }
}

/// `photos/<generation>/` of a provider-wide account: a photo is served only while
/// `state.json` names that generation, whatever cleanup left behind.
pub(crate) fn provider_photo_dir(dir: &Path, generation: u64) -> PathBuf {
    dir.join(PHOTOS_DIR).join(generation.to_string())
}

/// Creates `dir` (and parents) with mode 0700 on Unix.
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        b.mode(0o700);
    }
    b.create(dir).map_err(|e| IntegrationError::io(dir, e))
}

/// Atomically replaces `path` with `bytes`, mode 0600 on Unix.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    ensure_private_dir(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
    let write = || -> std::io::Result<()> {
        let mut o = OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(IntegrationError::io(path, e));
    }
    Ok(())
}

pub(crate) fn write_json_private<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_private(path, &json_bytes(path, value)?)
}

/// [`write_json_private`], except that a file already holding exactly these bytes is left
/// alone: an unchanged copy is not rewritten and synced again.
pub(crate) fn write_json_private_if_changed<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = json_bytes(path, value)?;
    match read_regular_bytes(path) {
        Ok(existing) if existing == bytes => Ok(()),
        Ok(_) => write_private(path, &bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_private(path, &bytes),
        Err(e) => Err(IntegrationError::io(path, e)),
    }
}

fn json_bytes<T: Serialize>(path: &Path, value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| {
        IntegrationError::Core(broscribe_core::Error::Internal(format!(
            "serialising {}: {e}",
            path.display()
        )))
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_regular_bytes(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = broscribe_platform::open_regular_nofollow(OpenOptions::new().read(true), path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Reads JSON; `None` when the file does not exist.
pub(crate) fn read_json_opt<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match read_regular_bytes(path) {
        Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| {
            IntegrationError::Core(broscribe_core::Error::Schema(format!(
                "{}: {e}",
                path.display()
            )))
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(IntegrationError::io(path, e)),
    }
}

pub(crate) fn remove_file_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(IntegrationError::io(path, e)),
    }
}

/// Check every component within a verified data folder before a transaction
/// creates its locks, writes state or removes cached files. Missing directories
/// may be created by the existing transaction setup.
pub(crate) fn check_profile_directories(data: &Path, target: &Path) -> Result<()> {
    let relative = target.strip_prefix(data).map_err(|_| {
        crate::service::service_error(
            "unsafe_integration_path",
            "integration is outside the profile",
        )
    })?;
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(crate::service::service_error(
            "unsafe_integration_path",
            "invalid integration path",
        ));
    }
    let mut path = data.to_path_buf();
    for part in std::iter::once(None).chain(relative.iter().map(Some)) {
        if let Some(name) = part {
            path.push(name);
        }
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() && !broscribe_platform::release_layout::is_reparse(&meta) => {
            }
            Ok(_) => {
                return Err(crate::service::service_error(
                    "unsafe_integration_path",
                    "An integration directory is a link or is not a regular directory.",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && part.is_some() => {
                return Ok(());
            }
            Err(error) => return Err(IntegrationError::io(&path, error)),
        }
    }
    Ok(())
}

/// Removes only cache files written for people, at the root or under one
/// account generation. Unrecognized entries and links remain in place.
pub(crate) fn remove_cached_photos(path: &Path) -> Result<()> {
    clean_photo_dir(path, true)
}

fn clean_photo_dir(path: &Path, visit_generations: bool) -> Result<()> {
    use std::io::ErrorKind;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(IntegrationError::io(path, error)),
    };
    if broscribe_platform::release_layout::is_reparse(&meta) {
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(IntegrationError::io(
            path,
            std::io::Error::from(ErrorKind::NotADirectory),
        ));
    }
    for entry in std::fs::read_dir(path).map_err(|error| IntegrationError::io(path, error))? {
        let entry = entry.map_err(|error| IntegrationError::io(path, error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let child = entry.path();
        let meta = match std::fs::symlink_metadata(&child) {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(IntegrationError::io(&child, error)),
        };
        if broscribe_platform::release_layout::is_reparse(&meta) {
            continue;
        }
        if meta.is_dir()
            && visit_generations
            && broscribe_platform::profile_cleanup::canonical_generation(name)
        {
            clean_photo_dir(&child, false)?;
        } else if meta.is_file()
            && name
                .strip_suffix(".img")
                .is_some_and(|id| broscribe_core::PersonId::parse(id).is_ok())
        {
            remove_file_if_exists(&child)?;
        }
    }
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(IntegrationError::io(path, error)),
    }
}

/// An exclusive OS file lock (cross-process); released on drop.
pub(crate) struct FileLock {
    file: File,
}

impl FileLock {
    const POLL: Duration = Duration::from_millis(20);

    pub fn acquire(path: &Path, wait: Duration) -> Result<Self> {
        if let Some(dir) = path.parent() {
            ensure_private_dir(dir)?;
        }
        let mut o = OpenOptions::new();
        o.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            o.mode(0o600);
        }
        let file = broscribe_platform::open_regular_nofollow(&mut o, path)
            .map_err(|e| IntegrationError::io(path, e))?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(IntegrationError::Core(broscribe_core::Error::LibraryBusy(
                            path.display().to_string(),
                        )));
                    }
                    std::thread::sleep(Self::POLL.min(deadline - now));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(IntegrationError::io(path, e)),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Whether every integration's cached photos of a forgotten person are gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhotoPurgeStatus {
    Complete,
    /// Some photo stores could not be purged; see the failures. Running the purge again
    /// removes what is left.
    Partial,
}

/// One integration whose photo store could not be fully purged, with a sanitized error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhotoPurgeFailure {
    pub integration: IntegrationId,
    #[serde(flatten)]
    pub error: crate::error::PublicError,
}

/// What [`forget_person_photos`] removed. The forget it follows is already committed: a
/// partial purge is reported beside that success, never as its failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhotoPurge {
    pub status: PhotoPurgeStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<PhotoPurgeFailure>,
}

/// Deletes the cached photo of `person_id` from every integration and account
/// (forget-person). Every store is attempted even after another fails; only an invalid
/// `person_id` is an error, before anything is touched.
pub fn forget_person_photos(data_dir: &Path, person_id: &str) -> Result<PhotoPurge> {
    let name = photo_file_name(person_id)?;
    let mut failures = Vec::new();
    for id in IntegrationId::ALL {
        if let Err(error) = purge_integration_photos(&account_dir(data_dir, id), &name) {
            tracing::warn!(integration = %id, code = error.code(), "cached photos of a forgotten person remain");
            failures.push(PhotoPurgeFailure {
                integration: id,
                error: error.public(),
            });
        }
    }
    Ok(PhotoPurge {
        status: if failures.is_empty() {
            PhotoPurgeStatus::Complete
        } else {
            PhotoPurgeStatus::Partial
        },
        failures,
    })
}

/// Purges one integration's provider and per-account photo stores, continuing past a
/// failing account; returns the first failure.
fn purge_integration_photos(dir: &Path, name: &str) -> Result<()> {
    let _lock = FileLock::acquire(&dir.join(STATE_LOCK), Duration::from_secs(70))?;
    let _accounts_lock = FileLock::acquire(&dir.join("accounts.lock"), Duration::from_secs(70))?;
    let mut first = remove_photo_versions(&dir.join(PHOTOS_DIR), name).err();
    let accounts = dir.join("accounts");
    let entries = match std::fs::read_dir(&accounts) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return first.map_or(Ok(()), Err),
        Err(e) => return Err(first.unwrap_or_else(|| IntegrationError::io(&accounts, e))),
    };
    for entry in entries {
        let result = entry
            .map_err(|e| IntegrationError::io(&accounts, e))
            .and_then(|entry| {
                let is_dir = entry
                    .file_type()
                    .map_err(|e| IntegrationError::io(&entry.path(), e))?
                    .is_dir();
                if is_dir {
                    remove_photo_versions(&entry.path().join(PHOTOS_DIR), name)?;
                }
                Ok(())
            });
        if let Err(error) = result
            && first.is_none()
        {
            first = Some(error);
        }
    }
    first.map_or(Ok(()), Err)
}

/// Removes `photos/<name>` and `photos/<generation>/<name>`, trying every version even
/// after one fails; returns the first failure. Only a missing `photos/` means nothing is
/// left: any other failure to list it leaves generations unvisited and is reported.
fn remove_photo_versions(photos: &Path, name: &str) -> Result<()> {
    let mut first = remove_file_if_exists(&photos.join(name)).err();
    let generations = match std::fs::read_dir(photos) {
        Ok(generations) => generations,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return first.map_or(Ok(()), Err),
        Err(e) => return Err(first.unwrap_or_else(|| IntegrationError::io(photos, e))),
    };
    for generation in generations {
        let result = generation
            .map_err(|e| IntegrationError::io(photos, e))
            .and_then(|generation| {
                let is_dir = generation
                    .file_type()
                    .map_err(|e| IntegrationError::io(&generation.path(), e))?
                    .is_dir();
                if is_dir {
                    remove_file_if_exists(&generation.path().join(name))?;
                }
                Ok(())
            });
        if let Err(error) = result
            && first.is_none()
        {
            first = Some(error);
        }
    }
    first.map_or(Ok(()), Err)
}

/// The cached photos of `people` (each a caller key and its person ID), for display.
/// Each account's authorized photo directory is resolved once for this call from its
/// durable state and the current configuration, never kept across calls, so a photo left
/// by an interrupted cleanup or an earlier sign-in is never served. `load` reads each
/// person's most recently cached photo; a person with none, or whose file is removed
/// meanwhile or fails to load, is left out. After every read the directories are resolved
/// again, and a value is returned only while the directory it was read from is still
/// authorized: a sign-out, client retirement or lost directory permission that commits
/// while the files are read hides what was read. No lock is held.
pub fn cached_photos<'p, K, T>(
    data_dir: &Path,
    people: impl IntoIterator<Item = (K, &'p str)>,
    mut load: impl FnMut(&Path) -> Option<T>,
) -> Vec<(K, T)> {
    let roots = photo_roots(data_dir);
    let mut loaded = Vec::new();
    for (key, person_id) in people {
        let Ok(name) = photo_file_name(person_id) else {
            continue;
        };
        let newest = roots
            .iter()
            .filter_map(|root| {
                let path = root.join(&name);
                Some((std::fs::metadata(&path).ok()?.modified().ok()?, root, path))
            })
            .max_by_key(|(modified, _, _)| *modified);
        if let Some((_, root, path)) = newest
            && let Some(value) = load(&path)
        {
            loaded.push((root, key, value));
        }
    }
    if loaded.is_empty() {
        return Vec::new();
    }
    let current = photo_roots(data_dir);
    loaded
        .into_iter()
        .filter(|(root, _, _)| current.contains(root))
        .map(|(_, key, value)| (key, value))
        .collect()
}

/// Every account's authorized photo directory: a provider-wide account's per
/// [`provider_photo_root`], each Microsoft account's per `microsoft_photo_roots`.
/// Unreadable state or configuration authorizes nothing.
fn photo_roots(data_dir: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for id in IntegrationId::ALL {
        if id.is_graph() {
            roots.extend(crate::service::microsoft_photo_roots(data_dir, id).unwrap_or_default());
        } else if !id.is_native_calendar()
            && let Some(root) = provider_photo_root(data_dir, id)
        {
            roots.push(root);
        }
    }
    roots
}

/// The photo directory of a provider-wide account at its current generation, only while
/// that account is active, issued to the configured client, its directory access granted,
/// not denied and still requested by the current configuration.
fn provider_photo_root(data_dir: &Path, id: IntegrationId) -> Option<PathBuf> {
    let dir = account_dir(data_dir, id);
    let state: AccountState = read_json_opt(&dir.join(STATE_FILE)).ok()??;
    let settings = crate::IntegrationsConfig::load_settings(data_dir, id).ok()?;
    let current = state.status == AccountStatus::Active
        && state.capabilities.directory != Capability::Denied
        && crate::service::google_directory_granted(&state.scopes)
        && crate::config::directory_requested(id, &settings)
        && state.client_id.as_deref().is_some()
        && state.client_id.as_deref() == settings.effective_client_id(id);
    current.then(|| provider_photo_dir(&dir, state.generation))
}

/// The account's current generation (0 before the first sign-in). A caller
/// holding results from an older generation must discard them: the account
/// signed out or was replaced since.
pub fn account_generation(data_dir: &Path, id: IntegrationId) -> Result<u64> {
    if id.is_graph() {
        return Err(crate::service::service_error(
            "account_required",
            "Microsoft generations require an explicit account handle",
        ));
    }
    let state: Option<AccountState> = read_json_opt(&account_dir(data_dir, id).join(STATE_FILE))?;
    Ok(state.map_or(0, |s| s.generation))
}

/// `<person_id>.img`; person IDs are `[A-Za-z0-9_-]` only.
pub(crate) fn photo_file_name(person_id: &str) -> Result<String> {
    if person_id.is_empty()
        || !person_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(IntegrationError::Core(broscribe_core::Error::InvalidInput(
            format!("invalid person ID {person_id:?}"),
        )));
    }
    Ok(format!("{person_id}.img"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::cached_photo;

    #[test]
    #[cfg(unix)]
    fn private_files_are_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a/b/c.json");
        write_json_private(&p, &AccountState::default()).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dmode = std::fs::metadata(p.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dmode, 0o700);
        let back: AccountState = read_json_opt(&p).unwrap().unwrap();
        assert_eq!(back, AccountState::default());
        assert!(
            read_json_opt::<AccountState>(&tmp.path().join("none"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[cfg(unix)]
    fn unchanged_json_rejects_a_link_with_matching_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target.json");
        let link = tmp.path().join("link.json");
        let state = AccountState::default();
        write_json_private(&target, &state).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(read_json_opt::<AccountState>(&link).is_err());
        assert!(write_json_private_if_changed(&link, &state).is_err());
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(read_json_opt::<AccountState>(&target).unwrap(), Some(state));
    }

    #[test]
    fn photo_names_are_checked() {
        assert_eq!(photo_file_name("p-3c9e1a").unwrap(), "p-3c9e1a.img");
        assert!(photo_file_name("../x").is_err());
        assert!(photo_file_name("").is_err());
    }

    #[test]
    fn cached_photo_follows_durable_state_not_leftover_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(cached_photo(tmp.path(), "p-synthetic"), None);
        assert_eq!(cached_photo(tmp.path(), "../escape"), None);
        assert_eq!(
            account_generation(tmp.path(), IntegrationId::Google).unwrap(),
            0
        );
        let config = broscribe_core::config::Config::path(tmp.path());
        let workspace =
            "[integration.google]\nclient_id = \"synthetic-client\"\nworkspace = true\n";
        std::fs::write(&config, workspace).unwrap();
        let dir = account_dir(tmp.path(), IntegrationId::Google);
        // Left behind by a cleanup that failed: legacy flat file and an older generation.
        write_private(&dir.join(PHOTOS_DIR).join("p-synthetic.img"), b"old").unwrap();
        write_private(&provider_photo_dir(&dir, 3).join("p-synthetic.img"), b"old").unwrap();
        let mut state = AccountState {
            generation: 4,
            status: AccountStatus::Active,
            client_id: Some("synthetic-client".into()),
            scopes: vec![crate::config::GOOGLE_DIRECTORY_READONLY.into()],
            ..Default::default()
        };
        write_json_private(&dir.join(STATE_FILE), &state).unwrap();
        assert_eq!(cached_photo(tmp.path(), "p-synthetic"), None);
        let current = provider_photo_dir(&dir, 4).join("p-synthetic.img");
        write_private(&current, b"img").unwrap();
        assert_eq!(
            cached_photo(tmp.path(), "p-synthetic"),
            Some(current.clone())
        );
        assert_eq!(
            account_generation(tmp.path(), IntegrationId::Google).unwrap(),
            4
        );
        // Workspace turned off in the current configuration, or a grant without the
        // directory: the file stays, the photo is not served.
        std::fs::write(
            &config,
            "[integration.google]\nclient_id = \"synthetic-client\"\n",
        )
        .unwrap();
        assert_eq!(cached_photo(tmp.path(), "p-synthetic"), None);
        std::fs::write(&config, workspace).unwrap();
        state.scopes.clear();
        write_json_private(&dir.join(STATE_FILE), &state).unwrap();
        assert_eq!(cached_photo(tmp.path(), "p-synthetic"), None);
        state.scopes = vec![crate::config::GOOGLE_DIRECTORY_READONLY.into()];
        write_json_private(&dir.join(STATE_FILE), &state).unwrap();
        assert_eq!(
            cached_photo(tmp.path(), "p-synthetic"),
            Some(current.clone())
        );
        // A calendar selection change or calendar denial is not directory identity.
        state.selection_generation += 1;
        state.capabilities.calendar = Capability::Denied;
        write_json_private(&dir.join(STATE_FILE), &state).unwrap();
        assert_eq!(
            cached_photo(tmp.path(), "p-synthetic"),
            Some(current.clone())
        );
        // A sign-out that commits while the photos are being read hides what was read,
        // though the file was read before it.
        let signed_out = AccountState {
            status: AccountStatus::SignedOut,
            ..state.clone()
        };
        let mut read_before_sign_out = false;
        let shown = cached_photos(tmp.path(), [("p-synthetic", "p-synthetic")], |path| {
            let bytes = std::fs::read(path).ok();
            read_before_sign_out = bytes.as_deref() == Some(b"img".as_slice());
            write_json_private(&dir.join(STATE_FILE), &signed_out).unwrap();
            bytes
        });
        assert!(read_before_sign_out);
        assert!(shown.is_empty());
        write_json_private(&dir.join(STATE_FILE), &state).unwrap();
        assert_eq!(
            cached_photo(tmp.path(), "p-synthetic"),
            Some(current.clone())
        );
        // Signed out (or issued to another client): the file stays, the photo is not served.
        state.status = AccountStatus::SignedOut;
        write_json_private(&dir.join(STATE_FILE), &state).unwrap();
        assert_eq!(cached_photo(tmp.path(), "p-synthetic"), None);
        state.status = AccountStatus::Active;
        state.client_id = Some("another-client".into());
        write_json_private(&dir.join(STATE_FILE), &state).unwrap();
        assert_eq!(cached_photo(tmp.path(), "p-synthetic"), None);
        let purge = forget_person_photos(tmp.path(), "p-synthetic").unwrap();
        assert_eq!(purge.status, PhotoPurgeStatus::Complete);
        assert!(purge.failures.is_empty());
        assert!(!current.exists());
        assert!(!dir.join(PHOTOS_DIR).join("p-synthetic.img").exists());
        assert!(!provider_photo_dir(&dir, 3).join("p-synthetic.img").exists());
    }

    #[test]
    fn a_failing_photo_store_reports_a_partial_purge_and_the_others_are_still_purged() {
        let tmp = tempfile::tempdir().unwrap();
        let google = account_dir(tmp.path(), IntegrationId::Google);
        // Synthetic obstruction: a directory where the photo file would be cannot be
        // removed as a file.
        let blocked = provider_photo_dir(&google, 2).join("p-synthetic.img");
        ensure_private_dir(&blocked).unwrap();
        let kept_going = provider_photo_dir(&google, 1).join("p-synthetic.img");
        write_private(&kept_going, b"img").unwrap();
        let microsoft = account_dir(tmp.path(), IntegrationId::MicrosoftGraph)
            .join("accounts")
            .join("synthetic-account")
            .join(PHOTOS_DIR)
            .join("5")
            .join("p-synthetic.img");
        write_private(&microsoft, b"img").unwrap();
        let purge = forget_person_photos(tmp.path(), "p-synthetic").unwrap();
        assert_eq!(purge.status, PhotoPurgeStatus::Partial);
        assert_eq!(purge.failures.len(), 1);
        assert_eq!(purge.failures[0].integration, IntegrationId::Google);
        let text = serde_json::to_string(&purge).unwrap();
        let root = tmp.path().to_string_lossy();
        assert!(!text.contains(root.as_ref()), "{text}");
        assert!(!kept_going.exists());
        assert!(!microsoft.exists());
        // Retrying after the obstruction is gone completes the purge.
        std::fs::remove_dir(&blocked).unwrap();
        assert_eq!(
            forget_person_photos(tmp.path(), "p-synthetic")
                .unwrap()
                .status,
            PhotoPurgeStatus::Complete
        );
        assert!(forget_person_photos(tmp.path(), "../escape").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn an_unlistable_photo_store_is_reported_partial_and_the_others_are_still_purged() {
        use std::os::unix::fs::PermissionsExt as _;
        /// Makes the store listable again however the test ends.
        struct Restore<'a>(&'a Path);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700));
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let google = account_dir(tmp.path(), IntegrationId::Google);
        let photos = google.join(PHOTOS_DIR);
        // No flat photo: only listing `photos/` finds the generation that holds one.
        let retained = provider_photo_dir(&google, 4).join("p-synthetic.img");
        write_private(&retained, b"img").unwrap();
        let microsoft = account_dir(tmp.path(), IntegrationId::MicrosoftGraph)
            .join("accounts")
            .join("synthetic-account")
            .join(PHOTOS_DIR)
            .join("5")
            .join("p-synthetic.img");
        write_private(&microsoft, b"img").unwrap();
        // Traversable but not listable.
        std::fs::set_permissions(&photos, std::fs::Permissions::from_mode(0o111)).unwrap();
        let restore = Restore(&photos);
        if std::fs::read_dir(&photos).is_ok() {
            // A privileged user lists any directory: this failure needs an ordinary user.
            return;
        }
        let purge = forget_person_photos(tmp.path(), "p-synthetic").unwrap();
        assert_eq!(purge.status, PhotoPurgeStatus::Partial);
        assert_eq!(purge.failures.len(), 1);
        assert_eq!(purge.failures[0].integration, IntegrationId::Google);
        assert_eq!(purge.failures[0].error.code, "io");
        let text = serde_json::to_string(&purge).unwrap();
        let root = tmp.path().to_string_lossy();
        assert!(!text.contains(root.as_ref()), "{text}");
        assert!(!microsoft.exists());
        drop(restore);
        assert!(retained.exists());
        // Retrying once the store is listable removes what was left.
        assert_eq!(
            forget_person_photos(tmp.path(), "p-synthetic")
                .unwrap()
                .status,
            PhotoPurgeStatus::Complete
        );
        assert!(!retained.exists());
    }

    #[test]
    fn google_secret_references_stay_in_their_namespace() {
        let version = google_secret_version();
        validate_google_secret_key(&version).unwrap();
        validate_google_secret_key(&tokens_secret_key(IntegrationId::Google)).unwrap();
        for key in [
            "integration/google/tokens/../../microsoft-graph/tokens",
            "integration/microsoft-graph/tokens",
            "integration/google/client_secret",
            "integration/google/tokens/ABCDEF",
        ] {
            assert_eq!(
                validate_google_secret_key(key).unwrap_err().code(),
                "invalid_secret_reference"
            );
        }
    }
}
