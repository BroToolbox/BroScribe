//! What model code needs from the platform to run outside the process safely:
//! containment of every child process tree it starts (agent CLI calls, their
//! `--version`/`--help`/feature probes and model listings: descendants end
//! with the call on success, timeout, cancellation and a failed prompt
//! write, on every OS), and the TLS policy of every HTTPS request
//! (HTTP LLM calls, remote embeddings, model lists, model downloads).
//!
//! Entry points build one context from the native platform; tests pass
//! explicit fakes. There is no default. The strategy and initial availability
//! probe are cached on first HTTP use. A failed probe refuses HTTPS before
//! connecting; no other roots are substituted. Native verification can consult
//! changing OS trust policy and perform synchronous OS retrieval outside socket
//! deadlines. Later native failures are TLS errors, never a weaker retry.
//! Agent CLIs, loopback HTTP, mirror pulls and local inference keep working.
//! Certificate verification is never disabled.

use std::sync::{Arc, OnceLock};

use broscribe_core::{Error, Result};
use broscribe_platform::{ProcessTreeControl, TlsTrust, TrustStore};
use serde::Serialize;
use ureq::tls::{Certificate, RootCerts};

/// Stable code of HTTPS work refused because the OS trust store gave no
/// usable roots (the same code integrations and updates use).
pub const TRUST_STORE_UNAVAILABLE: &str = "trust_store_unavailable";

/// Process-tree containment and TLS trust for model work. Cheap to clone;
/// clones share the trust choice and the HTTP agent (its connection pool and
/// TLS configuration are reused by every request of every client).
#[derive(Clone)]
pub struct ExecutionContext {
    inner: Arc<Inner>,
}

struct Inner {
    process: Arc<dyn ProcessTreeControl>,
    trust: Arc<dyn TrustStore>,
    http: OnceLock<Http>,
}

/// The trust choice, made once.
struct Http {
    agent: ureq::Agent,
    state: TlsState,
}

/// What HTTPS requests are verified against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TlsState {
    /// Native OS policy; `roots` is the initial probe count, not a frozen trust set.
    Platform { roots: usize },
    /// An exact snapshot of system or explicitly overridden roots.
    System { roots: usize },
    /// The store could not be read or held no roots: HTTPS is refused.
    Unavailable { reason: String },
}

impl std::fmt::Debug for ExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionContext")
            .field("tls", &self.inner.http.get().map(|http| &http.state))
            .finish_non_exhaustive()
    }
}

impl ExecutionContext {
    /// `process` contains child trees (`NativePlatform::process_tree_control`);
    /// `trust` supplies the roots (`NativePlatform::trust_store`), read on
    /// first HTTP use.
    pub fn new(process: Arc<dyn ProcessTreeControl>, trust: Arc<dyn TrustStore>) -> Self {
        Self {
            inner: Arc::new(Inner {
                process,
                trust,
                http: OnceLock::new(),
            }),
        }
    }

    /// The containment every child process of model code goes through.
    pub fn process_tree(&self) -> &dyn ProcessTreeControl {
        self.inner.process.as_ref()
    }

    /// The platform store shared with integrations; no credential material is exposed.
    pub fn trust_store(&self) -> &dyn TrustStore {
        self.inner.trust.as_ref()
    }

    /// The trust choice (reads the OS store on first use).
    pub fn tls_state(&self) -> &TlsState {
        &self.http().state
    }

    fn http(&self) -> &Http {
        self.inner
            .http
            .get_or_init(|| Http::choose(self.inner.trust.as_ref()))
    }

    /// The shared agent for a request to `url`: HTTP statuses are returned,
    /// not raised, and callers set their timeouts per request. Anything but
    /// plain `http://` (loopback only, checked by the callers' URL rules)
    /// needs the OS roots; without them it fails here, before connecting.
    pub(crate) fn http_agent(&self, url: &str) -> Result<&ureq::Agent> {
        let http = self.http();
        let plain = url
            .get(..7)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"));
        match &http.state {
            TlsState::Unavailable { reason } if !plain => Err(Error::Service {
                code: TRUST_STORE_UNAVAILABLE,
                message: format!(
                    "the system trust store is unavailable ({reason}), so HTTPS connections are off; local models and agent CLIs still work"
                ),
            }),
            _ => Ok(&http.agent),
        }
    }
}

impl Http {
    fn choose(trust: &dyn TrustStore) -> Self {
        let (roots, state) = match trust.tls_trust() {
            Ok(TlsTrust::Platform { roots }) if roots != 0 => {
                (RootCerts::PlatformVerifier, TlsState::Platform { roots })
            }
            Ok(TlsTrust::Certificates(der)) if !der.is_empty() => {
                let state = TlsState::System { roots: der.len() };
                let roots = RootCerts::from(
                    der.iter()
                        .map(|cert| Certificate::from_der(cert).to_owned()),
                );
                (roots, state)
            }
            outcome => {
                let reason = match outcome {
                    Err(e) => {
                        tracing::warn!(
                            code = e.code(),
                            "system trust store unreadable; HTTPS model connections are off"
                        );
                        e.code().to_owned()
                    }
                    Ok(_) => {
                        tracing::warn!(
                            "system trust store has no roots; HTTPS model connections are off"
                        );
                        "no root certificates".to_owned()
                    }
                };
                // Trusts nothing: HTTPS is refused before connecting anyway,
                // and a redirect off loopback cannot reach bundled roots.
                (
                    RootCerts::Specific(Arc::new(Vec::new())),
                    TlsState::Unavailable { reason },
                )
            }
        };
        Self {
            agent: crate::http::agent(roots),
            state,
        }
    }
}

/// A context for unit tests: synthetic process-tree control (leader only,
/// counted) and a trust store without roots (HTTPS refused).
#[cfg(test)]
pub(crate) fn test_context() -> (
    Arc<broscribe_platform::fakes::FakeProcessTreeControl>,
    ExecutionContext,
) {
    let tree = Arc::new(broscribe_platform::fakes::FakeProcessTreeControl::default());
    let context = ExecutionContext::new(
        tree.clone(),
        Arc::new(broscribe_platform::fakes::FakeTrustStore::default()),
    );
    (tree, context)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use broscribe_platform::fakes::{FakeProcessTreeControl, FakeTrustStore};

    use super::*;

    fn context(roots: Option<Vec<Vec<u8>>>) -> (Arc<FakeTrustStore>, ExecutionContext) {
        let trust = Arc::new(FakeTrustStore::default());
        *trust.policy.lock().unwrap() = roots.map(TlsTrust::Certificates);
        let context =
            ExecutionContext::new(Arc::new(FakeProcessTreeControl::default()), trust.clone());
        (trust, context)
    }

    #[test]
    fn system_roots_are_used_verbatim_with_verification_on() {
        let der = vec![0x30, 0x03, 0x02, 0x01, 0x01];
        let (trust, context) = context(Some(vec![der.clone()]));
        assert_eq!(trust.reads.load(Ordering::SeqCst), 0, "read on first use");
        assert_eq!(context.tls_state(), &TlsState::System { roots: 1 });
        let agent = context.http_agent("https://api.example.org/v1").unwrap();
        let tls = agent.config().tls_config();
        assert!(!tls.disable_verification());
        match tls.root_certs() {
            RootCerts::Specific(certs) => {
                assert_eq!(certs.len(), 1);
                assert_eq!(certs[0].der(), der.as_slice());
            }
            _ => panic!("the OS roots must be the specific DER roots"),
        }
        // One choice and one agent for every client of the context.
        let clone = context.clone();
        assert!(clone.http_agent("http://127.0.0.1:1/v1").is_ok());
        assert_eq!(trust.reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unavailable_store_refuses_https_before_connecting_and_keeps_loopback() {
        for roots in [None, Some(Vec::new())] {
            let (trust, context) = context(roots);
            for url in [
                "https://api.example.org/v1/models",
                "HTTPS://api.example.org/v1",
                "ftp://example.org/x",
            ] {
                let err = context.http_agent(url).unwrap_err();
                assert_eq!(err.code(), TRUST_STORE_UNAVAILABLE, "{url}");
            }
            assert!(matches!(context.tls_state(), TlsState::Unavailable { .. }));
            let agent = context.http_agent("http://127.0.0.1:11434/v1").unwrap();
            let tls = agent.config().tls_config();
            assert!(!tls.disable_verification());
            assert!(
                matches!(tls.root_certs(), RootCerts::Specific(certs) if certs.is_empty()),
                "never the bundled roots after the OS store failed"
            );
            assert_eq!(trust.reads.load(Ordering::SeqCst), 1, "read once");
        }
    }
}
