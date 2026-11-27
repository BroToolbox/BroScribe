//! Operating-system side of the signed, opt-in updater (SH23).
//!
//! `broscribe-updates` owns the release envelope, download, extraction,
//! journal and recovery; everything that touches the OS goes through
//! [`UpdatePlatform`]: locating and qualifying the running installation,
//! reading a payload's sealed identity, native signature verification and
//! the directory-entry operations replacement needs. Native backends live in
//! the platform crates; [`FakeUpdatePlatform`] is the deterministic double.
//!
//! Trust boundary: the Ed25519-signed release manifest (pinned keys from the
//! configuration) binds version, build, target, size and SHA-256 of every
//! artifact on every platform. On top of it, macOS requires the payload to
//! satisfy the installed app's designated requirement, the pinned team and
//! the expected identifier with strict nested validation
//! (Security.framework; this says nothing about notarization), Windows
//! requires valid Authenticode signatures on every executable image
//! (WinVerifyTrust) whose leaf signing certificate has the pinned SHA-256
//! fingerprint (never a display name), and Linux has no OS code-signing
//! facility: the signed manifest is the only signature, with native checks
//! limited to architecture, ownership and permission hygiene. A signer
//! change (certificate rotation, new team) needs an explicit change of the
//! configured pin; nothing follows a signer silently.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Name of the build information file a payload carries
/// (`scripts/release-metadata.py build-info`).
pub const BUILD_INFO_FILE: &str = "build-info.json";
/// Upper bound on the build information file; it is a few hundred bytes.
pub const BUILD_INFO_MAX_BYTES: u64 = 64 * 1024;

/// What a payload says it is: product, application identifier and the
/// release coordinates from its build information.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadIdentity {
    /// `product` of the build information (`BroScribe`).
    pub product: String,
    /// macOS: the code-signing identifier (the bundle identifier). Linux and
    /// Windows: the product name; there is no other sealed identifier.
    pub app_id: String,
    /// `MAJOR.MINOR.PATCH`.
    pub version: String,
    /// Monotonic build number.
    pub build: u64,
    /// Rust target triple the payload was built for.
    pub target: String,
}

/// The OS facility that established a signer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SigningFacility {
    /// Security.framework static code validation. Says nothing about
    /// notarization or Gatekeeper assessment.
    AppleCodeSigning,
    /// WinVerifyTrust Authenticode validation.
    Authenticode,
    /// No OS code-signing facility: only the signed release manifest vouches
    /// for the payload.
    ManifestOnly,
    /// [`FakeUpdatePlatform`] only; no native backend reports it.
    Simulated,
}

/// Who signed a payload, as established by the platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignerIdentity {
    pub facility: SigningFacility,
    /// The pinnable signer: the Apple team identifier, or the lowercase hex
    /// SHA-256 fingerprint of the Authenticode leaf signing certificate.
    /// `None` for ad-hoc/unsigned code and [`SigningFacility::ManifestOnly`].
    pub signer: Option<String>,
    /// macOS: the designated requirement text a replacement must satisfy so
    /// that permissions granted to the installed app carry over.
    pub requirement: Option<String>,
}

/// The installation an update replaces as one unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installation {
    /// The directory replaced as a whole: the `.app` bundle on macOS, the
    /// installation directory elsewhere. Canonical, not a symbolic link.
    pub root: PathBuf,
    pub identity: PayloadIdentity,
    pub signer: SignerIdentity,
}

/// A live process whose executable image lies inside an installation
/// ([`UpdatePlatform::installation_processes`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallationProcess {
    pub pid: u32,
    /// The image path as the OS reports it, canonical where it resolves;
    /// only the image's file name when the process could not be queried
    /// and is counted because an executable of that name ships inside.
    pub image: PathBuf,
}

/// How an installation can be replaced while BroScribe runs from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replacement {
    /// The installation's directory entry can be exchanged or renamed while
    /// its executables run (macOS bundles, Linux folders): `update install`
    /// replaces it itself.
    InPlace,
    /// Files in use cannot be moved (Windows): every process running from
    /// the installation must exit first, so the replacement is made by the
    /// external update runner copied outside the installation, which names
    /// the original executable through `NativePlatform::updater_for`.
    AfterExit,
}

/// What [`UpdatePlatform::verify`] checks a candidate payload against.
#[derive(Clone, Copy, Debug)]
pub struct VerifyRequest<'a> {
    /// Identity the signed manifest promises (application identifier and target).
    pub expected: &'a PayloadIdentity,
    /// The signer of the running installation. macOS requires its
    /// designated requirement; elsewhere the pin alone decides, so a
    /// certificate rotation needs an explicit pin change.
    pub installed: &'a SignerIdentity,
    /// Signer pinned in the configuration ([`SignerIdentity::signer`] form),
    /// required wherever an OS signing facility exists.
    pub pin: Option<&'a str>,
    /// Previously installed third-party files, absent during verification of
    /// the download and exempted only from the post-swap publisher check.
    pub preserved_files: &'a [String],
}

/// The executable whose installation an [`UpdatePlatform`] qualifies,
/// canonical: the running executable, or `handed_off`, the original
/// executable that BroScribe's own `update install` hands to the external
/// update runner (which runs from a copy outside the installation, so its
/// own executable says nothing about it). A handed-off path must be
/// absolute and resolve to a regular file. Resolution is all it trusts:
/// links resolve to the real file, and every native location, ownership,
/// identity and signer check of [`UpdatePlatform::installation`] then
/// applies to it exactly as to the running executable.
pub fn installed_executable(handed_off: Option<&Path>) -> Result<PathBuf> {
    let Some(path) = handed_off else {
        return std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|e| Error::io("locating the running executable", e));
    };
    if !path.is_absolute() {
        return Err(Error::InvalidInput(format!(
            "the installed executable must be an absolute path, not {}",
            path.display()
        )));
    }
    let canonical = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::NotFound(path.display().to_string()));
        }
        Err(e) => return Err(Error::io_at(path, e)),
    };
    let meta = std::fs::metadata(&canonical).map_err(|e| Error::io_at(&canonical, e))?;
    if !meta.is_file() {
        return Err(Error::InvalidInput(format!(
            "{} is not an executable file",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// OS operations of the updater. Every method fails closed: a check that
/// cannot be performed is an error, never a pass.
pub trait UpdatePlatform: Send + Sync {
    /// Publisher pin compiled into this platform's updater.
    fn default_signer_pin(&self) -> Option<&'static str> {
        None
    }

    /// The installation an update replaces: that of the running executable,
    /// or of the handed-off original executable for a platform made for the
    /// external update runner ([`installed_executable`]). Refuses locations
    /// an update must not replace: development builds without build
    /// information, translocated, temporary, network or read-only
    /// locations, and trees the user does not own or cannot rename.
    fn installation(&self) -> Result<Installation>;

    /// Whether the installation can be replaced while BroScribe runs from
    /// it. A capability of the platform, never a guess: a platform that
    /// cannot move files in use reports [`Replacement::AfterExit`].
    fn replacement(&self) -> Replacement;

    /// Check the native seal and, when there is no authenticated inventory,
    /// the known release layout. The caller checks any supplied inventory.
    fn validate_release_tree(
        &self,
        root: &Path,
        signer: &SignerIdentity,
        has_inventory: bool,
    ) -> Result<()>;

    fn directory_identity(
        &self,
        path: &Path,
    ) -> Result<Option<crate::uninstall::DirectoryIdentity>> {
        crate::uninstall::directory_identity(path)
    }

    fn inventory_exceptions(&self) -> &[&str] {
        &[]
    }

    fn ignores_finder_metadata(&self) -> bool {
        false
    }

    /// Third-party side-by-side files to carry into a verified update.
    /// Returns exact basenames of regular files at the installation root.
    fn preserved_files(&self, _root: &Path) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// The identity of the payload rooted at `root` (installed or staged),
    /// from its build information and, where one exists, its sealed
    /// identifier. A missing `root` is [`Error::NotFound`]. Inspection is not
    /// verification: call [`Self::verify`] before trusting the answer.
    fn inspect(&self, root: &Path) -> Result<PayloadIdentity>;

    /// Native verification of the payload at `root`: every signature the OS
    /// can check, the signer against `request.pin` and the installed signer,
    /// and that executables run on this machine's architecture.
    fn verify(&self, root: &Path, request: &VerifyRequest<'_>) -> Result<SignerIdentity>;

    /// Atomically exchanges two directory entries on one volume and makes it
    /// durable. `Ok(false)` only when the volume has no atomic exchange, in
    /// which case nothing changed.
    fn exchange(&self, a: &Path, b: &Path) -> Result<bool>;

    /// Renames `from` to `to` on one volume, refusing an existing `to`, and
    /// makes it durable.
    fn rename_new(&self, from: &Path, to: &Path) -> Result<()>;

    /// Makes completed changes to the entries of directory `dir` durable.
    fn sync_dir(&self, dir: &Path) -> Result<()>;

    /// Gives an extracted regular file the archive's execute permission
    /// (owner read/write always; no set-ID, sticky or group/other write bits).
    fn set_executable(&self, file: &Path, executable: bool) -> Result<()>;

    /// Creates a relative symbolic link at `link`. Platforms whose payloads
    /// never contain links refuse with [`Error::Unsupported`].
    fn create_symlink(&self, link: &Path, target: &Path) -> Result<()>;

    /// The live processes, other than the caller, whose executable image
    /// lies inside `root` ([`Installation::root`]). Each call reads every
    /// process's image again, so a process ID reused by a program outside
    /// `root` never counts. A process whose image cannot be read counts when
    /// an executable of its name ships inside `root`; a process list that
    /// cannot be read is an error. [`Replacement::AfterExit`] platforms
    /// replace the installation only once this is empty. In-place platforms
    /// need no exit barrier and do not offer this query.
    fn installation_processes(&self, _root: &Path) -> Result<Vec<InstallationProcess>> {
        Err(Error::Unsupported(
            "this platform replaces installations in place without an exit barrier".into(),
        ))
    }
}

#[derive(Deserialize)]
struct BuildInfo {
    schema: u32,
    product: String,
    version: String,
    build: u64,
    target: String,
}

/// Reads `file` as build information and pairs it with `app_id`. The file
/// must be a regular file (not a link) of at most [`BUILD_INFO_MAX_BYTES`].
pub fn read_build_info(file: &Path, app_id: String) -> Result<PayloadIdentity> {
    let meta = std::fs::symlink_metadata(file).map_err(|e| Error::io_at(file, e))?;
    if !meta.file_type().is_file() {
        return Err(Error::InvalidManifest(format!(
            "{} is not a regular file",
            file.display()
        )));
    }
    let mut text = Vec::new();
    std::fs::File::open(file)
        .map_err(|e| Error::io_at(file, e))?
        .take(BUILD_INFO_MAX_BYTES + 1)
        .read_to_end(&mut text)
        .map_err(|e| Error::io_at(file, e))?;
    if text.len() as u64 > BUILD_INFO_MAX_BYTES {
        return Err(Error::InvalidManifest(format!(
            "{} is larger than {BUILD_INFO_MAX_BYTES} bytes",
            file.display()
        )));
    }
    let info: BuildInfo = serde_json::from_slice(&text)
        .map_err(|e| Error::InvalidManifest(format!("{}: {e}", file.display())))?;
    if info.schema != 1 {
        return Err(Error::InvalidManifest(format!(
            "{}: unsupported build information schema {}",
            file.display(),
            info.schema
        )));
    }
    if info.product.is_empty() || info.target.is_empty() || info.version.is_empty() {
        return Err(Error::InvalidManifest(format!(
            "{}: empty product, version or target",
            file.display()
        )));
    }
    Ok(PayloadIdentity {
        product: info.product,
        app_id,
        version: info.version,
        build: info.build,
        target: info.target,
    })
}

/// Whether target triple `target` names this process's architecture and
/// operating system (`aarch64-apple-darwin` on Apple silicon Macs).
pub fn target_matches_host(target: &str) -> bool {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => return false,
    };
    let mut parts = target.splitn(2, '-');
    parts.next() == Some(std::env::consts::ARCH)
        && parts.next().is_some_and(|rest| rest.contains(os))
}

/// A fault the fake raises instead of performing an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeFault {
    /// Panics before the n-th (zero-based) directory-entry change, as if the
    /// process died: no error path runs, only unwinding.
    CrashBeforeChange(usize),
    /// Fails a rename of the installed root with the supplied OS error.
    RenameInstallation(i32),
}

#[derive(Default)]
struct FakeState {
    changes: usize,
    log: Vec<String>,
    processes: Vec<InstallationProcess>,
}

/// Deterministic [`UpdatePlatform`] over real directories.
///
/// A payload is a directory with `build-info.json` whose extra `app_id`
/// field supplies the application identifier, and a `signature` file
/// holding the simulated signer. Verification fails when the signature file
/// is missing or empty, when a `tampered` file exists, when the signer
/// differs from the pin or the installed signer, or when the payload's
/// target is not [`FakeUpdatePlatform::host_target`]. Directory changes are
/// real renames, logged; [`FakeFault`] simulates a crash between them.
pub struct FakeUpdatePlatform {
    pub installed_root: PathBuf,
    pub host_target: String,
    /// Whether [`UpdatePlatform::exchange`] is available.
    pub atomic_exchange: bool,
    /// What [`UpdatePlatform::replacement`] reports. [`Self::new`] sets
    /// [`Replacement::InPlace`] (the fake's renames are real and work on
    /// directories in use on Unix); tests of the Windows flow set
    /// [`Replacement::AfterExit`].
    pub replacement: Replacement,
    pub release_layout: Option<crate::release_layout::ReleaseLayout>,
    pub side_by_side: &'static [&'static str],
    fault: Mutex<Option<FakeFault>>,
    state: Mutex<FakeState>,
}

impl FakeUpdatePlatform {
    pub fn new(installed_root: PathBuf, host_target: &str) -> Self {
        Self {
            installed_root,
            host_target: host_target.to_owned(),
            atomic_exchange: true,
            replacement: Replacement::InPlace,
            release_layout: None,
            side_by_side: &[],
            fault: Mutex::new(None),
            state: Mutex::new(FakeState::default()),
        }
    }

    pub fn set_fault(&self, fault: Option<FakeFault>) {
        *self.fault.lock().unwrap() = fault;
    }

    /// The live processes [`UpdatePlatform::installation_processes`]
    /// considers; it lists those whose image lies inside the root asked about.
    pub fn set_processes(&self, processes: Vec<InstallationProcess>) {
        self.state.lock().unwrap().processes = processes;
    }

    /// Directory-entry changes performed so far, in order.
    pub fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }

    /// Writes a fake payload at `root`.
    pub fn write_payload(root: &Path, identity: &PayloadIdentity, signer: &str) -> Result<()> {
        std::fs::create_dir_all(root).map_err(|e| Error::io_at(root, e))?;
        let info = serde_json::json!({
            "schema": 1,
            "product": identity.product,
            "app_id": identity.app_id,
            "version": identity.version,
            "build": identity.build,
            "target": identity.target,
        });
        let path = root.join(BUILD_INFO_FILE);
        std::fs::write(&path, info.to_string()).map_err(|e| Error::io_at(&path, e))?;
        let path = root.join("signature");
        std::fs::write(&path, signer).map_err(|e| Error::io_at(&path, e))
    }

    fn change(&self, what: String) {
        let fault = *self.fault.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let index = state.changes;
        state.changes += 1;
        if fault == Some(FakeFault::CrashBeforeChange(index)) {
            // No lock is held while unwinding, so the fake stays usable.
            drop(state);
            panic!("simulated crash before change {index}: {what}");
        }
        state.log.push(what);
    }

    fn signer_at(&self, root: &Path) -> Option<String> {
        let file = if self.release_layout.is_some() {
            "NOTICE"
        } else {
            "signature"
        };
        std::fs::read_to_string(root.join(file))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }
}

impl UpdatePlatform for FakeUpdatePlatform {
    fn installation(&self) -> Result<Installation> {
        let identity = self.inspect(&self.installed_root)?;
        Ok(Installation {
            root: self.installed_root.clone(),
            identity,
            signer: SignerIdentity {
                facility: SigningFacility::Simulated,
                signer: self.signer_at(&self.installed_root),
                requirement: None,
            },
        })
    }

    fn replacement(&self) -> Replacement {
        self.replacement
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
        self.release_layout
            .map_or(Ok(()), |layout| layout.validate(root, false))
    }

    fn inventory_exceptions(&self) -> &[&str] {
        self.side_by_side
    }

    fn preserved_files(&self, root: &Path) -> Result<Vec<String>> {
        Ok(self
            .side_by_side
            .iter()
            .copied()
            .filter(|file| {
                std::fs::symlink_metadata(root.join(file)).is_ok_and(|meta| meta.is_file())
            })
            .map(str::to_owned)
            .collect())
    }

    fn inspect(&self, root: &Path) -> Result<PayloadIdentity> {
        if !root.exists() {
            return Err(Error::NotFound(root.display().to_string()));
        }
        let file = root.join(BUILD_INFO_FILE);
        let app_id = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value.get("app_id")?.as_str().map(str::to_owned))
            .ok_or_else(|| Error::InvalidManifest(format!("{}: no app_id", file.display())))?;
        read_build_info(&file, app_id)
    }

    fn verify(&self, root: &Path, request: &VerifyRequest<'_>) -> Result<SignerIdentity> {
        let refuse = |why: String| Error::Service {
            code: "update_signature_invalid",
            message: why,
        };
        if root.join("tampered").exists() {
            return Err(refuse(
                "the payload's signature does not seal its contents".into(),
            ));
        }
        let signer = self
            .signer_at(root)
            .ok_or_else(|| refuse("the payload is not signed".into()))?;
        if request.pin != Some(signer.as_str())
            || request.installed.signer.as_deref() != Some(signer.as_str())
        {
            return Err(Error::Service {
                code: "update_signer_mismatch",
                message: format!("the payload is signed by {signer}, not the pinned signer"),
            });
        }
        let identity = self.inspect(root)?;
        if identity.target != self.host_target {
            return Err(Error::Service {
                code: "update_wrong_target",
                message: format!("the payload is built for {}", identity.target),
            });
        }
        if identity.app_id != request.expected.app_id {
            return Err(Error::Service {
                code: "update_wrong_app",
                message: format!("the payload is {}", identity.app_id),
            });
        }
        Ok(SignerIdentity {
            facility: SigningFacility::Simulated,
            signer: Some(signer),
            requirement: None,
        })
    }

    fn exchange(&self, a: &Path, b: &Path) -> Result<bool> {
        if !self.atomic_exchange {
            return Ok(false);
        }
        self.change(format!("exchange {} {}", a.display(), b.display()));
        let parked = a.with_extension("fake-exchange");
        let io = |e| Error::io("simulated exchange", e);
        std::fs::rename(a, &parked).map_err(io)?;
        std::fs::rename(b, a).map_err(io)?;
        std::fs::rename(&parked, b).map_err(io)?;
        Ok(true)
    }

    fn rename_new(&self, from: &Path, to: &Path) -> Result<()> {
        if from == self.installed_root
            && let Some(FakeFault::RenameInstallation(code)) = *self.fault.lock().unwrap()
        {
            return Err(Error::io_at(from, std::io::Error::from_raw_os_error(code)));
        }
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(Error::AlreadyExists(to.display().to_string()));
        }
        self.change(format!("rename {} {}", from.display(), to.display()));
        std::fs::rename(from, to).map_err(|e| Error::io_at(from, e))
    }

    fn sync_dir(&self, _dir: &Path) -> Result<()> {
        Ok(())
    }

    fn set_executable(&self, file: &Path, executable: bool) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode))
                .map_err(|e| Error::io_at(file, e))
        }
        #[cfg(not(unix))]
        {
            let _ = (file, executable);
            Ok(())
        }
    }

    fn create_symlink(&self, link: &Path, target: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).map_err(|e| Error::io_at(link, e))
        }
        #[cfg(not(unix))]
        {
            let _ = (link, target);
            Err(Error::Unsupported(
                "the fake creates symbolic links on Unix only".into(),
            ))
        }
    }

    fn installation_processes(&self, root: &Path) -> Result<Vec<InstallationProcess>> {
        let state = self.state.lock().unwrap();
        Ok(state
            .processes
            .iter()
            .filter(|process| process.image.starts_with(root))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_target_matching_needs_arch_and_os() {
        let arch = std::env::consts::ARCH;
        let os = match std::env::consts::OS {
            "macos" => "apple-darwin",
            "linux" => "unknown-linux-gnu",
            "windows" => "pc-windows-msvc",
            other => other,
        };
        assert!(target_matches_host(&format!("{arch}-{os}")));
        assert!(!target_matches_host(&format!("{arch}x-{os}")));
        assert!(!target_matches_host("riscv64gc-unknown-none-elf"));
        assert!(!target_matches_host(arch));
    }

    #[test]
    fn build_info_must_be_a_bounded_regular_file_of_schema_one() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(BUILD_INFO_FILE);
        std::fs::write(
            &file,
            r#"{"schema":2,"product":"BroScribe","version":"1.0.0","build":1,"target":"t"}"#,
        )
        .unwrap();
        assert!(read_build_info(&file, "a".into()).is_err());
        std::fs::write(&file, vec![b' '; BUILD_INFO_MAX_BYTES as usize + 1]).unwrap();
        assert!(read_build_info(&file, "a".into()).is_err());
        std::fs::write(
            &file,
            r#"{"schema":1,"product":"BroScribe","version":"1.0.0","build":7,"target":"t","extra":1}"#,
        )
        .unwrap();
        let identity = read_build_info(&file, "a".into()).unwrap();
        assert_eq!((identity.build, identity.app_id.as_str()), (7, "a"));
    }

    #[test]
    fn the_running_executable_is_the_default_installed_executable() {
        let running = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(installed_executable(None).unwrap(), running);
    }

    #[test]
    fn handed_off_executables_must_be_absolute_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("broscribe");
        std::fs::write(&exe, b"synthetic").unwrap();
        let canonical = std::fs::canonicalize(&exe).unwrap();
        assert_eq!(installed_executable(Some(&exe)).unwrap(), canonical);

        let relative = Path::new("bin").join("broscribe");
        let error = installed_executable(Some(&relative)).unwrap_err();
        assert_eq!(error.code(), "invalid_input");
        let error = installed_executable(Some(&dir.path().join("missing"))).unwrap_err();
        assert_eq!(error.code(), "not_found");
        let error = installed_executable(Some(dir.path())).unwrap_err();
        assert_eq!(error.code(), "invalid_input");
    }

    #[cfg(unix)]
    #[test]
    fn handed_off_links_resolve_to_the_real_installation() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install");
        std::fs::create_dir(&install).unwrap();
        let exe = install.join("broscribe");
        std::fs::write(&exe, b"synthetic").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&exe, &link).unwrap();
        assert_eq!(
            installed_executable(Some(&link)).unwrap(),
            std::fs::canonicalize(&exe).unwrap()
        );
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink(install.join("gone"), &dangling).unwrap();
        assert_eq!(
            installed_executable(Some(&dangling)).unwrap_err().code(),
            "not_found"
        );
    }

    #[test]
    fn the_fake_reports_its_configured_replacement_capability() {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = FakeUpdatePlatform::new(dir.path().to_path_buf(), "t");
        assert_eq!(fake.replacement(), Replacement::InPlace);
        fake.replacement = Replacement::AfterExit;
        assert_eq!(fake.replacement(), Replacement::AfterExit);
        assert_eq!(
            serde_json::to_string(&Replacement::AfterExit).unwrap(),
            "\"after_exit\""
        );
    }
}
