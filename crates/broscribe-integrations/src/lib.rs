//! Opt-in calendar and directory integrations. Blocking operations belong on the caller's
//! worker pool, outside any library lock. Never send tokens or cached photos over MCP.
//!
//! OAuth client IDs come from `[integration.<name>]`. After editing a client ID, call
//! [`Integrations::retire_changed_client`]. Credentials live only in `SecretStore`.
//! The CLI/daemon should share this facade over IPC rather than perform parallel logins.
//! Logout removes account tokens and caches, not the configured Google client secret;
//! [`Integrations::clear_client_secret`] removes that once signed out. Logout is local: the
//! grant stays live at Google unless the user explicitly chooses
//! [`Integrations::logout_and_revoke`], which revokes it for the whole OAuth project.
//! Replacement sign-in keeps the existing account until authorization succeeds. API 401s
//! refresh and retry once before requiring sign-in; Google rate limits remain transient.
//! Google reads only the calendars explicitly chosen with
//! [`Integrations::configure_google_calendars`]; its events carry the account and selection
//! generations that [`Integrations::google_authority`] must still answer. A selected
//! calendar Google no longer lets the account read leaves the selection durably, even when
//! the sync then fails or its token refresh does; only an explicit selection reads it again.
//! Provider TLS verifies against bundled public roots unless the caller injects the OS trust
//! store with [`Integrations::with_trust_store`]; verification is never disabled. An
//! unreadable store turns off provider traffic only (`trust_store_unavailable`); native
//! calendars, sign-out and account removal keep working (an explicit revocation at Google
//! then reports `trust_store_unavailable`).
//! Standalone CLI callbacks that only print must use
//! [`with_reauth_notification_delivery(false)`](Integrations::with_reauth_notification_delivery)
//! so daemon notification stays pending.
//! Every interactive OAuth sign-in runs under a caller-named [`LoginAttemptId`]; any
//! process may [`cancel`](Integrations::cancel_login) it or read its terminal
//! [result](Integrations::login_attempt). A cancelled attempt never commits; a process
//! that ends is not presumed cancelled.
//! Real tenant consent and MFA need owner verification.
//!
//! Native calendars need no client ID or tokens: only the installed helper injects
//! [`Integrations::with_system_calendar`] (EventKit). Its explicit opt-in, selection and
//! generation gate reads. Elsewhere connect, configure and sync report `helper_required`,
//! while disconnect always works offline.

mod backend;
pub mod calendar;
pub mod config;
mod encode;
mod error;
mod http;
pub mod loopback;
mod oauth;
pub mod pkce;
mod service;
mod store;
mod sync_row;

pub use service::{
    AccountCalendarSync, AccountDirectoryResult, AccountDirectorySource, AccountPermission,
    AccountSyncOutcome, CalendarDescriptor, CalendarListError, CalendarRouteRead,
    CalendarTargetOutcome, CalendarTargetRemoval, CalendarTargetResult, ClientChange,
    GoogleCalendarCatalog, GoogleSignOut, IntegrationStatus, Integrations, LegacyMicrosoftStatus,
    LoginAttemptId, LoginAttemptState, LoginAttemptStatus, MicrosoftAccount,
    MicrosoftCalendarCatalog, ReauthNotification, RevocationOutcome, RouteCopy, SetupInstructions,
    SystemCalendarStatus, VerifiedMicrosoftIdentity, integration_id, setup,
};
pub use sync_row::{ProviderSyncOutcome, SyncRow};

pub use backend::{DirectoryPerson, GoogleCalendar};
pub use broscribe_core::calendar::{
    AccountAuthority, AccountId, AccountRef, CalendarTarget, GoogleCalendarStamp,
    RemoteCalendarStamp, RemoteIntegration,
};
pub use calendar::{
    Attendee, CalendarCache, CalendarEvent, CalendarParticipant, CalendarPrior, TitleSource,
};
pub use config::{
    Endpoints, GraphDirectory, IntegrationId, IntegrationSettings, IntegrationsConfig,
    InvalidTables, SignInFlow,
};
pub use error::{IntegrationError, PublicError, Result};
pub use store::{
    AccountInfo, AccountState, AccountStatus, Capabilities, Capability, PhotoPurge,
    PhotoPurgeFailure, PhotoPurgeStatus, account_dir, account_generation, cached_photos,
    client_secret_key, forget_person_photos, tokens_secret_key,
};

#[cfg(test)]
mod native_tests;
#[cfg(test)]
mod tests;
