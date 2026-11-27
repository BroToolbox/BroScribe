//! Windows side of the signed updater.
//!
//! With a configured pin, every `.exe` and `.dll` in the payload must carry a
//! valid Authenticode signature (WinVerifyTrust, generic verify v2, no UI)
//! whose leaf signing certificate has exactly the pinned SHA-256
//! fingerprint, and be a PE image for this machine. Display names are never
//! trusted; moving to a new certificate needs an explicit pin change.
//! Revocation is not checked (it needs the network at install time); the
//! signed release manifest and the fingerprint pin bind the publisher.
//! Without a pin, the signed manifest and digest establish publisher trust;
//! PE architecture and layout checks still apply.
//!
//! Layout: the installation root is the folder holding `build-info.json`:
//! the running (or handed-off original) executable's folder or its parent.
//! Windows has no atomic directory exchange, so
//! [`UpdatePlatform::exchange`] reports `false` and the updater uses its
//! journaled two-step replacement, which needs every BroScribe process
//! stopped first (files in use cannot be moved): the replacement is
//! [`Replacement::AfterExit`], made by the external update runner, copied
//! outside the installation, which qualifies the original executable's
//! installation through [`WindowsUpdatePlatform::for_executable`] and
//! replaces it once [`UpdatePlatform::installation_processes`] finds no
//! process running from it (app, agent, MCP servers and workers alike).
//! Payloads never contain links: archive links are refused.

use std::collections::HashSet;
use std::io::Read;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use broscribe_platform::release_layout::WINDOWS_FFMPEG;
use broscribe_platform::update::{
    BUILD_INFO_FILE, Installation, InstallationProcess, PayloadIdentity, Replacement,
    SignerIdentity, SigningFacility, UpdatePlatform, VerifyRequest, installed_executable,
    read_build_info, target_matches_host,
};
use broscribe_platform::{Error, Result};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, HANDLE, HWND, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::Cryptography::{
    CERT_SHA256_HASH_PROP_ID, CertGetCertificateContextProperty,
};
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
const IMAGE_FILE_MACHINE_ARM64: u16 = 0xaa64;

/// Updates the installation folder the running executable belongs to, or
/// that of the original executable handed to the external update runner.
#[derive(Clone, Debug, Default)]
pub struct WindowsUpdatePlatform {
    /// `None`: the running executable.
    executable: Option<PathBuf>,
}

impl WindowsUpdatePlatform {
    pub fn new() -> Self {
        Self::default()
    }

    /// Qualifies the installation of `executable` (BroScribe's original
    /// executable, handed off by its own `update install`) with exactly the
    /// checks [`Self::new`] applies to the running one, its Authenticode
    /// signer included.
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

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn host_machine() -> Result<u16> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(IMAGE_FILE_MACHINE_AMD64),
        "aarch64" => Ok(IMAGE_FILE_MACHINE_ARM64),
        other => Err(Error::Unsupported(format!(
            "updates are not built for {other} Windows"
        ))),
    }
}

/// The PE machine of the image at `path`.
fn pe_machine(path: &Path) -> Result<u16> {
    let mut head = vec![0u8; 4096];
    let mut file = std::fs::File::open(path).map_err(|e| Error::io_at(path, e))?;
    let read = file.read(&mut head).map_err(|e| Error::io_at(path, e))?;
    head.truncate(read);
    let not_pe = || {
        service(
            "update_signature_invalid",
            format!("{} is not a PE image", path.display()),
        )
    };
    if !head.starts_with(b"MZ") {
        return Err(not_pe());
    }
    let offset = head
        .get(0x3c..0x40)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .ok_or_else(not_pe)?;
    match head.get(offset..offset + 6) {
        Some(pe) if &pe[..4] == b"PE\0\0" => Ok(u16::from_le_bytes([pe[4], pe[5]])),
        _ => Err(not_pe()),
    }
}

/// Verifies the Authenticode signature of `path` and returns the SHA-256
/// fingerprint of its leaf signing certificate.
fn authenticode_leaf(path: &Path) -> Result<String> {
    let path_w = wide(path);
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(path_w.as_ptr()),
        hFile: HANDLE::default(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let no_ui = HWND(INVALID_HANDLE_VALUE.0);
    // SAFETY: `data` and `file` outlive both calls; `path_w` is NUL-terminated.
    let status =
        unsafe { WinVerifyTrust(no_ui, &mut action, (&mut data as *mut WINTRUST_DATA).cast()) };
    let leaf = if status == 0 {
        // SAFETY: the state data is valid until the close call below.
        unsafe { leaf_fingerprint(data.hWVTStateData) }
    } else {
        Err(service(
            "update_signature_invalid",
            format!(
                "{} has no valid Authenticode signature (0x{:08x})",
                path.display(),
                status as u32
            ),
        ))
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    // SAFETY: releases the state opened by the verify call.
    unsafe {
        WinVerifyTrust(no_ui, &mut action, (&mut data as *mut WINTRUST_DATA).cast());
    }
    leaf
}

/// # Safety
/// `state` must be the live state data of a successful WinVerifyTrust call.
unsafe fn leaf_fingerprint(state: HANDLE) -> Result<String> {
    let missing = || {
        service(
            "update_signature_invalid",
            "the signature has no signer certificate",
        )
    };
    // SAFETY: per this function's contract.
    let provider = unsafe { WTHelperProvDataFromStateData(state) };
    if provider.is_null() {
        return Err(missing());
    }
    // SAFETY: `provider` is live; signer 0 is the primary signer.
    let signer = unsafe { WTHelperGetProvSignerFromChain(provider, 0, false, 0) };
    // SAFETY: a non-null signer points into the provider data.
    let signer = unsafe { signer.as_ref() }.ok_or_else(missing)?;
    if signer.csCertChain == 0 || signer.pasCertChain.is_null() {
        return Err(missing());
    }
    // SAFETY: element 0 of a non-empty chain is the leaf.
    let cert = unsafe { (*signer.pasCertChain).pCert };
    if cert.is_null() {
        return Err(missing());
    }
    let mut digest = [0u8; 32];
    let mut length = digest.len() as u32;
    // SAFETY: `cert` is live; the buffer holds `length` bytes.
    unsafe {
        CertGetCertificateContextProperty(
            cert,
            CERT_SHA256_HASH_PROP_ID,
            Some(digest.as_mut_ptr().cast()),
            &mut length,
        )
    }
    .map_err(|e| Error::Internal(format!("certificate fingerprint unavailable: {e}")))?;
    if length != 32 {
        return Err(missing());
    }
    Ok(hex(&digest))
}

fn is_reparse(meta: &std::fs::Metadata) -> bool {
    meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

fn ffmpeg_name(name: &std::ffi::OsStr) -> bool {
    WINDOWS_FFMPEG
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

fn side_by_side_ffmpeg(root: &Path) -> Result<Vec<String>> {
    let mut tools = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|e| Error::io_at(root, e))? {
        let entry = entry.map_err(|e| Error::io_at(root, e))?;
        let name = entry.file_name();
        if !ffmpeg_name(&name) {
            continue;
        }
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
        if meta.is_file() && !is_reparse(&meta) {
            tools.push(name.to_string_lossy().into_owned());
        }
    }
    tools.sort();
    Ok(tools)
}

/// Verifies every shipped image under `root` and counts the executables.
fn check_tree(root: &Path, pin: Option<&str>, machine: u16, preserved: &[String]) -> Result<usize> {
    let mut executables = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| Error::io_at(&dir, e))? {
            let path = entry.map_err(|e| Error::io_at(&dir, e))?.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
            if is_reparse(&meta) {
                return Err(service(
                    "update_signature_invalid",
                    format!("{} is a link or reparse point", path.display()),
                ));
            }
            if dir == root
                && path.file_name().is_some_and(|name| {
                    ffmpeg_name(name) && preserved.iter().any(|file| name == file.as_str())
                })
            {
                if !meta.is_file() {
                    return Err(service(
                        "update_verification_failed",
                        "a preserved FFmpeg tool is not a regular file",
                    ));
                }
                continue;
            }
            if meta.is_dir() {
                pending.push(path);
                continue;
            }
            let extension = path
                .extension()
                .map(|ext| ext.to_string_lossy().to_ascii_lowercase());
            let executable = extension.as_deref() == Some("exe");
            if !executable && extension.as_deref() != Some("dll") {
                continue;
            }
            if pe_machine(&path)? != machine {
                return Err(service(
                    "update_wrong_target",
                    format!("{} is built for another architecture", path.display()),
                ));
            }
            if let Some(pin) = pin
                && authenticode_leaf(&path)? != pin
            {
                return Err(service(
                    "update_signer_mismatch",
                    format!(
                        "{} is signed by another certificate than the pinned one",
                        path.display()
                    ),
                ));
            }
            if executable {
                executables += 1;
            }
        }
    }
    Ok(executables)
}

fn installation_root(executable: &Path) -> Option<PathBuf> {
    let folder = executable.parent()?;
    [Some(folder), folder.parent()]
        .into_iter()
        .flatten()
        .find(|candidate| candidate.join(BUILD_INFO_FILE).is_file())
        .map(Path::to_path_buf)
}

/// Lowercase file names of the executables shipped under `root`; links and
/// reparse points are not followed.
fn shipped_executables(root: &Path) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| Error::io_at(&dir, e))? {
            let path = entry.map_err(|e| Error::io_at(&dir, e))?.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
            if is_reparse(&meta) {
                continue;
            }
            if meta.is_dir() {
                pending.push(path);
            } else if let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_lowercase())
                && name.ends_with(".exe")
            {
                names.insert(name);
            }
        }
    }
    Ok(names)
}

/// Every process's ID and image file name, from one snapshot.
fn process_snapshot() -> Result<Vec<(u32, String)>> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: the snapshot handle is owned and closed by `OwnedHandle`;
    // `entry` is a correctly sized PROCESSENTRY32W for both calls.
    unsafe {
        let snapshot = crate::native::OwnedHandle(
            CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
                .map_err(|e| crate::error("listing processes", e))?,
        );
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        Process32FirstW(snapshot.0, &mut entry)
            .map_err(|e| crate::error("listing processes", e))?;
        let mut processes = Vec::new();
        loop {
            let name = &entry.szExeFile;
            let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
            processes.push((entry.th32ProcessID, String::from_utf16_lossy(&name[..len])));
            if Process32NextW(snapshot.0, &mut entry).is_err() {
                break;
            }
        }
        Ok(processes)
    }
}

/// One listed process: its ID, image file name and, when it could be
/// queried, its canonical image path.
struct Listed {
    pid: u32,
    name: String,
    image: Option<PathBuf>,
}

/// The listed processes other than `own` that run from `root` (canonical):
/// an image path inside it, or, when the image could not be read, a file
/// name `shipped` inside it (counted, since it cannot be excluded). A
/// readable image elsewhere never counts, whatever its process ID was.
fn installation_members(
    root: &Path,
    shipped: &HashSet<String>,
    own: u32,
    listed: impl IntoIterator<Item = Listed>,
) -> Vec<InstallationProcess> {
    listed
        .into_iter()
        .filter(|process| process.pid != 0 && process.pid != own)
        .filter_map(|process| match process.image {
            Some(image) => image.starts_with(root).then_some(InstallationProcess {
                pid: process.pid,
                image,
            }),
            None => shipped
                .contains(&process.name.to_lowercase())
                .then(|| InstallationProcess {
                    pid: process.pid,
                    image: PathBuf::from(process.name),
                }),
        })
        .collect()
}

/// The user must be able to create and remove entries beside the root.
fn check_writable(parent: &Path) -> Result<()> {
    let probe = parent.join(format!(".broscribe-update-probe-{}", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|_| {
            service(
                "update_location_unsupported",
                format!("you cannot replace entries in {}", parent.display()),
            )
        })?;
    std::fs::remove_file(&probe).map_err(|e| Error::io_at(&probe, e))
}

fn sync_dir(dir: &Path) -> Result<()> {
    let handle = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(dir)
        .map_err(|e| Error::io_at(dir, e))?;
    handle.sync_all().map_err(|e| Error::io_at(dir, e))
}

impl UpdatePlatform for WindowsUpdatePlatform {
    fn installation(&self) -> Result<Installation> {
        let executable = installed_executable(self.executable.as_deref())?;
        let temp = std::env::temp_dir();
        if let Some(problem) = crate::install_location::location_problem(
            &executable.to_string_lossy(),
            std::fs::canonicalize(&temp)
                .ok()
                .as_deref()
                .and_then(Path::to_str),
        ) {
            return Err(service("update_location_unsupported", problem));
        }
        let root = installation_root(&executable).ok_or_else(|| {
            Error::NotConfigured(
                "this is not an installed release build (no build information)".into(),
            )
        })?;
        let meta = std::fs::symlink_metadata(&root).map_err(|e| Error::io_at(&root, e))?;
        if is_reparse(&meta) {
            return Err(service(
                "update_location_unsupported",
                "the installation folder is a link; install BroScribe in a plain folder",
            ));
        }
        let parent = root.parent().ok_or_else(|| {
            service(
                "update_location_unsupported",
                "the installation folder has no parent",
            )
        })?;
        check_writable(parent)?;
        let identity = self.inspect(&root)?;
        if !target_matches_host(&identity.target) {
            return Err(service(
                "update_wrong_target",
                format!("the installation is built for {}", identity.target),
            ));
        }
        Ok(Installation {
            signer: SignerIdentity {
                facility: SigningFacility::Authenticode,
                signer: authenticode_leaf(&executable).ok(),
                requirement: None,
            },
            root,
            identity,
        })
    }

    fn replacement(&self) -> Replacement {
        // Files in use cannot be moved: every process running from the
        // installation must exit before the runner replaces it.
        Replacement::AfterExit
    }

    fn directory_identity(
        &self,
        path: &Path,
    ) -> Result<Option<broscribe_platform::uninstall::DirectoryIdentity>> {
        crate::uninstall::directory_identity(path)
    }

    fn inventory_exceptions(&self) -> &[&str] {
        broscribe_platform::release_layout::WINDOWS_FFMPEG
    }

    fn preserved_files(&self, root: &Path) -> Result<Vec<String>> {
        side_by_side_ffmpeg(root)
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
        broscribe_platform::release_layout::ReleaseLayout::Windows.validate(root, false)
    }

    fn inspect(&self, root: &Path) -> Result<PayloadIdentity> {
        match std::fs::symlink_metadata(root) {
            Ok(meta) if meta.is_dir() && !is_reparse(&meta) => {}
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
        let pin = request.pin;
        if pin.is_some_and(|pin| {
            pin.len() != 64
                || !pin
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(Error::InvalidInput(
                "updates.signer must be the lowercase hex SHA-256 fingerprint of the signing certificate"
                    .into(),
            ));
        }
        if check_tree(root, pin, host_machine()?, request.preserved_files)? == 0 {
            return Err(service(
                "update_wrong_target",
                "the payload holds no executable",
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
            facility: if pin.is_some() {
                SigningFacility::Authenticode
            } else {
                SigningFacility::ManifestOnly
            },
            signer: pin.map(str::to_owned),
            requirement: None,
        })
    }

    fn exchange(&self, _a: &Path, _b: &Path) -> Result<bool> {
        // Windows offers no atomic exchange of two directory entries.
        Ok(false)
    }

    fn rename_new(&self, from: &Path, to: &Path) -> Result<()> {
        let from_w = wide(from);
        let to_w = wide(to);
        // SAFETY: both paths are NUL-terminated and live for the call.
        let moved = unsafe {
            MoveFileExW(
                PCWSTR(from_w.as_ptr()),
                PCWSTR(to_w.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        };
        match moved {
            Ok(()) => Ok(()),
            Err(e)
                if e.code() == ERROR_ALREADY_EXISTS.to_hresult()
                    || e.code() == ERROR_FILE_EXISTS.to_hresult() =>
            {
                Err(Error::AlreadyExists(to.display().to_string()))
            }
            Err(e) => Err(Error::io_at(
                from,
                std::io::Error::from_raw_os_error(e.code().0 & 0xffff),
            )),
        }
    }

    fn sync_dir(&self, dir: &Path) -> Result<()> {
        sync_dir(dir)
    }

    fn set_executable(&self, _file: &Path, _executable: bool) -> Result<()> {
        // Windows has no execute permission bit; images run by extension.
        Ok(())
    }

    fn create_symlink(&self, link: &Path, _target: &Path) -> Result<()> {
        Err(Error::Unsupported(format!(
            "Windows update payloads never contain links ({})",
            link.display()
        )))
    }

    fn installation_processes(&self, root: &Path) -> Result<Vec<InstallationProcess>> {
        let shipped = shipped_executables(root)?;
        let listed = process_snapshot()?.into_iter().map(|(pid, name)| Listed {
            pid,
            image: crate::native::process_path(pid)
                .and_then(|path| std::fs::canonicalize(path).ok()),
            name,
        });
        Ok(installation_members(
            root,
            &shipped,
            std::process::id(),
            listed,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handed_off_executable_is_qualified_through_its_own_installation() {
        let error =
            WindowsUpdatePlatform::for_executable(PathBuf::from(r"BroScribe\broscribe.exe"))
                .installation()
                .unwrap_err();
        assert_eq!(error.code(), "invalid_input");

        // An installation inside the temporary folder (an unextracted
        // download): the handed-off executable's own location is refused
        // before anything trusts its build information or signature.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("BroScribe");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join(BUILD_INFO_FILE),
            r#"{"schema":1,"product":"BroScribe","version":"1.0.0","build":1,"target":"x86_64-pc-windows-msvc"}"#,
        )
        .unwrap();
        let executable = root.join("broscribe.exe");
        std::fs::write(&executable, b"MZ").unwrap();
        let error = WindowsUpdatePlatform::for_executable(executable)
            .installation()
            .unwrap_err();
        assert_eq!(error.code(), "update_location_unsupported");
    }

    #[test]
    fn installations_are_replaced_after_exit() {
        assert_eq!(
            WindowsUpdatePlatform::new().replacement(),
            Replacement::AfterExit
        );
        assert_eq!(
            WindowsUpdatePlatform::for_executable(PathBuf::from(r"C:\BroScribe\broscribe.exe"))
                .replacement(),
            Replacement::AfterExit
        );
    }

    #[test]
    fn installation_members_are_identified_by_their_image_not_their_pid() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("BroScribe");
        let shipped: HashSet<String> = ["broscribe.exe".to_owned(), "broscribe-app.exe".to_owned()]
            .into_iter()
            .collect();
        let listed = |pid: u32, name: &str, image: Option<PathBuf>| Listed {
            pid,
            name: name.to_owned(),
            image,
        };
        let members = installation_members(
            &root,
            &shipped,
            7,
            [
                // The app and an MCP server running from the installation.
                listed(
                    10,
                    "broscribe-app.exe",
                    Some(root.join("broscribe-app.exe")),
                ),
                listed(
                    11,
                    "broscribe.exe",
                    Some(root.join("bin").join("broscribe.exe")),
                ),
                // A reused PID now running a program elsewhere, even one of
                // a shipped name, and a sibling folder sharing the prefix.
                listed(12, "broscribe.exe", Some(dir.path().join("broscribe.exe"))),
                listed(
                    13,
                    "tool.exe",
                    Some(root.with_file_name("BroScribe2").join("tool.exe")),
                ),
                // Unreadable images: counted only under a shipped name.
                listed(14, "BroScribe.EXE", None),
                listed(15, "explorer.exe", None),
                // The caller and the idle process never count.
                listed(
                    7,
                    "broscribe-update-runner.exe",
                    Some(root.join("runner.exe")),
                ),
                listed(0, "broscribe.exe", None),
            ],
        );
        assert_eq!(
            members,
            vec![
                InstallationProcess {
                    pid: 10,
                    image: root.join("broscribe-app.exe"),
                },
                InstallationProcess {
                    pid: 11,
                    image: root.join("bin").join("broscribe.exe"),
                },
                InstallationProcess {
                    pid: 14,
                    image: PathBuf::from("BroScribe.EXE"),
                },
            ]
        );
    }

    #[test]
    fn shipped_executables_are_found_throughout_the_installation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("BroScribe");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("BroScribe-App.exe"), b"MZ").unwrap();
        std::fs::write(root.join("bin").join("broscribe.exe"), b"MZ").unwrap();
        std::fs::write(root.join("bin").join("helper.dll"), b"MZ").unwrap();
        let names = shipped_executables(&root).unwrap();
        assert_eq!(
            names,
            ["broscribe-app.exe".to_owned(), "broscribe.exe".to_owned()]
                .into_iter()
                .collect::<HashSet<_>>()
        );
    }

    #[test]
    fn unsigned_payload_requires_manifest_only_trust_and_is_refused_with_a_pin() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(
            root.join(BUILD_INFO_FILE),
            r#"{"schema":1,"product":"BroScribe","version":"1.0.0","build":1,"target":"x86_64-pc-windows-msvc"}"#,
        ).unwrap();
        let mut image = vec![0; 0x86];
        image[..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        image[0x80..0x84].copy_from_slice(b"PE\0\0");
        image[0x84..].copy_from_slice(&host_machine().unwrap().to_le_bytes());
        std::fs::write(root.join("broscribe.exe"), image).unwrap();
        let platform = WindowsUpdatePlatform::new();
        let expected = platform.inspect(root).unwrap();
        let installed = SignerIdentity {
            facility: SigningFacility::Authenticode,
            signer: None,
            requirement: None,
        };
        let request = VerifyRequest {
            expected: &expected,
            preserved_files: &[],
            installed: &installed,
            pin: None,
        };
        let signer = platform.verify(root, &request).unwrap();
        assert_eq!(signer.facility, SigningFacility::ManifestOnly);
        assert_eq!(signer.signer, None);
        let pin = "ab".repeat(32);
        let request = VerifyRequest {
            pin: Some(&pin),
            ..request
        };
        assert_eq!(
            platform.verify(root, &request).unwrap_err().code(),
            "update_signature_invalid"
        );
    }

    #[test]
    fn regular_ffmpeg_tools_are_selected_regardless_of_ascii_case() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("ffmpeg.exe"), b"user tool").unwrap();
        std::fs::write(root.join("FFPLAY.EXE"), b"other case").unwrap();
        std::fs::create_dir(root.join("ffprobe.exe")).unwrap();
        std::fs::write(root.join("other.exe"), b"other").unwrap();
        std::fs::write(root.join("avcodec.dll"), b"shared build dependency").unwrap();
        assert_eq!(
            side_by_side_ffmpeg(root).unwrap(),
            ["FFPLAY.EXE", "ffmpeg.exe"]
        );
    }

    #[test]
    fn pe_machine_is_read_from_the_nt_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.exe");
        let mut image = b"MZ".to_vec();
        image.resize(0x3c, 0);
        image.extend_from_slice(&0x80u32.to_le_bytes());
        image.resize(0x80, 0);
        image.extend_from_slice(b"PE\0\0");
        image.extend_from_slice(&IMAGE_FILE_MACHINE_ARM64.to_le_bytes());
        std::fs::write(&path, &image).unwrap();
        assert_eq!(pe_machine(&path).unwrap(), IMAGE_FILE_MACHINE_ARM64);
        std::fs::write(&path, b"\x7fELF").unwrap();
        assert!(pe_machine(&path).is_err());
    }
}
