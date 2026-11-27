//! macOS side of the signed updater.
//!
//! Native verification uses Security.framework static code validation with
//! strict, nested, all-architecture checks, links restricted to the bundle
//! and app-like bundle rules. A payload must satisfy both the installed
//! app's designated requirement (so permissions granted to BroScribe carry
//! over) and `anchor apple generic` with the pinned team and the expected
//! identifier. Ad-hoc signed installations have per-build requirements and
//! are refused. Nothing here checks or claims notarization or a Gatekeeper
//! assessment: the update is downloaded by BroScribe itself (no quarantine)
//! and trusted through the signed release manifest plus code signing.
//!
//! The installation is the outermost `.app` bundle holding the running
//! executable (or the original executable handed to the external update
//! runner); translocated, temporary, read-only, network and foreign-owned
//! locations are refused, before anything inspects its signature.
//! Temporary folders are About's rule ([`temporary_folders`]). Replacement
//! uses `renamex_np` (`RENAME_SWAP`, `RENAME_EXCL`) on the bundle's volume.

use std::ffi::{CString, c_void};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use broscribe_platform::about::is_temporary;
use broscribe_platform::update::{
    BUILD_INFO_FILE, Installation, PayloadIdentity, Replacement, SignerIdentity, SigningFacility,
    UpdatePlatform, VerifyRequest, installed_executable, read_build_info, target_matches_host,
};
use broscribe_platform::{Error, Result};
use objc2_core_foundation::{
    CFArray, CFDictionary, CFRetained, CFString, CFType, CFURL, CFURLPathStyle,
};

use crate::about::temporary_folders;

type OsStatus = i32;
type SecCsFlags = u32;

const CHECK_ALL_ARCHITECTURES: SecCsFlags = 1 << 0;
const CHECK_NESTED_CODE: SecCsFlags = 1 << 3;
const STRICT_VALIDATE: SecCsFlags = 1 << 4;
const RESTRICT_SYMLINKS: SecCsFlags = 1 << 7;
const RESTRICT_TO_APP_LIKE: SecCsFlags = 1 << 8;
const VALIDATE: SecCsFlags = CHECK_ALL_ARCHITECTURES
    | CHECK_NESTED_CODE
    | STRICT_VALIDATE
    | RESTRICT_SYMLINKS
    | RESTRICT_TO_APP_LIKE;
/// `kSecCSSigningInformation` for `SecCodeCopySigningInformation`.
const SIGNING_INFORMATION: SecCsFlags = 1 << 1;
const ERR_SEC_CS_UNSIGNED: OsStatus = -67062;
const ERR_SEC_CS_REQ_FAILED: OsStatus = -67050;
const MNT_RDONLY: u32 = 0x0000_0001;
const NETWORK_FS: [&str; 5] = ["nfs", "smbfs", "afpfs", "webdav", "ftp"];
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecStaticCodeCreateWithPath(
        path: NonNull<CFURL>,
        flags: SecCsFlags,
        code: *mut *const c_void,
    ) -> OsStatus;
    fn SecStaticCodeCheckValidityWithErrors(
        code: *const c_void,
        flags: SecCsFlags,
        requirement: *const c_void,
        error: *mut *const c_void,
    ) -> OsStatus;
    fn SecRequirementCreateWithString(
        text: NonNull<CFString>,
        flags: SecCsFlags,
        requirement: *mut *const c_void,
    ) -> OsStatus;
    fn SecCodeCopyDesignatedRequirement(
        code: *const c_void,
        flags: SecCsFlags,
        requirement: *mut *const c_void,
    ) -> OsStatus;
    fn SecRequirementCopyString(
        requirement: *const c_void,
        flags: SecCsFlags,
        text: *mut *const CFString,
    ) -> OsStatus;
    fn SecCodeCopySigningInformation(
        code: *const c_void,
        flags: SecCsFlags,
        information: *mut *const CFDictionary,
    ) -> OsStatus;
    static kSecCodeInfoIdentifier: *const CFString;
    static kSecCodeInfoTeamIdentifier: *const CFString;
    static kSecCFErrorResourceAdded: *const CFString;
    static kSecCFErrorResourceAltered: *const CFString;
    static kSecCFErrorResourceMissing: *const CFString;
    static kSecCFErrorResourceSideband: *const CFString;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
    fn CFErrorCopyUserInfo(error: *const c_void) -> *const CFDictionary;
}

/// A Core Foundation object returned under the create rule.
struct Owned(NonNull<c_void>);

impl Owned {
    fn new(pointer: *const c_void) -> Option<Self> {
        NonNull::new(pointer.cast_mut()).map(Self)
    }

    fn get(&self) -> *const c_void {
        self.0.as_ptr()
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: created by a Copy/Create function and released exactly once.
        unsafe { CFRelease(self.0.as_ptr()) }
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

fn static_code(root: &Path) -> Result<Owned> {
    let url = CFURL::from_directory_path(root)
        .ok_or_else(|| Error::InvalidInput(format!("{} is not a valid path", root.display())))?;
    let mut code = std::ptr::null();
    // SAFETY: `url` is a live CFURL; `code` receives a +1 reference.
    let status = unsafe { SecStaticCodeCreateWithPath(CFRetained::as_ptr(&url), 0, &mut code) };
    if status != 0 {
        return Err(service(
            "update_signature_invalid",
            format!(
                "{} has no readable code signature ({status})",
                root.display()
            ),
        ));
    }
    Owned::new(code)
        .ok_or_else(|| Error::Internal("SecStaticCodeCreateWithPath returned null".into()))
}

fn requirement(text: &str) -> Result<Owned> {
    let text = CFString::from_str(text);
    let mut requirement = std::ptr::null();
    // SAFETY: `text` is a live CFString; `requirement` receives a +1 reference.
    let status =
        unsafe { SecRequirementCreateWithString(CFRetained::as_ptr(&text), 0, &mut requirement) };
    if status != 0 {
        return Err(Error::InvalidInput(format!(
            "code requirement does not compile ({status})"
        )));
    }
    Owned::new(requirement).ok_or_else(|| Error::Internal("null code requirement".into()))
}

/// Validates the signature, returning any affected resource paths the OS can name.
fn check(code: &Owned, requirement: &Owned, what: &str) -> Result<()> {
    let mut error = std::ptr::null();
    // SAFETY: both Security objects are live and any returned error is +1.
    let status = unsafe {
        SecStaticCodeCheckValidityWithErrors(code.get(), VALIDATE, requirement.get(), &mut error)
    };
    let error = Owned::new(error);
    match status {
        0 => Ok(()),
        ERR_SEC_CS_REQ_FAILED => Err(service(
            "update_signer_mismatch",
            format!("the payload does not satisfy {what}"),
        )),
        ERR_SEC_CS_UNSIGNED => Err(service(
            "update_signature_invalid",
            "the payload is not signed",
        )),
        status => {
            let mut message = format!("the payload's code signature is invalid ({status})");
            if let Some(paths) = error.as_ref().and_then(resource_paths) {
                message.push_str(&format!(" for {}", paths.join(", ")));
            }
            Err(service("update_signature_invalid", message))
        }
    }
}

fn resource_paths(error: &Owned) -> Option<Vec<String>> {
    // SAFETY: error is a live CFError from the Security check; the dictionary
    // is returned +1 and owns its values for the duration of the loop.
    let info = Owned::new(unsafe { CFErrorCopyUserInfo(error.get()) }.cast())?;
    let dictionary = unsafe { &*info.get().cast::<CFDictionary>() };
    let keys = unsafe {
        [
            kSecCFErrorResourceAdded,
            kSecCFErrorResourceAltered,
            kSecCFErrorResourceMissing,
            kSecCFErrorResourceSideband,
        ]
    };
    let path = |value: &CFType| {
        let url = value.downcast_ref::<CFURL>()?;
        Some(
            url.file_system_path(CFURLPathStyle::CFURLPOSIXPathStyle)?
                .to_string(),
        )
    };
    let mut paths = Vec::new();
    for key in keys {
        let Some(value) = (unsafe { dictionary.value(key.cast()).cast::<CFType>().as_ref() })
        else {
            continue;
        };
        if let Some(array) = value.downcast_ref::<CFArray>() {
            for index in 0..array.count().min(4) {
                // SAFETY: the index is in bounds and the dictionary owns the array.
                let entry = unsafe { array.value_at_index(index).cast::<CFType>().as_ref() };
                paths.extend(entry.and_then(path));
            }
        } else {
            paths.extend(path(value));
        }
    }
    paths.truncate(4);
    (!paths.is_empty()).then_some(paths)
}

/// The existing Security.framework check verifies nested code and sealed resources.
pub(crate) fn validate_bundle_tree(root: &Path, installed: Option<&str>) -> Result<()> {
    let verify = || {
        if !std::fs::symlink_metadata(root)?.is_dir() {
            return Err(Error::InvalidInput(
                "The bundle is not a regular directory.".into(),
            ));
        }
        let code = static_code(root)?;
        let (identifier, _) = signing_info(&code)?;
        if identifier.as_deref() != Some("org.broscribe.BroScribe") {
            return Err(Error::InvalidInput(
                "The bundle has another code identifier.".into(),
            ));
        }
        let own = designated_requirement(&code)?;
        check(
            &code,
            &requirement(installed.unwrap_or(&own))?,
            "the designated requirement",
        )
    };
    verify().map_err(|error| service(
        broscribe_platform::release_layout::RELEASE_TREE_UNSAFE,
        format!("Could not verify the sealed installation at {}. Move any personal files out and restore the signed BroScribe bundle before retrying. {error}", root.display()),
    ))
}

/// Identifier and team from the signing information (no validation).
fn signing_info(code: &Owned) -> Result<(Option<String>, Option<String>)> {
    let mut info: *const CFDictionary = std::ptr::null();
    // SAFETY: `code` is live; `info` receives a +1 dictionary.
    let status =
        unsafe { SecCodeCopySigningInformation(code.get(), SIGNING_INFORMATION, &mut info) };
    if status != 0 {
        return Err(service(
            "update_signature_invalid",
            format!("the signing information is unreadable ({status})"),
        ));
    }
    let owned = Owned::new(info.cast())
        .ok_or_else(|| Error::Internal("null signing information".into()))?;
    let dictionary = owned.get().cast::<CFDictionary>();
    let text = |key: *const CFString| -> Option<String> {
        // SAFETY: the dictionary is live for this closure; values for these
        // keys are CFStrings owned by it (get rule).
        unsafe {
            let value = (*dictionary).value(key.cast());
            (!value.is_null()).then(|| (*value.cast::<CFString>()).to_string())
        }
    };
    // SAFETY: framework constants, initialised at load time.
    let (identifier, team) = unsafe { (kSecCodeInfoIdentifier, kSecCodeInfoTeamIdentifier) };
    Ok((text(identifier), text(team)))
}

fn designated_requirement(code: &Owned) -> Result<String> {
    let mut requirement = std::ptr::null();
    // SAFETY: `code` is live; `requirement` receives a +1 reference.
    let status = unsafe { SecCodeCopyDesignatedRequirement(code.get(), 0, &mut requirement) };
    let requirement = (status == 0)
        .then(|| Owned::new(requirement))
        .flatten()
        .ok_or_else(|| {
            service(
                "update_signature_invalid",
                format!("the designated requirement is unreadable ({status})"),
            )
        })?;
    let mut text: *const CFString = std::ptr::null();
    // SAFETY: `requirement` is live; `text` receives a +1 CFString.
    let status = unsafe { SecRequirementCopyString(requirement.get(), 0, &mut text) };
    let owned = (status == 0)
        .then(|| Owned::new(text.cast()))
        .flatten()
        .ok_or_else(|| Error::Internal(format!("requirement text unavailable ({status})")))?;
    // SAFETY: a live CFString.
    Ok(unsafe { (*owned.get().cast::<CFString>()).to_string() })
}

/// Requirement-language string literal, refusing characters that would
/// need escaping (identifiers and team IDs never contain them).
fn literal(value: &str) -> Result<String> {
    if value.is_empty()
        || value
            .chars()
            .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return Err(Error::InvalidInput(format!(
            "{value:?} cannot be used in a code requirement"
        )));
    }
    Ok(format!("\"{value}\""))
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::InvalidInput(format!("{} contains a NUL byte", path.display())))
}

fn host_cpu() -> Result<u32> {
    match std::env::consts::ARCH {
        "aarch64" => Ok(CPU_TYPE_ARM64),
        "x86_64" => Ok(CPU_TYPE_X86_64),
        other => Err(Error::Unsupported(format!(
            "updates are not built for {other} Macs"
        ))),
    }
}

/// Whether the Mach-O (thin or universal) at `path` runs on `cpu`. Scripts
/// are accepted; any other executable format is refused.
fn runs_on(path: &Path, cpu: u32) -> Result<bool> {
    let mut file = std::fs::File::open(path).map_err(|e| Error::io_at(path, e))?;
    let mut head = vec![0u8; 8 + 32 * 16];
    let read = file.read(&mut head).map_err(|e| Error::io_at(path, e))?;
    head.truncate(read);
    if head.starts_with(b"#!") {
        return Ok(true);
    }
    let be = |at: usize| {
        head.get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let le = |at: usize| {
        head.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    match be(0) {
        Some(0xcafe_babe) | Some(0xcafe_babf) => {
            let wide = be(0) == Some(0xcafe_babf);
            let stride = if wide { 32 } else { 20 };
            let count = be(4).unwrap_or(0) as usize;
            Ok((0..count.min(16)).any(|i| be(8 + i * stride) == Some(cpu)))
        }
        _ if le(0) == Some(0xfeed_facf) => Ok(le(4) == Some(cpu)),
        _ => Err(service(
            "update_signature_invalid",
            format!(
                "{} is executable but not a Mach-O image or script",
                path.display()
            ),
        )),
    }
}

/// Every executable in `Contents/MacOS` runs on this Mac.
fn check_architecture(root: &Path) -> Result<()> {
    let cpu = host_cpu()?;
    let folder = root.join("Contents/MacOS");
    let mut any = false;
    for entry in std::fs::read_dir(&folder).map_err(|e| Error::io_at(&folder, e))? {
        let path = entry.map_err(|e| Error::io_at(&folder, e))?.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            continue;
        }
        if !runs_on(&path, cpu)? {
            return Err(service(
                "update_wrong_target",
                format!("{} does not run on this Mac's architecture", path.display()),
            ));
        }
        any = true;
    }
    if !any {
        return Err(service(
            "update_wrong_target",
            "the payload has no executable",
        ));
    }
    Ok(())
}

fn check_location(root: &Path) -> Result<()> {
    check_location_in(root, &temporary_folders())
}

/// [`check_location`] against the given canonical `temporary` folders.
fn check_location_in(root: &Path, temporary: &[PathBuf]) -> Result<()> {
    let text = root.to_string_lossy();
    if text.contains("/AppTranslocation/") {
        return Err(location(
            "BroScribe runs translocated; move it to Applications with Finder and open it again",
        ));
    }
    if is_temporary(root, temporary) {
        return Err(location(
            "BroScribe runs from a temporary folder the system may empty; move it to \
             Applications with Finder and open it from there",
        ));
    }
    let path = c_path(root)?;
    // SAFETY: zeroed is a valid statfs; the path is NUL-terminated.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut fs) } != 0 {
        return Err(Error::io_at(root, std::io::Error::last_os_error()));
    }
    if fs.f_flags & MNT_RDONLY != 0 {
        return Err(location(
            "BroScribe runs from a read-only volume (a disk image?); install it first",
        ));
    }
    // SAFETY: f_fstypename is NUL-terminated by the kernel.
    let kind = unsafe { std::ffi::CStr::from_ptr(fs.f_fstypename.as_ptr()) }.to_string_lossy();
    if NETWORK_FS.contains(&kind.as_ref()) {
        return Err(location(
            "BroScribe runs from a network volume; install it locally",
        ));
    }
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let meta = std::fs::symlink_metadata(root).map_err(|e| Error::io_at(root, e))?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(location(
            "BroScribe.app belongs to another user; reinstall it as yourself to update it",
        ));
    }
    let parent = root
        .parent()
        .ok_or_else(|| location("the app has no parent folder"))?;
    for folder in [root, parent] {
        let c = c_path(folder)?;
        // SAFETY: NUL-terminated path.
        if unsafe { libc::access(c.as_ptr(), libc::W_OK) } != 0 {
            return Err(location(format!("you cannot replace {}", folder.display())));
        }
    }
    Ok(())
}

fn bundle_root(executable: &Path) -> Option<PathBuf> {
    executable
        .ancestors()
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "app")
                && path.join("Contents/Info.plist").is_file()
        })
        .last()
        .map(Path::to_path_buf)
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

fn renamex(from: &Path, to: &Path, flags: libc::c_uint) -> std::io::Result<()> {
    let from_c = CString::new(from.as_os_str().as_bytes())?;
    let to_c = CString::new(to.as_os_str().as_bytes())?;
    // SAFETY: both paths are NUL-terminated.
    if unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn flag_unsupported(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::EINVAL))
}

/// Updates the `.app` bundle the running executable belongs to, or that of
/// the original executable handed to the external update runner.
#[derive(Clone, Debug, Default)]
pub struct MacUpdatePlatform {
    /// `None`: the running executable.
    executable: Option<PathBuf>,
}

impl MacUpdatePlatform {
    pub fn new() -> Self {
        Self::default()
    }

    /// Qualifies the bundle of `executable` (BroScribe's original
    /// executable, handed off by its own `update install`) with exactly the
    /// checks [`Self::new`] applies to the running one.
    pub fn for_executable(executable: PathBuf) -> Self {
        Self {
            executable: Some(executable),
        }
    }
}

impl UpdatePlatform for MacUpdatePlatform {
    fn default_signer_pin(&self) -> Option<&'static str> {
        Some("3CHB92P2NM")
    }

    fn installation(&self) -> Result<Installation> {
        let executable = installed_executable(self.executable.as_deref())?;
        let root = bundle_root(&executable).ok_or_else(|| {
            Error::NotConfigured("BroScribe is not running from an app bundle".into())
        })?;
        check_location(&root)?;
        let identity = self.inspect(&root)?;
        if !target_matches_host(&identity.target) {
            return Err(service(
                "update_wrong_target",
                format!("the installation is built for {}", identity.target),
            ));
        }
        let code = static_code(&root)?;
        let (_, team) = signing_info(&code)?;
        let requirement = designated_requirement(&code)?;
        Ok(Installation {
            root,
            identity,
            signer: SignerIdentity {
                facility: SigningFacility::AppleCodeSigning,
                signer: team,
                requirement: Some(requirement),
            },
        })
    }

    fn replacement(&self) -> Replacement {
        // `renamex_np` moves the bundle's directory entry while its
        // executables run.
        Replacement::InPlace
    }

    fn validate_release_tree(&self, root: &Path, signer: &SignerIdentity, _: bool) -> Result<()> {
        let installed = signer.requirement.as_deref().ok_or_else(|| {
            service(
                "update_signature_invalid",
                "The recorded designated requirement is missing.",
            )
        })?;
        validate_bundle_tree(root, Some(installed))
    }

    fn ignores_finder_metadata(&self) -> bool {
        true
    }

    fn inspect(&self, root: &Path) -> Result<PayloadIdentity> {
        match std::fs::symlink_metadata(root) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(Error::InvalidInput(format!(
                    "{} is not a bundle",
                    root.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(root.display().to_string()));
            }
            Err(e) => return Err(Error::io_at(root, e)),
        }
        let (identifier, _) = signing_info(&static_code(root)?)?;
        let identifier = identifier.ok_or_else(|| {
            service(
                "update_signature_invalid",
                format!("{} is not signed", root.display()),
            )
        })?;
        read_build_info(
            &root.join("Contents/Resources").join(BUILD_INFO_FILE),
            identifier,
        )
    }

    fn verify(&self, root: &Path, request: &VerifyRequest<'_>) -> Result<SignerIdentity> {
        let pin = request.pin.ok_or_else(|| {
            Error::NotConfigured("updates.signer must name the Apple team identifier".into())
        })?;
        let installed = request
            .installed
            .requirement
            .as_deref()
            .filter(|text| !text.contains("cdhash"))
            .ok_or_else(|| {
                service(
                    "update_signer_mismatch",
                    "the installed app is ad-hoc signed; its designated requirement cannot accept another build",
                )
            })?;
        if request.installed.signer.as_deref() != Some(pin) {
            return Err(service(
                "update_signer_mismatch",
                "the installed app's team is not the pinned team; a team change cannot keep its permissions and needs a manual reinstall",
            ));
        }
        let code = static_code(root)?;
        let (identifier, team) = signing_info(&code)?;
        if identifier.as_deref() != Some(request.expected.app_id.as_str()) {
            return Err(service(
                "update_wrong_app",
                format!("the payload is signed as {identifier:?}"),
            ));
        }
        if team.as_deref() != Some(pin) {
            return Err(service(
                "update_signer_mismatch",
                format!("the payload is signed by team {team:?}, not the pinned team"),
            ));
        }
        check(
            &code,
            &requirement(installed)?,
            "the installed app's designated requirement",
        )?;
        let pinned = format!(
            "anchor apple generic and identifier {} and certificate leaf[subject.OU] = {}",
            literal(&request.expected.app_id)?,
            literal(pin)?
        );
        check(
            &code,
            &requirement(&pinned)?,
            "the pinned team and identifier",
        )?;
        check_architecture(root)?;
        let identity = self.inspect(root)?;
        if !target_matches_host(&identity.target) {
            return Err(service(
                "update_wrong_target",
                format!("the payload is built for {}", identity.target),
            ));
        }
        Ok(SignerIdentity {
            facility: SigningFacility::AppleCodeSigning,
            signer: team,
            requirement: Some(designated_requirement(&code)?),
        })
    }

    fn exchange(&self, a: &Path, b: &Path) -> Result<bool> {
        match renamex(a, b, libc::RENAME_SWAP) {
            Ok(()) => {
                sync_parents(a, b)?;
                Ok(true)
            }
            Err(e) if flag_unsupported(&e) => Ok(false),
            Err(e) => Err(Error::io_at(a, e)),
        }
    }

    fn rename_new(&self, from: &Path, to: &Path) -> Result<()> {
        match renamex(from, to, libc::RENAME_EXCL) {
            Ok(()) => sync_parents(from, to),
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
                Err(Error::AlreadyExists(to.display().to_string()))
            }
            Err(e) if flag_unsupported(&e) => Err(Error::Unsupported(format!(
                "the volume of {} cannot rename without replacing",
                to.display()
            ))),
            Err(e) => Err(Error::io_at(from, e)),
        }
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
    fn bundle_root_is_the_outermost_app() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("BroScribe.app");
        let helper = app.join("Contents/Library/LoginItems/BroScribe Agent.app");
        for bundle in [&app, &helper] {
            std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
            std::fs::write(bundle.join("Contents/Info.plist"), "").unwrap();
        }
        let agent = helper.join("Contents/MacOS/broscribed");
        assert_eq!(bundle_root(&agent).unwrap(), app);
        assert!(bundle_root(&dir.path().join("broscribe")).is_none());
    }

    fn synthetic_bundle(at: &Path) -> PathBuf {
        std::fs::create_dir_all(at.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(at.join("Contents/Resources")).unwrap();
        std::fs::write(at.join("Contents/Info.plist"), "").unwrap();
        let executable = at.join("Contents/MacOS/broscribe");
        std::fs::write(&executable, b"synthetic").unwrap();
        executable
    }

    #[test]
    fn a_handed_off_executable_is_qualified_through_its_own_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("broscribe");
        std::fs::write(&outside, b"synthetic").unwrap();
        let error = MacUpdatePlatform::for_executable(outside)
            .installation()
            .unwrap_err();
        assert_eq!(error.code(), "not_configured");
        let error = MacUpdatePlatform::for_executable(PathBuf::from("BroScribe.app"))
            .installation()
            .unwrap_err();
        assert_eq!(error.code(), "invalid_input");

        // A bundle in a temporary folder is refused for its location before
        // its (missing) signature is examined: the canonical temporary
        // folder, and `/tmp` and `/var/tmp`, which resolve to their
        // `/private` folders.
        let canonical = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let var_tmp = tempfile::tempdir_in("/var/tmp").unwrap();
        for folder in [canonical.path(), tmp.path(), var_tmp.path()] {
            let unsigned = synthetic_bundle(&folder.join("Applications/BroScribe.app"));
            let error = MacUpdatePlatform::for_executable(unsigned)
                .installation()
                .unwrap_err();
            assert_eq!(
                error.code(),
                "update_location_unsupported",
                "{}",
                folder.display()
            );
        }
    }

    #[test]
    fn a_durable_local_bundle_passes_the_location_rule_and_needs_its_signature() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("Applications/BroScribe.app");
        synthetic_bundle(&root);
        // Where no temporary folder holds it, a user-owned, writable local
        // bundle qualifies; the real rule refuses this one, which lies in
        // the canonical temporary folder.
        check_location_in(&root, &[]).unwrap();
        assert_eq!(
            check_location_in(&root, &temporary_folders())
                .unwrap_err()
                .code(),
            "update_location_unsupported"
        );
        // Qualified, it is still refused for its missing signature, never
        // taken on trust.
        assert_eq!(
            MacUpdatePlatform::new().inspect(&root).unwrap_err().code(),
            "update_signature_invalid"
        );

        // Translocated and read-only locations stay refused outside
        // temporary folders too.
        let translocated = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("AppTranslocation/0000/d/BroScribe.app");
        synthetic_bundle(&translocated);
        assert_eq!(
            check_location_in(&translocated, &[]).unwrap_err().code(),
            "update_location_unsupported"
        );
        // The sealed system volume is mounted read-only.
        assert_eq!(
            check_location_in(Path::new("/System/Library/CoreServices/Finder.app"), &[])
                .unwrap_err()
                .code(),
            "update_location_unsupported"
        );
    }

    #[test]
    fn temporary_folders_resolve_the_system_aliases() {
        let temporary = temporary_folders();
        for path in [
            "/private/tmp/x/BroScribe.app",
            "/private/var/tmp/x/BroScribe.app",
        ] {
            assert!(is_temporary(Path::new(path), &temporary), "{path}");
            // Refused by path alone: nothing at it is read.
            assert_eq!(
                check_location_in(Path::new(path), &temporary)
                    .unwrap_err()
                    .code(),
                "update_location_unsupported"
            );
        }
        for path in [
            "/Applications/BroScribe.app",
            "/private/tmpfiles/BroScribe.app",
        ] {
            assert!(!is_temporary(Path::new(path), &temporary), "{path}");
        }
    }

    #[test]
    fn bundles_are_replaced_in_place() {
        assert_eq!(MacUpdatePlatform::new().replacement(), Replacement::InPlace);
        assert_eq!(
            MacUpdatePlatform::for_executable(PathBuf::from("/Applications/BroScribe.app"))
                .replacement(),
            Replacement::InPlace
        );
    }

    #[test]
    fn mach_o_architectures_are_read_from_thin_and_universal_headers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        let thin = |cpu: u32| {
            let mut head = 0xfeed_facf_u32.to_le_bytes().to_vec();
            head.extend_from_slice(&cpu.to_le_bytes());
            head
        };
        std::fs::write(&path, thin(CPU_TYPE_ARM64)).unwrap();
        assert!(runs_on(&path, CPU_TYPE_ARM64).unwrap());
        assert!(!runs_on(&path, CPU_TYPE_X86_64).unwrap());
        let mut fat = 0xcafe_babe_u32.to_be_bytes().to_vec();
        fat.extend_from_slice(&2u32.to_be_bytes());
        for cpu in [CPU_TYPE_X86_64, CPU_TYPE_ARM64] {
            let mut arch = cpu.to_be_bytes().to_vec();
            arch.resize(20, 0);
            fat.extend_from_slice(&arch);
        }
        std::fs::write(&path, fat).unwrap();
        assert!(runs_on(&path, CPU_TYPE_ARM64).unwrap());
        std::fs::write(&path, b"MZ").unwrap();
        assert!(runs_on(&path, CPU_TYPE_ARM64).is_err());
    }

    #[test]
    fn a_sealed_bundle_reports_an_added_library_without_changing_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("BroScribe.app");
        let contents = root.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir_all(contents.join("Resources")).unwrap();
        std::fs::write(contents.join("Resources/fixture.txt"), "sealed resource").unwrap();
        std::fs::copy("/usr/bin/true", contents.join("MacOS/fixture")).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>org.broscribe.BroScribe</string>
<key>CFBundleExecutable</key><string>fixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#,
        )
        .unwrap();
        let signature = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&root)
            .output()
            .unwrap();
        assert!(
            signature.status.success(),
            "{}",
            String::from_utf8_lossy(&signature.stderr)
        );
        validate_bundle_tree(&root, None).unwrap();

        let library = contents.join("Resources/meeting-library");
        std::fs::create_dir_all(&library).unwrap();
        let note = library.join("keep.md");
        std::fs::write(&note, "synthetic meeting").unwrap();
        let error = validate_bundle_tree(&root, None).unwrap_err().to_string();
        assert!(
            error.contains("meeting-library") && error.contains("Move any personal files out"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&note).unwrap(), "synthetic meeting");
    }

    #[test]
    fn requirement_literals_refuse_quotes() {
        assert_eq!(literal("ABCDE12345").unwrap(), "\"ABCDE12345\"");
        assert!(literal("a\" or true").is_err());
        assert!(literal("").is_err());
    }
}
