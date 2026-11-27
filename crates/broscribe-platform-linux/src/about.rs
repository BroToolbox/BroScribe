//! Linux About facts: the installation folder holding the build
//! information (the executable's folder or its parent), or the executable
//! itself when unbundled. Linux has no OS code-signing facility for
//! application trees, so the signature is always
//! [`SignatureKind::ManifestOnly`] and never checked here.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use broscribe_platform::about::{
    AboutFacts, InspectedInstallation, LocationNote, Packaging, PlatformAboutReader, SealState,
    SignatureFacts, SignatureKind, canonical_temp_dir, collect_about, install_folder,
};
use broscribe_platform::{Error, Result};

/// Network file systems (`statfs` magic numbers).
const NETWORK_FS: [u64; 7] = [
    0x6969,      // NFS
    0x517b,      // SMB
    0xff53_4d42, // CIFS
    0xfe53_4d42, // SMB2
    0x0102_1997, // 9P (v9fs)
    0x5346_414f, // AFS
    0x6b41_4653, // kAFS
];
const TEMPORARY: [&str; 4] = ["/tmp", "/var/tmp", "/dev/shm", "/run"];

/// Whether a canonical installation root is temporary on this system.
pub(crate) fn is_temporary_location(root: &Path) -> bool {
    canonical_temp_dir().is_some_and(|temporary| root.starts_with(temporary))
        || TEMPORARY
            .iter()
            .any(|temporary| root.starts_with(temporary))
}

#[allow(clippy::unnecessary_cast)] // `f_type`'s type differs between libcs.
fn location_notes(root: &Path) -> Vec<LocationNote> {
    let mut notes = Vec::new();
    if let Ok(path) = CString::new(root.as_os_str().as_bytes()) {
        // SAFETY: zeroed is a valid statvfs; the path is NUL-terminated.
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut vfs) } == 0
            && vfs.f_flag & libc::ST_RDONLY != 0
        {
            notes.push(LocationNote::ReadOnlyVolume);
        }
        // SAFETY: zeroed is a valid statfs; the path is NUL-terminated.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut fs) } == 0
            && NETWORK_FS.contains(&((fs.f_type as u64) & 0xffff_ffff))
        {
            notes.push(LocationNote::NetworkVolume);
        }
    }
    if is_temporary_location(root) {
        notes.push(LocationNote::Temporary);
    }
    notes
}

fn inspect(executable: &Path) -> InspectedInstallation {
    let folder = install_folder(executable);
    let location = folder.as_deref().or_else(|| executable.parent());
    InspectedInstallation {
        packaging: if folder.is_some() {
            Packaging::InstallFolder
        } else {
            Packaging::Unbundled
        },
        location_notes: location.map(location_notes).unwrap_or_default(),
        resources: folder,
        bundle: None,
        signature: SignatureFacts::without_notarization(
            SignatureKind::ManifestOnly,
            SealState::NotChecked,
        ),
        translated: None,
    }
}

/// About facts of the running build on Linux.
#[derive(Clone, Debug)]
pub struct LinuxAboutReader {
    app_version: String,
}

impl LinuxAboutReader {
    /// `app_version` is the calling executable's `CARGO_PKG_VERSION`.
    pub fn new(app_version: impl Into<String>) -> Self {
        Self {
            app_version: app_version.into(),
        }
    }
}

impl PlatformAboutReader for LinuxAboutReader {
    fn read(&self) -> Result<AboutFacts> {
        let executable = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|e| Error::io("locating the running executable", e))?;
        Ok(collect_about(&self.app_version, inspect(&executable)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::about::Notarization;
    use broscribe_platform::update::BUILD_INFO_FILE;

    #[test]
    fn installation_folders_and_unbundled_builds_are_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("broscribe");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let unbundled = inspect(&root.join("bin/broscribe"));
        assert_eq!(unbundled.packaging, Packaging::Unbundled);
        assert_eq!(unbundled.resources, None);
        std::fs::write(root.join(BUILD_INFO_FILE), "{}").unwrap();
        let installed = inspect(&root.join("bin/broscribe"));
        assert_eq!(installed.packaging, Packaging::InstallFolder);
        assert_eq!(installed.resources.as_deref(), Some(root.as_path()));
        assert_eq!(installed.signature.kind, SignatureKind::ManifestOnly);
        assert_eq!(installed.signature.seal, SealState::NotChecked);
        assert_eq!(
            installed.signature.notarization,
            Notarization::NotApplicable
        );
    }

    #[test]
    fn temporary_folders_are_noted() {
        assert!(location_notes(Path::new("/tmp/broscribe")).contains(&LocationNote::Temporary));
        assert!(!location_notes(Path::new("/opt/broscribe")).contains(&LocationNote::Temporary));
    }
}
