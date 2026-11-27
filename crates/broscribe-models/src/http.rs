//! The HTTP agent of model downloads, remote text embeddings, model lists and
//! HTTP LLM providers. One agent per [`crate::ExecutionContext`] serves every
//! client (connections and TLS configuration reused); certificate
//! verification is always on, against the roots the context chose.

use ureq::tls::{RootCerts, TlsConfig};

/// `User-Agent` of every request.
pub const USER_AGENT: &str = concat!("broscribe/", env!("CARGO_PKG_VERSION"));

/// The shared agent: the BroScribe user agent, `roots`, HTTP statuses returned
/// rather than raised. Callers set their timeouts (and status handling, when
/// they want errors) per request.
pub(crate) fn agent(roots: RootCerts) -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .tls_config(TlsConfig::builder().root_certs(roots).build())
        .http_status_as_error(false)
        .build()
        .into()
}
