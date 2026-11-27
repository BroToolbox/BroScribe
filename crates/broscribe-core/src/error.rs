//! Error type with stable codes (used as `{"error":{"code",…}}` under `--json`).

use std::path::Path;

/// Result alias for BroScribe operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("daemon unavailable: {0}")]
    DaemonUnavailable(String),
    #[error("not configured: {0}")]
    NotConfigured(String),
    #[error("consent required: {0}")]
    ConsentRequired(String),
    #[error("sign-in required: {0}")]
    ReauthRequired(String),
    #[error("MCP is turned off in BroScribe: {0}")]
    McpDisabled(String),
    #[error("library busy: another process is writing to {0}")]
    LibraryBusy(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("Microsoft account already exists: {account_id}; reconnect that account explicitly")]
    AccountExists {
        account_id: crate::calendar::AccountId,
    },
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("model missing: {0}")]
    ModelMissing(String),
    #[error("schema error: {0}")]
    Schema(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("decode error: {0}")]
    Decode(String),
    #[error("inference error: {0}")]
    Inference(String),
    #[error("lease lost: {0}")]
    LeaseLost(String),
    /// A known service failure whose stable code must survive CLI/IPC translation.
    #[error("{message}")]
    Service { code: &'static str, message: String },
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Error::DaemonUnavailable(_) => "daemon_unavailable",
            Error::NotConfigured(_) => "not_configured",
            Error::ConsentRequired(_) => "consent_required",
            Error::ReauthRequired(_) => "reauth_required",
            Error::McpDisabled(_) => "mcp_disabled",
            Error::LibraryBusy(_) => "library_busy",
            Error::NotFound(_) => "not_found",
            Error::AlreadyExists(_) => "already_exists",
            Error::AccountExists { .. } => "account_exists",
            Error::InvalidInput(_) => "invalid_input",
            Error::InvalidManifest(_) => "invalid_manifest",
            Error::Unsupported(_) => "unsupported",
            Error::ModelMissing(_) => "model_missing",
            Error::Schema(_) => "schema",
            Error::Io { .. } => "io",
            Error::Decode(_) => "decode",
            Error::Inference(_) => "inference",
            Error::LeaseLost(_) => "lease_lost",
            Error::Service { code, .. } => code,
            Error::Internal(_) => "internal",
        }
    }

    /// I/O error with a human context such as the path involved.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// I/O error about `path`.
    pub fn io_at(path: &Path, source: std::io::Error) -> Self {
        Error::io(path.display().to_string(), source)
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::io("I/O error", source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(Error::LibraryBusy("x".into()).code(), "library_busy");
        assert_eq!(
            Error::DaemonUnavailable("x".into()).code(),
            "daemon_unavailable"
        );
        assert_eq!(Error::NotConfigured("x".into()).code(), "not_configured");
        assert_eq!(
            Error::ConsentRequired("x".into()).code(),
            "consent_required"
        );
        assert_eq!(Error::ReauthRequired("x".into()).code(), "reauth_required");
        assert_eq!(Error::ModelMissing("x".into()).code(), "model_missing");
        assert_eq!(Error::McpDisabled("x".into()).code(), "mcp_disabled");
        assert_eq!(Error::from(std::io::Error::other("x")).code(), "io");
        assert_eq!(
            Error::Service {
                code: crate::secret::TIMEOUT_CODE,
                message: "x".into()
            }
            .code(),
            "secret_store_timeout"
        );
    }
}
