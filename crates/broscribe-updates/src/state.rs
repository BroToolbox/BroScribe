//! The updater's state folder: an exclusive owner lock, the replacement
//! journal and the highest manifest accepted so far; and the owner record
//! of the staging folder beside the installation.
//!
//! The lock is an OS file lock held for the [`crate::Updater`]'s lifetime;
//! the OS releases it when the owner exits or dies, which is what entitles
//! the next owner to recover an interrupted replacement.
//!
//! The staging folder is shared by every state folder updating the same
//! installation, and its contents (the incoming payload, the previous
//! installation kept for rollback) outlive the process that made them. Its
//! first entry is therefore a [`StageOwner`] record naming the state folder
//! whose journal it belongs to and the update it holds: only that state
//! folder ever removes or uses it, so another one never deletes an
//! update or a rollback copy still pending elsewhere.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use broscribe_core::{Error, Result};
use broscribe_platform::same_path;
use broscribe_platform::uninstall::DirectoryIdentity;
use broscribe_platform::update::{PayloadIdentity, SignerIdentity, UpdatePlatform};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Cancel, Limits, archive, codes, fail, inventory};

const LOCK: &str = ".lock";
const JOURNAL: &str = "journal.json";
const INSTALLED_INVENTORY: &str = "installed-inventory.json";
const SEEN: &str = "seen.json";
/// The staging folder's owner record; written first, removed last.
const STAGE_OWNER: &str = "owner.json";
const STAGE_OWNER_TEMP: &str = "owner.json.tmp";
const ARCHIVE_ENTRIES: &str = "archive-entries.json";
const ARCHIVE_ENTRIES_TEMP: &str = "archive-entries.json.tmp";
const MAX_STATE_BYTES: u64 = 256 * 1024;
// Up to the archive entry limit with 1 KiB paths and per-entry metadata.
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
/// Schema 2 journals the staged signer (schema 1 did not).
pub(crate) const JOURNAL_SCHEMA: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// Verified payload staged; the installation is untouched.
    Staged,
    /// Replacement started; the filesystem may hold any intermediate state.
    Swapping,
    /// The new payload is at the installation root, not yet verified there.
    Swapped,
    /// Installed and verified; the previous copy is kept until confirmed.
    Committed,
    /// Restoring the previous copy.
    RollingBack,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Journal {
    pub schema: u32,
    pub phase: Phase,
    pub install_root: PathBuf,
    pub staging: PathBuf,
    pub payload_root: String,
    pub from: PayloadIdentity,
    pub to: PayloadIdentity,
    pub installed_signer: SignerIdentity,
    /// The staged payload's signer, as natively verified when it was staged.
    pub signer: SignerIdentity,
    // Old staged journals lack an inventory and must be downloaded again.
    #[serde(default)]
    pub inventory: Option<crate::inventory::Inventory>,
    #[serde(default)]
    pub preserved_files: Vec<String>,
    pub owner_pid: u32,
    pub fence: Option<u64>,
    pub updated_at: DateTime<Utc>,
}

impl Journal {
    fn install_name(&self) -> &std::ffi::OsStr {
        self.install_root
            .file_name()
            .expect("installation roots have a name")
    }

    /// Where the staged payload waits.
    pub fn incoming(&self) -> PathBuf {
        self.staging.join("incoming").join(&self.payload_root)
    }

    /// Where the replaced installation is kept.
    pub fn previous(&self) -> PathBuf {
        self.staging.join("previous").join(self.install_name())
    }

    /// Where a rollback parks the rejected payload.
    pub fn discard(&self) -> PathBuf {
        self.staging.join("discard").join(self.install_name())
    }
}

/// An inventory is usable only for the installation and build it came from.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstalledInventory {
    schema: u32,
    root: PathBuf,
    identity: PayloadIdentity,
    directory: DirectoryIdentity,
    #[serde(default)]
    recorded_at: Option<DateTime<Utc>>,
    contents: inventory::Inventory,
}

impl InstalledInventory {
    fn applies(
        &self,
        install_root: &Path,
        tree: &Path,
        identity: &PayloadIdentity,
        platform: &dyn UpdatePlatform,
    ) -> Result<bool> {
        Ok(same_path(&self.root, install_root)
            && (&self.identity == identity
                || platform.directory_identity(tree)? == Some(self.directory)))
    }

    pub(crate) fn check_identity(&self, tree: &Path, identity: &PayloadIdentity) -> Result<()> {
        if &self.identity != identity {
            return Err(fail(
                codes::INSTALLATION_CHANGED_OUTSIDE,
                format!(
                    "The installation at {} was changed outside BroScribe. Close BroScribe, delete the old installation folder and install the current release again. Keep your data folder and library.",
                    tree.display()
                ),
            ));
        }
        Ok(())
    }
}

/// Monotonic record of the newest manifest accepted (freeze/replay defence).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Seen {
    pub highest_build: u64,
    #[serde(default)]
    pub highest_version: Option<String>,
    #[serde(default)]
    pub feed_scope: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub previous_scopes: std::collections::BTreeMap<String, u64>,
}

/// Who a staging folder belongs to: the state folder whose journal owns it
/// and the update it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageOwner {
    schema: u32,
    /// The owning state folder, canonical.
    state_dir: PathBuf,
    from: PayloadIdentity,
    to: PayloadIdentity,
}

/// Written only after the signed manifest's archive digest has matched and
/// the archive table has been validated. Binds the list to the stage claim.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArchiveEntries {
    owner: StageOwner,
    entries: archive::Entries,
}

/// Whose a staging folder is, as seen by one state folder.
pub(crate) enum StageClaim {
    /// No staging folder.
    Absent,
    /// A folder with no owner record and nothing but a claim interrupted
    /// before its record was renamed into place.
    Unclaimed,
    /// This state folder's, holding the update asked about (if any).
    Ours,
    /// This state folder's, but holding another update than asked about.
    OursOtherUpdate,
    /// Another state folder's: its canonical path.
    Foreign(PathBuf),
    /// A folder with contents but no owner record: nobody can tell whose.
    Unknown,
}

/// Removes a staging folder this state folder may remove, its owner record
/// last, so an interrupted removal still says whose the rest is. A link in
/// its place is refused, never followed.
pub(crate) fn remove_stage(staging: &Path) -> Result<()> {
    match std::fs::symlink_metadata(staging) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io_at(staging, e)),
        Ok(meta) if !meta.is_dir() || broscribe_platform::release_layout::is_reparse(&meta) => {
            return Err(fail(
                codes::ARCHIVE_UNSAFE,
                format!(
                    "{} is not a folder; refusing to remove it",
                    staging.display()
                ),
            ));
        }
        Ok(_) => {}
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(staging).map_err(|e| Error::io_at(staging, e))? {
        let entry = entry.map_err(|e| Error::io_at(staging, e))?;
        let name = entry.file_name();
        let path = entry.path();
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|error| Error::io_at(&path, error))?;
        let known = [
            STAGE_OWNER,
            STAGE_OWNER_TEMP,
            ARCHIVE_ENTRIES,
            ARCHIVE_ENTRIES_TEMP,
            "download.zip",
        ]
        .iter()
        .any(|file| name == *file)
            || RELEASE_SLOTS.iter().any(|slot| {
                name == deletion_record(slot).as_str()
                    || name == format!("{}.tmp", deletion_record(slot)).as_str()
            });
        if !known
            || !metadata.is_file()
            || broscribe_platform::release_layout::is_reparse(&metadata)
        {
            return Err(fail(
                codes::RELEASE_TREE_UNSAFE,
                format!(
                    "Unexpected update staging entry: {}. Move it out and run broscribe update recover.",
                    path.display()
                ),
            ));
        }
        if name != STAGE_OWNER {
            files.push(path);
        }
    }
    for path in files {
        std::fs::remove_file(&path).map_err(|error| Error::io_at(&path, error))?;
    }
    let owner = staging.join(STAGE_OWNER);
    match std::fs::remove_file(&owner) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io_at(&owner, e)),
    }
    std::fs::remove_dir(staging).map_err(|e| Error::io_at(staging, e))
}

pub(crate) struct StateDir {
    /// Canonical, as owner records name it.
    dir: PathBuf,
    _lock: ExclusiveLock,
}

impl StateDir {
    /// Takes the exclusive owner lock or fails with [`codes::BUSY`].
    pub fn open(dir: &Path) -> Result<Self> {
        ensure_update_directory(dir)?;
        let dir = std::fs::canonicalize(dir).map_err(|e| Error::io_at(dir, e))?;
        let lock_path = dir.join(LOCK);
        let lock = lock_exclusive(&lock_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let metadata = lock
                .0
                .metadata()
                .map_err(|error| Error::io_at(&lock_path, error))?;
            check_private_record(&lock_path, &metadata)?;
            if metadata.mode() & 0o7777 != 0o600 {
                lock.0
                    .set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|error| Error::io_at(&lock_path, error))?;
            }
        }
        Ok(Self { _lock: lock, dir })
    }

    /// Whose `staging` is, for this state folder; call it under the
    /// installation lock. With `update` (`from`, `to`), a stage of this
    /// state folder is [`StageClaim::Ours`] only while it holds exactly that
    /// update. A staging path that is not a real folder (a link, a file) or
    /// an unreadable owner record is refused.
    pub fn stage_claim(
        &self,
        staging: &Path,
        update: Option<(&PayloadIdentity, &PayloadIdentity)>,
    ) -> Result<StageClaim> {
        stage_claim(staging, update, |owner| same_path(owner, &self.dir))
    }

    /// Creates the staging folder (which must not exist) with this state
    /// folder's owner record for the update `from` → `to` as its first entry.
    pub fn claim_stage(
        &self,
        staging: &Path,
        from: &PayloadIdentity,
        to: &PayloadIdentity,
        platform: &dyn UpdatePlatform,
    ) -> Result<()> {
        create_private_directory(staging)?;
        let owner = StageOwner {
            schema: 1,
            state_dir: self.dir.clone(),
            from: from.clone(),
            to: to.clone(),
        };
        write_json(staging, STAGE_OWNER, &owner, platform)?;
        let parent = staging.parent().expect("staging folders have a parent");
        platform.sync_dir(parent)
    }

    pub fn remember_archive_entries(
        &self,
        staging: &Path,
        entries: archive::Entries,
        platform: &dyn UpdatePlatform,
    ) -> Result<()> {
        let owner: StageOwner = read_json(staging, STAGE_OWNER)?
            .filter(|owner: &StageOwner| same_path(&owner.state_dir, &self.dir))
            .ok_or_else(|| {
                fail(
                    codes::RECOVERY_FAILED,
                    "the extraction has no matching staging owner",
                )
            })?;
        write_json(
            staging,
            ARCHIVE_ENTRIES,
            &ArchiveEntries { owner, entries },
            platform,
        )
    }

    pub fn journal(&self) -> Result<Option<Journal>> {
        Self::read_journal(&self.dir)
    }

    /// Bounded schema-checked observation without creating state or taking its lock.
    pub fn read_journal(dir: &Path) -> Result<Option<Journal>> {
        let journal: Option<Journal> = read_json(dir, JOURNAL)?;
        if let Some(journal) = &journal
            && journal.schema != JOURNAL_SCHEMA
        {
            return Err(fail(
                codes::RECOVERY_FAILED,
                "the update journal has an unknown schema; nothing was changed",
            ));
        }
        Ok(journal)
    }

    pub fn write_journal(&self, journal: &Journal, platform: &dyn UpdatePlatform) -> Result<()> {
        write_json(&self.dir, JOURNAL, journal, platform)
    }

    pub fn clear_journal(&self, platform: &dyn UpdatePlatform) -> Result<()> {
        let path = self.dir.join(JOURNAL);
        match std::fs::remove_file(&path) {
            Ok(()) => platform.sync_dir(&self.dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io_at(&path, e)),
        }
    }

    pub fn installed_inventory(
        &self,
        profiles: &[PathBuf],
        tree: (&Path, &Path, &PayloadIdentity),
        platform: &dyn UpdatePlatform,
        warnings: &mut Vec<InventoryWarning>,
    ) -> Result<Option<InstalledInventory>> {
        newest_inventory(
            &self.dir,
            profiles.iter().map(|data| data.join("updates")),
            tree,
            platform,
            warnings,
        )
    }

    pub fn remember_installation(
        &self,
        journal: &Journal,
        platform: &dyn UpdatePlatform,
    ) -> Result<()> {
        if let Some(contents) = &journal.inventory {
            let record = InstalledInventory {
                schema: 1,
                root: journal.install_root.clone(),
                identity: journal.to.clone(),
                directory: platform
                    .directory_identity(&journal.install_root)?
                    .ok_or_else(|| {
                        fail(
                            codes::INSTALLATION_CHANGED,
                            "the confirmed installation directory is missing or has changed",
                        )
                    })?,
                recorded_at: Some(Utc::now()),
                contents: contents.clone(),
            };
            write_json(&self.dir, INSTALLED_INVENTORY, &record, platform)?;
        }
        Ok(())
    }

    pub fn seen(&self) -> Result<Seen> {
        Ok(read_json(&self.dir, SEEN)?.unwrap_or_default())
    }

    pub fn highest_seen(
        &self,
        profiles: &[PathBuf],
        scope: &str,
        warnings: &mut Vec<InventoryWarning>,
    ) -> Result<u64> {
        let mut highest = 0;
        for_inventory_sources(
            &self.dir,
            profiles.iter().map(|data| data.join("updates")),
            warnings,
            |dir| {
                let Some(seen) = read_json::<Seen>(dir, SEEN)? else {
                    return Ok(());
                };
                highest = highest.max(seen.previous_scopes.get(scope).copied().unwrap_or(0));
                if seen.feed_scope.as_deref() == Some(scope)
                    || (seen.feed_scope.is_none() && same_path(dir, &self.dir))
                {
                    highest = highest.max(seen.highest_build);
                }
                Ok(())
            },
        )?;
        Ok(highest)
    }

    pub fn write_seen(&self, seen: &Seen, platform: &dyn UpdatePlatform) -> Result<()> {
        write_json(&self.dir, SEEN, seen, platform)
    }
}

/// Contents of an installation protected by a locally recorded, authenticated
/// inventory. The caller also checks the platform's native seal.
pub struct RemovalInventory {
    contents: inventory::Inventory,
}

impl RemovalInventory {
    pub fn validate(&self, root: &Path, platform: &dyn UpdatePlatform) -> Result<()> {
        inventory::verify(
            root,
            &self.contents,
            &Limits::default(),
            &Cancel::new(),
            platform.inventory_exceptions(),
            platform.ignores_finder_metadata(),
        )
    }

    pub fn windows_plan(&self) -> Result<Vec<u8>> {
        inventory::windows_plan(&self.contents)
    }
}

/// An unusable update-state source belonging to another profile.
pub struct InventoryWarning {
    pub state_dir: PathBuf,
    pub error: Error,
}

/// Find the authenticated inventory for this exact installation and build
/// before profile cleanup can remove its update state. The first directory
/// is the current profile.
pub fn removal_inventory(
    root: &Path,
    state_dirs: &[PathBuf],
    platform: &dyn UpdatePlatform,
) -> Result<Option<RemovalInventory>> {
    let Some((current, others)) = state_dirs.split_first() else {
        return Ok(None);
    };
    removal_inventory_with_warnings(root, current, others, platform, &mut Vec::new())
}

pub fn removal_inventory_with_warnings(
    root: &Path,
    current: &Path,
    other_dirs: &[PathBuf],
    platform: &dyn UpdatePlatform,
    warnings: &mut Vec<InventoryWarning>,
) -> Result<Option<RemovalInventory>> {
    let (mut journals, mut records) = (Vec::new(), Vec::new());
    for_inventory_sources(current, other_dirs.iter().cloned(), warnings, |dir| {
        inspect_update_directory(dir)?;
        let journal = StateDir::read_journal(dir)?;
        let record = timed_inventory(dir)?;
        if let Some(journal) = journal
            && same_path(&journal.install_root, root)
            && journal.inventory.is_some()
        {
            journals.push(journal);
        }
        if let Some(record) = record
            && same_path(&record.1.root, root)
        {
            records.push(record);
        }
        Ok(())
    })?;
    if records.is_empty() && journals.is_empty() {
        return Ok(None);
    }
    let identity = platform.inspect(root)?;
    if let Some(journal) = journals
        .into_iter()
        .filter(|journal| journal.to == identity)
        .max_by_key(|journal| journal.updated_at)
    {
        return Ok(journal
            .inventory
            .map(|contents| RemovalInventory { contents }));
    }
    select_inventory(records, root, root, &identity, platform)?
        .map(|record| {
            record.check_identity(root, &identity)?;
            Ok(RemovalInventory {
                contents: record.contents,
            })
        })
        .transpose()
}

fn for_inventory_sources(
    current: &Path,
    others: impl IntoIterator<Item = PathBuf>,
    warnings: &mut Vec<InventoryWarning>,
    mut read: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    read(current)?;
    let mut visited = std::collections::BTreeSet::from([broscribe_platform::stable_path(current)]);
    for dir in others {
        if visited.insert(broscribe_platform::stable_path(&dir))
            && let Err(error) = read(&dir)
            && !warnings
                .iter()
                .any(|warning| same_path(&dir, &warning.state_dir))
        {
            warnings.push(InventoryWarning {
                state_dir: dir,
                error,
            });
        }
    }
    Ok(())
}

fn newest_inventory(
    current: &Path,
    others: impl IntoIterator<Item = PathBuf>,
    tree: (&Path, &Path, &PayloadIdentity),
    platform: &dyn UpdatePlatform,
    warnings: &mut Vec<InventoryWarning>,
) -> Result<Option<InstalledInventory>> {
    let mut records = Vec::new();
    for_inventory_sources(current, others, warnings, |dir| {
        inspect_update_directory(dir)?;
        records.extend(timed_inventory(dir)?);
        Ok(())
    })?;
    let (install_root, path, identity) = tree;
    select_inventory(records, install_root, path, identity, platform)
}

type TimedInventory = (DateTime<Utc>, InstalledInventory);

fn timed_inventory(dir: &Path) -> Result<Option<TimedInventory>> {
    read_installed_inventory(dir)?
        .map(|record| {
            let time = if let Some(recorded_at) = record.recorded_at {
                recorded_at
            } else {
                let path = dir.join(INSTALLED_INVENTORY);
                std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .map(DateTime::from)
                    .map_err(|error| Error::io_at(&path, error))?
            };
            Ok((time, record))
        })
        .transpose()
}

fn select_inventory(
    records: Vec<TimedInventory>,
    install_root: &Path,
    tree: &Path,
    identity: &PayloadIdentity,
    platform: &dyn UpdatePlatform,
) -> Result<Option<InstalledInventory>> {
    let mut newest = None;
    for (time, record) in records {
        if record.applies(install_root, tree, identity, platform)?
            && newest.as_ref().is_none_or(|(previous, _)| time > *previous)
        {
            newest = Some((time, record));
        }
    }
    Ok(newest.map(|(_, record)| record))
}

fn read_installed_inventory(dir: &Path) -> Result<Option<InstalledInventory>> {
    let installed: Option<InstalledInventory> = read_json(dir, INSTALLED_INVENTORY)?;
    if installed.as_ref().is_some_and(|record| record.schema != 1) {
        return Err(fail(
            codes::RECOVERY_FAILED,
            "the installed inventory has an unknown schema",
        ));
    }
    Ok(installed)
}

/// Determines which authenticated inventory applies even if an exchange was
/// interrupted with the old tree in `incoming` or the new tree in `previous`.
pub(crate) fn validate_swap_tree(
    root: &Path,
    journal: &Journal,
    identity: &PayloadIdentity,
    prior: Option<&InstalledInventory>,
    platform: &dyn UpdatePlatform,
    limits: &Limits,
) -> Result<()> {
    let (contents, signer) = if identity == &journal.to {
        (journal.inventory.as_ref(), &journal.signer)
    } else if identity == &journal.from {
        let contents = match prior {
            Some(record) if record.applies(&journal.install_root, root, identity, platform)? => {
                record.check_identity(root, identity)?;
                Some(&record.contents)
            }
            _ => None,
        };
        (contents, &journal.installed_signer)
    } else {
        return Err(fail(
            codes::RELEASE_TREE_UNSAFE,
            format!(
                "Unexpected release identity at {}. Restore the release files before retrying.",
                root.display()
            ),
        ));
    };
    if let Some(contents) = contents {
        inventory::verify(
            root,
            contents,
            limits,
            &Cancel::new(),
            platform.inventory_exceptions(),
            platform.ignores_finder_metadata(),
        )?;
    }
    platform.validate_release_tree(root, signer, contents.is_some())
}

/// Validates one slot that can hold an installation moved during a swap.
pub(crate) fn check_release_slot(
    slot: &Path,
    name: &std::ffi::OsStr,
    validate: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let invalid = |path: &Path| {
        fail(
            codes::RELEASE_TREE_UNSAFE,
            format!(
                "Unexpected installation entry: {}. Move it out of update staging before retrying.",
                path.display()
            ),
        )
    };
    match std::fs::symlink_metadata(slot) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::io_at(slot, error)),
        Ok(meta) if !meta.is_dir() || broscribe_platform::release_layout::is_reparse(&meta) => {
            return Err(invalid(slot));
        }
        Ok(_) => {}
    }
    let mut found = false;
    let enumeration_error = |error| {
        if slot.file_name().is_some_and(|name| name == "incoming") {
            fail(
                codes::RELEASE_TREE_UNSAFE,
                format!(
                    "Cannot enumerate incoming update slot {}: {error}",
                    slot.display()
                ),
            )
        } else {
            Error::io_at(slot, error)
        }
    };
    for entry in std::fs::read_dir(slot).map_err(enumeration_error)? {
        let entry = entry.map_err(enumeration_error)?;
        if found || entry.file_name() != name {
            return Err(invalid(&entry.path()));
        }
        found = true;
    }
    if found {
        validate(&slot.join(name))?;
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeletionRecord {
    slot: DirectoryIdentity,
    release: Option<DirectoryIdentity>,
}

const RELEASE_SLOTS: &[&str] = &["incoming", "previous", "discard"];

fn deletion_record(slot: &str) -> String {
    format!("deleting-{slot}.json")
}

fn tombstone(staging: &Path, slot: &str) -> PathBuf {
    staging.join(format!("deleting-{slot}"))
}

fn deletion_identity(
    path: &Path,
    platform: &dyn UpdatePlatform,
) -> Result<Option<DirectoryIdentity>> {
    let id = platform.directory_identity(path)?;
    if id.is_none() {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {
                return Err(fail(
                    codes::RELEASE_TREE_UNSAFE,
                    format!("Unexpected entry at {}", path.display()),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io_at(path, error)),
        }
    }
    Ok(id)
}

pub(crate) fn check_cleanup_slots(
    staging: &Path,
    incoming_name: &std::ffi::OsStr,
    install_name: &std::ffi::OsStr,
    validate: impl Fn(&Path) -> Result<()>,
    platform: &dyn UpdatePlatform,
) -> Result<()> {
    let entries = match std::fs::read_dir(staging) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::io_at(staging, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| Error::io_at(staging, error))?;
        let name = entry.file_name();
        let directory = RELEASE_SLOTS
            .iter()
            .any(|slot| name == *slot || name == format!("deleting-{slot}").as_str());
        let state_file = [
            STAGE_OWNER,
            STAGE_OWNER_TEMP,
            ARCHIVE_ENTRIES,
            ARCHIVE_ENTRIES_TEMP,
            "download.zip",
        ]
        .iter()
        .any(|slot| name == *slot)
            || RELEASE_SLOTS.iter().any(|slot| {
                name == deletion_record(slot).as_str()
                    || name == format!("{}.tmp", deletion_record(slot)).as_str()
            });
        if !directory
            && (!state_file
                || !entry
                    .file_type()
                    .map_err(|error| Error::io_at(&entry.path(), error))?
                    .is_file())
        {
            return Err(fail(
                codes::RELEASE_TREE_UNSAFE,
                format!(
                    "Unexpected update staging entry: {}. Move it out before retrying.",
                    entry.path().display()
                ),
            ));
        }
    }
    for &slot in RELEASE_SLOTS {
        let name = if slot == "incoming" {
            incoming_name
        } else {
            install_name
        };
        let active = staging.join(slot);
        let retired = tombstone(staging, slot);
        let record = read_json::<DeletionRecord>(staging, &deletion_record(slot))?;
        let retired_id = deletion_identity(&retired, platform)?;
        if deletion_identity(&active, platform)?.is_some() {
            if record.is_some() || retired_id.is_some() {
                return Err(fail(
                    codes::RELEASE_TREE_UNSAFE,
                    format!(
                        "A release slot reappeared during cleanup: {}",
                        active.display()
                    ),
                ));
            }
            check_release_slot(&active, name, &validate)?;
        }
        match (record, retired_id) {
            (Some(expected), Some(actual)) if expected.slot != actual => {
                return Err(fail(
                    codes::RELEASE_TREE_UNSAFE,
                    format!(
                        "A retired update directory changed during cleanup: {}",
                        retired.display()
                    ),
                ));
            }
            (Some(expected), Some(_)) => {
                check_release_slot(&retired, name, |tree| {
                    if deletion_identity(tree, platform)? != expected.release {
                        return Err(fail(
                            codes::RELEASE_TREE_UNSAFE,
                            format!(
                                "A retired release tree changed during cleanup: {}",
                                tree.display()
                            ),
                        ));
                    }
                    Ok(())
                })?;
            }
            (None, Some(_)) => check_release_slot(&retired, name, &validate)?,
            _ => {}
        }
    }
    Ok(())
}

/// Under the installation lock, move every live slot to a tombstone before
/// deleting it. A crash after rename but before recording the identity can
/// revalidate the complete tree. A recorded tombstone can be partly deleted.
pub(crate) fn cleanup_release_slots(
    staging: &Path,
    incoming_name: &std::ffi::OsStr,
    install_name: &std::ffi::OsStr,
    validate: impl Fn(&Path) -> Result<()>,
    platform: &dyn UpdatePlatform,
) -> Result<()> {
    check_cleanup_slots(staging, incoming_name, install_name, &validate, platform)?;
    for &slot in RELEASE_SLOTS {
        let active = staging.join(slot);
        let retired = tombstone(staging, slot);
        let name = if slot == "incoming" {
            incoming_name
        } else {
            install_name
        };
        if deletion_identity(&active, platform)?.is_some() {
            platform.rename_new(&active, &retired)?;
        }
        if let Some(id) = deletion_identity(&retired, platform)? {
            let record = deletion_record(slot);
            if read_json::<DeletionRecord>(staging, &record)?.is_none() {
                check_release_slot(&retired, name, &validate)?;
                let release = deletion_identity(&retired.join(name), platform)?;
                write_json(
                    staging,
                    &record,
                    &DeletionRecord { slot: id, release },
                    platform,
                )?;
            }
            std::fs::remove_dir_all(&retired).map_err(|error| Error::io_at(&retired, error))?;
            platform.sync_dir(staging)?;
        }
    }
    Ok(())
}

/// Removes a stage owned by one of the specified state folders. The
/// uninstaller must hold this installation's existing updater lock.
pub fn remove_owned_stage(
    staging: &Path,
    install_name: &std::ffi::OsStr,
    state_dirs: &[PathBuf],
    platform: &dyn UpdatePlatform,
) -> Result<bool> {
    if !matches!(
        stage_claim(staging, None, |owner| state_dirs
            .iter()
            .any(|dir| same_path(dir, owner)))?,
        StageClaim::Ours
    ) {
        return Ok(false);
    }
    let owner: StageOwner = read_json(staging, STAGE_OWNER)?.expect("owned staging has a record");
    if let Some(journal) = StateDir::read_journal(&owner.state_dir)? {
        crate::updater::check_layout(&journal)?;
        if !same_path(&journal.staging, staging)
            || journal.from != owner.from
            || journal.to != owner.to
        {
            return Err(fail(
                codes::RECOVERY_FAILED,
                "the update journal does not own this staging folder",
            ));
        }
        cleanup_release_slots(
            staging,
            journal.payload_root.as_ref(),
            install_name,
            |tree| {
                let identity = platform.inspect(tree)?;
                let prior = newest_inventory(
                    &owner.state_dir,
                    state_dirs.iter().cloned(),
                    (&journal.install_root, tree, &identity),
                    platform,
                    &mut Vec::new(),
                )?;
                validate_swap_tree(
                    tree,
                    &journal,
                    &identity,
                    prior.as_ref(),
                    platform,
                    &Limits::default(),
                )
            },
            platform,
        )?;
    } else {
        remove_unjournaled_stage(staging, platform)?;
        return Ok(true);
    }
    remove_stage(staging)?;
    Ok(true)
}

/// Unjournaled extraction can only contain a partial known release, never a
/// retired installation. Used by updater recovery and uninstall under the lock.
pub(crate) fn remove_unjournaled_stage(
    staging: &Path,
    platform: &dyn UpdatePlatform,
) -> Result<()> {
    if !check_private_directory(staging)? {
        return Ok(());
    }
    let owner: Option<StageOwner> = read_json(staging, STAGE_OWNER)?;
    let layout = owner.as_ref().and_then(|owner| {
        broscribe_platform::release_layout::ReleaseLayout::for_target(&owner.to.target)
    });
    let entries: Option<ArchiveEntries> = read_json(staging, ARCHIVE_ENTRIES)?;
    let mut incoming = None;
    let refusal = |path: &Path| {
        fail(
            codes::RELEASE_TREE_UNSAFE,
            format!(
                "Unexpected update staging entry at {}. Move it out and run broscribe update recover.",
                path.display()
            ),
        )
    };
    if entries
        .as_ref()
        .is_some_and(|record| Some(&record.owner) != owner.as_ref())
    {
        return Err(refusal(&staging.join(ARCHIVE_ENTRIES)));
    }
    for entry in std::fs::read_dir(staging).map_err(|error| Error::io_at(staging, error))? {
        let entry = entry.map_err(|error| Error::io_at(staging, error))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|error| Error::io_at(&path, error))?;
        if broscribe_platform::release_layout::is_reparse(&meta) {
            return Err(refusal(&path));
        }
        match entry.file_name().to_str() {
            Some(STAGE_OWNER_TEMP) if meta.is_file() => {}
            Some(STAGE_OWNER | ARCHIVE_ENTRIES | ARCHIVE_ENTRIES_TEMP | "download.zip")
                if owner.is_some() && meta.is_file() => {}
            Some("deleting-incoming" | "deleting-incoming.json" | "deleting-incoming.json.tmp")
                if layout.is_some()
                    && read_json::<DeletionRecord>(staging, "deleting-incoming.json")?
                        .is_some()
                    && (entry.file_name() == "deleting-incoming") == meta.is_dir() => {}
            Some("incoming") if owner.is_some() && meta.is_dir() => {
                if let Some(record) = &entries {
                    check_partial_extraction(&path, &record.entries, &refusal)?;
                } else {
                    for payload in
                        std::fs::read_dir(&path).map_err(|error| Error::io_at(&path, error))?
                    {
                        let payload = payload.map_err(|error| Error::io_at(&path, error))?;
                        let Some(layout) =
                            layout.filter(|layout| payload.file_name() == layout.archive_root())
                        else {
                            return Err(refusal(&payload.path()));
                        };
                        layout
                            .validate_partial_archive(&payload.path())
                            .map_err(|error| {
                                fail(
                                    codes::RELEASE_TREE_UNSAFE,
                                    format!("{error} Then run broscribe update recover."),
                                )
                            })?;
                    }
                }
                incoming = Some(path);
            }
            _ => return Err(refusal(&path)),
        }
    }
    if read_json::<DeletionRecord>(staging, "deleting-incoming.json")?.is_some() {
        let layout = layout.ok_or_else(|| refusal(staging))?;
        let name = std::ffi::OsStr::new(layout.archive_root());
        cleanup_release_slots(staging, name, name, |tree| Err(refusal(tree)), platform)?;
    }
    if let Some(incoming) = incoming {
        std::fs::remove_dir_all(&incoming).map_err(|error| Error::io_at(&incoming, error))?;
    }
    // Stay within the prevalidated paths. A partial deletion can be revalidated.
    remove_stage(staging)?;
    if let Some(parent) = staging.parent() {
        platform.sync_dir(parent)?;
    }
    Ok(())
}

fn check_partial_extraction(
    root: &Path,
    expected: &archive::Entries,
    refusal: &impl Fn(&Path) -> Error,
) -> Result<()> {
    use archive::EntryType;
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((folder, prefix)) = pending.pop() {
        for entry in std::fs::read_dir(&folder).map_err(|error| Error::io_at(&folder, error))? {
            let entry = entry.map_err(|error| Error::io_at(&folder, error))?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| refusal(&path))?;
            let relative = format!("{prefix}{name}");
            let meta =
                std::fs::symlink_metadata(&path).map_err(|error| Error::io_at(&path, error))?;
            let kind = if meta.file_type().is_symlink() {
                EntryType::Link
            } else if broscribe_platform::release_layout::is_reparse(&meta) {
                return Err(refusal(&path));
            } else if meta.is_dir() {
                EntryType::Directory
            } else if meta.is_file() {
                EntryType::File
            } else {
                return Err(refusal(&path));
            };
            if expected.get(&relative) != Some(&kind) {
                return Err(refusal(&path));
            }
            if kind == EntryType::Directory {
                pending.push((path, format!("{relative}/")));
            }
        }
    }
    Ok(())
}

fn stage_claim(
    staging: &Path,
    update: Option<(&PayloadIdentity, &PayloadIdentity)>,
    owns: impl FnOnce(&Path) -> bool,
) -> Result<StageClaim> {
    match std::fs::symlink_metadata(staging) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StageClaim::Absent),
        Err(e) => return Err(Error::io_at(staging, e)),
        Ok(meta) if !meta.is_dir() || broscribe_platform::release_layout::is_reparse(&meta) => {
            return Err(fail(
                codes::RECOVERY_FAILED,
                format!("{} is not a folder; nothing was changed", staging.display()),
            ));
        }
        Ok(_) => {}
    }
    if let Some(owner) = read_json::<StageOwner>(staging, STAGE_OWNER)? {
        if owner.schema != 1 {
            return Err(fail(
                codes::RECOVERY_FAILED,
                "the staging folder's owner record has an unknown schema; nothing was changed",
            ));
        }
        if !owns(&owner.state_dir) {
            return Ok(StageClaim::Foreign(owner.state_dir));
        }
        return Ok(match update {
            Some((from, to)) if owner.from != *from || owner.to != *to => {
                StageClaim::OursOtherUpdate
            }
            _ => StageClaim::Ours,
        });
    }
    for entry in std::fs::read_dir(staging).map_err(|e| Error::io_at(staging, e))? {
        let entry = entry.map_err(|e| Error::io_at(staging, e))?;
        if entry.file_name() != STAGE_OWNER_TEMP {
            return Ok(StageClaim::Unknown);
        }
    }
    Ok(StageClaim::Unclaimed)
}

fn private_state_error(path: &Path, why: &str) -> Error {
    fail(
        codes::RECOVERY_FAILED,
        format!("{} {why}; nothing was changed", path.display()),
    )
}

fn create_private_directory(path: &Path) -> Result<()> {
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    builder.create(path).map_err(|e| Error::io_at(path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| Error::io_at(path, error))?;
    }
    check_private_directory(path)?;
    Ok(())
}

/// Create the updater-owned directory, with ordinary permissions on any
/// missing profile ancestors. Opening legacy updater directories tightens them.
pub fn ensure_update_directory(dir: &Path) -> Result<()> {
    if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Error::io_at(parent, e))?;
    }
    match create_private_directory(dir) {
        Ok(()) => Ok(()),
        Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            tighten_update_directory(dir)
        }
        Err(error) => Err(error),
    }
}

/// Check the existing updater directory without creating it.
fn check_private_directory(path: &Path) -> Result<bool> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io_at(path, error)),
    };
    if !meta.is_dir() || broscribe_platform::release_layout::is_reparse(&meta) {
        return Err(private_state_error(path, "is not a regular directory"));
    }
    #[cfg(unix)]
    check_unix_permissions(path, &meta, 0o700)?;
    Ok(true)
}

/// Validate metadata from an opened file before trusting its contents.
fn check_private_record(path: &Path, meta: &std::fs::Metadata) -> Result<()> {
    if !meta.is_file() || broscribe_platform::release_layout::is_reparse(meta) {
        return Err(private_state_error(path, "is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if meta.nlink() != 1 {
            return Err(private_state_error(path, "is hard-linked"));
        }
        check_unix_permissions(path, meta, 0o600)?;
    }
    Ok(())
}

const PROFILE_RECORDS: &[&str] = &[
    LOCK,
    JOURNAL,
    INSTALLED_INVENTORY,
    SEEN,
    "install-record.json",
    "runner-handoff.json",
    "runner-report.json",
    "manual-admission.json",
];

/// Validate existing updater state without creating files or changing modes.
pub fn inspect_update_directory(dir: &Path) -> Result<()> {
    if check_private_directory(dir)? {
        for name in PROFILE_RECORDS {
            let path = dir.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) => check_private_record(&path, &meta)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::io_at(&path, error)),
            }
        }
    }
    Ok(())
}

/// Migrate verified legacy state only for callers that are making changes.
pub fn tighten_update_directory(dir: &Path) -> Result<()> {
    inspect_update_directory(dir)?;
    #[cfg(unix)]
    if check_private_directory(dir)? {
        tighten_unix_mode(dir, 0o700)?;
        for name in PROFILE_RECORDS {
            tighten_unix_mode(&dir.join(name), 0o600)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn tighten_unix_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    let mut flags = libc::O_NOFOLLOW | libc::O_NONBLOCK;
    if mode == 0o700 {
        flags |= libc::O_DIRECTORY;
    }
    let file = match File::options().read(true).custom_flags(flags).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::io_at(path, error)),
    };
    let meta = file.metadata().map_err(|error| Error::io_at(path, error))?;
    if mode == 0o600 {
        check_private_record(path, &meta)?;
    } else {
        check_unix_permissions(path, &meta, mode)?;
    }
    if meta.mode() & 0o7777 != mode {
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|error| Error::io_at(path, error))?;
    }
    Ok(())
}

#[cfg(unix)]
fn check_unix_permissions(path: &Path, meta: &std::fs::Metadata, mode: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: geteuid has no arguments and reads the process identity.
    let owner = unsafe { libc::geteuid() };
    let reason = if meta.uid() != owner {
        "belongs to another user"
    } else if meta.mode() & 0o022 != 0 {
        "is group- or world-writable"
    } else {
        return Ok(());
    };
    let path_arg = path.to_string_lossy().replace('\'', "'\"'\"'");
    Err(fail(
        codes::RECOVERY_FAILED,
        format!(
            "{} {reason}. Verify the contents and owner (expected UID {owner}); if this is your update state, run chown -- {owner} '{path_arg}' and chmod -- {mode:o} '{path_arg}' as needed, then retry.",
            path.display()
        ),
    ))
}

/// Reads a private, regular update record, or returns `None` when absent.
pub fn read_update_record<T: serde::de::DeserializeOwned>(
    dir: &Path,
    name: &str,
) -> Result<Option<T>> {
    if Path::new(name).file_name().and_then(|part| part.to_str()) != Some(name) {
        return Err(Error::InvalidInput(
            "Update record name must be a single file name".into(),
        ));
    }
    read_json_with(dir, name, |path, error| {
        Error::Schema(format!("{} is unreadable ({error})", path.display()))
    })
}

fn read_json<T: serde::de::DeserializeOwned>(dir: &Path, name: &str) -> Result<Option<T>> {
    read_json_with(dir, name, |path, error| {
        fail(
            codes::RECOVERY_FAILED,
            format!(
                "{} is unreadable ({error}); nothing was changed",
                path.display()
            ),
        )
    })
}

fn read_json_with<T: serde::de::DeserializeOwned>(
    dir: &Path,
    name: &str,
    invalid: impl FnOnce(&Path, serde_json::Error) -> Error,
) -> Result<Option<T>> {
    if !check_private_directory(dir)? {
        return Ok(None);
    }
    let path = dir.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => check_private_record(&path, &meta)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io_at(&path, e)),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(&path)
        .map_err(|error| Error::io_at(&path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| Error::io_at(&path, error))?;
    check_private_record(&path, &metadata)?;
    let mut bytes = Vec::new();
    file.take(state_limit(name) + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::io_at(&path, error))?;
    if bytes.len() as u64 > state_limit(name) {
        return Err(fail(
            codes::RECOVERY_FAILED,
            format!("{} is too large; nothing was changed", path.display()),
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| invalid(&path, error))
}

/// Write-to-temporary, flush, rename over, then sync the folder.
fn state_limit(name: &str) -> u64 {
    if matches!(name, JOURNAL | INSTALLED_INVENTORY | ARCHIVE_ENTRIES) {
        MAX_JOURNAL_BYTES
    } else {
        MAX_STATE_BYTES
    }
}

fn write_json<T: Serialize>(
    dir: &Path,
    name: &str,
    value: &T,
    platform: &dyn UpdatePlatform,
) -> Result<()> {
    let path = dir.join(name);
    let temp = dir.join(format!("{name}.tmp"));
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| Error::Internal(format!("serializing {name}: {e}")))?;
    if bytes.len() as u64 > state_limit(name) {
        return Err(fail(
            codes::ARCHIVE_TOO_LARGE,
            format!("{name} exceeds its size limit"),
        ));
    }
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(Error::io_at(&temp, error)),
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(|e| Error::io_at(&temp, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| Error::io_at(&temp, error))?;
    }
    file.write_all(&bytes).map_err(|e| Error::io_at(&temp, e))?;
    file.sync_all().map_err(|e| Error::io_at(&temp, e))?;
    drop(file);
    std::fs::rename(&temp, &path).map_err(|e| Error::io_at(&path, e))?;
    platform.sync_dir(dir)
}

/// Opens (creating) `path` and takes its exclusive OS lock without waiting,
/// or fails with [`codes::BUSY`]. The lock lasts as long as the guard.
pub(crate) fn lock_exclusive(path: &Path) -> Result<ExclusiveLock> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let lock = options.open(path).map_err(|e| Error::io_at(path, e))?;
    match lock.try_lock() {
        Ok(()) => Ok(ExclusiveLock(lock)),
        Err(TryLockError::WouldBlock) => Err(fail(
            codes::BUSY,
            "another BroScribe process is updating this installation",
        )),
        Err(TryLockError::Error(e)) => Err(Error::io_at(path, e)),
    }
}

/// An exclusive OS lock held for the guard's life. Unlocks explicitly on
/// drop: flock locks belong to the open file description, and a child forked
/// by another thread holds a copy of it until its exec closes it
/// (close-on-exec). Closing only this handle would leave the lock held.
#[derive(Debug)]
pub(crate) struct ExclusiveLock(File);

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(all(unix, test))]
mod permission_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn readable_legacy_state_is_tightened_only_on_mutating_open() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("updates");
        std::fs::create_dir(&dir).unwrap();
        let seen = dir.join(SEEN);
        let installed = dir.join(INSTALLED_INVENTORY);
        std::fs::write(&seen, r#"{"highest_build":12}"#).unwrap();
        std::fs::write(&installed, b"synthetic private record").unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        for (path, value) in [(&dir, 0o755), (&seen, 0o644), (&installed, 0o644)] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(value)).unwrap();
        }
        inspect_update_directory(&dir).unwrap();
        assert_eq!(
            (mode(&dir), mode(&seen), mode(&installed)),
            (0o755, 0o644, 0o644)
        );
        let state = StateDir::open(&dir).unwrap();
        assert_eq!(state.seen().unwrap().highest_build, 12);
        assert_eq!(
            (mode(&dir), mode(&seen), mode(&installed)),
            (0o700, 0o600, 0o600)
        );
        drop(state);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        let error = inspect_update_directory(&dir).unwrap_err().to_string();
        assert!(
            error.contains("chmod -- 700")
                && error.contains("chown --")
                && error.contains("group- or world-writable")
                && error.contains(&*dir.to_string_lossy())
        );
        assert_eq!(mode(&dir), 0o775);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o664)).unwrap();
        let error = tighten_update_directory(&dir).unwrap_err().to_string();
        assert!(error.contains("chmod -- 600") && error.contains("installed-inventory.json"));
        assert_eq!(mode(&installed), 0o664);
    }
}
