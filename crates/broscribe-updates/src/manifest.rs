//! The signed release envelope and its manifest.
//!
//! Envelope (`application/json`, at most [`Limits::max_envelope_bytes`]):
//!
//! ```json
//! {
//!   "format": "broscribe-update-envelope",
//!   "schema": 1,
//!   "manifest": "<the manifest JSON text, as one JSON string>",
//!   "signatures": [{ "key_id": "<16 hex>", "ed25519": "<128 hex>" }]
//! }
//! ```
//!
//! Each signature is Ed25519 over [`SIGNING_CONTEXT`] followed by the UTF-8
//! bytes of `manifest` exactly as carried; nothing is re-serialized. At
//! least one signature must come from a pinned key and verify; a signature
//! naming a pinned key that does not verify rejects the whole envelope;
//! signatures by other keys are ignored.
//!
//! Manifest (unknown fields are refused; a new field needs a new schema):
//!
//! ```json
//! {
//!   "format": "broscribe-release-manifest",
//!   "schema": 1,
//!   "product": "BroScribe",
//!   "channel": "stable",
//!   "version": "1.4.0",
//!   "build": 57,
//!   "published_at": "2026-11-17T10:00:00Z",
//!   "expires_at": "2026-12-17T10:00:00Z",
//!   "artifacts": [{
//!     "target": "aarch64-apple-darwin",
//!     "app_id": "org.broscribe.BroScribe",
//!     "url": "https://downloads.example/BroScribe-1.4.0-57-macos-arm64.zip",
//!     "size": 123456789,
//!     "sha256": "<64 lowercase hex>",
//!     "archive": "zip",
//!     "root": "BroScribe.app",
//!     "entries": 812,
//!     "installed_size": 456789012
//!   }]
//! }
//! ```
//!
//! `entries` counts every archive entry (folders included); `installed_size`
//! sums the uncompressed sizes of files and link targets. Both are exact.
//! `app_id` is the code-signing identifier on macOS and the product name on
//! Linux and Windows.

use broscribe_core::Result;
use chrono::{DateTime, Duration, Utc};
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::{Deserialize, Serialize};

use crate::settings::{Limits, TrustAnchors, check_https_url, hex, unhex, valid_channel};
use crate::{KeyId, codes, fail};

pub const ENVELOPE_FORMAT: &str = "broscribe-update-envelope";
pub const MANIFEST_FORMAT: &str = "broscribe-release-manifest";
/// Domain separation prefix of every manifest signature.
pub const SIGNING_CONTEXT: &[u8] = b"broscribe-update-manifest-v1\n";
/// Clock skew tolerated for `published_at`.
const SKEW: Duration = Duration::minutes(10);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: String,
    schema: u32,
    manifest: String,
    signatures: Vec<EnvelopeSignature>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvelopeSignature {
    key_id: String,
    ed25519: String,
}

/// Archive formats the updater extracts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    Zip,
    #[serde(rename = "tar.gz")]
    TarGz,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub target: String,
    pub app_id: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
    pub archive: ArchiveFormat,
    pub root: String,
    pub entries: u64,
    pub installed_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub format: String,
    pub schema: u32,
    pub product: String,
    pub channel: String,
    pub version: String,
    pub build: u64,
    pub published_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_notes: Option<String>,
    pub artifacts: Vec<Artifact>,
}

impl ReleaseManifest {
    /// The parsed release version (validated by [`verify_envelope`]).
    pub fn semver(&self) -> semver::Version {
        semver::Version::parse(&self.version).expect("validated release version")
    }

    /// The artifact for `target`, if the release has one.
    pub fn artifact_for(&self, target: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.target == target)
    }
}

/// Parses a release version: exactly `MAJOR.MINOR.PATCH`, no pre-release or
/// build metadata.
pub(crate) fn release_version(text: &str) -> Option<semver::Version> {
    let version = semver::Version::parse(text).ok()?;
    (version.pre.is_empty() && version.build.is_empty() && version.to_string() == text)
        .then_some(version)
}

/// Verifies `bytes` as an envelope signed by `anchors` and validates its
/// manifest at time `now`.
pub fn verify_envelope(
    bytes: &[u8],
    anchors: &TrustAnchors,
    limits: &Limits,
    now: DateTime<Utc>,
) -> Result<ReleaseManifest> {
    verify_release_envelope(bytes, anchors, limits, now, false)
}

/// Publisher verification for an existing baseline. Only expiry may be
/// waived. Updater clients must use [`verify_envelope`].
pub fn verify_release_envelope(
    bytes: &[u8],
    anchors: &TrustAnchors,
    limits: &Limits,
    now: DateTime<Utc>,
    allow_expired: bool,
) -> Result<ReleaseManifest> {
    let invalid = |why: String| fail(codes::MANIFEST_INVALID, why);
    if bytes.len() as u64 > limits.max_envelope_bytes {
        return Err(invalid("the envelope exceeds its size limit".into()));
    }
    let envelope: Envelope =
        serde_json::from_slice(bytes).map_err(|e| invalid(format!("envelope: {e}")))?;
    if envelope.format != ENVELOPE_FORMAT || envelope.schema != 1 {
        return Err(invalid("not a schema 1 BroScribe update envelope".into()));
    }
    if envelope.signatures.is_empty() || envelope.signatures.len() > limits.max_signatures {
        return Err(invalid(
            "the envelope carries no or too many signatures".into(),
        ));
    }
    let mut message = Vec::with_capacity(SIGNING_CONTEXT.len() + envelope.manifest.len());
    message.extend_from_slice(SIGNING_CONTEXT);
    message.extend_from_slice(envelope.manifest.as_bytes());
    let mut verified = false;
    for signature in &envelope.signatures {
        let id = unhex(&signature.key_id)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(KeyId)
            .ok_or_else(|| invalid("a signature key_id is not 16 lowercase hex digits".into()))?;
        let Some(key) = anchors.get(&id) else {
            continue;
        };
        let sig = unhex(&signature.ed25519)
            .filter(|sig| sig.len() == 64)
            .ok_or_else(|| invalid("a signature is not 128 lowercase hex digits".into()))?;
        UnparsedPublicKey::new(&ED25519, key)
            .verify(&message, &sig)
            .map_err(|_| {
                fail(
                    codes::SIGNATURE_INVALID,
                    format!("the signature by pinned key {id} does not verify"),
                )
            })?;
        verified = true;
    }
    if !verified {
        return Err(fail(
            codes::SIGNATURE_INVALID,
            "the envelope is not signed by any pinned release key",
        ));
    }
    let manifest: ReleaseManifest =
        serde_json::from_str(&envelope.manifest).map_err(|e| invalid(format!("manifest: {e}")))?;
    validate(&manifest, limits, now, allow_expired)?;
    Ok(manifest)
}

fn validate(
    manifest: &ReleaseManifest,
    limits: &Limits,
    now: DateTime<Utc>,
    allow_expired: bool,
) -> Result<()> {
    let invalid = |why: String| Err(fail(codes::MANIFEST_INVALID, why));
    if manifest.format != MANIFEST_FORMAT || manifest.schema != 1 {
        return invalid("not a schema 1 BroScribe release manifest".into());
    }
    if manifest.product.is_empty() || manifest.product.len() > 64 {
        return invalid("bad product".into());
    }
    if !valid_channel(&manifest.channel) {
        return invalid("bad channel".into());
    }
    if release_version(&manifest.version).is_none() {
        return invalid(format!(
            "version {:?} is not MAJOR.MINOR.PATCH",
            manifest.version
        ));
    }
    if manifest.build == 0 {
        return invalid("build must be positive".into());
    }
    if manifest.expires_at <= manifest.published_at {
        return invalid("expires_at is not after published_at".into());
    }
    if manifest.published_at > now + SKEW {
        return Err(fail(
            codes::EXPIRED,
            "the manifest is published in the future",
        ));
    }
    if !allow_expired && manifest.expires_at <= now {
        return Err(fail(codes::EXPIRED, "the manifest has expired"));
    }
    if manifest.artifacts.is_empty() || manifest.artifacts.len() > limits.max_artifacts {
        return invalid("no or too many artifacts".into());
    }
    for (i, artifact) in manifest.artifacts.iter().enumerate() {
        if manifest.artifacts[..i]
            .iter()
            .any(|other| other.target == artifact.target)
        {
            return invalid(format!("two artifacts for {}", artifact.target));
        }
        if artifact.target.is_empty()
            || artifact.target.len() > 64
            || !artifact
                .target
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return invalid("bad artifact target".into());
        }
        if artifact.app_id.is_empty() || artifact.app_id.len() > 255 {
            return invalid("bad artifact app_id".into());
        }
        check_https_url(&artifact.url).map_err(|e| fail(codes::MANIFEST_INVALID, e.to_string()))?;
        if artifact.size == 0 || artifact.size > limits.max_artifact_bytes {
            return Err(fail(
                codes::ARCHIVE_TOO_LARGE,
                format!("artifact size {} is outside the limit", artifact.size),
            ));
        }
        if artifact.installed_size > limits.max_installed_bytes
            || artifact.entries == 0
            || artifact.entries > limits.max_entries
        {
            return Err(fail(
                codes::ARCHIVE_TOO_LARGE,
                "the artifact's installed size or entry count is outside the limits",
            ));
        }
        if unhex(&artifact.sha256).is_none_or(|digest| digest.len() != 32) {
            return invalid("artifact sha256 is not 64 lowercase hex digits".into());
        }
        if !valid_root(&artifact.root) {
            return invalid(format!(
                "artifact root {:?} is not one plain name",
                artifact.root
            ));
        }
    }
    Ok(())
}

/// One plain path component, safe on every platform.
pub(crate) fn valid_root(root: &str) -> bool {
    !root.is_empty()
        && root.len() <= 255
        && root != "."
        && root != ".."
        && !root.starts_with("._")
        && root != "__MACOSX"
        && !root.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
}

/// The artifact's SHA-256 as bytes (validated).
pub(crate) fn digest_of(artifact: &Artifact) -> [u8; 32] {
    unhex(&artifact.sha256)
        .and_then(|bytes| bytes.try_into().ok())
        .expect("validated artifact digest")
}

pub(crate) fn hex_digest(digest: &[u8]) -> String {
    hex(digest)
}
