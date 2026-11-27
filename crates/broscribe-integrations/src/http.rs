//! Blocking HTTP over `ureq`. Errors name the failure kind only: ureq
//! messages can carry URLs, and bodies of token requests carry secrets.

use std::time::Duration;

use crate::{IntegrationError, Result};

const TIMEOUT: Duration = Duration::from_secs(30);
const BODY_LIMIT: u64 = 16 << 20;

/// A response with any status (HTTP errors are not transport errors here).
pub(crate) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self, what: &str) -> Result<T> {
        serde_json::from_slice(&self.body)
            .map_err(|_| IntegrationError::Service(format!("{what}: unexpected reply")))
    }

    /// 429 and 5xx: worth one retry.
    pub fn is_transient(&self) -> bool {
        self.status == 429 || self.status >= 500
    }
}

/// A provider transport. A failed availability probe leaves `agent` unavailable
/// before connection, with no bundled-root fallback. Later native verifier
/// initialization/evaluation errors remain sanitized transfer failures.
/// Local calendars, removal and plain sign-out never use this transport; only an
/// explicit revoking sign-out does, after the local sign-out is done.
#[derive(Clone)]
pub(crate) struct Http {
    agent: Option<ureq::Agent>,
}

impl Http {
    /// Verifies TLS against the bundled public (WebPKI) roots.
    pub fn new() -> Self {
        Self::with_tls(ureq::tls::TlsConfig::default())
    }

    /// Use the platform-selected strategy, or an exact injected root snapshot.
    /// An unreadable or empty strategy leaves the transport unavailable.
    pub fn from_trust_store(store: &dyn broscribe_platform::TrustStore) -> Self {
        match store.tls_trust() {
            Ok(policy) if policy.root_count() != 0 => {
                let roots = match policy {
                    broscribe_platform::TlsTrust::Platform { .. } => {
                        ureq::tls::RootCerts::PlatformVerifier
                    }
                    broscribe_platform::TlsTrust::Certificates(roots) => {
                        ureq::tls::RootCerts::from(
                            roots
                                .iter()
                                .map(|der| ureq::tls::Certificate::from_der(der).to_owned()),
                        )
                    }
                };
                Self::with_tls(ureq::tls::TlsConfig::builder().root_certs(roots).build())
            }
            Ok(_) => {
                tracing::warn!("system trust store has no roots; provider connections are off");
                Self { agent: None }
            }
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    "system trust store unreadable; provider connections are off"
                );
                Self { agent: None }
            }
        }
    }

    fn with_tls(tls: ureq::tls::TlsConfig) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .max_redirects(0)
            .tls_config(tls)
            .user_agent(concat!("broscribe/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent: Some(agent) }
    }

    /// The transport, or `trust_store_unavailable` when no trust anchors could be read.
    /// Callers check it before user-visible steps (opening a sign-in browser).
    pub fn ready(&self) -> Result<&ureq::Agent> {
        self.agent.as_ref().ok_or_else(|| {
            crate::service::service_error(
                "trust_store_unavailable",
                "the system trust store could not be read, so provider connections are off; local calendars and sign-out still work",
            )
        })
    }

    /// `application/x-www-form-urlencoded` POST (token and device endpoints).
    pub fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<Reply> {
        let resp = self
            .ready()?
            .post(url)
            .header("Accept", "application/json")
            .send_form(form.iter().copied());
        read(resp)
    }

    /// [`Self::post_form`] under a shorter whole-request `timeout`. ureq's timers do not
    /// bound native (Apple `SecTrust`) certificate evaluation, so this is not a hard limit.
    pub fn post_form_with_timeout(
        &self,
        url: &str,
        form: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Reply> {
        let resp = self
            .ready()?
            .post(url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .header("Accept", "application/json")
            .send_form(form.iter().copied());
        read(resp)
    }

    /// GET with an optional bearer token, query pairs and extra headers.
    pub fn get(
        &self,
        url: &str,
        bearer: Option<&str>,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Result<Reply> {
        let mut req = self.ready()?.get(url).query_pairs(query.iter().copied());
        if let Some(token) = bearer {
            req = req.header("Authorization", &format!("Bearer {token}"));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        read(req.call())
    }
}

fn read(resp: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Reply> {
    let mut resp = resp.map_err(|e| IntegrationError::Transient(error_kind(&e).into()))?;
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_vec()
        .map_err(|e| {
            IntegrationError::Transient(format!("reading the reply: {}", error_kind(&e)))
        })?;
    Ok(Reply { status, body })
}

fn error_kind(e: &ureq::Error) -> &'static str {
    match e {
        ureq::Error::Timeout(_) => "timed out",
        ureq::Error::Io(_) => "connection error",
        ureq::Error::HostNotFound => "host not found",
        ureq::Error::BodyExceedsLimit(_) => "reply too large",
        _ => "HTTP error",
    }
}
