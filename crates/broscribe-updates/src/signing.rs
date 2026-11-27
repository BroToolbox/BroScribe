//! Release signing: the publisher's side of the envelope [`verify_envelope`]
//! checks. Profile keys live in the platform [`SecretStore`]. Release
//! automation can also use a private, 32-byte POSIX seed file. Both paths
//! return public keys and signatures only, and wipe their seed buffers.
//!
//! Each signature is Ed25519 over [`SIGNING_CONTEXT`] followed by the
//! manifest text exactly as given; the envelope carries that text unchanged.
//! Before an envelope is returned it goes through [`verify_envelope`] with
//! the signers' public keys pinned, so a malformed, expired or unverifiable
//! manifest never leaves as a signed release.
//!
//! Key rotation is explicit: [`generate_release_key`] refuses an occupied
//! label unless asked to replace it ([`ExistingKey::Replace`]), and during an
//! overlap the publisher passes both labels to [`sign_manifest`] so clients
//! that pin either key accept the release. Nothing rotates on its own.

use std::path::Path;

use broscribe_core::{Error, Result};
use broscribe_platform::SecretStore;
use chrono::{DateTime, Utc};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Serialize, Serializer};

use crate::settings::{Limits, hex};
use crate::{
    Cancel, ENVELOPE_FORMAT, KeyId, ReleaseManifest, SIGNING_CONTEXT, TrustAnchors, codes, fail,
    verify_envelope,
};

/// The label already holds a release key and replacing it was not asked for.
pub const KEY_EXISTS: &str = "update_signing_key_exists";
/// The secret store holds something that is not a release key under the
/// label, or did not keep a newly generated key.
pub const KEY_INVALID: &str = "update_signing_key_invalid";

/// Prefix of the stored value; the 64 lowercase hex digits of the seed follow.
const SECRET_FORMAT: &str = "broscribe-release-key-v1:ed25519-seed:";
const SEED_LEN: usize = 32;

/// The name of one release key in the secret store: 1-32 lowercase ASCII
/// letters, digits or `-`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReleaseKeyLabel(String);

impl ReleaseKeyLabel {
    pub fn parse(label: &str) -> Result<Self> {
        if (1..=32).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            Ok(Self(label.to_owned()))
        } else {
            Err(Error::InvalidInput(
                "a release key label must be 1-32 lowercase letters, digits or '-'".into(),
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The secret store entry holding this key.
    pub fn secret_name(&self) -> String {
        format!("updates/release_key/{}", self.0)
    }
}

/// The public half of a stored release key. Serializes as
/// `{"label", "key_id", "public_key"}` (lowercase hex); `public_key` is the
/// value `[updates] trusted_keys` pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseKey {
    pub label: ReleaseKeyLabel,
    pub key_id: KeyId,
    pub public_key: [u8; 32],
}

impl ReleaseKey {
    pub fn public_key_hex(&self) -> String {
        hex(&self.public_key)
    }
}

impl Serialize for ReleaseKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            label: &'a str,
            key_id: String,
            public_key: String,
        }
        Wire {
            label: self.label.as_str(),
            key_id: self.key_id.to_string(),
            public_key: self.public_key_hex(),
        }
        .serialize(serializer)
    }
}

/// What [`generate_release_key`] does when the label already holds a value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExistingKey {
    /// Refuse with [`KEY_EXISTS`]; nothing is written.
    #[default]
    Refuse,
    /// Replace it (explicit rotation): the previous private key is gone
    /// afterwards, so releases it signed can no longer be re-signed by it.
    Replace,
}

/// A newly generated release key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GeneratedReleaseKey {
    pub key: ReleaseKey,
    /// The key [`ExistingKey::Replace`] replaced; `None` when the label was
    /// empty or held a value that was not a release key.
    pub replaced: Option<ReleaseKey>,
}

/// A signed release envelope ready to publish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SignedRelease {
    /// The envelope JSON text, already verified by [`verify_envelope`].
    pub envelope: String,
    /// One per signature, in envelope order.
    pub signers: Vec<ReleaseKey>,
}

/// An Ed25519 seed, wiped when dropped.
struct Seed([u8; SEED_LEN]);

impl Drop for Seed {
    fn drop(&mut self) {
        self.0.fill(0);
        let _ = std::hint::black_box(&self.0);
    }
}

impl Seed {
    fn generate() -> Result<Self> {
        let mut seed = Self([0; SEED_LEN]);
        SystemRandom::new()
            .fill(&mut seed.0)
            .map_err(|_| Error::Internal("the system random generator failed".into()))?;
        Ok(seed)
    }

    fn key_pair(&self) -> Result<Ed25519KeyPair> {
        Ed25519KeyPair::from_seed_unchecked(&self.0)
            .map_err(|_| fail(KEY_INVALID, "the stored release key is not an Ed25519 seed"))
    }

    /// The stored form. The caller hands it to the store and wipes it.
    fn encode(&self) -> String {
        let mut value = String::with_capacity(SECRET_FORMAT.len() + 2 * SEED_LEN);
        value.push_str(SECRET_FORMAT);
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        for byte in self.0 {
            value.push(char::from(DIGITS[usize::from(byte >> 4)]));
            value.push(char::from(DIGITS[usize::from(byte & 15)]));
        }
        value
    }

    /// Parses a stored value and wipes it; errors never echo it.
    fn decode(value: String, label: &ReleaseKeyLabel) -> Result<Self> {
        let mut bytes = value.into_bytes();
        let seed = Self::parse(&bytes);
        wipe(&mut bytes);
        seed.ok_or_else(|| {
            fail(
                KEY_INVALID,
                format!(
                    "the secret store entry for release key {} is not a release key",
                    label.as_str()
                ),
            )
        })
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        let digits = bytes.strip_prefix(SECRET_FORMAT.as_bytes())?;
        if digits.len() != 2 * SEED_LEN {
            return None;
        }
        let digit = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        let mut seed = Self([0; SEED_LEN]);
        for (out, pair) in seed.0.iter_mut().zip(digits.as_chunks::<2>().0) {
            *out = digit(pair[0])? << 4 | digit(pair[1])?;
        }
        Some(seed)
    }
}

fn wipe(bytes: &mut [u8]) {
    bytes.fill(0);
    let _ = std::hint::black_box(&*bytes);
}

fn public_of(label: &ReleaseKeyLabel, pair: &Ed25519KeyPair) -> ReleaseKey {
    let mut public_key = [0; 32];
    public_key.copy_from_slice(pair.public_key().as_ref());
    ReleaseKey {
        label: label.clone(),
        key_id: KeyId::of(&public_key),
        public_key,
    }
}

fn load_seed(store: &dyn SecretStore, label: &ReleaseKeyLabel) -> Result<Seed> {
    let value = store.get(&label.secret_name())?.ok_or_else(|| {
        Error::NotConfigured(format!(
            "no release key {} in the secret store",
            label.as_str()
        ))
    })?;
    Seed::decode(value, label)
}

/// Generates a release key from the system random generator, stores it
/// under `label` and returns its public half after reading it back from the
/// store and checking that it signs and verifies.
pub fn generate_release_key(
    store: &dyn SecretStore,
    label: &ReleaseKeyLabel,
    existing: ExistingKey,
) -> Result<GeneratedReleaseKey> {
    let name = label.secret_name();
    let replaced = match store.get(&name)? {
        None => None,
        Some(value) => {
            let previous = Seed::decode(value, label)
                .and_then(|seed| seed.key_pair())
                .map(|pair| public_of(label, &pair));
            match existing {
                ExistingKey::Refuse => {
                    let which = previous
                        .map(|key| format!(" ({})", key.key_id))
                        .unwrap_or_default();
                    return Err(fail(
                        KEY_EXISTS,
                        format!(
                            "release key {}{which} already exists; replacing it must be asked for",
                            label.as_str()
                        ),
                    ));
                }
                ExistingKey::Replace => previous.ok(),
            }
        }
    };
    let seed = Seed::generate()?;
    let key = public_of(label, &seed.key_pair()?);
    let mut value = seed.encode().into_bytes();
    let stored = store.set(
        &name,
        std::str::from_utf8(&value).expect("the encoded key is ASCII"),
    );
    wipe(&mut value);
    stored?;
    let kept = match load_seed(store, label) {
        Err(Error::NotConfigured(_)) => Err(fail(
            KEY_INVALID,
            format!(
                "the secret store did not keep release key {}",
                label.as_str()
            ),
        )),
        other => other,
    }?;
    if kept.0 != seed.0 {
        return Err(fail(
            KEY_INVALID,
            format!(
                "the secret store returned another value for release key {}",
                label.as_str()
            ),
        ));
    }
    let pair = kept.key_pair()?;
    let probe = b"broscribe-release-key-self-test";
    UnparsedPublicKey::new(&ED25519, &key.public_key)
        .verify(probe, pair.sign(probe).as_ref())
        .map_err(|_| fail(KEY_INVALID, "the new release key does not verify"))?;
    Ok(GeneratedReleaseKey { key, replaced })
}

/// The public half of the release key stored under `label`.
pub fn release_key(store: &dyn SecretStore, label: &ReleaseKeyLabel) -> Result<ReleaseKey> {
    Ok(public_of(label, &load_seed(store, label)?.key_pair()?))
}

#[derive(Serialize)]
struct EnvelopeOut<'a> {
    format: &'a str,
    schema: u32,
    manifest: &'a str,
    signatures: Vec<SignatureOut>,
}

#[derive(Serialize)]
struct SignatureOut {
    key_id: String,
    ed25519: String,
}

/// Signs `manifest` (the manifest JSON text, carried exactly) with every
/// key in `labels`, in order, and returns the envelope once
/// [`verify_envelope`] accepts it at `now` with exactly those keys pinned.
///
/// Refuses no labels, a repeated label or key, more signatures than
/// [`Limits::max_signatures`], a missing or malformed stored key, and any
/// manifest the updater would refuse (`update_manifest_invalid`,
/// `update_expired`, `update_archive_too_large`). `cancel` is checked
/// before each key is read and before the envelope is returned.
pub fn sign_manifest(
    store: &dyn SecretStore,
    labels: &[ReleaseKeyLabel],
    manifest: &str,
    now: DateTime<Utc>,
    cancel: &Cancel,
) -> Result<SignedRelease> {
    sign_with(labels, manifest, now, cancel, |label| {
        load_seed(store, label)
    })
}

/// Generates a standalone seed file, creating it as mode 0600 and refusing
/// to replace an existing path. Returns the public half only.
pub fn new_release_key_file(path: &Path) -> Result<ReleaseKey> {
    let seed = Seed::generate()?;
    let key = public_of(&file_label(), &seed.key_pair()?);
    write_seed_file(path, &seed)?;
    Ok(key)
}

fn file_label() -> ReleaseKeyLabel {
    ReleaseKeyLabel("key-file".into())
}

#[cfg(unix)]
fn write_seed_file(path: &Path, seed: &Seed) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::io_at(path, e))?;
    // A stricter umask may have removed owner bits. The descriptor already
    // points at the newly created file, regardless of a pathname change.
    use std::os::unix::fs::PermissionsExt as _;
    let result = file
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .and_then(|()| file.write_all(&seed.0))
        .and_then(|()| file.sync_all());
    if let Err(error) = result {
        let _ = std::fs::remove_file(path);
        return Err(Error::io_at(path, error));
    }
    Ok(())
}

#[cfg(unix)]
fn read_seed_file(path: &Path) -> Result<Seed> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    // Refuse links and avoid blocking if the path is a FIFO or device.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| Error::io_at(path, e))?;
    let meta = file.metadata().map_err(|e| Error::io_at(path, e))?;
    if !meta.is_file() || meta.mode() & 0o7777 != 0o600 || meta.len() != SEED_LEN as u64 {
        return Err(fail(
            KEY_INVALID,
            "a release seed file must be a mode-0600 regular file of exactly 32 bytes",
        ));
    }
    let mut seed = Seed([0; SEED_LEN]);
    file.read_exact(&mut seed.0)
        .map_err(|e| Error::io_at(path, e))?;
    let mut extra = [0; 1];
    let read = file.read(&mut extra);
    wipe(&mut extra);
    if read.map_err(|e| Error::io_at(path, e))? != 0 {
        return Err(fail(
            KEY_INVALID,
            "the release seed file changed while being read",
        ));
    }
    Ok(seed)
}

#[cfg(not(unix))]
fn write_seed_file(_: &Path, _: &Seed) -> Result<()> {
    Err(Error::Unsupported(
        "standalone release seed files require POSIX mode 0600".into(),
    ))
}

#[cfg(not(unix))]
fn read_seed_file(_: &Path) -> Result<Seed> {
    Err(Error::Unsupported(
        "standalone release seed files require POSIX mode 0600".into(),
    ))
}

/// Signs with one raw, mode-0600 Ed25519 seed file.
pub fn sign_manifest_file(
    path: &Path,
    manifest: &str,
    now: DateTime<Utc>,
    cancel: &Cancel,
) -> Result<SignedRelease> {
    sign_with(&[file_label()], manifest, now, cancel, |_| {
        read_seed_file(path)
    })
}

fn sign_with(
    labels: &[ReleaseKeyLabel],
    manifest: &str,
    now: DateTime<Utc>,
    cancel: &Cancel,
    mut seed: impl FnMut(&ReleaseKeyLabel) -> Result<Seed>,
) -> Result<SignedRelease> {
    cancel.check()?;
    let limits = Limits::default();
    if labels.is_empty() || labels.len() > limits.max_signatures {
        return Err(Error::InvalidInput(format!(
            "a release is signed by 1-{} release keys",
            limits.max_signatures
        )));
    }
    for (i, label) in labels.iter().enumerate() {
        if labels[..i].contains(label) {
            return Err(Error::InvalidInput(format!(
                "release key {} is listed twice",
                label.as_str()
            )));
        }
    }
    serde_json::from_str::<ReleaseManifest>(manifest)
        .map_err(|e| fail(codes::MANIFEST_INVALID, format!("manifest: {e}")))?;
    let mut message = Vec::with_capacity(SIGNING_CONTEXT.len() + manifest.len());
    message.extend_from_slice(SIGNING_CONTEXT);
    message.extend_from_slice(manifest.as_bytes());
    let mut signers: Vec<ReleaseKey> = Vec::with_capacity(labels.len());
    let mut signatures = Vec::with_capacity(labels.len());
    for label in labels {
        cancel.check()?;
        let pair = seed(label)?.key_pair()?;
        let key = public_of(label, &pair);
        if let Some(same) = signers.iter().find(|other| other.key_id == key.key_id) {
            return Err(Error::InvalidInput(format!(
                "release keys {} and {} are the same key",
                same.label.as_str(),
                label.as_str()
            )));
        }
        signatures.push(SignatureOut {
            key_id: key.key_id.to_string(),
            ed25519: hex(pair.sign(&message).as_ref()),
        });
        signers.push(key);
    }
    let envelope = serde_json::to_string(&EnvelopeOut {
        format: ENVELOPE_FORMAT,
        schema: 1,
        manifest,
        signatures,
    })
    .map_err(|e| Error::Internal(format!("serializing the envelope: {e}")))?;
    let anchors = TrustAnchors::from_hex(
        &signers
            .iter()
            .map(ReleaseKey::public_key_hex)
            .collect::<Vec<_>>(),
    )?;
    verify_envelope(envelope.as_bytes(), &anchors, &limits, now)?;
    cancel.check()?;
    Ok(SignedRelease { envelope, signers })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use broscribe_platform::fakes::FakeSecretStore;
    use chrono::Duration;
    use serde_json::{Value, json};

    use super::*;
    use crate::settings::unhex;

    const SEED_A: [u8; SEED_LEN] = [7; SEED_LEN];
    const SEED_B: [u8; SEED_LEN] = [42; SEED_LEN];

    fn label(text: &str) -> ReleaseKeyLabel {
        ReleaseKeyLabel::parse(text).unwrap()
    }

    /// A store holding invented seeds under the given labels.
    fn store_with(keys: &[(&str, [u8; SEED_LEN])]) -> FakeSecretStore {
        let store = FakeSecretStore::default();
        for (name, seed) in keys {
            store
                .set(&label(name).secret_name(), &Seed(*seed).encode())
                .unwrap();
        }
        store
    }

    fn public_hex(seed: [u8; SEED_LEN]) -> String {
        hex(Ed25519KeyPair::from_seed_unchecked(&seed)
            .unwrap()
            .public_key()
            .as_ref())
    }

    fn anchors(seeds: &[[u8; SEED_LEN]]) -> TrustAnchors {
        TrustAnchors::from_hex(&seeds.iter().map(|s| public_hex(*s)).collect::<Vec<_>>()).unwrap()
    }

    fn manifest_value(now: DateTime<Utc>) -> Value {
        json!({
            "format": crate::MANIFEST_FORMAT,
            "schema": 1,
            "product": "BroScribe",
            "channel": "stable",
            "version": "1.4.0",
            "build": 57,
            "published_at": (now - Duration::hours(1)).to_rfc3339(),
            "expires_at": (now + Duration::days(30)).to_rfc3339(),
            "artifacts": [{
                "target": "aarch64-apple-darwin",
                "app_id": "org.broscribe.BroScribe",
                "url": "https://updates.example/BroScribe-1.4.0-57-macos-arm64.zip",
                "size": 1234,
                "sha256": "ab".repeat(32),
                "archive": "zip",
                "root": "BroScribe.app",
                "entries": 3,
                "installed_size": 4096,
            }],
        })
    }

    fn sign(
        store: &dyn SecretStore,
        labels: &[&str],
        manifest: &str,
        now: DateTime<Utc>,
    ) -> Result<SignedRelease> {
        let labels: Vec<_> = labels.iter().map(|l| label(l)).collect();
        sign_manifest(store, &labels, manifest, now, &Cancel::new())
    }

    #[derive(Default)]
    struct BrokenStore {
        fail_get: bool,
        fail_set: bool,
        /// Accepts writes without keeping them.
        forget: bool,
        values: Mutex<Vec<(String, String)>>,
    }

    impl SecretStore for BrokenStore {
        fn get(&self, key: &str) -> Result<Option<String>> {
            if self.fail_get {
                return Err(Error::Unsupported("synthetic keychain refusal".into()));
            }
            Ok(self
                .values
                .lock()
                .unwrap()
                .iter()
                .find_map(|(k, v)| (k == key).then(|| v.clone())))
        }
        fn set(&self, key: &str, value: &str) -> Result<()> {
            if self.fail_set {
                return Err(Error::Unsupported("synthetic keychain refusal".into()));
            }
            if !self.forget {
                self.values.lock().unwrap().push((key.into(), value.into()));
            }
            Ok(())
        }
        fn delete(&self, _key: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn labels_are_plain_names() {
        assert_eq!(
            label("release-2026").secret_name(),
            "updates/release_key/release-2026"
        );
        for bad in ["", "Release", "a/b", "a b", "a.b", &"a".repeat(33)] {
            assert_eq!(
                ReleaseKeyLabel::parse(bad).unwrap_err().code(),
                "invalid_input",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_generated_key_lives_only_in_the_store_and_signs_envelopes_the_updater_accepts() {
        let store = FakeSecretStore::default();
        let generated =
            generate_release_key(&store, &label("release-a"), ExistingKey::default()).unwrap();
        assert_eq!(generated.replaced, None);
        let stored = store
            .get(&label("release-a").secret_name())
            .unwrap()
            .expect("stored in the secret store");
        let secret_digits = stored.strip_prefix(SECRET_FORMAT).unwrap();
        let shown = serde_json::to_string(&generated).unwrap();
        assert!(!shown.contains(secret_digits), "no key material in output");
        let shown: Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(
            shown["key"]["public_key"],
            json!(generated.key.public_key_hex())
        );
        assert_eq!(
            shown["key"]["key_id"],
            json!(generated.key.key_id.to_string())
        );
        assert_eq!(
            release_key(&store, &label("release-a")).unwrap(),
            generated.key
        );

        let now = Utc::now();
        let text = manifest_value(now).to_string();
        let signed = sign(&store, &["release-a"], &text, now).unwrap();
        assert!(!signed.envelope.contains(secret_digits));
        assert_eq!(signed.signers, vec![generated.key.clone()]);
        let pinned = TrustAnchors::from_hex(&[generated.key.public_key_hex()]).unwrap();
        let manifest =
            verify_envelope(signed.envelope.as_bytes(), &pinned, &Limits::default(), now).unwrap();
        assert_eq!(manifest.build, 57);
        assert_eq!(manifest.version, "1.4.0");
    }

    #[test]
    fn the_manifest_text_is_signed_and_carried_exactly() {
        let store = store_with(&[("release-a", SEED_A)]);
        let now = Utc::now();
        let text = serde_json::to_string_pretty(&manifest_value(now)).unwrap() + "\n";
        let signed = sign(&store, &["release-a"], &text, now).unwrap();
        let envelope: Value = serde_json::from_str(&signed.envelope).unwrap();
        assert_eq!(envelope["format"], json!(ENVELOPE_FORMAT));
        assert_eq!(envelope["schema"], json!(1));
        assert_eq!(envelope["manifest"], json!(text));
        let signature = &envelope["signatures"][0];
        let public = unhex(&public_hex(SEED_A)).unwrap();
        assert_eq!(signature["key_id"], json!(KeyId::of(&public).to_string()));
        let mut message = SIGNING_CONTEXT.to_vec();
        message.extend_from_slice(text.as_bytes());
        let sig = unhex(signature["ed25519"].as_str().unwrap()).unwrap();
        UnparsedPublicKey::new(&ED25519, &public)
            .verify(&message, &sig)
            .expect("Ed25519 over the context and the exact text");
    }

    #[test]
    fn tampered_unsigned_and_wrong_key_envelopes_are_refused() {
        let store = store_with(&[("release-a", SEED_A)]);
        let now = Utc::now();
        let signed = sign(
            &store,
            &["release-a"],
            &manifest_value(now).to_string(),
            now,
        )
        .unwrap();
        let limits = Limits::default();
        let verify = |bytes: &[u8], seeds: &[[u8; SEED_LEN]]| {
            verify_envelope(bytes, &anchors(seeds), &limits, now)
        };
        assert!(verify(signed.envelope.as_bytes(), &[SEED_A]).is_ok());

        let mut tampered: Value = serde_json::from_str(&signed.envelope).unwrap();
        let manifest = tampered["manifest"]
            .as_str()
            .unwrap()
            .replace("\"build\":57", "\"build\":58");
        tampered["manifest"] = json!(manifest);
        let error = verify(tampered.to_string().as_bytes(), &[SEED_A]).unwrap_err();
        assert_eq!(error.code(), codes::SIGNATURE_INVALID);

        let error = verify(signed.envelope.as_bytes(), &[SEED_B]).unwrap_err();
        assert_eq!(error.code(), codes::SIGNATURE_INVALID);

        let mut unsigned: Value = serde_json::from_str(&signed.envelope).unwrap();
        unsigned["signatures"] = json!([]);
        let error = verify(unsigned.to_string().as_bytes(), &[SEED_A]).unwrap_err();
        assert_eq!(error.code(), codes::MANIFEST_INVALID);
    }

    #[test]
    fn renewal_accepts_an_expired_envelope_only_with_an_authentic_signature() {
        let store = store_with(&[("release-a", SEED_A)]);
        let now = Utc::now();
        let past = now - chrono::Duration::days(120);
        let signed = sign(
            &store,
            &["release-a"],
            &manifest_value(past).to_string(),
            past,
        )
        .unwrap();
        let signing_keys = anchors(&[SEED_A]);
        let limits = Limits::default();
        let authenticate =
            |bytes| crate::verify_release_envelope(bytes, &signing_keys, &limits, now, true);
        assert_eq!(
            verify_envelope(signed.envelope.as_bytes(), &signing_keys, &limits, now)
                .unwrap_err()
                .code(),
            codes::EXPIRED
        );
        assert!(authenticate(signed.envelope.as_bytes()).is_ok());
        let mut forged: Value = serde_json::from_str(&signed.envelope).unwrap();
        forged["signatures"][0]["ed25519"] = json!("0".repeat(128));
        assert_eq!(
            authenticate(forged.to_string().as_bytes())
                .unwrap_err()
                .code(),
            codes::SIGNATURE_INVALID
        );
    }

    #[test]
    fn rotation_is_explicit_and_overlap_signs_with_both_keys() {
        let store = store_with(&[("release-a", SEED_A)]);
        let before = store.get(&label("release-a").secret_name()).unwrap();
        let error =
            generate_release_key(&store, &label("release-a"), ExistingKey::Refuse).unwrap_err();
        assert_eq!(error.code(), KEY_EXISTS);
        assert_eq!(
            store.get(&label("release-a").secret_name()).unwrap(),
            before,
            "a refused generation writes nothing"
        );

        let rotated =
            generate_release_key(&store, &label("release-a"), ExistingKey::Replace).unwrap();
        let replaced = rotated.replaced.expect("reports the replaced public key");
        assert_eq!(replaced.public_key_hex(), public_hex(SEED_A));
        assert_ne!(rotated.key.public_key_hex(), public_hex(SEED_A));

        let store = store_with(&[("old", SEED_A), ("new", SEED_B)]);
        let now = Utc::now();
        let signed = sign(
            &store,
            &["old", "new"],
            &manifest_value(now).to_string(),
            now,
        )
        .unwrap();
        assert_eq!(signed.signers.len(), 2);
        for seed in [SEED_A, SEED_B] {
            verify_envelope(
                signed.envelope.as_bytes(),
                &anchors(&[seed]),
                &Limits::default(),
                now,
            )
            .expect("clients pinning either key accept the overlap release");
        }
    }

    #[test]
    fn secret_store_failures_and_bad_stored_keys_sign_nothing() {
        let now = Utc::now();
        let text = manifest_value(now).to_string();
        let refusing = BrokenStore {
            fail_get: true,
            ..BrokenStore::default()
        };
        let error = sign(&refusing, &["release-a"], &text, now).unwrap_err();
        assert_eq!(error.code(), "unsupported");
        let error =
            generate_release_key(&refusing, &label("release-a"), ExistingKey::Refuse).unwrap_err();
        assert_eq!(error.code(), "unsupported");

        let unwritable = BrokenStore {
            fail_set: true,
            ..BrokenStore::default()
        };
        let error = generate_release_key(&unwritable, &label("release-a"), ExistingKey::Refuse)
            .unwrap_err();
        assert_eq!(error.code(), "unsupported");

        let forgetful = BrokenStore {
            forget: true,
            ..BrokenStore::default()
        };
        let error =
            generate_release_key(&forgetful, &label("release-a"), ExistingKey::Refuse).unwrap_err();
        assert_eq!(error.code(), KEY_INVALID);

        let missing = FakeSecretStore::default();
        let error = sign(&missing, &["release-a"], &text, now).unwrap_err();
        assert_eq!(error.code(), "not_configured");
        assert_eq!(
            release_key(&missing, &label("release-a"))
                .unwrap_err()
                .code(),
            "not_configured"
        );

        let malformed = FakeSecretStore::default();
        let junk = "sk-synthetic-not-a-release-key-0123456789";
        malformed
            .set(&label("release-a").secret_name(), junk)
            .unwrap();
        let error = sign(&malformed, &["release-a"], &text, now).unwrap_err();
        assert_eq!(error.code(), KEY_INVALID);
        assert!(
            !error.to_string().contains(junk),
            "errors never echo the stored value"
        );
    }

    #[test]
    fn cancellation_signs_nothing() {
        let store = store_with(&[("release-a", SEED_A)]);
        let now = Utc::now();
        let cancel = Cancel::new();
        cancel.cancel();
        let error = sign_manifest(
            &store,
            &[label("release-a")],
            &manifest_value(now).to_string(),
            now,
            &cancel,
        )
        .unwrap_err();
        assert_eq!(error.code(), codes::CANCELLED);
    }

    #[test]
    fn manifests_the_updater_would_refuse_are_not_signed() {
        let store = store_with(&[("release-a", SEED_A), ("same-a", SEED_A)]);
        let now = Utc::now();
        let refused = |change: &dyn Fn(&mut Value)| {
            let mut manifest = manifest_value(now);
            change(&mut manifest);
            sign(&store, &["release-a"], &manifest.to_string(), now).unwrap_err()
        };

        let error = refused(&|m| m["unexpected"] = json!(true));
        assert_eq!(error.code(), codes::MANIFEST_INVALID);
        assert!(
            error.to_string().contains("unknown field `unexpected`"),
            "the parser's message is kept: {error}"
        );
        let error = refused(&|m| {
            m["published_at"] = json!((now - Duration::hours(2)).to_rfc3339());
            m["expires_at"] = json!((now - Duration::hours(1)).to_rfc3339());
        });
        assert_eq!(error.code(), codes::EXPIRED);
        let error = refused(&|m| m["artifacts"][0]["sha256"] = json!("AB".repeat(32)));
        assert_eq!(error.code(), codes::MANIFEST_INVALID);
        let error = refused(&|m| m["artifacts"][0]["url"] = json!("http://updates.example/x.zip"));
        assert_eq!(error.code(), codes::MANIFEST_INVALID);
        let error = refused(&|m| m["artifacts"][0]["entries"] = json!(0));
        assert_eq!(error.code(), codes::ARCHIVE_TOO_LARGE);
        let error = refused(&|m| m["version"] = json!("1.4"));
        assert_eq!(error.code(), codes::MANIFEST_INVALID);

        let text = manifest_value(now).to_string();
        assert_eq!(
            sign(&store, &[], &text, now).unwrap_err().code(),
            "invalid_input"
        );
        assert_eq!(
            sign(&store, &["release-a", "release-a"], &text, now)
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        assert_eq!(
            sign(&store, &["release-a", "same-a"], &text, now)
                .unwrap_err()
                .code(),
            "invalid_input",
            "one key under two labels"
        );
    }
    #[cfg(unix)]
    #[test]
    fn seed_files_are_private_exclusive_and_sign_without_a_secret_store() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.seed");
        let key = new_release_key_file(&path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        assert_eq!(metadata.len(), 32);
        assert!(new_release_key_file(&path).is_err());
        let now = Utc::now();
        let text = manifest_value(now).to_string();
        let signed = sign_manifest_file(&path, &text, now, &Cancel::new()).unwrap();
        assert_eq!(signed.signers, [key]);
        let compiled =
            crate::UpdateConfig::from_settings(&broscribe_core::config::UpdateSettings::default())
                .unwrap();
        assert!(
            verify_envelope(
                signed.envelope.as_bytes(),
                &compiled.anchors,
                &compiled.limits,
                now
            )
            .is_err()
        );
        let alias = dir.path().join("alias.seed");
        symlink(&path, &alias).unwrap();
        assert!(sign_manifest_file(&alias, &text, now, &Cancel::new()).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            sign_manifest_file(&path, &text, now, &Cancel::new())
                .unwrap_err()
                .code(),
            KEY_INVALID
        );
    }
}
