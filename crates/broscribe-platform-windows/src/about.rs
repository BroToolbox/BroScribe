//! Windows About facts: the installation folder holding the build
//! information (the executable's folder or its parent), or the executable
//! itself when unbundled, and the running executable's Authenticode
//! signature. WinVerifyTrust runs without UI, without revocation checks and
//! with cached URL retrieval only, so About never reaches the network;
//! Windows has no notarization.
//!
//! The classification of WinVerifyTrust results and the build information
//! lookup are pure and unit-tested on every OS; the native reader is
//! Windows-only.

use std::path::Path;

use broscribe_platform::about::{BuildInfoState, build_info_state, install_folder};

/// What a WinVerifyTrust result says about a signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// A valid Authenticode signature chaining to a trusted root.
    Verified,
    /// No signature.
    Unsigned,
    /// A signature that does not verify (bad digest, untrusted or expired
    /// chain, revoked or disallowed certificate).
    Invalid,
    /// The check itself could not run on this file.
    Unknown,
}

const TRUST_E_PROVIDER_UNKNOWN: u32 = 0x800B_0001;
const TRUST_E_ACTION_UNKNOWN: u32 = 0x800B_0002;
const TRUST_E_SUBJECT_FORM_UNKNOWN: u32 = 0x800B_0003;
const TRUST_E_NOSIGNATURE: u32 = 0x800B_0100;
/// Security-facility results that still mean a present, broken signature.
const TRUST_E_CERT_SIGNATURE: u32 = 0x8009_6004;
const TRUST_E_BAD_DIGEST: u32 = 0x8009_6010;
const FACILITY_CERT: u32 = 0x0B;

fn verdict(status: i32) -> Verdict {
    let code = status as u32;
    match code {
        0 => Verdict::Verified,
        TRUST_E_NOSIGNATURE => Verdict::Unsigned,
        TRUST_E_PROVIDER_UNKNOWN | TRUST_E_ACTION_UNKNOWN | TRUST_E_SUBJECT_FORM_UNKNOWN => {
            Verdict::Unknown
        }
        TRUST_E_CERT_SIGNATURE | TRUST_E_BAD_DIGEST => Verdict::Invalid,
        _ if code & 0x8000_0000 != 0 && (code >> 16) & 0x1fff == FACILITY_CERT => Verdict::Invalid,
        _ => Verdict::Unknown,
    }
}

/// Whether a process built for `arch` runs emulated on a machine whose
/// native PE machine is `native`. `None` for architectures not built.
fn translated(arch: &str, native: u16) -> Option<bool> {
    let own = match arch {
        "x86_64" => 0x8664,
        "aarch64" => 0xaa64,
        _ => return None,
    };
    Some(native != own)
}

/// The build information of `executable`'s installation folder, without
/// anything else the native reader establishes (no Authenticode check, no
/// volume or emulation queries).
fn build_info(executable: &Path) -> BuildInfoState {
    build_info_state(install_folder(executable).as_deref())
}

#[cfg(target_os = "windows")]
pub use native::WinAboutReader;

#[cfg(target_os = "windows")]
mod native {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use broscribe_platform::Result;
    use broscribe_platform::about::{
        AboutFacts, BuildInfoState, InspectedInstallation, LocationNote, Packaging,
        PlatformAboutReader, SealState, SignatureFacts, SignatureKind, canonical_temp_dir,
        collect_about, install_folder, is_temporary, running_executable,
    };
    use windows::Win32::Foundation::{HANDLE, HWND, INVALID_HANDLE_VALUE};
    use windows::Win32::Security::Cryptography::{
        CERT_SHA256_HASH_PROP_ID, CertGetCertificateContextProperty,
    };
    use windows::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
        WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
        WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTHelperGetProvSignerFromChain,
        WTHelperProvDataFromStateData, WinVerifyTrust,
    };
    use windows::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetVolumeInformationW, GetVolumePathNameW,
    };
    use windows::Win32::System::SystemInformation::IMAGE_FILE_MACHINE;
    use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
    use windows::core::PCWSTR;

    use super::{Verdict, build_info, translated, verdict};

    /// `GetDriveTypeW`: a remote (network) drive.
    const DRIVE_REMOTE: u32 = 4;
    /// `GetVolumeInformationW` file-system flag of a read-only volume.
    const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// The WinVerifyTrust result for `path` and, when it verified, the
    /// SHA-256 fingerprint of the leaf signing certificate.
    fn authenticode(path: &Path) -> (i32, Option<String>) {
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
            dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
            ..Default::default()
        };
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let no_ui = HWND(INVALID_HANDLE_VALUE.0);
        // SAFETY: `data` and `file` outlive both calls; `path_w` is NUL-terminated.
        let status =
            unsafe { WinVerifyTrust(no_ui, &mut action, (&mut data as *mut WINTRUST_DATA).cast()) };
        // SAFETY: the state data is valid until the close call below.
        let fingerprint = (status == 0)
            .then(|| unsafe { leaf_fingerprint(data.hWVTStateData) })
            .flatten();
        data.dwStateAction = WTD_STATEACTION_CLOSE;
        // SAFETY: releases the state opened by the verify call.
        unsafe {
            WinVerifyTrust(no_ui, &mut action, (&mut data as *mut WINTRUST_DATA).cast());
        }
        (status, fingerprint)
    }

    /// # Safety
    /// `state` must be the live state data of a successful WinVerifyTrust call.
    unsafe fn leaf_fingerprint(state: HANDLE) -> Option<String> {
        // SAFETY: per this function's contract.
        let provider = unsafe { WTHelperProvDataFromStateData(state) };
        if provider.is_null() {
            return None;
        }
        // SAFETY: `provider` is live; signer 0 is the primary signer.
        let signer = unsafe { WTHelperGetProvSignerFromChain(provider, 0, false, 0) };
        // SAFETY: a non-null signer points into the provider data.
        let signer = unsafe { signer.as_ref() }?;
        if signer.csCertChain == 0 || signer.pasCertChain.is_null() {
            return None;
        }
        // SAFETY: element 0 of a non-empty chain is the leaf.
        let cert = unsafe { (*signer.pasCertChain).pCert };
        if cert.is_null() {
            return None;
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
        .ok()?;
        (length == 32).then(|| digest.iter().map(|b| format!("{b:02x}")).collect())
    }

    fn signature(executable: &Path) -> SignatureFacts {
        let (status, fingerprint) = authenticode(executable);
        let code = status as u32;
        let (kind, seal) = match verdict(status) {
            Verdict::Verified => (
                SignatureKind::Authenticode { fingerprint },
                SealState::Intact,
            ),
            Verdict::Unsigned => (SignatureKind::Unsigned, SealState::NotChecked),
            Verdict::Invalid => (
                SignatureKind::Authenticode { fingerprint: None },
                SealState::Broken {
                    status: code.into(),
                },
            ),
            Verdict::Unknown => (
                SignatureKind::Unknown {
                    reason: format!("Windows could not check the signature (0x{code:08x})"),
                },
                SealState::NotChecked,
            ),
        };
        SignatureFacts::without_notarization(kind, seal)
    }

    fn native_translated() -> Option<bool> {
        let mut process = IMAGE_FILE_MACHINE::default();
        let mut native = IMAGE_FILE_MACHINE::default();
        // SAFETY: the pseudo handle of this process and two live outputs.
        unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, Some(&mut native)) }.ok()?;
        translated(std::env::consts::ARCH, native.0)
    }

    fn location_notes(root: &Path) -> Vec<LocationNote> {
        let mut notes = Vec::new();
        let text = root.to_string_lossy();
        let share = text.starts_with(r"\\?\UNC\")
            || (text.starts_with(r"\\") && !text.starts_with(r"\\?\"));
        let root_w = wide(root);
        let mut volume = vec![0u16; 1024];
        // SAFETY: `root_w` is NUL-terminated; `volume` is a writable buffer.
        if unsafe { GetVolumePathNameW(PCWSTR(root_w.as_ptr()), &mut volume) }.is_ok() {
            let volume = PCWSTR(volume.as_ptr());
            // SAFETY: `volume` is the NUL-terminated volume path just written.
            let remote = unsafe { GetDriveTypeW(volume) } == DRIVE_REMOTE;
            if share || remote {
                notes.push(LocationNote::NetworkVolume);
            }
            let mut flags = 0u32;
            // SAFETY: as above; `flags` is a live output.
            let read =
                unsafe { GetVolumeInformationW(volume, None, None, None, Some(&mut flags), None) };
            if read.is_ok() && flags & FILE_READ_ONLY_VOLUME != 0 {
                notes.push(LocationNote::ReadOnlyVolume);
            }
        } else if share {
            notes.push(LocationNote::NetworkVolume);
        }
        let temporary: Vec<PathBuf> = canonical_temp_dir().into_iter().collect();
        if is_temporary(root, &temporary) {
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
            signature: signature(executable),
            translated: native_translated(),
        }
    }

    /// About facts of the running build on Windows.
    #[derive(Clone, Debug)]
    pub struct WinAboutReader {
        app_version: String,
    }

    impl WinAboutReader {
        /// `app_version` is the calling executable's `CARGO_PKG_VERSION`.
        pub fn new(app_version: impl Into<String>) -> Self {
            Self {
                app_version: app_version.into(),
            }
        }
    }

    impl PlatformAboutReader for WinAboutReader {
        fn read(&self) -> Result<AboutFacts> {
            Ok(collect_about(
                &self.app_version,
                inspect(&running_executable()?),
            ))
        }

        fn read_build(&self) -> Result<BuildInfoState> {
            Ok(build_info(&running_executable()?))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn an_unsigned_synthetic_executable_is_unsigned_or_unknown_never_trusted() {
            let dir = tempfile::tempdir().unwrap();
            let exe = std::fs::canonicalize(dir.path())
                .unwrap()
                .join("synthetic.exe");
            std::fs::write(&exe, b"MZ").unwrap();
            let facts = signature(&exe);
            assert!(
                matches!(
                    facts.kind,
                    SignatureKind::Unsigned | SignatureKind::Unknown { .. }
                ),
                "{facts:?}"
            );
            assert_ne!(facts.seal, SealState::Intact);
            let inspected = inspect(&exe);
            assert_eq!(inspected.packaging, Packaging::Unbundled);
            assert!(inspected.location_notes.contains(&LocationNote::Temporary));
            assert_eq!(build_info(&exe), collect_about("1.2.3", inspected).build);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_results_separate_unsigned_invalid_and_unchecked() {
        assert_eq!(verdict(0), Verdict::Verified);
        assert_eq!(verdict(0x800B_0100_u32 as i32), Verdict::Unsigned);
        // CERT_E_UNTRUSTEDROOT, TRUST_E_BAD_DIGEST, CERT_E_EXPIRED,
        // TRUST_E_EXPLICIT_DISTRUST.
        for status in [0x800B_0109_u32, 0x8009_6010, 0x800B_0101, 0x800B_0111] {
            assert_eq!(verdict(status as i32), Verdict::Invalid, "0x{status:08x}");
        }
        // CRYPT_E_FILE_ERROR: the check could not read the file.
        assert_eq!(verdict(0x8009_2003_u32 as i32), Verdict::Unknown);
        assert_eq!(verdict(0x800B_0003_u32 as i32), Verdict::Unknown);
        assert_eq!(verdict(0x8007_0002_u32 as i32), Verdict::Unknown);
    }

    #[test]
    fn build_information_comes_from_the_installation_folder_only() {
        use broscribe_platform::update::BUILD_INFO_FILE;

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("BroScribe");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        for executable in [
            root.join("bin").join("broscribed.exe"),
            root.join("broscribed.exe"),
        ] {
            let file = root.join(BUILD_INFO_FILE);
            let _ = std::fs::remove_file(&file);
            // Unbundled: no installation folder, so no build information.
            assert_eq!(build_info(&executable), BuildInfoState::Missing);
            std::fs::write(&file, "{\"schema\": 2}").unwrap();
            let BuildInfoState::Invalid { reason } = build_info(&executable) else {
                panic!("expected invalid build information");
            };
            assert!(!reason.contains(&root.display().to_string()), "{reason}");
            std::fs::write(
                &file,
                r#"{"schema": 1, "product": "BroScribe", "version": "1.2.3", "build": 42, "target": "x86_64-pc-windows-msvc"}"#,
            )
            .unwrap();
            assert!(matches!(
                build_info(&executable),
                BuildInfoState::Present(ref info) if info.build == 42
            ));
        }
    }

    #[test]
    fn emulation_is_a_native_machine_other_than_the_build_s() {
        assert_eq!(translated("x86_64", 0xaa64), Some(true));
        assert_eq!(translated("x86_64", 0x8664), Some(false));
        assert_eq!(translated("aarch64", 0xaa64), Some(false));
        assert_eq!(translated("riscv64", 0xaa64), None);
    }
}
