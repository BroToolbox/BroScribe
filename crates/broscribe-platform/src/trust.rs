//! Platform-selected HTTPS verification. Native verification retains OS trust
//! policy; explicit certificate overrides and fakes use an exact root snapshot.

use crate::Result;

/// The verification strategy and its initial availability evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TlsTrust {
    /// Use the OS verifier. `roots` is the initial probe's count, not the
    /// immutable trust set subsequently consulted by the OS.
    Platform { roots: usize },
    /// Verify against exactly these DER anchors, with no bundled-root fallback.
    Certificates(Vec<Vec<u8>>),
}

impl TlsTrust {
    pub fn root_count(&self) -> usize {
        match self {
            Self::Platform { roots } => *roots,
            Self::Certificates(certs) => certs.len(),
        }
    }
}

/// The TLS strategy HTTPS clients must use.
pub trait TrustStore: Send + Sync {
    /// An unreadable store or empty strategy is an error. Consumers fail
    /// closed before connecting and never disable certificate verification.
    /// Native evaluation failures must not fall back to a weaker strategy.
    fn tls_trust(&self) -> Result<TlsTrust>;
}
