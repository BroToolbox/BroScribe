//! What About BroScribe states about the running build (SH24): version,
//! build number, architecture, target, packaging, code signature and the
//! third-party licences the build ships.
//!
//! [`PlatformAboutReader`] inspects whatever is running, including a source
//! build or a bundle in a temporary, translocated or read-only location: it
//! never refuses a location the way [`crate::update::UpdatePlatform`] does,
//! and it never turns a missing fact into a default. Missing build
//! information is [`BuildInfoState::Missing`], a signature the platform
//! cannot classify is [`SignatureKind::Unknown`], and notarization is
//! [`Notarization::Unknown`] unless the signature itself settles it.
//!
//! Native readers live in the platform crates and only inspect: no network,
//! no Gatekeeper assessment, no security or permission changes. The facts
//! carry no paths (only [`AboutDocument::path`], kept out of serialization,
//! for opening a licence document the installation itself ships).

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::update::{BUILD_INFO_FILE, read_build_info, target_matches_host};
use crate::{Error, Result};

/// Licence inventory a build carries beside its build information
/// (`scripts/third-party-notices.py`).
pub const INVENTORY_FILE: &str = "third-party-inventory.json";
/// Third-party notices a build carries beside its build information.
pub const NOTICES_FILE: &str = "THIRD_PARTY_NOTICES.txt";
/// BroScribe's own licence and notice, copied beside the build information.
pub const LICENCE_FILE: &str = "LICENSE";
pub const NOTICE_FILE: &str = "NOTICE";
/// The inventory lists a few hundred components; anything far larger is not one.
const INVENTORY_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Backend lookup and permission policy, not evidence that a device works or
/// permission is granted. Native factories select this; consumers still probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagnosticPolicy {
    /// A missing PATH tool is inconclusive when a sibling executable may exist.
    pub external_tools_beside_executable: bool,
    pub native_playback: bool,
    /// Another process cannot infer the recording app's own reader grant.
    pub reader_permission_is_app_specific: bool,
}

/// Reads the facts About shows about the running build.
pub trait PlatformAboutReader: Send + Sync {
    /// Blocking (the macOS reader validates the code signature): call it off
    /// the UI loop. Fails only when the running executable cannot be
    /// located; everything else degrades to an explicit unknown state.
    fn read(&self) -> Result<AboutFacts>;

    /// Only the running installation's build information, the same
    /// [`AboutFacts::build`] that [`read`](Self::read) states: no signature
    /// check, bundle metadata, location or licence inventory. Cheap enough
    /// for a process start that must answer clients at once. Fails only
    /// when the running executable cannot be located.
    fn read_build(&self) -> Result<BuildInfoState>;
}

/// Everything About states about the running build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AboutFacts {
    /// The version the calling executable was compiled as.
    pub app_version: String,
    pub architecture: Architecture,
    pub packaging: Packaging,
    /// Where the build runs from, when that is noteworthy.
    pub location_notes: Vec<LocationNote>,
    pub build: BuildInfoState,
    /// macOS: the bundle's Info.plist. `None` elsewhere and when unbundled.
    pub bundle: Option<BundleMetadata>,
    pub signature: SignatureFacts,
    pub licences: LicenceInventoryState,
    /// Licence documents the installation ships, in [`AboutDocumentKind`] order.
    pub documents: Vec<AboutDocument>,
}

/// The process's architecture as it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Architecture {
    /// `std::env::consts::ARCH` of the running executable.
    pub arch: String,
    /// `std::env::consts::OS`.
    pub os: String,
    /// Whether the OS translates this process for another CPU (Rosetta 2,
    /// Windows on Arm emulation). `None` where it cannot be observed.
    pub translated: Option<bool>,
}

impl Architecture {
    pub fn current(translated: Option<bool>) -> Self {
        Self {
            arch: std::env::consts::ARCH.to_owned(),
            os: std::env::consts::OS.to_owned(),
            translated,
        }
    }
}

/// How the running build is packaged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Packaging {
    /// A macOS `.app` bundle.
    AppBundle,
    /// An installation folder holding build information (Windows, Linux).
    InstallFolder,
    /// Neither: a development or source build run in place.
    Unbundled,
}

/// A noteworthy location of the running build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationNote {
    /// macOS App Translocation runs the app from a randomized read-only path.
    Translocated,
    /// A read-only volume, such as a mounted disk image.
    ReadOnlyVolume,
    /// A network volume or share.
    NetworkVolume,
    /// A temporary folder.
    Temporary,
}

/// The build information the build carries ([`BUILD_INFO_FILE`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BuildInfoState {
    Present(BuildInfo),
    /// No build information: an unbundled build, or a bundle without it.
    Missing,
    /// Present but unusable; `reason` names no path.
    Invalid {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BuildInfo {
    pub product: String,
    /// `MAJOR.MINOR.PATCH`.
    pub version: String,
    /// Monotonic build number.
    pub build: u64,
    /// Rust target triple the build was made for.
    pub target: String,
}

/// macOS bundle metadata from its Info.plist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BundleMetadata {
    /// `CFBundleIdentifier`.
    pub identifier: Option<String>,
    /// `CFBundleShortVersionString`.
    pub short_version: Option<String>,
    /// `CFBundleVersion` (the build number).
    pub bundle_version: Option<String>,
}

/// The code signature of the running build, as the OS reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SignatureFacts {
    pub kind: SignatureKind,
    /// macOS: the signing identifier. `None` elsewhere or when unsigned.
    pub identifier: Option<String>,
    pub seal: SealState,
    pub notarization: Notarization,
}

impl SignatureFacts {
    /// An Apple code signature. Apple notarizes only Developer ID software,
    /// so other kinds are [`Notarization::NotEligible`]; Developer ID and
    /// unrecognised kinds stay [`Notarization::Unknown`]: a code signature
    /// alone never shows that a build was notarized.
    pub fn apple(kind: SignatureKind, identifier: Option<String>, seal: SealState) -> Self {
        let notarization = match kind {
            SignatureKind::Unsigned
            | SignatureKind::AdHoc
            | SignatureKind::AppleDevelopment { .. } => Notarization::NotEligible,
            _ => Notarization::Unknown,
        };
        Self {
            kind,
            identifier,
            seal,
            notarization,
        }
    }

    /// A signature on a system without notarization (Windows, Linux).
    pub fn without_notarization(kind: SignatureKind, seal: SealState) -> Self {
        Self {
            kind,
            identifier: None,
            seal,
            notarization: Notarization::NotApplicable,
        }
    }
}

/// What signed the running build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignatureKind {
    /// No code signature.
    Unsigned,
    /// Signed without a certificate (a local or linker-signed build).
    AdHoc,
    /// An Apple Development (or Mac Developer) certificate: development
    /// only, never distribution trust.
    AppleDevelopment { team: Option<String> },
    /// A Developer ID Application certificate.
    DeveloperId { team: Option<String> },
    /// An Apple-issued certificate of another type (Apple Distribution, Mac
    /// App Store).
    AppleOther { team: Option<String> },
    /// Windows Authenticode; `fingerprint` is the leaf certificate's
    /// lowercase hex SHA-256 when the signature verified.
    Authenticode { fingerprint: Option<String> },
    /// No OS code-signing facility: only a signed release manifest vouches
    /// for releases.
    ManifestOnly,
    /// The OS could not classify the signature; `reason` names no path.
    Unknown { reason: String },
}

/// Whether the signature still seals the build, checked offline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SealState {
    /// The OS validated the signature against the files.
    Intact,
    /// The OS rejected it; `status` is the OS result code.
    Broken { status: i64 },
    /// Not checked: unsigned, or no OS signing facility.
    NotChecked,
}

/// Whether Apple notarized the build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Notarization {
    /// The system has no notarization.
    NotApplicable,
    /// Only Developer ID builds are notarized; this one is not one.
    NotEligible,
    /// Not established offline from the signature.
    Unknown,
}

/// The licence inventory the build ships ([`INVENTORY_FILE`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LicenceInventoryState {
    Present(LicenceInventory),
    Missing,
    /// Present but unusable; `reason` names no path.
    Invalid {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LicenceInventory {
    /// Rust target the inventory was made for.
    pub target: String,
    /// Every component's licence text was resolved.
    pub complete: bool,
    pub unresolved: usize,
    /// Rust packages, native libraries and embedded files (fonts)
    /// distributed in the build.
    pub components: usize,
    /// Components per licence expression, most common first.
    pub by_licence: Vec<(String, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AboutDocumentKind {
    /// BroScribe's licence ([`LICENCE_FILE`]).
    Licence,
    /// BroScribe's notice ([`NOTICE_FILE`]).
    Notice,
    /// The third-party notices ([`NOTICES_FILE`]).
    ThirdPartyNotices,
}

impl AboutDocumentKind {
    pub const ALL: [Self; 3] = [Self::Licence, Self::Notice, Self::ThirdPartyNotices];

    pub fn file_name(self) -> &'static str {
        match self {
            Self::Licence => LICENCE_FILE,
            Self::Notice => NOTICE_FILE,
            Self::ThirdPartyNotices => NOTICES_FILE,
        }
    }
}

/// A licence document the installation ships: a regular file (not a link)
/// beside its build information.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AboutDocument {
    pub kind: AboutDocumentKind,
    #[serde(skip)]
    path: PathBuf,
}

impl AboutDocument {
    /// Where to open the document from; never serialized.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A disagreement between facts that a correct build keeps consistent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mismatch {
    /// The executable's version differs from the build information.
    AppVersion { running: String, build_info: String },
    /// Info.plist `CFBundleShortVersionString` differs from the build information.
    BundleVersion { bundle: String, build_info: String },
    /// Info.plist `CFBundleVersion` differs from the build number.
    BundleBuild { bundle: String, build_info: u64 },
    /// The build information names a target for another machine.
    Target { build_info: String },
    /// The licence inventory was made for another target.
    InventoryTarget {
        inventory: String,
        build_info: String,
    },
}

impl AboutFacts {
    /// Every disagreement between the facts; empty for a consistent build.
    pub fn mismatches(&self) -> Vec<Mismatch> {
        let mut found = Vec::new();
        let BuildInfoState::Present(info) = &self.build else {
            return found;
        };
        if info.version != self.app_version {
            found.push(Mismatch::AppVersion {
                running: self.app_version.clone(),
                build_info: info.version.clone(),
            });
        }
        if let Some(bundle) = &self.bundle {
            if let Some(short) = bundle
                .short_version
                .as_ref()
                .filter(|v| **v != info.version)
            {
                found.push(Mismatch::BundleVersion {
                    bundle: short.clone(),
                    build_info: info.version.clone(),
                });
            }
            if let Some(build) = bundle
                .bundle_version
                .as_ref()
                .filter(|v| **v != info.build.to_string())
            {
                found.push(Mismatch::BundleBuild {
                    bundle: build.clone(),
                    build_info: info.build,
                });
            }
        }
        if !target_matches_host(&info.target) {
            found.push(Mismatch::Target {
                build_info: info.target.clone(),
            });
        }
        if let LicenceInventoryState::Present(inventory) = &self.licences
            && inventory.target != info.target
        {
            found.push(Mismatch::InventoryTarget {
                inventory: inventory.target.clone(),
                build_info: info.target.clone(),
            });
        }
        found
    }
}

/// What a native reader established about the running installation;
/// [`collect_about`] adds the facts read from its resources.
#[derive(Clone, Debug)]
pub struct InspectedInstallation {
    pub packaging: Packaging,
    /// The folder holding the build information and licence documents
    /// (`Contents/Resources` of a bundle, an installation folder); `None`
    /// when unbundled.
    pub resources: Option<PathBuf>,
    pub location_notes: Vec<LocationNote>,
    pub bundle: Option<BundleMetadata>,
    pub signature: SignatureFacts,
    pub translated: Option<bool>,
}

/// The facts of `inspected`, reading build information, the licence
/// inventory and licence documents from its resources only.
pub fn collect_about(app_version: &str, inspected: InspectedInstallation) -> AboutFacts {
    let resources = inspected.resources.as_deref();
    let build = build_info_state(resources);
    let (licences, documents) = match resources {
        Some(dir) => (
            read_inventory(&dir.join(INVENTORY_FILE)),
            AboutDocumentKind::ALL
                .into_iter()
                .filter_map(|kind| {
                    let path = dir.join(kind.file_name());
                    is_regular_file(&path).then_some(AboutDocument { kind, path })
                })
                .collect(),
        ),
        None => (LicenceInventoryState::Missing, Vec::new()),
    };
    AboutFacts {
        app_version: app_version.to_owned(),
        architecture: Architecture::current(inspected.translated),
        packaging: inspected.packaging,
        location_notes: inspected.location_notes,
        build,
        bundle: inspected.bundle,
        signature: inspected.signature,
        licences,
        documents,
    }
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

/// Replaces `path` in `message` by its file name, so reasons name no path.
fn without_path(message: String, path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    message.replace(&path.display().to_string(), &name)
}

/// The build information in `resources` (see
/// [`InspectedInstallation::resources`]): [`BuildInfoState::Missing`] when
/// unbundled or absent, [`BuildInfoState::Invalid`] (naming no path) when
/// unusable. The one parser behind [`collect_about`] and every
/// [`PlatformAboutReader::read_build`].
pub fn build_info_state(resources: Option<&Path>) -> BuildInfoState {
    let Some(dir) = resources else {
        return BuildInfoState::Missing;
    };
    let file = dir.join(BUILD_INFO_FILE);
    match std::fs::symlink_metadata(&file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BuildInfoState::Missing,
        _ => {}
    }
    match read_build_info(&file, String::new()) {
        Ok(identity) => BuildInfoState::Present(BuildInfo {
            product: identity.product,
            version: identity.version,
            build: identity.build,
            target: identity.target,
        }),
        Err(e) => BuildInfoState::Invalid {
            reason: without_path(e.to_string(), &file),
        },
    }
}

#[derive(Deserialize)]
struct InventoryFile {
    schema: u32,
    target: String,
    complete: bool,
    #[serde(default)]
    unresolved: Vec<String>,
    #[serde(default)]
    packages: Vec<InventoryEntry>,
    #[serde(default)]
    native: Vec<InventoryEntry>,
    /// Third-party files compiled into the app (the bundled fonts).
    #[serde(default)]
    embedded: Vec<InventoryEntry>,
}

#[derive(Deserialize)]
struct InventoryEntry {
    license: Option<String>,
}

/// Reads a build's licence inventory at `path` (a regular file, not a link).
pub fn read_inventory(path: &Path) -> LicenceInventoryState {
    let invalid = |reason: String| LicenceInventoryState::Invalid { reason };
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return LicenceInventoryState::Missing;
        }
        Err(e) => return invalid(e.to_string()),
        Ok(meta) if !meta.file_type().is_file() => {
            return invalid("the inventory is not a regular file".into());
        }
        Ok(_) => {}
    }
    let mut text = Vec::new();
    if let Err(e) = std::fs::File::open(path).and_then(|file| {
        file.take(INVENTORY_MAX_BYTES + 1)
            .read_to_end(&mut text)
            .map(drop)
    }) {
        return invalid(e.to_string());
    }
    if text.len() as u64 > INVENTORY_MAX_BYTES {
        return invalid(format!(
            "the inventory is larger than {INVENTORY_MAX_BYTES} bytes"
        ));
    }
    let file: InventoryFile = match serde_json::from_slice(&text) {
        Ok(file) => file,
        Err(e) => return invalid(e.to_string()),
    };
    if file.schema != 1 {
        return invalid(format!("unsupported inventory schema {}", file.schema));
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for entry in file
        .packages
        .iter()
        .chain(&file.native)
        .chain(&file.embedded)
    {
        let licence = entry
            .license
            .clone()
            .unwrap_or_else(|| "Unresolved".to_owned());
        *counts.entry(licence).or_default() += 1;
    }
    let mut by_licence: Vec<(String, usize)> = counts.into_iter().collect();
    by_licence.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    LicenceInventoryState::Present(LicenceInventory {
        target: file.target,
        complete: file.complete,
        unresolved: file.unresolved.len(),
        components: file.packages.len() + file.native.len() + file.embedded.len(),
        by_licence,
    })
}

/// The installation folder of `executable` (canonical): the folder holding
/// [`BUILD_INFO_FILE`] as a regular file, being the executable's own folder
/// or its parent (`<root>/bin/<exe>`). `None` for an unbundled build.
pub fn install_folder(executable: &Path) -> Option<PathBuf> {
    let folder = executable.parent()?;
    [Some(folder), folder.parent()]
        .into_iter()
        .flatten()
        .find(|candidate| is_regular_file(&candidate.join(BUILD_INFO_FILE)))
        .map(Path::to_path_buf)
}

/// Whether `path` lies in one of `temporary` folders. Compare canonical
/// paths: both sides as the file system resolves them.
pub fn is_temporary(path: &Path, temporary: &[PathBuf]) -> bool {
    temporary.iter().any(|folder| path.starts_with(folder))
}

/// The running executable, canonical: what every native reader inspects.
pub fn running_executable() -> Result<PathBuf> {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| Error::io("locating the running executable", e))
}

/// [`std::env::temp_dir`], canonical, when it resolves.
pub fn canonical_temp_dir() -> Option<PathBuf> {
    std::fs::canonicalize(std::env::temp_dir()).ok()
}

/// Deterministic [`PlatformAboutReader`]: returns the facts it was given,
/// or fails as if the executable could not be located.
pub struct FakeAboutReader {
    result: std::result::Result<AboutFacts, String>,
}

impl FakeAboutReader {
    pub fn new(facts: AboutFacts) -> Self {
        Self { result: Ok(facts) }
    }

    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            result: Err(message.into()),
        }
    }
}

impl PlatformAboutReader for FakeAboutReader {
    fn read(&self) -> Result<AboutFacts> {
        self.result.clone().map_err(|message| {
            Error::io(
                "locating the running executable",
                std::io::Error::other(message),
            )
        })
    }

    fn read_build(&self) -> Result<BuildInfoState> {
        self.read().map(|facts| facts.build)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_target() -> String {
        let os = match std::env::consts::OS {
            "macos" => "apple-darwin",
            "linux" => "unknown-linux-gnu",
            _ => "pc-windows-msvc",
        };
        format!("{}-{os}", std::env::consts::ARCH)
    }

    fn build_info(version: &str, build: u64, target: &str) -> String {
        serde_json::json!({
            "schema": 1,
            "product": "BroScribe",
            "version": version,
            "build": build,
            "target": target,
            "git_commit": "0000000000000000000000000000000000000000",
        })
        .to_string()
    }

    fn inventory(target: &str) -> String {
        serde_json::json!({
            "schema": 1,
            "target": target,
            "complete": false,
            "unresolved": ["synthetic-c"],
            "packages": [
                {"name": "synthetic-a", "license": "MIT"},
                {"name": "synthetic-b", "license": "Apache-2.0 OR MIT"},
                {"name": "synthetic-d", "license": "MIT"},
            ],
            "native": [{"name": "synthetic-c"}],
            "embedded": [{"file": "fonts/Synthetic-Regular.ttf", "license": "OFL-1.1"}],
        })
        .to_string()
    }

    fn inspected(resources: Option<PathBuf>) -> InspectedInstallation {
        InspectedInstallation {
            packaging: if resources.is_some() {
                Packaging::InstallFolder
            } else {
                Packaging::Unbundled
            },
            resources,
            location_notes: vec![LocationNote::Temporary],
            bundle: None,
            signature: SignatureFacts::without_notarization(
                SignatureKind::ManifestOnly,
                SealState::NotChecked,
            ),
            translated: None,
        }
    }

    #[test]
    fn an_installation_reports_its_build_inventory_and_owned_documents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = host_target();
        std::fs::write(root.join(BUILD_INFO_FILE), build_info("1.2.3", 42, &target)).unwrap();
        std::fs::write(root.join(INVENTORY_FILE), inventory(&target)).unwrap();
        std::fs::write(root.join(LICENCE_FILE), "synthetic licence").unwrap();
        std::fs::write(root.join(NOTICES_FILE), "synthetic notices").unwrap();
        let facts = collect_about("1.2.3", inspected(Some(root.to_path_buf())));
        assert_eq!(
            facts.build,
            BuildInfoState::Present(BuildInfo {
                product: "BroScribe".into(),
                version: "1.2.3".into(),
                build: 42,
                target: target.clone(),
            })
        );
        let LicenceInventoryState::Present(licences) = &facts.licences else {
            panic!("inventory: {:?}", facts.licences);
        };
        assert!(!licences.complete);
        assert_eq!((licences.components, licences.unresolved), (5, 1));
        assert_eq!(
            licences.by_licence,
            vec![
                ("MIT".to_owned(), 2),
                ("Apache-2.0 OR MIT".to_owned(), 1),
                ("OFL-1.1".to_owned(), 1),
                ("Unresolved".to_owned(), 1),
            ]
        );
        let kinds: Vec<_> = facts.documents.iter().map(|d| d.kind).collect();
        assert_eq!(
            kinds,
            [
                AboutDocumentKind::Licence,
                AboutDocumentKind::ThirdPartyNotices
            ]
        );
        assert_eq!(facts.documents[0].path(), root.join(LICENCE_FILE));
        assert!(facts.mismatches().is_empty());
        let json = serde_json::to_string(&facts).unwrap();
        assert!(!json.contains(&root.display().to_string()), "{json}");
    }

    #[test]
    fn an_unbundled_build_states_missing_facts_instead_of_defaults() {
        let facts = collect_about("1.2.3", inspected(None));
        assert_eq!(facts.packaging, Packaging::Unbundled);
        assert_eq!(facts.build, BuildInfoState::Missing);
        assert_eq!(facts.licences, LicenceInventoryState::Missing);
        assert!(facts.documents.is_empty());
        assert!(facts.mismatches().is_empty());
    }

    #[test]
    fn broken_metadata_is_invalid_without_naming_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(BUILD_INFO_FILE), "{\"schema\": 2}").unwrap();
        std::fs::write(
            root.join(INVENTORY_FILE),
            "{\"schema\": 9, \"target\": \"x\", \"complete\": true}",
        )
        .unwrap();
        let facts = collect_about("1.2.3", inspected(Some(root.to_path_buf())));
        let path = root.display().to_string();
        let BuildInfoState::Invalid { reason } = &facts.build else {
            panic!("build: {:?}", facts.build);
        };
        assert!(!reason.contains(&path), "{reason}");
        assert_eq!(
            facts.licences,
            LicenceInventoryState::Invalid {
                reason: "unsupported inventory schema 9".into()
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_documents_and_metadata_are_not_the_installation_s_own() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        let root = dir.path().join("install");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&outside, build_info("1.2.3", 1, &host_target())).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(BUILD_INFO_FILE)).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(LICENCE_FILE)).unwrap();
        std::os::unix::fs::symlink(&outside, root.join(INVENTORY_FILE)).unwrap();
        let facts = collect_about("1.2.3", inspected(Some(root.clone())));
        assert!(matches!(facts.build, BuildInfoState::Invalid { .. }));
        assert!(matches!(
            facts.licences,
            LicenceInventoryState::Invalid { .. }
        ));
        assert!(facts.documents.is_empty());
        assert_eq!(install_folder(&root.join("broscribe")), None);
    }

    #[test]
    fn mismatches_name_each_inconsistent_fact() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join(BUILD_INFO_FILE),
            build_info("1.2.4", 7, "sparc64-unknown-none"),
        )
        .unwrap();
        std::fs::write(root.join(INVENTORY_FILE), inventory(&host_target())).unwrap();
        let mut install = inspected(Some(root.to_path_buf()));
        install.bundle = Some(BundleMetadata {
            identifier: Some("org.example.synthetic".into()),
            short_version: Some("1.2.3".into()),
            bundle_version: Some("8".into()),
        });
        let facts = collect_about("1.2.3", install);
        assert_eq!(
            facts.mismatches(),
            vec![
                Mismatch::AppVersion {
                    running: "1.2.3".into(),
                    build_info: "1.2.4".into()
                },
                Mismatch::BundleVersion {
                    bundle: "1.2.3".into(),
                    build_info: "1.2.4".into()
                },
                Mismatch::BundleBuild {
                    bundle: "8".into(),
                    build_info: 7
                },
                Mismatch::Target {
                    build_info: "sparc64-unknown-none".into()
                },
                Mismatch::InventoryTarget {
                    inventory: host_target(),
                    build_info: "sparc64-unknown-none".into()
                },
            ]
        );
    }

    #[test]
    fn install_folders_are_the_executable_folder_or_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("BroScribe");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        assert_eq!(install_folder(&root.join("bin/broscribe")), None);
        std::fs::write(root.join(BUILD_INFO_FILE), "{}").unwrap();
        assert_eq!(
            install_folder(&root.join("bin/broscribe")),
            Some(root.clone())
        );
        assert_eq!(install_folder(&root.join("broscribe")), Some(root.clone()));
        let temp = vec![dir.path().to_path_buf()];
        assert!(is_temporary(&root, &temp));
        assert!(!is_temporary(Path::new("/opt/BroScribe"), &temp));
    }

    #[test]
    fn notarization_follows_only_from_what_the_signature_settles() {
        let apple = |kind| SignatureFacts::apple(kind, None, SealState::Intact).notarization;
        assert_eq!(apple(SignatureKind::AdHoc), Notarization::NotEligible);
        assert_eq!(apple(SignatureKind::Unsigned), Notarization::NotEligible);
        assert_eq!(
            apple(SignatureKind::AppleDevelopment { team: None }),
            Notarization::NotEligible
        );
        assert_eq!(
            apple(SignatureKind::DeveloperId {
                team: Some("ABCDE12345".into())
            }),
            Notarization::Unknown
        );
        assert_eq!(
            apple(SignatureKind::Unknown {
                reason: "synthetic".into()
            }),
            Notarization::Unknown
        );
        let other = SignatureFacts::without_notarization(
            SignatureKind::Authenticode { fingerprint: None },
            SealState::Broken { status: -1 },
        );
        assert_eq!(other.notarization, Notarization::NotApplicable);
    }

    #[test]
    fn build_information_alone_matches_the_full_facts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(build_info_state(None), BuildInfoState::Missing);
        assert_eq!(build_info_state(Some(root)), BuildInfoState::Missing);
        let full = |root: &Path| collect_about("1.2.3", inspected(Some(root.to_path_buf()))).build;
        assert_eq!(build_info_state(Some(root)), full(root));
        std::fs::write(root.join(BUILD_INFO_FILE), "{\"schema\": 2}").unwrap();
        let invalid = build_info_state(Some(root));
        let BuildInfoState::Invalid { reason } = &invalid else {
            panic!("build: {invalid:?}");
        };
        assert!(!reason.contains(&root.display().to_string()), "{reason}");
        assert_eq!(invalid, full(root));
        std::fs::write(
            root.join(BUILD_INFO_FILE),
            build_info("1.2.3", 42, &host_target()),
        )
        .unwrap();
        let present = build_info_state(Some(root));
        assert!(matches!(present, BuildInfoState::Present(_)), "{present:?}");
        assert_eq!(present, full(root));
    }

    #[test]
    fn the_fake_returns_its_facts_or_fails() {
        let facts = collect_about("1.2.3", inspected(None));
        assert_eq!(FakeAboutReader::new(facts.clone()).read().unwrap(), facts);
        assert_eq!(
            FakeAboutReader::new(facts.clone()).read_build().unwrap(),
            facts.build
        );
        assert_eq!(
            FakeAboutReader::failing("synthetic")
                .read()
                .unwrap_err()
                .code(),
            "io"
        );
    }
}
