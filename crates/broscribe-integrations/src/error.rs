//! Integration errors with stable codes (`{"error":{"code",…}}` under `--json`).
//! Messages never contain tokens, codes or verifiers.

/// Result alias for integration operations.
pub type Result<T, E = IntegrationError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    /// No client ID (or another required setting) in `config.toml`.
    #[error("{0} is not configured: {1}")]
    NotConfigured(String, String),
    #[error("{0}: not signed in (run `broscribe integrations login {0}`)")]
    SignedOut(String),
    /// The refresh token was rejected or the provider asks for interaction;
    /// only a user-started sign-in can fix it.
    #[error("{0}: sign-in required (run `broscribe integrations login {0}`)")]
    ReauthRequired(String),
    /// The user cancelled or the provider refused the sign-in.
    #[error("sign-in was not completed: {0}")]
    Denied(String),
    #[error("sign-in timed out")]
    Timeout,
    /// The service refused one capability (e.g. directory access denied).
    #[error("{0}: access denied by the service")]
    Forbidden(String),
    #[error("OAuth error: {0}")]
    OAuth(String),
    #[error("service error: {0}")]
    Service(String),
    /// Network failure, timeout, 429 or 5xx.
    #[error("network error: {0}")]
    Transient(String),
    /// The account was signed out or replaced while the request ran; its
    /// result was discarded.
    #[error("{0}: account changed while the request was running")]
    Superseded(String),
    #[error("account already exists: {account_id}")]
    AccountExists {
        account_id: broscribe_core::calendar::AccountId,
    },
    #[error(
        "Microsoft Conditional Access blocked this request. Ask your organisation's administrator to inspect the sign-in correlation logs; device-registration status alone does not establish the cause. BroScribe will not retry with another client or bypass the policy."
    )]
    ConditionalAccessBlocked,
    #[error(transparent)]
    Core(#[from] broscribe_core::Error),
}

impl IntegrationError {
    /// Stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured(..) => "not_configured",
            Self::SignedOut(_) => "signed_out",
            Self::ReauthRequired(_) => "reauth_required",
            Self::Denied(_) => "denied",
            Self::Timeout => "timeout",
            Self::Forbidden(_) => "forbidden",
            Self::OAuth(_) => "oauth",
            Self::Service(_) => "service",
            Self::Transient(_) => "transient",
            Self::Superseded(_) => "superseded",
            Self::AccountExists { .. } => "account_exists",
            Self::ConditionalAccessBlocked => "conditional_access_blocked",
            Self::Core(e) => e.code(),
        }
    }

    pub(crate) fn io(context: &std::path::Path, e: std::io::Error) -> Self {
        Self::Core(broscribe_core::Error::io_at(context, e))
    }
}

/// A failure's stable code and a message safe to show, log, persist or send over IPC:
/// never raw provider text, tokens, codes, verifiers or local paths.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublicError {
    pub code: String,
    pub message: String,
}

impl IntegrationError {
    /// The sanitized form of this error. Provider failures are already reduced to fixed
    /// text where they are classified; local storage failures, whose text names files,
    /// keep only their code.
    pub fn public(&self) -> PublicError {
        use broscribe_core::Error as Core;
        let message = match self {
            Self::Core(Core::Io { .. } | Core::Schema(_)) => {
                "BroScribe could not read or write its private integration data".into()
            }
            Self::Core(Core::LibraryBusy(_)) => {
                "another BroScribe process is updating this integration; try again".into()
            }
            Self::Core(Core::Internal(_)) => "an internal integration error occurred".into(),
            other => other.to_string(),
        };
        PublicError {
            code: self.code().to_owned(),
            message,
        }
    }
}
