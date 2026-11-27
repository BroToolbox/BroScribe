//! Platform operations needed by the desktop uninstaller.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

#[derive(Default)]
pub struct Layout {
    pub installation: Option<PathBuf>,
    pub shell_retry: bool,
    pub desktop_paths: Vec<PathBuf>,
    pub manual_steps: Vec<&'static str>,
}

/// Paths protected against a recursive removal. Home and designated parent
/// directories may contain the removal target, but may never be inside it.
#[derive(Default)]
pub struct RemovalProtection {
    pub overlap: Vec<PathBuf>,
    pub contains: Vec<PathBuf>,
}

impl RemovalProtection {
    pub fn conflicts(&self, target: &Path, ignore_case: bool) -> bool {
        self.contains
            .iter()
            .any(|path| path_within(path, target, ignore_case))
            || self.overlap.iter().any(|path| {
                path_within(path, target, ignore_case) || path_within(target, path, ignore_case)
            })
    }
}

pub fn path_within(path: &Path, parent: &Path, ignore_case: bool) -> bool {
    let path = crate::stable_path(path);
    let parent = crate::stable_path(parent);
    if ignore_case {
        let path = path.to_string_lossy().to_lowercase();
        let parent = parent.to_string_lossy().to_lowercase();
        return Path::new(&path).starts_with(Path::new(&parent));
    }
    path.starts_with(parent)
}

/// Filesystem identity of a profile directory, independent of its current name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectoryIdentity {
    pub volume: u64,
    pub file: u64,
    pub generation: Option<u64>,
}

pub fn directory_identity(path: &Path) -> Result<Option<DirectoryIdentity>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io_at(path, error)),
    };
    if !meta.is_dir() || crate::release_layout::is_reparse(&meta) {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let generation = meta.created().ok().and_then(|created| {
            created
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_nanos()
                .try_into()
                .ok()
        });
        Ok(Some(DirectoryIdentity {
            volume: meta.dev(),
            file: meta.ino(),
            generation,
        }))
    }
    // Only fakes use this fallback on Windows. NativeUninstall uses the
    // Windows file ID API without relying on file creation time alone.
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        Ok(Some(DirectoryIdentity {
            volume: 0,
            file: meta.creation_time(),
            generation: None,
        }))
    }
}

pub struct CredentialProfile {
    pub data_dir: PathBuf,
    pub namespace: String,
}

pub struct DeferredProfile {
    pub data_dir: PathBuf,
    pub identity: DirectoryIdentity,
    pub marker: Option<String>,
    pub remove_data: bool,
    pub discard_unsaved: bool,
    pub remove_update_runner: bool,
    pub protection: RemovalProtection,
    pub default_cleanup: Option<DeferredDefault>,
}

/// Cleanup belonging to the retry list's default profile, after a secondary.
pub struct DeferredDefault {
    pub data_dir: PathBuf,
    pub identity: DirectoryIdentity,
    pub marker: Option<String>,
    /// A protection set authorizes removal of the entire default data folder.
    pub remove_data: Option<RemovalProtection>,
    pub created_for_pending_list: bool,
}

/// Profile identity to recheck before the after-exit helper removes update state.
pub struct DeferredUpdateCleanup {
    pub data_dir: PathBuf,
    pub identity: DirectoryIdentity,
    pub marker: Option<String>,
    pub remove_data: bool,
    pub discard_unsaved: bool,
}

/// The CLI carries release validation through the final move or removal. On
/// Windows, the same inventory can be checked independently after exit.
pub struct ReleaseRemovalCheck<'a> {
    pub validate: &'a dyn Fn(&Path) -> Result<()>,
    pub windows_inventory: Option<&'a [u8]>,
    pub windows_update_cleanup: &'a [DeferredUpdateCleanup],
    /// Publish the retry reference before starting an installation-only helper
    /// and return its path so the helper can remove it on success.
    pub windows_before_launch: &'a dyn Fn(&Path) -> Result<PathBuf>,
}

pub trait DesktopUninstall: Send + Sync {
    fn layout(&self, executable: &Path) -> Result<Layout>;
    fn validate_installation(&self, _root: &Path, _has_inventory: bool) -> Result<()> {
        Ok(())
    }
    fn remove_desktop_entry(&self, _executable: &Path, _path: &Path, _root: &Path) -> Result<bool> {
        Err(Error::Unsupported(
            "removing desktop entries on this platform".into(),
        ))
    }
    fn shortcut_profiles(&self) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
    fn remove_shortcuts(&self, _profiles: &[PathBuf]) -> Result<usize> {
        Ok(0)
    }
    fn runtime_paths(&self, _data_dir: &Path) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
    /// Cleans separately located regular runtime files after the agent is stopped.
    fn remove_runtime_files(&self, _data_dir: &Path) -> Result<crate::profile_cleanup::Cleanup> {
        Ok(crate::profile_cleanup::Cleanup::default())
    }
    fn registration_lock_paths(&self) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }
    fn directory_identity(&self, profile: &Path) -> Result<Option<DirectoryIdentity>> {
        directory_identity(profile)
    }
    fn credential_namespace(&self, profile: &Path) -> String {
        profile.to_string_lossy().into_owned()
    }
    fn remove_credentials(&self, profiles: &[CredentialProfile], all: bool) -> Result<usize>;
    fn secret_store(&self, profile: &Path) -> std::sync::Arc<dyn crate::SecretStore>;
    /// The returned path, if any, is where an after-exit removal writes its result.
    fn remove_installation(
        &self,
        executable: &Path,
        root: &Path,
        check: &ReleaseRemovalCheck<'_>,
    ) -> Result<Option<PathBuf>>;
    /// Whether the desktop app holds runtime files open until it exits.
    fn defers_settings_cleanup(&self) -> bool {
        false
    }
    fn remove_after_exit(
        &self,
        _executable: &Path,
        _root: Option<&Path>,
        _check: Option<&ReleaseRemovalCheck<'_>>,
        _profile: &DeferredProfile,
        _before_launch: &mut dyn FnMut(&Path) -> Result<PathBuf>,
    ) -> Result<PathBuf> {
        Err(Error::Unsupported(
            "removing profile files after exit".into(),
        ))
    }
}

/// Only the exact, generated profile suffix is in scope for a bulk cleanup.
pub fn owns_namespace(base: &str, service: &str) -> bool {
    service
        .strip_prefix(base)
        .and_then(|suffix| suffix.strip_prefix(".profile-"))
        .is_some_and(|hash| {
            hash.len() == 16
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// Retired on-disk entries (including dangling links) are idempotent.
/// `remove_dir_all` does not traverse directory symlinks within the tree.
pub fn remove_path(path: &Path) -> Result<bool> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

/// Rename into a private same-volume tombstone, then validate the moved
/// directory before deleting anything. An interrupted delete leaves the
/// remaining files at the tombstone path.
pub fn retire_release_tree(
    root: &Path,
    platform: &dyn crate::update::UpdatePlatform,
    validate: impl Fn(&Path) -> Result<()>,
) -> Result<()> {
    validate(root)?;
    let parent = root.parent().expect("installation has a parent");
    let tombstone = tempfile::Builder::new()
        .prefix(".broscribe-uninstall-")
        .tempdir_in(parent)?;
    let retired = tombstone
        .path()
        .join(root.file_name().expect("installation name"));
    // Preserve the tombstone if either rename or deletion is interrupted.
    let tombstone = tombstone.keep();
    if let Err(error) = platform.rename_new(root, &retired) {
        let _ = std::fs::remove_dir(&tombstone);
        return Err(error);
    }
    if let Err(error) = validate(&retired) {
        if platform.rename_new(&retired, root).is_ok() {
            let _ = std::fs::remove_dir(&tombstone);
        }
        return Err(error);
    }
    remove_path(&retired)?;
    std::fs::remove_dir(&tombstone).map_err(|error| Error::io_at(&tombstone, error))?;
    Ok(())
}

#[derive(Default)]
pub struct FakeDesktopUninstall {
    pub installation: Option<PathBuf>,
    pub profiles: Vec<PathBuf>,
    pub release_layout: Option<crate::release_layout::ReleaseLayout>,
    pub credentials: std::sync::Arc<crate::fakes::FakeSecretStore>,
    pub credentials_unreachable: bool,
}

impl DesktopUninstall for FakeDesktopUninstall {
    fn layout(&self, _: &Path) -> Result<Layout> {
        Ok(Layout {
            installation: self.installation.clone(),
            shell_retry: matches!(
                self.release_layout,
                Some(crate::release_layout::ReleaseLayout::Linux)
            ),
            ..Layout::default()
        })
    }

    fn validate_installation(&self, root: &Path, has_inventory: bool) -> Result<()> {
        if has_inventory {
            return Ok(());
        }
        self.release_layout
            .map_or(Ok(()), |layout| layout.validate(root, false))
    }

    fn shortcut_profiles(&self) -> Result<Vec<PathBuf>> {
        Ok(self.profiles.clone())
    }

    fn secret_store(&self, _: &Path) -> std::sync::Arc<dyn crate::SecretStore> {
        self.credentials.clone()
    }

    fn remove_credentials(&self, _: &[CredentialProfile], _: bool) -> Result<usize> {
        if self.credentials_unreachable {
            return Err(Error::Unsupported(
                "synthetic credential store unavailable".into(),
            ));
        }
        let mut secrets = self.credentials.secrets.lock().unwrap();
        let count = secrets.len();
        secrets.clear();
        Ok(count)
    }

    fn remove_installation(
        &self,
        _: &Path,
        root: &Path,
        check: &ReleaseRemovalCheck<'_>,
    ) -> Result<Option<PathBuf>> {
        (check.validate)(root)?;
        remove_path(root)?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::FakeUpdatePlatform;

    #[test]
    fn a_release_changed_during_retirement_is_restored_and_never_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("BroScribe");
        std::fs::create_dir(&root).unwrap();
        let platform = FakeUpdatePlatform::new(root.clone(), "synthetic");
        let check = |tree: &Path| {
            if tree != root {
                let extra = tree.join("kept-library");
                std::fs::create_dir(&extra).unwrap();
                std::fs::write(extra.join("keep.md"), "synthetic meeting").unwrap();
                return Err(Error::InvalidInput("unexpected tree entry".into()));
            }
            Ok(())
        };
        assert!(retire_release_tree(&root, &platform, check).is_err());
        assert!(root.join("kept-library/keep.md").is_file());
        assert_eq!(temp.path().read_dir().unwrap().count(), 1);
    }
}
