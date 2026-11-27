//! macOS About facts: the outermost `.app` bundle holding the running
//! executable (or the executable itself when unbundled), its Info.plist,
//! and its code signature as Security.framework reports it offline.
//!
//! Classification uses only the signature: ad-hoc from the code-signing
//! flags, Developer ID from Apple's Developer ID requirement, Apple
//! Development from the Apple-anchored leaf certificate's type (its subject
//! summary is inspected, never reported). Every check passes
//! `kSecCSNoNetworkAccess` and performs no Gatekeeper assessment, so
//! notarization is never claimed here; see
//! [`SignatureFacts::apple`].

use std::ffi::{CStr, CString, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use broscribe_platform::about::{
    AboutFacts, BundleMetadata, InspectedInstallation, LocationNote, Packaging,
    PlatformAboutReader, SealState, SignatureFacts, SignatureKind, canonical_temp_dir,
    collect_about, is_temporary,
};
use broscribe_platform::{Error, Result};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL};
use objc2_foundation::{NSBundle, NSString};

type OsStatus = i32;
type SecCsFlags = u32;

const CHECK_ALL_ARCHITECTURES: SecCsFlags = 1 << 0;
const BASIC_VALIDATE_ONLY: SecCsFlags = (1 << 1) | (1 << 2);
const CHECK_NESTED_CODE: SecCsFlags = 1 << 3;
const STRICT_VALIDATE: SecCsFlags = 1 << 4;
const NO_NETWORK_ACCESS: SecCsFlags = 1 << 29;
/// Full offline validation of the signature against the files.
const SEAL_CHECK: SecCsFlags =
    CHECK_ALL_ARCHITECTURES | CHECK_NESTED_CODE | STRICT_VALIDATE | NO_NETWORK_ACCESS;
/// Requirement evaluation without rehashing the files.
const REQUIREMENT_CHECK: SecCsFlags = BASIC_VALIDATE_ONLY | NO_NETWORK_ACCESS;
/// `kSecCSSigningInformation` for `SecCodeCopySigningInformation`.
const SIGNING_INFORMATION: SecCsFlags = 1 << 1;
/// `kSecCodeSignatureAdhoc` in `kSecCodeInfoFlags`.
const SIGNATURE_ADHOC: u32 = 0x0002;
const ERR_SEC_CS_UNSIGNED: OsStatus = -67062;
const MNT_RDONLY: u32 = 0x0000_0001;
const NETWORK_FS: [&str; 5] = ["nfs", "smbfs", "afpfs", "webdav", "ftp"];
const APPLE_ANCHOR: &str = "anchor apple generic";
/// Apple's Developer ID requirement: the Developer ID intermediate and the
/// Developer ID Application leaf extension.
const DEVELOPER_ID: &str = "anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";
/// Leaf subject prefixes of development certificates.
const DEVELOPMENT_PREFIXES: [&str; 2] = ["Apple Development:", "Mac Developer:"];

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecStaticCodeCreateWithPath(
        path: NonNull<CFURL>,
        flags: SecCsFlags,
        code: *mut *const c_void,
    ) -> OsStatus;
    fn SecStaticCodeCheckValidity(
        code: *const c_void,
        flags: SecCsFlags,
        requirement: *const c_void,
    ) -> OsStatus;
    fn SecRequirementCreateWithString(
        text: NonNull<CFString>,
        flags: SecCsFlags,
        requirement: *mut *const c_void,
    ) -> OsStatus;
    fn SecCodeCopySigningInformation(
        code: *const c_void,
        flags: SecCsFlags,
        information: *mut *const CFDictionary,
    ) -> OsStatus;
    fn SecCertificateCopySubjectSummary(certificate: *const c_void) -> *const CFString;
    static kSecCodeInfoIdentifier: *const CFString;
    static kSecCodeInfoTeamIdentifier: *const CFString;
    static kSecCodeInfoFlags: *const CFString;
    static kSecCodeInfoCertificates: *const CFString;
}

/// Takes ownership of a +1 Core Foundation reference.
fn owned(pointer: *const c_void) -> Option<CFRetained<CFType>> {
    // SAFETY: callers pass references returned under the create/copy rule.
    NonNull::new(pointer.cast_mut().cast::<CFType>()).map(|p| unsafe { CFRetained::from_raw(p) })
}

fn raw(object: &CFRetained<CFType>) -> *const c_void {
    CFRetained::as_ptr(object).as_ptr().cast_const().cast()
}

/// What the signing information says, before any validation.
#[derive(Debug, Default, PartialEq, Eq)]
struct SigningSummary {
    identifier: Option<String>,
    team: Option<String>,
    flags: Option<u32>,
    /// Subject summary of the leaf certificate: classification only.
    leaf: Option<String>,
}

/// Results of the offline requirement checks the classification needs.
#[derive(Clone, Copy, Debug, Default)]
struct Checks {
    apple_anchor: bool,
    developer_id: bool,
}

/// Code without a signing identifier is unsigned, whatever else it carries.
fn classify(summary: &SigningSummary, checks: Checks) -> SignatureKind {
    if summary.identifier.is_none() {
        return SignatureKind::Unsigned;
    }
    if summary
        .flags
        .is_some_and(|flags| flags & SIGNATURE_ADHOC != 0)
    {
        return SignatureKind::AdHoc;
    }
    let team = summary.team.clone();
    if checks.developer_id {
        return SignatureKind::DeveloperId { team };
    }
    if checks.apple_anchor {
        let development = summary.leaf.as_deref().is_some_and(|leaf| {
            DEVELOPMENT_PREFIXES
                .iter()
                .any(|prefix| leaf.starts_with(prefix))
        });
        return if development {
            SignatureKind::AppleDevelopment { team }
        } else {
            SignatureKind::AppleOther { team }
        };
    }
    SignatureKind::Unknown {
        reason: "the signing certificate does not chain to Apple".into(),
    }
}

fn static_code(path: &Path, bundle: bool) -> std::result::Result<CFRetained<CFType>, String> {
    let url = match bundle {
        true => CFURL::from_directory_path(path),
        false => CFURL::from_file_path(path),
    };
    let url = url.ok_or_else(|| "the path is not a valid URL".to_owned())?;
    let mut code = std::ptr::null();
    // SAFETY: `url` is a live CFURL; `code` receives a +1 reference.
    let status = unsafe { SecStaticCodeCreateWithPath(CFRetained::as_ptr(&url), 0, &mut code) };
    if status != 0 {
        return Err(format!("the code object is unreadable ({status})"));
    }
    owned(code).ok_or_else(|| "the code object is null".to_owned())
}

fn signing_summary(code: &CFRetained<CFType>) -> std::result::Result<SigningSummary, String> {
    let mut info: *const CFDictionary = std::ptr::null();
    // SAFETY: `code` is a live static code; `info` receives a +1 dictionary.
    let status =
        unsafe { SecCodeCopySigningInformation(raw(code), SIGNING_INFORMATION, &mut info) };
    if status != 0 {
        return Err(format!("the signing information is unreadable ({status})"));
    }
    let info = owned(info.cast()).ok_or_else(|| "the signing information is null".to_owned())?;
    let dictionary = info
        .downcast_ref::<CFDictionary>()
        .ok_or_else(|| "the signing information is not a dictionary".to_owned())?;
    // SAFETY: framework constants, initialised at load time.
    let (identifier, team, flags, certificates) = unsafe {
        (
            kSecCodeInfoIdentifier,
            kSecCodeInfoTeamIdentifier,
            kSecCodeInfoFlags,
            kSecCodeInfoCertificates,
        )
    };
    let value = |key| entry(dictionary, key);
    let text = |key| {
        value(key)
            .and_then(|v| v.downcast_ref::<CFString>())
            .map(|s| s.to_string())
    };
    let leaf = value(certificates)
        .and_then(|v| v.downcast_ref::<CFArray>())
        .filter(|array| array.count() > 0)
        // SAFETY: index 0 of a non-empty array; the certificate is owned by
        // the array (get rule) and the summary is returned +1.
        .and_then(|array| {
            owned(unsafe { SecCertificateCopySubjectSummary(array.value_at_index(0)) }.cast())
        })
        .and_then(|summary| summary.downcast_ref::<CFString>().map(|s| s.to_string()));
    Ok(SigningSummary {
        identifier: text(identifier),
        team: text(team),
        flags: value(flags)
            .and_then(|v| v.downcast_ref::<CFNumber>())
            .and_then(CFNumber::as_i64)
            .and_then(|flags| u32::try_from(flags).ok()),
        leaf,
    })
}

/// The value for framework key `key` in `dictionary`, borrowed from it.
fn entry(dictionary: &CFDictionary, key: *const CFString) -> Option<&CFType> {
    // SAFETY: `key` is a framework CFString constant; the value is owned by
    // the dictionary (get rule) and lives as long as the borrow.
    unsafe { dictionary.value(key.cast()).cast::<CFType>().as_ref() }
}

fn validity(code: &CFRetained<CFType>, flags: SecCsFlags, requirement: Option<&str>) -> OsStatus {
    let requirement = match requirement {
        Some(text) => {
            let text = CFString::from_str(text);
            let mut compiled = std::ptr::null();
            // SAFETY: `text` is a live CFString; `compiled` receives +1.
            let status = unsafe {
                SecRequirementCreateWithString(CFRetained::as_ptr(&text), 0, &mut compiled)
            };
            match (status, owned(compiled)) {
                (0, Some(compiled)) => Some(compiled),
                (status, _) => return if status == 0 { -1 } else { status },
            }
        }
        None => None,
    };
    let requirement = requirement.as_ref().map_or(std::ptr::null(), raw);
    // SAFETY: `code` is a live static code; `requirement` is null or live.
    unsafe { SecStaticCodeCheckValidity(raw(code), flags, requirement) }
}

/// The signature of the bundle at `path` (or the executable when unbundled).
fn signature(path: &Path, bundle: bool) -> SignatureFacts {
    let unknown = |reason: String| {
        SignatureFacts::apple(
            SignatureKind::Unknown { reason },
            None,
            SealState::NotChecked,
        )
    };
    let code = match static_code(path, bundle) {
        Ok(code) => code,
        Err(reason) => return unknown(reason),
    };
    let status = validity(&code, SEAL_CHECK, None);
    let summary = match signing_summary(&code) {
        Ok(summary) => summary,
        Err(_) if status == ERR_SEC_CS_UNSIGNED => SigningSummary::default(),
        Err(reason) => return unknown(reason),
    };
    let checks = Checks {
        apple_anchor: validity(&code, REQUIREMENT_CHECK, Some(APPLE_ANCHOR)) == 0,
        developer_id: validity(&code, REQUIREMENT_CHECK, Some(DEVELOPER_ID)) == 0,
    };
    let kind = classify(&summary, checks);
    let seal = match (&kind, status) {
        (SignatureKind::Unsigned, _) => SealState::NotChecked,
        (_, 0) => SealState::Intact,
        (_, status) => SealState::Broken {
            status: status.into(),
        },
    };
    let identifier = (kind != SignatureKind::Unsigned)
        .then_some(summary.identifier)
        .flatten();
    SignatureFacts::apple(kind, identifier, seal)
}

fn bundle_metadata(bundle: &Path) -> Option<BundleMetadata> {
    let path = NSString::from_str(bundle.to_str()?);
    let bundle = NSBundle::bundleWithPath(&path)?;
    let text = |key: &str| {
        bundle
            .objectForInfoDictionaryKey(&NSString::from_str(key))?
            .downcast::<NSString>()
            .ok()
            .map(|value| value.to_string())
    };
    Some(BundleMetadata {
        identifier: text("CFBundleIdentifier"),
        short_version: text("CFBundleShortVersionString"),
        bundle_version: text("CFBundleVersion"),
    })
}

/// The outermost `.app` bundle on the way to `executable`.
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

fn location_notes(root: &Path) -> Vec<LocationNote> {
    let mut notes = Vec::new();
    if root
        .components()
        .any(|part| part.as_os_str() == "AppTranslocation")
    {
        notes.push(LocationNote::Translocated);
    }
    if let Ok(path) = CString::new(root.as_os_str().as_bytes()) {
        // SAFETY: zeroed is a valid statfs; the path is NUL-terminated.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut fs) } == 0 {
            if fs.f_flags & MNT_RDONLY != 0 {
                notes.push(LocationNote::ReadOnlyVolume);
            }
            // SAFETY: f_fstypename is NUL-terminated by the kernel.
            let kind = unsafe { CStr::from_ptr(fs.f_fstypename.as_ptr()) }.to_string_lossy();
            if NETWORK_FS.contains(&kind.as_ref()) {
                notes.push(LocationNote::NetworkVolume);
            }
        }
    }
    if is_temporary(root, &temporary_folders()) {
        notes.push(LocationNote::Temporary);
    }
    notes
}

/// The temporary folders macOS or the user may empty at any time: the
/// canonical per-user temporary folder and the system ones (`/tmp` and
/// `/var/tmp`, canonical). Compare canonical paths with
/// [`is_temporary`]. The one rule About's location notes, the updater's
/// installation and the agent's registration share.
pub(crate) fn temporary_folders() -> Vec<PathBuf> {
    let private = Path::new("/private");
    canonical_temp_dir()
        .into_iter()
        .chain([private.join("tmp"), private.join("var").join("tmp")])
        .collect()
}

/// Whether Rosetta 2 translates this process (`sysctl.proc_translated`).
/// The key does not exist on Intel Macs, which never translate.
fn translated() -> Option<bool> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: a NUL-terminated name and a buffer of `size` bytes.
    let status = unsafe {
        libc::sysctlbyname(
            c"sysctl.proc_translated".as_ptr(),
            (&mut value as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status == 0 {
        Some(value == 1)
    } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
        Some(false)
    } else {
        None
    }
}

fn inspect(executable: &Path) -> InspectedInstallation {
    match bundle_root(executable) {
        Some(root) => {
            let resources = root.join("Contents/Resources");
            InspectedInstallation {
                packaging: Packaging::AppBundle,
                resources: resources.is_dir().then_some(resources),
                location_notes: location_notes(&root),
                bundle: bundle_metadata(&root),
                signature: signature(&root, true),
                translated: translated(),
            }
        }
        None => InspectedInstallation {
            packaging: Packaging::Unbundled,
            resources: None,
            location_notes: executable.parent().map(location_notes).unwrap_or_default(),
            bundle: None,
            signature: signature(executable, false),
            translated: translated(),
        },
    }
}

/// About facts of the running build on macOS.
#[derive(Clone, Debug)]
pub struct MacAboutReader {
    app_version: String,
}

impl MacAboutReader {
    /// `app_version` is the calling executable's `CARGO_PKG_VERSION`.
    pub fn new(app_version: impl Into<String>) -> Self {
        Self {
            app_version: app_version.into(),
        }
    }
}

impl PlatformAboutReader for MacAboutReader {
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
    use broscribe_platform::about::{BuildInfoState, Notarization};
    use broscribe_platform::update::BUILD_INFO_FILE;

    fn summary(flags: u32, leaf: Option<&str>) -> SigningSummary {
        SigningSummary {
            identifier: Some("org.example.synthetic".into()),
            team: Some("ABCDE12345".into()),
            flags: Some(flags),
            leaf: leaf.map(str::to_owned),
        }
    }

    #[test]
    fn signatures_are_classified_from_flags_requirements_and_leaf_type() {
        let team = Some("ABCDE12345".to_owned());
        let apple = Checks {
            apple_anchor: true,
            ..Checks::default()
        };
        assert_eq!(
            classify(&SigningSummary::default(), Checks::default()),
            SignatureKind::Unsigned
        );
        assert_eq!(
            classify(
                &summary(SIGNATURE_ADHOC | 0x2_0000, None),
                Checks::default()
            ),
            SignatureKind::AdHoc
        );
        assert_eq!(
            classify(
                &summary(0, Some("Developer ID Application: Example (ABCDE12345)")),
                Checks {
                    apple_anchor: true,
                    developer_id: true,
                }
            ),
            SignatureKind::DeveloperId { team: team.clone() }
        );
        for leaf in [
            "Apple Development: Someone (XYZ)",
            "Mac Developer: Someone (XYZ)",
        ] {
            assert_eq!(
                classify(&summary(0, Some(leaf)), apple),
                SignatureKind::AppleDevelopment { team: team.clone() }
            );
        }
        assert_eq!(
            classify(
                &summary(0, Some("Apple Distribution: Example (ABCDE12345)")),
                apple
            ),
            SignatureKind::AppleOther { team: team.clone() }
        );
        // A self-made certificate named like a development one is not Apple's.
        assert!(matches!(
            classify(
                &summary(0, Some("Apple Development: Fake")),
                Checks::default()
            ),
            SignatureKind::Unknown { .. }
        ));
    }

    #[test]
    fn an_unsigned_synthetic_bundle_reports_itself_without_claiming_trust() {
        let dir = tempfile::tempdir().unwrap();
        let app = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("Synthetic.app");
        let contents = app.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir_all(contents.join("Resources")).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>org.example.synthetic</string>
<key>CFBundleExecutable</key><string>synthetic</string>
<key>CFBundleShortVersionString</key><string>1.2.3</string>
<key>CFBundleVersion</key><string>42</string>
</dict></plist>"#,
        )
        .unwrap();
        std::fs::write(contents.join("MacOS/synthetic"), b"#!/bin/sh\n").unwrap();
        std::fs::write(
            contents.join("Resources").join(BUILD_INFO_FILE),
            r#"{"schema": 1, "product": "BroScribe", "version": "1.2.3", "build": 42, "target": "aarch64-apple-darwin"}"#,
        )
        .unwrap();
        let inspected = inspect(&contents.join("MacOS/synthetic"));
        assert_eq!(inspected.packaging, Packaging::AppBundle);
        assert!(inspected.location_notes.contains(&LocationNote::Temporary));
        assert_eq!(
            inspected.bundle,
            Some(BundleMetadata {
                identifier: Some("org.example.synthetic".into()),
                short_version: Some("1.2.3".into()),
                bundle_version: Some("42".into()),
            })
        );
        assert_eq!(inspected.signature.kind, SignatureKind::Unsigned);
        assert_eq!(inspected.signature.seal, SealState::NotChecked);
        assert_eq!(inspected.signature.identifier, None);
        assert_eq!(inspected.signature.notarization, Notarization::NotEligible);
        let facts = collect_about("1.2.3", inspected);
        assert!(matches!(facts.build, BuildInfoState::Present(_)));
    }

    #[test]
    fn an_unbundled_executable_has_no_resources() {
        let dir = tempfile::tempdir().unwrap();
        let tool = std::fs::canonicalize(dir.path()).unwrap().join("broscribe");
        std::fs::write(&tool, b"#!/bin/sh\n").unwrap();
        let inspected = inspect(&tool);
        assert_eq!(inspected.packaging, Packaging::Unbundled);
        assert_eq!(inspected.resources, None);
        assert_eq!(inspected.bundle, None);
    }
}
