//! `SecretStore` on the macOS Keychain: generic passwords under one service
//! per data folder (`org.broscribe.BroScribe` for the default one,
//! `org.broscribe.BroScribe.profile-<hash>` for others), account = key.
//! Values are never logged or put in error messages.

use std::path::Path;

use broscribe_platform::{Error, Result, SecretStore};
use security_framework::passwords::{
    PasswordOptions, delete_generic_password, generic_password, set_generic_password,
};

/// The default data folder's service; other profiles append `.profile-<hash>`.
pub const KEYCHAIN_SERVICE: &str = "org.broscribe.BroScribe";

/// A data folder's identity for its secret namespace: the
/// [`broscribe_platform::stable_path`] bytes, unfolded, since `canonicalize`
/// returns the on-disk case of every existing component. A not-yet-created
/// tail keeps the caller's spelling, so on a case-insensitive volume two
/// spellings of a folder that does not exist yet differ until it is created
/// (a first-run-only edge).
pub fn profile_identity(data_dir: &Path) -> Vec<u8> {
    broscribe_platform::stable_path(data_dir)
        .into_os_string()
        .into_encoded_bytes()
}

/// `errSecItemNotFound`.
const ITEM_NOT_FOUND: i32 = -25300;

pub struct MacKeychain {
    service: String,
}

impl MacKeychain {
    /// The store of one data folder's namespace (`secret_namespace`); tests
    /// use `.test` services so they never touch real entries.
    pub fn with_service(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }
}

fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.chars().any(char::is_control) {
        return Err(Error::InvalidInput("invalid secret key".into()));
    }
    Ok(())
}

fn failure(op: &str, key: &str, e: security_framework::base::Error) -> Error {
    Error::Internal(format!(
        "keychain {op} of {key} failed (OSStatus {}{})",
        e.code(),
        e.message().map(|m| format!(": {m}")).unwrap_or_default()
    ))
}

impl SecretStore for MacKeychain {
    fn get(&self, key: &str) -> Result<Option<String>> {
        check_key(key)?;
        match generic_password(PasswordOptions::new_generic_password(&self.service, key)) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| Error::Internal(format!("keychain item {key} is not UTF-8"))),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(failure("read", key, e)),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        check_key(key)?;
        set_generic_password(&self.service, key, value.as_bytes())
            .map_err(|e| failure("write", key, e))
    }

    fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        match delete_generic_password(&self.service, key) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(failure("delete", key, e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes to the login keychain (may prompt); run explicitly.
    #[test]
    #[ignore = "touches the user's login keychain"]
    fn round_trip() {
        let kc = MacKeychain::with_service("org.broscribe.BroScribe.test");
        let key = format!("provider/test-{}/api_key", std::process::id());
        kc.delete(&key).unwrap();
        assert_eq!(kc.get(&key).unwrap(), None);
        kc.set(&key, "synthetic-value-1").unwrap();
        assert_eq!(kc.get(&key).unwrap().as_deref(), Some("synthetic-value-1"));
        kc.set(&key, "synthetic-value-2").unwrap();
        assert_eq!(kc.get(&key).unwrap().as_deref(), Some("synthetic-value-2"));
        kc.delete(&key).unwrap();
        assert_eq!(kc.get(&key).unwrap(), None);
        kc.delete(&key).unwrap();
    }

    /// Writes to the login keychain (may prompt); run explicitly.
    #[test]
    #[ignore = "touches the user's login keychain"]
    fn services_isolate_the_same_key() {
        let one = MacKeychain::with_service("org.broscribe.BroScribe.test.profile-one");
        let two = MacKeychain::with_service("org.broscribe.BroScribe.test.profile-two");
        let key = format!("provider/isolation-{}/api_key", std::process::id());
        one.set(&key, "synthetic-one").unwrap();
        assert_eq!(two.get(&key).unwrap(), None);
        two.set(&key, "synthetic-two").unwrap();
        assert_eq!(one.get(&key).unwrap().as_deref(), Some("synthetic-one"));
        two.delete(&key).unwrap();
        assert_eq!(one.get(&key).unwrap().as_deref(), Some("synthetic-one"));
        assert_eq!(two.get(&key).unwrap(), None);
        one.delete(&key).unwrap();
        assert_eq!(one.get(&key).unwrap(), None);
    }

    #[test]
    fn rejects_bad_keys() {
        let kc = MacKeychain::with_service("org.broscribe.BroScribe.test");
        assert!(kc.get("").is_err());
        assert!(kc.set("a\nb", "x").is_err());
    }
}
