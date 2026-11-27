//! PKCE S256 (RFC 7636), the OAuth `state` value and constant-time comparison.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// A code verifier (32 random bytes, 43 base64url characters) and its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Self {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes[..]);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce").finish_non_exhaustive()
    }
}

/// `BASE64URL(SHA256(verifier))` without padding.
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(&Sha256::digest(verifier.as_bytes())[..])
}

/// A 128-bit random `state` value, base64url.
pub fn new_state() -> String {
    let mut bytes = [0u8; 16];
    rand::fill(&mut bytes[..]);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Compares two byte strings in time independent of where they differ.
/// Lengths are not secret (state values have a fixed length).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a
        .iter()
        .zip(b)
        .fold(0u8, |acc, (x, y)| acc | std::hint::black_box(x ^ y));
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_appendix_b() {
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_and_state_shape() {
        let p = Pkce::new();
        assert_eq!(p.verifier.len(), 43);
        assert_eq!(p.challenge, challenge_for(&p.verifier));
        assert_ne!(Pkce::new().verifier, p.verifier);
        let s = new_state();
        assert_eq!(s.len(), 22);
        assert_ne!(new_state(), s);
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }
}
