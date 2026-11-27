//! Linux side of the signed updater.
//!
//! Trust boundary: Linux has no operating-system code-signing facility for
//! application trees, so the Ed25519-signed release manifest (with its
//! SHA-256 of the archive) is the only signature. Native checks here are
//! hygiene, not authentication: the tree is owned by the user, not group- or
//! world-writable, has no set-ID bits, special files or links leaving it,
//! and its executables are ELF images for this machine (or scripts). A
//! configured signer pin is refused rather than silently ignored.
//!
//! Layout: the installation root is the folder holding `build-info.json`:
//! the running (or handed-off original) executable's folder, or its parent
//! (`<root>/bin/<exe>`).

use std::ffi::CString;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use broscribe_platform::update::{
    BUILD_INFO_FILE, Installation, PayloadIdentity, Replacement, SignerIdentity, SigningFacility,
    UpdatePlatform, VerifyRequest, installed_executable, read_build_info, target_matches_host,
};
use broscribe_platform::{Error, Result};

const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
/// Network filesystems an update must not replace in place.
const NETWORK_FS: [u64; 7] = [
    0x6969,      // NFS
    0x517b,      // SMB
    0xff53_4d42, // CIFS
    0xfe53_4d42, // SMB2
    0x0102_1997, // 9P (v9fs)
    0x5346_414f, // AFS
    0x6b41_4653, // kAFS
];

/// Updates the installation the running executable belongs to, or that of
/// the original executable handed to the external update runner.
#[derive(Clone, Debug, Default)]
pub struct LinuxUpdatePlatform {
    /// `None`: the running executable.
    executable: Option<PathBuf>,
}

impl LinuxUpdatePlatform {
    pub fn new() -> Self {
        Self::default()
    }

    /// Qualifies the installation of `executable` (BroScribe's original
    /// executable, handed off by its own `update install`) with exactly the
    /// checks [`Self::new`] applies to the running one.
    pub fn for_executable(executable: PathBuf) -> Self {
        Self {
            executable: Some(executable),
        }
    }
}

fn service(code: &'static str, message: impl Into<String>) -> Error {
    Error::Service {
        code,
        message: message.into(),
    }
}

fn location(message: impl Into<String>) -> Error {
    service("update_location_unsupported", message)
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::InvalidInput(format!("{} contains a NUL byte", path.display())))
}

fn host_machine() -> Result<u16> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(EM_X86_64),
        "aarch64" => Ok(EM_AARCH64),
        other => Err(Error::Unsupported(format!(
            "updates are not built for {other} Linux"
        ))),
    }
}

/// Refuses temporary, network, read-only, foreign-owned or shared-writable
/// locations, and roots whose parent the user cannot rename entries in.
#[allow(clippy::unnecessary_cast)] // `f_type`'s type differs between libcs.
fn check_location(root: &Path) -> Result<()> {
    if crate::about::is_temporary_location(root) {
        return Err(location(
            "BroScribe runs from a temporary location; install it first",
        ));
    }
    let path = c_path(root)?;
    // SAFETY: zeroed is a valid statfs/statvfs; the path is NUL-terminated.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut fs) } != 0 {
        return Err(Error::io_at(root, std::io::Error::last_os_error()));
    }
    if NETWORK_FS.contains(&((fs.f_type as u64) & 0xffff_ffff)) {
        return Err(location(
            "BroScribe runs from a network filesystem; install it locally",
        ));
    }
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut vfs) } != 0 {
        return Err(Error::io_at(root, std::io::Error::last_os_error()));
    }
    if vfs.f_flag & libc::ST_RDONLY != 0 {
        return Err(location("BroScribe runs from a read-only filesystem"));
    }
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let meta = std::fs::symlink_metadata(root).map_err(|e| Error::io_at(root, e))?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(location(
            "the installation folder must be a folder owned by you and writable only by you",
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(location(format!(
            "the installation folder is group- or world-writable; run chmod -R go-w <folder> on {}",
            root.display()
        )));
    }
    let parent = root
        .parent()
        .ok_or_else(|| location("the installation folder has no parent"))?;
    let parent_c = c_path(parent)?;
    // SAFETY: NUL-terminated path.
    if unsafe { libc::access(parent_c.as_ptr(), libc::W_OK) } != 0 {
        return Err(location(format!(
            "you cannot replace entries in {}",
            parent.display()
        )));
    }
    Ok(())
}

/// Whether a link target, resolved lexically from `link`'s folder, stays
/// inside `root`. Leading `..` steps only, then plain names.
fn link_inside(root: &Path, link: &Path, target: &Path) -> bool {
    let Ok(relative) = link.parent().unwrap_or(root).strip_prefix(root) else {
        return false;
    };
    let mut depth = relative.components().count();
    let mut climbing = true;
    for component in target.components() {
        match component {
            Component::ParentDir if climbing => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Component::Normal(_) => climbing = false,
            _ => return false,
        }
    }
    true
}

/// Checks one executable's format: an ELF image for `machine`, or a script.
fn check_executable(path: &Path, machine: u16) -> Result<bool> {
    let mut head = [0u8; 20];
    let mut file = std::fs::File::open(path).map_err(|e| Error::io_at(path, e))?;
    let read = file.read(&mut head).map_err(|e| Error::io_at(path, e))?;
    if read >= 2 && &head[..2] == b"#!" {
        return Ok(false);
    }
    // e_ident: magic, class 2 (64-bit), data 1 (little-endian); e_machine at 18.
    if read < 20 || &head[..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 {
        return Err(service(
            "update_signature_invalid",
            format!(
                "{} is executable but not a 64-bit ELF image or script",
                path.display()
            ),
        ));
    }
    if u16::from_le_bytes([head[18], head[19]]) != machine {
        return Err(service(
            "update_wrong_target",
            format!("{} is built for another architecture", path.display()),
        ));
    }
    Ok(true)
}

/// Walks the tree without following links and returns how many host ELF
/// executables it holds.
fn check_tree(root: &Path, uid: u32, machine: u16) -> Result<usize> {
    let refuse = |path: &Path, why: &str| {
        service(
            "update_signature_invalid",
            format!("{}: {why}", path.display()),
        )
    };
    let mut images = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| Error::io_at(&dir, e))? {
            let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
            let kind = meta.file_type();
            if kind.is_symlink() {
                let target = std::fs::read_link(&path).map_err(|e| Error::io_at(&path, e))?;
                if !link_inside(root, &path, &target) {
                    return Err(refuse(&path, "a link leaves the payload"));
                }
                continue;
            }
            if meta.uid() != uid {
                return Err(refuse(&path, "owned by another user"));
            }
            let mode = meta.permissions().mode();
            if mode & 0o7000 != 0 || mode & 0o022 != 0 {
                return Err(refuse(&path, "set-ID, sticky or shared-writable"));
            }
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                if mode & 0o111 != 0 && check_executable(&path, machine)? {
                    images += 1;
                }
            } else {
                return Err(refuse(&path, "a special file"));
            }
        }
    }
    Ok(images)
}

fn sync_dir(dir: &Path) -> Result<()> {
    let file = std::fs::File::open(dir).map_err(|e| Error::io_at(dir, e))?;
    file.sync_all().map_err(|e| Error::io_at(dir, e))
}

fn sync_parents(a: &Path, b: &Path) -> Result<()> {
    for path in [a, b] {
        if let Some(parent) = path.parent() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}

fn renameat2(from: &Path, to: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    let from_c = CString::new(from.as_os_str().as_bytes())?;
    let to_c = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both paths are NUL-terminated; AT_FDCWD resolves them as given.
    let status = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from_c.as_ptr(),
            libc::AT_FDCWD,
            to_c.as_ptr(),
            flags,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn rename_new_with(
    from: &Path,
    to: &Path,
    rename_no_replace: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let exists = || Error::AlreadyExists(to.display().to_string());
    match rename_no_replace(from, to) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => return Err(exists()),
        Err(error) if flag_unsupported(&error) => {
            // The caller holds the per-installation lock or uses a private
            // tombstone. Filesystems without the flag still need this check.
            match std::fs::symlink_metadata(to) {
                Ok(_) => return Err(exists()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::io_at(to, error)),
            }
            std::fs::rename(from, to).map_err(|error| Error::io_at(from, error))?;
        }
        Err(error) => return Err(Error::io_at(from, error)),
    }
    sync_parents(from, to)
}

fn flag_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
    )
}

fn installation_root(executable: &Path) -> Option<PathBuf> {
    let folder = executable.parent()?;
    [Some(folder), folder.parent()]
        .into_iter()
        .flatten()
        .find(|candidate| candidate.join(BUILD_INFO_FILE).is_file())
        .map(Path::to_path_buf)
}

impl UpdatePlatform for LinuxUpdatePlatform {
    fn installation(&self) -> Result<Installation> {
        let executable = installed_executable(self.executable.as_deref())?;
        let root = installation_root(&executable).ok_or_else(|| {
            Error::NotConfigured(
                "this is not an installed release build (no build information)".into(),
            )
        })?;
        check_location(&root)?;
        let identity = self.inspect(&root)?;
        if !target_matches_host(&identity.target) {
            return Err(service(
                "update_wrong_target",
                format!("the installation is built for {}", identity.target),
            ));
        }
        Ok(Installation {
            root,
            identity,
            signer: SignerIdentity {
                facility: SigningFacility::ManifestOnly,
                signer: None,
                requirement: None,
            },
        })
    }

    fn replacement(&self) -> Replacement {
        // `renameat2` moves the folder's directory entry while its
        // executables run.
        Replacement::InPlace
    }

    fn validate_release_tree(
        &self,
        root: &Path,
        _: &SignerIdentity,
        has_inventory: bool,
    ) -> Result<()> {
        if has_inventory {
            return Ok(());
        }
        broscribe_platform::release_layout::ReleaseLayout::Linux.validate(root, false)
    }

    fn inspect(&self, root: &Path) -> Result<PayloadIdentity> {
        match std::fs::symlink_metadata(root) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(Error::InvalidInput(format!(
                    "{} is not a folder",
                    root.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(root.display().to_string()));
            }
            Err(e) => return Err(Error::io_at(root, e)),
        }
        let mut identity = read_build_info(&root.join(BUILD_INFO_FILE), String::new())?;
        identity.app_id = identity.product.clone();
        Ok(identity)
    }

    fn verify(&self, root: &Path, request: &VerifyRequest<'_>) -> Result<SignerIdentity> {
        if request.pin.is_some() {
            return Err(Error::InvalidInput(
                "Linux has no OS code signing to enforce a signer pin; remove updates.signer"
                    .into(),
            ));
        }
        let machine = host_machine()?;
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if check_tree(root, uid, machine)? == 0 {
            return Err(service(
                "update_wrong_target",
                "the payload holds no executable for this machine",
            ));
        }
        let identity = self.inspect(root)?;
        if !target_matches_host(&identity.target) {
            return Err(service(
                "update_wrong_target",
                format!("the payload is built for {}", identity.target),
            ));
        }
        if identity.app_id != request.expected.app_id {
            return Err(service(
                "update_wrong_app",
                format!("the payload is {}", identity.app_id),
            ));
        }
        Ok(SignerIdentity {
            facility: SigningFacility::ManifestOnly,
            signer: None,
            requirement: None,
        })
    }

    fn exchange(&self, a: &Path, b: &Path) -> Result<bool> {
        match renameat2(a, b, libc::RENAME_EXCHANGE) {
            Ok(()) => {
                sync_parents(a, b)?;
                Ok(true)
            }
            Err(e) if flag_unsupported(&e) => Ok(false),
            Err(e) => Err(Error::io_at(a, e)),
        }
    }

    fn rename_new(&self, from: &Path, to: &Path) -> Result<()> {
        rename_new_with(from, to, |from, to| {
            renameat2(from, to, libc::RENAME_NOREPLACE)
        })
    }

    fn sync_dir(&self, dir: &Path) -> Result<()> {
        sync_dir(dir)
    }

    fn set_executable(&self, file: &Path, executable: bool) -> Result<()> {
        let mode = if executable { 0o755 } else { 0o644 };
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode))
            .map_err(|e| Error::io_at(file, e))
    }

    fn create_symlink(&self, link: &Path, target: &Path) -> Result<()> {
        std::os::unix::fs::symlink(target, link).map_err(|e| Error::io_at(link, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_must_stay_inside_the_payload() {
        let root = Path::new("/opt/BroScribe");
        let link = root.join("lib/libx.so");
        assert!(link_inside(root, &link, Path::new("libx.so.1")));
        assert!(link_inside(root, &link, Path::new("../bin/tool")));
        assert!(!link_inside(root, &link, Path::new("../../etc")));
        assert!(!link_inside(root, &link, Path::new("/etc/passwd")));
        assert!(!link_inside(root, &link, Path::new("a/../../..")));
    }

    #[test]
    fn a_handed_off_executable_is_qualified_through_its_own_installation() {
        let dir = tempfile::tempdir().unwrap();
        let loose = dir.path().join("broscribe");
        std::fs::write(&loose, b"synthetic").unwrap();
        let error = LinuxUpdatePlatform::for_executable(loose)
            .installation()
            .unwrap_err();
        assert_eq!(error.code(), "not_configured");
        let error = LinuxUpdatePlatform::for_executable(PathBuf::from("bin/broscribe"))
            .installation()
            .unwrap_err();
        assert_eq!(error.code(), "invalid_input");

        // `<root>/bin/<exe>` with build information built for another
        // machine: the handed-off installation's own checks refuse it.
        let root = dir.path().join("BroScribe");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            root.join(BUILD_INFO_FILE),
            r#"{"schema":1,"product":"BroScribe","version":"1.0.0","build":1,"target":"riscv64gc-unknown-none-elf"}"#,
        )
        .unwrap();
        let executable = root.join("bin/broscribe");
        std::fs::write(&executable, b"synthetic").unwrap();
        let error = LinuxUpdatePlatform::for_executable(executable)
            .installation()
            .unwrap_err();
        assert_eq!(error.code(), "update_location_unsupported");
    }

    #[test]
    fn a_custom_temporary_root_is_refused_before_payload_checks() {
        const CHILD: &str = "BROSCRIBE_TEST_CUSTOM_UPDATE_TMPDIR";
        if std::env::var_os(CHILD).is_none() {
            // The child alone gets TMPDIR; parallel tests keep their environment.
            let executable = std::env::current_exe().unwrap();
            let dir = tempfile::Builder::new()
                .prefix("broscribe-custom-tmp-")
                .tempdir_in(executable.parent().unwrap())
                .unwrap();
            let output = std::process::Command::new(executable)
                .args([
                    "--exact",
                    "update::tests::a_custom_temporary_root_is_refused_before_payload_checks",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("TMPDIR", dir.path())
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .contains("update_location_unsupported"),
                "custom TMPDIR qualification failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("BroScribe");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            root.join(BUILD_INFO_FILE),
            r#"{"schema":1,"product":"BroScribe","version":"1.0.0","build":1,"target":"riscv64gc-unknown-none-elf"}"#,
        )
        .unwrap();
        let executable = root.join("bin/broscribe");
        std::fs::write(&executable, b"synthetic").unwrap();
        let error = LinuxUpdatePlatform::for_executable(executable)
            .installation()
            .unwrap_err();
        assert_eq!(error.code(), "update_location_unsupported");
        assert!(
            error.to_string().contains("temporary location"),
            "wrong location refusal: {error}"
        );
        println!("{}", error.code());
    }

    #[test]
    fn folders_are_replaced_in_place() {
        assert_eq!(
            LinuxUpdatePlatform::new().replacement(),
            Replacement::InPlace
        );
        assert_eq!(
            LinuxUpdatePlatform::for_executable(PathBuf::from("/opt/BroScribe/broscribe"))
                .replacement(),
            Replacement::InPlace
        );
    }

    #[test]
    fn executables_must_match_this_machine() {
        let dir = tempfile::tempdir().unwrap();
        let machine = host_machine().unwrap();
        let other = if machine == EM_X86_64 {
            EM_AARCH64
        } else {
            EM_X86_64
        };
        let elf = |machine: u16| {
            let mut head = vec![0x7f, b'E', b'L', b'F', 2, 1];
            head.resize(18, 0);
            head.extend_from_slice(&machine.to_le_bytes());
            head
        };
        let path = dir.path().join("tool");
        std::fs::write(&path, elf(machine)).unwrap();
        assert!(check_executable(&path, machine).unwrap());
        std::fs::write(&path, elf(other)).unwrap();
        assert_eq!(
            check_executable(&path, machine).unwrap_err().code(),
            "update_wrong_target"
        );
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        assert!(!check_executable(&path, machine).unwrap());
        std::fs::write(&path, b"MZ not elf").unwrap();
        assert!(check_executable(&path, machine).is_err());
    }
}

#[cfg(test)]
#[test]
fn rename_without_flags_checks_for_existing_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let from = tmp.path().join("from");
    let to = tmp.path().join("to");
    std::fs::create_dir(&from).unwrap();
    std::fs::create_dir(&to).unwrap();
    let unsupported = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    assert!(matches!(
        rename_new_with(&from, &to, unsupported),
        Err(Error::AlreadyExists(_))
    ));
    assert!(from.exists());
    std::fs::remove_dir(&to).unwrap();
    rename_new_with(&from, &to, unsupported).unwrap();
    assert!(to.exists() && !from.exists());
}
