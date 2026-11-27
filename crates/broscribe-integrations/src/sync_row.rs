//! One row of a calendar sync report, shared by the agent and the CLI's local fallback:
//! outcomes, counts and sanitized failures only, never event contents or raw provider
//! text.

use crate::{AccountCalendarSync, IntegrationError, IntegrationId, PublicError};
use broscribe_core::calendar::AccountRef;
use serde::Serialize;

/// A provider-wide calendar read (Google, system calendars) that published its copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSyncOutcome {
    Available,
}

/// One synced provider or account, serialized as the `synced` array entries:
/// `{account,generation,selection_generation,outcome,targets,routes,event_count}`,
/// `{account,error:{code,message}}`, `{integration,outcome,events}` or
/// `{integration,error:{code,message}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum SyncRow {
    /// A Microsoft account's read of its selected routes.
    Account(AccountCalendarSync),
    /// A Microsoft account that was not read (blocked, sign-in required) or whose read
    /// failed.
    AccountFailed {
        account: AccountRef,
        error: PublicError,
    },
    /// A provider-wide calendar read, with the events its published copy holds.
    Provider {
        integration: IntegrationId,
        outcome: ProviderSyncOutcome,
        events: usize,
    },
    /// A provider-wide calendar read that failed.
    ProviderFailed {
        integration: IntegrationId,
        error: PublicError,
    },
}

impl SyncRow {
    /// `account`'s failed read, with the error's sanitized code and message.
    pub fn account_failed(account: AccountRef, error: &IntegrationError) -> Self {
        Self::AccountFailed {
            account,
            error: error.public(),
        }
    }

    /// `integration`'s failed read, with the error's sanitized code and message.
    pub fn provider_failed(integration: IntegrationId, error: &IntegrationError) -> Self {
        Self::ProviderFailed {
            integration,
            error: error.public(),
        }
    }

    /// `integration`'s published read of `events` events.
    pub fn provider(integration: IntegrationId, events: usize) -> Self {
        Self::Provider {
            integration,
            outcome: ProviderSyncOutcome::Available,
            events,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_keep_the_sync_report_keys_and_sanitize_local_failures() {
        let provider = serde_json::to_value(SyncRow::provider(IntegrationId::Google, 2)).unwrap();
        assert_eq!(
            provider,
            serde_json::json!({"integration":"google","outcome":"available","events":2})
        );
        let error = IntegrationError::Core(broscribe_core::Error::io_at(
            std::path::Path::new("/synthetic/private/state.json"),
            std::io::Error::other("synthetic"),
        ));
        let failed =
            serde_json::to_value(SyncRow::provider_failed(IntegrationId::Google, &error)).unwrap();
        assert_eq!(failed["integration"], "google");
        assert_eq!(failed["error"]["code"], error.code());
        assert!(
            !failed.to_string().contains("/synthetic/private"),
            "{failed}"
        );
    }
}
