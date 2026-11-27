use std::path::{Path, PathBuf};
use std::sync::Arc;

use broscribe_platform::uninstall::{
    CredentialProfile, DesktopUninstall, Layout, ReleaseRemovalCheck,
};
use broscribe_platform::{Result, SecretStore};

pub struct NativeUninstall;

impl DesktopUninstall for NativeUninstall {
    fn layout(&self, executable: &Path) -> Result<Layout> {
        super::imp::uninstall::layout(executable)
    }

    fn validate_installation(&self, root: &Path, has_inventory: bool) -> Result<()> {
        super::imp::uninstall::validate_installation(root, has_inventory)
    }

    #[cfg(target_os = "linux")]
    fn remove_desktop_entry(&self, executable: &Path, path: &Path, root: &Path) -> Result<bool> {
        super::imp::uninstall::remove_desktop_entry(executable, path, root)
    }

    fn shortcut_profiles(&self) -> Result<Vec<PathBuf>> {
        #[cfg(target_os = "windows")]
        return super::imp::uninstall::shortcut_profiles();
        #[cfg(not(target_os = "windows"))]
        Ok(Vec::new())
    }

    fn remove_shortcuts(&self, profiles: &[PathBuf]) -> Result<usize> {
        #[cfg(target_os = "windows")]
        return super::imp::uninstall::remove_shortcuts(profiles);
        #[cfg(not(target_os = "windows"))]
        {
            let _ = profiles;
            Ok(0)
        }
    }

    fn runtime_paths(&self, data_dir: &Path) -> Result<Vec<PathBuf>> {
        #[cfg(unix)]
        return super::imp::uninstall::runtime_paths(data_dir);
        #[cfg(not(unix))]
        {
            let _ = data_dir;
            Ok(Vec::new())
        }
    }

    #[cfg(target_os = "linux")]
    fn remove_runtime_files(
        &self,
        data_dir: &Path,
    ) -> Result<broscribe_platform::profile_cleanup::Cleanup> {
        super::imp::uninstall::remove_runtime_files(data_dir)
    }

    fn registration_lock_paths(&self) -> Result<Vec<PathBuf>> {
        super::imp::uninstall::registration_lock_paths()
    }

    fn secret_store(&self, profile: &Path) -> Arc<dyn SecretStore> {
        #[cfg(target_os = "linux")]
        return Arc::new(super::imp::LinuxSecretStore::for_uninstall(
            &super::NativePlatform::secret_service(super::imp::uninstall::SECRET_BASE, profile),
        ));
        #[cfg(not(target_os = "linux"))]
        Arc::from(super::NativePlatform::secret_store(profile))
    }

    #[cfg(target_os = "windows")]
    fn directory_identity(
        &self,
        profile: &Path,
    ) -> Result<Option<broscribe_platform::uninstall::DirectoryIdentity>> {
        super::imp::uninstall::directory_identity(profile)
    }

    fn credential_namespace(&self, profile: &Path) -> String {
        // The uninstaller has already resolved this path at discovery. A
        // legacy path-only retry record must not re-resolve the old path.
        #[cfg(windows)]
        let identity = super::imp::recorded_profile_identity(profile);
        #[cfg(windows)]
        let identity = identity.as_slice();
        #[cfg(unix)]
        let identity = profile.as_os_str().as_encoded_bytes();
        broscribe_platform::secret_namespace(
            super::imp::uninstall::SECRET_BASE,
            identity,
            &super::imp::profile_identity(&super::NativePlatform::paths().data_dir()),
        )
    }

    fn remove_credentials(&self, profiles: &[CredentialProfile], all: bool) -> Result<usize> {
        let base = super::imp::uninstall::SECRET_BASE;
        let services = profiles.iter().map(|profile| {
            let service = &profile.namespace;
            if service == base || broscribe_platform::uninstall::owns_namespace(base, service) {
                return Ok(service.clone());
            }
            Err(broscribe_platform::Error::InvalidInput(
                "The pending uninstall list contains an invalid BroScribe credential namespace.".into(),
            ))
        }).collect::<Result<Vec<_>>>()?;
        super::imp::uninstall::remove_credentials(&services, all)
    }

    fn remove_installation(
        &self,
        executable: &Path,
        root: &Path,
        check: &ReleaseRemovalCheck<'_>,
    ) -> Result<Option<PathBuf>> {
        super::imp::uninstall::remove_installation(executable, root, check)
    }

    #[cfg(target_os = "windows")]
    fn defers_settings_cleanup(&self) -> bool {
        true
    }

    #[cfg(target_os = "windows")]
    fn remove_after_exit(
        &self,
        executable: &Path,
        root: Option<&Path>,
        check: Option<&ReleaseRemovalCheck<'_>>,
        profile: &broscribe_platform::uninstall::DeferredProfile,
        before_launch: &mut dyn FnMut(&Path) -> Result<PathBuf>,
    ) -> Result<PathBuf> {
        super::imp::uninstall::remove_after_exit(
            executable,
            root,
            check,
            Some(profile),
            before_launch,
        )
    }
}
