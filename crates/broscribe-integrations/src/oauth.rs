//! OAuth 2.0 authorization code + PKCE and refresh-token grants.

use serde::{Deserialize, Serialize};

use crate::config::{Endpoints, IntegrationId};
use crate::encode::build_query;
use crate::http::{Http, Reply};
use crate::service::RevocationOutcome;
use crate::{IntegrationError, Result};

/// Access tokens are refreshed this long before they expire.
pub const REFRESH_MARGIN_S: i64 = 300;

/// What is stored in the `SecretStore` for one signed-in account.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
    /// Scopes granted, space separated, when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl Tokens {
    /// Usable without refreshing: more than [`REFRESH_MARGIN_S`] left.
    pub fn is_fresh(&self, now: i64) -> bool {
        self.expires_at.saturating_sub(REFRESH_MARGIN_S) > now
    }
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("expires_at", &self.expires_at)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

/// An OAuth error reply (RFC 6749 §5.2).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct OAuthFailure {
    #[serde(skip)]
    pub status: u16,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub error_codes: Vec<u32>,
    #[serde(skip)]
    pub conditional_access: bool,
}

impl OAuthFailure {
    fn from_reply(reply: &Reply) -> Self {
        let mut f: OAuthFailure = serde_json::from_slice(&reply.body).unwrap_or_default();
        f.status = reply.status;
        f.conditional_access = f.error_codes.contains(&53003)
            || serde_json::from_slice::<serde_json::Value>(&reply.body)
                .ok()
                .and_then(|body| {
                    body.get("error_description")
                        .and_then(serde_json::Value::as_str)
                        .map(|s| {
                            s.split(|c: char| !c.is_ascii_alphanumeric())
                                .any(|code| code == "AADSTS53003")
                        })
                })
                .unwrap_or(false);
        f
    }

    /// `invalid_grant` and the OpenID interaction errors: only a new
    /// interactive sign-in helps.
    fn needs_interaction(&self) -> bool {
        matches!(
            self.error.as_str(),
            "invalid_grant" | "interaction_required" | "login_required" | "consent_required"
        )
    }

    fn summary(&self) -> String {
        // Provider descriptions may echo credentials; never surface their body.
        format!("request rejected (HTTP {})", self.status)
    }
}

pub(crate) struct OAuthClient {
    pub id: IntegrationId,
    pub client_id: String,
    client_secret: Option<String>,
    pub endpoints: Endpoints,
    pub scopes: Vec<String>,
    http: Http,
}

impl OAuthClient {
    pub fn new(
        id: IntegrationId,
        client_id: String,
        client_secret: Option<String>,
        endpoints: Endpoints,
        scopes: Vec<String>,
        http: Http,
    ) -> Self {
        Self {
            id,
            client_id,
            client_secret,
            endpoints,
            scopes,
            http,
        }
    }

    fn scope(&self) -> String {
        self.scopes.join(" ")
    }

    /// A client secret is stored for this client (Google desktop clients need one).
    pub fn has_client_secret(&self) -> bool {
        self.client_secret.is_some()
    }

    /// The URL opened in the default browser (code flow, PKCE S256).
    pub fn authorize_url(&self, redirect_uri: &str, state: &str, challenge: &str) -> String {
        let scope = self.scope();
        let mut q = vec![
            ("response_type", "code"),
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", redirect_uri),
            ("scope", scope.as_str()),
            ("state", state),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ];
        if self.id == IntegrationId::Google {
            // A refresh token is issued only with offline access; `consent`
            // makes Google issue a new one when the user signs in again.
            q.extend([("access_type", "offline"), ("prompt", "consent")]);
        }
        let sep = if self.endpoints.authorize.contains('?') {
            '&'
        } else {
            '?'
        };
        format!("{}{sep}{}", self.endpoints.authorize, build_query(&q))
    }

    fn with_secret<'a>(&'a self, mut form: Vec<(&'a str, &'a str)>) -> Vec<(&'a str, &'a str)> {
        form.push(("client_id", self.client_id.as_str()));
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.as_str()));
        }
        form
    }

    /// Posts to the token endpoint: `Ok(Err(_))` is an OAuth error reply.
    fn token_request(
        &self,
        form: Vec<(&str, &str)>,
    ) -> Result<std::result::Result<Tokens, OAuthFailure>> {
        let reply = self
            .http
            .post_form(&self.endpoints.token, &self.with_secret(form))?;
        if !reply.ok() {
            if reply.is_transient() {
                return Err(IntegrationError::Transient(format!(
                    "token endpoint returned HTTP {}",
                    reply.status
                )));
            }
            return Ok(Err(OAuthFailure::from_reply(&reply)));
        }
        let t: TokenReply = reply.json("token endpoint")?;
        let now = chrono::Utc::now().timestamp();
        Ok(Ok(Tokens {
            access_token: t.access_token,
            refresh_token: t.refresh_token.filter(|r| !r.is_empty()),
            expires_at: now.saturating_add(t.expires_in.unwrap_or(3600).max(0)),
            scope: t.scope,
        }))
    }

    /// Exchanges an authorization code (with its PKCE verifier).
    pub fn exchange_code(&self, code: &str, verifier: &str, redirect_uri: &str) -> Result<Tokens> {
        self.token_request(vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ])?
        .map_err(|f| {
            if f.conditional_access {
                IntegrationError::ConditionalAccessBlocked
            } else {
                IntegrationError::OAuth(f.summary())
            }
        })
    }

    /// Refresh-token grant. A rotated refresh token replaces the old one;
    /// when none is returned (Google), the old one stays valid.
    /// `invalid_grant` / `interaction_required` → [`IntegrationError::ReauthRequired`].
    pub fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        let scope = self.scope();
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        // An unknown original grant is preserved by omitting scope, never guessed from config.
        if !scope.is_empty() {
            form.push(("scope", scope.as_str()));
        }
        match self.token_request(form)? {
            Ok(mut t) => {
                if t.refresh_token.is_none() {
                    t.refresh_token = Some(refresh_token.to_string());
                }
                Ok(t)
            }
            Err(f) if f.conditional_access => Err(IntegrationError::ConditionalAccessBlocked),
            Err(f) if f.needs_interaction() => {
                tracing::info!(integration = %self.id, "refresh token rejected; user sign-in required");
                Err(IntegrationError::ReauthRequired(self.id.to_string()))
            }
            Err(f) => Err(IntegrationError::OAuth(f.summary())),
        }
    }
}

/// Asks the revocation endpoint (RFC 7009) at `url` to revoke `token`. Never fails: every
/// outcome is reported with a stable code, never the token or the provider's text.
/// Google answers 200 on success and 400 `invalid_token` for a token it no longer
/// honours; both mean the grant is revoked or going away.
pub(crate) fn revoke(
    http: &Http,
    url: &str,
    token: &str,
    timeout: std::time::Duration,
) -> RevocationOutcome {
    let failed = |code: &str| RevocationOutcome::Failed { code: code.into() };
    let reply = match http.post_form_with_timeout(url, &[("token", token)], timeout) {
        Ok(reply) => reply,
        // `trust_store_unavailable`, or `transient` for a network failure or timeout.
        Err(error) => return failed(error.code()),
    };
    match reply.status {
        200..=299 => RevocationOutcome::RevocationRequested,
        400 if OAuthFailure::from_reply(&reply).error == "invalid_token" => {
            RevocationOutcome::RevocationRequested
        }
        429 => failed("rate_limited"),
        500.. => failed("provider_unavailable"),
        _ => failed("revocation_rejected"),
    }
}
