//! Validation of the `[updates]` settings (`broscribe_core::config::UpdateSettings`):
//! checks use the built-in HTTPS feed and public keys unless overridden.

use std::fmt;

use broscribe_core::config::UpdateSettings;
use broscribe_core::{Error, Result};
use sha2::{Digest, Sha256};

/// Hard bounds on what the updater reads and writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_envelope_bytes: u64,
    pub max_signatures: usize,
    pub max_artifacts: usize,
    pub max_artifact_bytes: u64,
    pub max_installed_bytes: u64,
    pub max_entries: u64,
    pub max_path_bytes: usize,
    pub max_depth: usize,
    pub max_link_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_envelope_bytes: 256 * 1024,
            max_signatures: 8,
            max_artifacts: 16,
            max_artifact_bytes: 4 << 30,
            max_installed_bytes: 16 << 30,
            max_entries: 200_000,
            max_path_bytes: 1024,
            max_depth: 64,
            max_link_bytes: 1024,
        }
    }
}

/// The first 8 bytes of the SHA-256 of a raw Ed25519 public key, shown as
/// 16 lowercase hex digits. Envelopes name the key that signed them by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyId(pub [u8; 8]);

impl KeyId {
    pub fn of(public_key: &[u8]) -> Self {
        let digest = Sha256::digest(public_key);
        let mut id = [0; 8];
        id.copy_from_slice(&digest[..8]);
        Self(id)
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex(&self.0))
    }
}

/// Pinned Ed25519 public keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustAnchors {
    keys: Vec<(KeyId, [u8; 32])>,
}

impl TrustAnchors {
    /// Parses lowercase hex public keys; empty, malformed or duplicate keys
    /// are refused.
    pub fn from_hex(keys: &[String]) -> Result<Self> {
        if keys.is_empty() {
            return Err(Error::NotConfigured(
                "updates need at least one trusted release key".into(),
            ));
        }
        let mut out: Vec<(KeyId, [u8; 32])> = Vec::with_capacity(keys.len());
        for key in keys {
            let bytes: [u8; 32] = unhex(key)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| {
                    Error::InvalidInput(
                        "a trusted release key is not 64 lowercase hex digits".into(),
                    )
                })?;
            let id = KeyId::of(&bytes);
            if out.iter().any(|(known, _)| *known == id) {
                return Err(Error::InvalidInput(format!(
                    "trusted release key {id} is listed twice"
                )));
            }
            out.push((id, bytes));
        }
        Ok(Self { keys: out })
    }

    pub(crate) fn get(&self, id: &KeyId) -> Option<&[u8; 32]> {
        self.keys
            .iter()
            .find_map(|(known, key)| (known == id).then_some(key))
    }

    pub fn key_ids(&self) -> impl Iterator<Item = KeyId> + '_ {
        self.keys.iter().map(|(id, _)| *id)
    }

    /// The identity of a feed's verification policy, independent of key order.
    pub(crate) fn feed_scope(&self, url: &str, channel: &str) -> String {
        let mut keys: Vec<_> = self.keys.iter().map(|(_, key)| key).collect();
        keys.sort_unstable();
        let mut digest = Sha256::new();
        digest.update(url.as_bytes());
        digest.update([0]);
        digest.update(channel.as_bytes());
        digest.update([0]);
        for key in keys {
            digest.update(key);
        }
        hex(&digest.finalize())
    }
}

/// Validated settings.
#[derive(Clone, Debug)]
pub struct UpdateConfig {
    pub feed_url: String,
    pub channel: String,
    pub anchors: TrustAnchors,
    pub signer: Option<String>,
    pub limits: Limits,
}

impl UpdateConfig {
    /// Refuses a disabled or incomplete configuration with
    /// [`Error::NotConfigured`] and malformed values with
    /// [`Error::InvalidInput`].
    pub fn from_settings(settings: &UpdateSettings) -> Result<Self> {
        if !settings.enabled {
            return Err(Error::NotConfigured("automatic updates are off".into()));
        }
        let feed_url = settings.effective_feed().to_owned();
        check_https_url(&feed_url)?;
        if !valid_channel(&settings.channel) {
            return Err(Error::InvalidInput(
                "the update channel must be 1-32 lowercase letters, digits or '-'".into(),
            ));
        }
        if let Some(signer) = &settings.signer
            && (signer.is_empty()
                || signer.len() > 128
                || signer.chars().any(|c| c.is_control() || c == '"'))
        {
            return Err(Error::InvalidInput(
                "the pinned update signer is malformed".into(),
            ));
        }
        Ok(Self {
            feed_url,
            channel: settings.channel.clone(),
            anchors: TrustAnchors::from_hex(
                &settings
                    .effective_keys()
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )?,
            signer: settings.signer.clone(),
            limits: Limits::default(),
        })
    }
}

pub(crate) fn valid_channel(channel: &str) -> bool {
    (1..=32).contains(&channel.len())
        && channel
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Accepts only `https://host[:port]/path[?query]` without credentials,
/// fragments, whitespace or control characters.
pub(crate) fn check_https_url(url: &str) -> Result<()> {
    let refuse = |why: &str| Err(Error::InvalidInput(format!("update URL refused ({why})")));
    if url.len() > 2048 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return refuse("too long, whitespace or control characters");
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return refuse("only https is allowed");
    };
    if url.contains('#') {
        return refuse("fragment");
    }
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    if authority.contains('@') || authority.contains('\\') {
        return refuse("credentials or backslash in the authority");
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, port)| {
            if port.bytes().all(|b| b.is_ascii_digit()) && !port.is_empty() {
                host
            } else {
                authority
            }
        });
    if host.is_empty() {
        return refuse("missing host");
    }
    Ok(())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 15)] as char);
    }
    out
}

/// Lowercase hex only, so one value has one spelling.
pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_defaults_and_explicit_overrides_are_validated() {
        let mut settings = UpdateSettings::default();
        let built_in = UpdateConfig::from_settings(&settings).unwrap();
        assert_eq!(built_in.feed_url, broscribe_core::config::RELEASE_FEED);
        assert!(built_in.anchors.key_ids().next().is_some());
        settings.enabled = false;
        assert_eq!(
            UpdateConfig::from_settings(&settings).unwrap_err().code(),
            "not_configured"
        );
        settings.enabled = true;
        settings.feed_url = Some("https://example.org/feed.json".into());
        assert_eq!(
            UpdateConfig::from_settings(&settings).unwrap_err().code(),
            "not_configured",
            "no trusted key"
        );
        settings.trusted_keys = vec!["AB".repeat(32)];
        assert_eq!(
            UpdateConfig::from_settings(&settings).unwrap_err().code(),
            "invalid_input",
            "uppercase hex"
        );
        settings.trusted_keys = vec!["ab".repeat(32), "ab".repeat(32)];
        assert!(
            UpdateConfig::from_settings(&settings).is_err(),
            "duplicate key"
        );
        settings.trusted_keys = vec!["ab".repeat(32)];
        assert!(UpdateConfig::from_settings(&settings).is_ok());
    }

    #[test]
    fn only_plain_https_feeds() {
        for ok in [
            "https://example.org/feed.json",
            "https://example.org:8443/a?b=c",
        ] {
            assert!(check_https_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.org/feed.json",
            "HTTPS://example.org/x",
            "https://user@example.org/x",
            "https:///x",
            "https://example.org/x#y",
            "https://example.org/a b",
            "file:///etc/passwd",
            "https://:443/x",
        ] {
            assert!(check_https_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_core_updates_table_is_what_gets_validated() {
        use broscribe_core::config::Config;

        let dir = tempfile::tempdir().unwrap();
        let defaults = Config::load(dir.path()).unwrap();
        assert_eq!(
            UpdateConfig::from_settings(&defaults.updates)
                .unwrap()
                .feed_url,
            broscribe_core::config::RELEASE_FEED
        );
        std::fs::write(
            Config::path(dir.path()),
            format!(
                "[updates]\nenabled = true\nfeed_url = 'https://example.org/feed.json'\n\
                 trusted_keys = ['{}']\n",
                "ab".repeat(32)
            ),
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        let validated = UpdateConfig::from_settings(&config.updates).unwrap();
        assert_eq!(validated.feed_url, "https://example.org/feed.json");
        assert_eq!(validated.channel, "stable");
    }
}
