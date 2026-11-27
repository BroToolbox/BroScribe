//! Contract shared by the platform credential stores (`SecretStore`) and the
//! callers that translate their errors.

/// Stable code of a credential-store call the platform did not answer in
/// time, typically a locked keyring waiting on its unlock prompt. It keeps
/// its identity through IPC and the CLI.
pub const TIMEOUT_CODE: &str = "secret_store_timeout";
