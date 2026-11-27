//! Account coordination. State locks are short: HTTP never runs under a state or library lock.
//! Separate refresh leases serialize refresh-token rotation across threads and processes.
//! Native EventKit calendars have no OAuth: the helper's macOS permission, explicit opt-in
//! and explicit calendar selection gate every read. OS prompts and queries run outside locks.
//! Google credentials are versioned `SecretStore` entries behind a pointer in `state.json`:
//! a replacement is staged, then committed, so a failed or interrupted one never loses the
//! working credential. Interactive sign-ins run under a caller-named attempt that any
//! process can cancel (see `login.rs`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::Duration;

use broscribe_platform::{
    BrowserLauncher, CalendarAuthorization, CalendarProvider, SecretStore, SystemCalendar,
    SystemCalendarInfo, TrustStore,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, GoogleCalendar};
use crate::config::{
    GOOGLE_DIRECTORY_READONLY, admin_controlled_scopes, directory_requested, scopes,
};
use crate::http::Http;
use crate::oauth::{OAuthClient, Tokens};
use crate::store::*;
use crate::*;

#[path = "accounts.rs"]
mod accounts;
pub(crate) use accounts::microsoft_photo_roots;
pub use accounts::{
    AccountCalendarSync, AccountDirectoryResult, AccountDirectorySource, AccountPermission,
    AccountSyncOutcome, CalendarDescriptor, CalendarRouteRead, CalendarTargetOutcome,
    CalendarTargetRemoval, CalendarTargetResult, LegacyMicrosoftStatus, MicrosoftAccount,
    MicrosoftCalendarCatalog, RouteCopy, VerifiedMicrosoftIdentity, integration_id,
};
#[path = "login.rs"]
mod login;
pub use login::{LoginAttemptId, LoginAttemptState, LoginAttemptStatus};

const LOCK_WAIT: Duration = Duration::from_secs(70);
const DAY: i64 = 86400;

#[derive(Default)]
struct Coordinator {
    state: Mutex<()>,
    refresh: Mutex<()>,
    /// Native calendars only: one OS permission request at a time, independent of capture.
    native_request: AtomicBool,
}

/// Holds a native calendar's request slot until dropped.
struct NativeRequest<'a>(&'a AtomicBool);
impl<'a> NativeRequest<'a> {
    fn begin(slot: &'a AtomicBool) -> Result<Self> {
        if slot.swap(true, Ordering::AcqRel) {
            return Err(service_error(
                "busy",
                "a calendar permission request is already waiting for macOS",
            ));
        }
        Ok(Self(slot))
    }
}
impl Drop for NativeRequest<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A known failure whose stable code survives IPC and CLI translation.
pub(crate) fn service_error(code: &'static str, message: &str) -> IntegrationError {
    IntegrationError::Core(broscribe_core::Error::Service {
        code,
        message: message.into(),
    })
}

/// Only full read access counts; every other state is reported, never assumed.
fn require_granted(authorization: CalendarAuthorization) -> Result<()> {
    match authorization {
        CalendarAuthorization::Granted => Ok(()),
        CalendarAuthorization::Denied => Err(service_error(
            "calendar_denied",
            "macOS denied calendar access to the BroScribe agent; allow it in System Settings > Privacy & Security > Calendars, then connect again",
        )),
        CalendarAuthorization::Restricted => Err(service_error(
            "calendar_restricted",
            "calendar access is restricted on this Mac (for example by a device management policy)",
        )),
        CalendarAuthorization::WriteOnly => Err(service_error(
            "calendar_write_only",
            "the BroScribe agent can only add events; reading meetings needs Full Access in System Settings > Privacy & Security > Calendars",
        )),
        CalendarAuthorization::NotDetermined => Err(service_error(
            "calendar_pending",
            "macOS has not answered the permission request; connect again and answer it",
        )),
        CalendarAuthorization::Unsupported => Err(IntegrationError::Core(
            broscribe_core::Error::Unsupported("native calendars require macOS".into()),
        )),
    }
}

/// Codes that report a native permission revoked or limited.
fn is_revocation(code: &str) -> bool {
    matches!(
        code,
        "calendar_denied" | "calendar_restricted" | "calendar_write_only" | "calendar_pending"
    )
}

fn coordinator(dir: &Path) -> Arc<Coordinator> {
    static ACCOUNTS: LazyLock<Mutex<HashMap<PathBuf, Weak<Coordinator>>>> =
        LazyLock::new(Mutex::default);
    let mut accounts = ACCOUNTS.lock().unwrap_or_else(|e| e.into_inner());
    accounts.retain(|_, v| v.strong_count() > 0);
    if let Some(c) = accounts.get(dir).and_then(Weak::upgrade) {
        return c;
    }
    let c = Arc::new(Coordinator::default());
    accounts.insert(dir.into(), Arc::downgrade(&c));
    c
}

fn bump(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| IntegrationError::Service("account generation exhausted".into()))
}

/// The scopes the provider reports granting with `tokens`, else those requested.
fn granted_scopes(tokens: &Tokens, requested: &[String]) -> Vec<String> {
    let granted: Vec<String> = tokens
        .scope
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if granted.is_empty() {
        requested.to_vec()
    } else {
        granted
    }
}

pub(crate) fn google_directory_granted(grant: &[String]) -> bool {
    grant.iter().any(|scope| scope == GOOGLE_DIRECTORY_READONLY)
}

/// Queues a credential version for deletion once.
fn queue_secret(state: &mut AccountState, key: String) {
    if !state.pending_secrets.contains(&key) {
        state.pending_secrets.push(key);
    }
}

/// One `state.json` transaction under the provider lock. [`Self::persist`] writes durably in
/// the middle of a transaction (a fence before fallible cleanup); the end of the transaction
/// writes only what changed since.
struct StateTx<'a> {
    dir: &'a Path,
    s: AccountState,
    written: AccountState,
}

impl StateTx<'_> {
    fn persist(&mut self) -> Result<()> {
        if self.s != self.written {
            write_json_private(&self.dir.join(STATE_FILE), &self.s)?;
            self.written = self.s.clone();
        }
        Ok(())
    }
}

/// What one Google request runs under, read in one state transaction: the account
/// generation, the calendar-selection generation, its credential, the stored grant, the
/// explicit calendar selection and the calendar denial fence.
struct Grant {
    generation: u64,
    selection_generation: u64,
    tokens: Tokens,
    scopes: Vec<String>,
    calendars: Vec<String>,
    calendar_fence: u64,
}

impl Grant {
    /// The Google calendar authority this grant's selection was read under.
    fn calendar_stamp(&self) -> GoogleCalendarStamp {
        GoogleCalendarStamp {
            generation: self.generation,
            selection_generation: self.selection_generation,
        }
    }
}

/// Notification request, not a browser launch. The caller routes clicks to explicit `login`.
#[derive(Clone, Debug, Serialize)]
pub struct ReauthNotification {
    pub integration: IntegrationId,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountRef>,
}

/// Native-calendar permission, helper availability and explicit selection are separate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemCalendarStatus {
    /// `None` when no helper authority answered: unknown, never a guessed permission.
    pub authorization: Option<CalendarAuthorization>,
    /// Only the installed helper may read native calendars.
    pub helper_required: bool,
    /// Eligible calendars to choose from, listed only after an explicit connect while
    /// macOS grants access. Labels only; event content never leaves the helper.
    #[serde(default)]
    pub calendars: Vec<SystemCalendarInfo>,
    /// Listing calendars failed: `calendars` is unknown, never a successful empty list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendars_error: Option<CalendarListError>,
}

/// Why listing a native provider's calendars failed: a stable code and its message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarListError {
    pub code: String,
    pub message: String,
}

/// The Google calendars the signed-in account may read, and the explicit selection, both
/// under `authority` (account and selection generations, flattened on the wire). Pass
/// `authority` back to [`Integrations::configure_google_calendars`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleCalendarCatalog {
    #[serde(flatten)]
    pub authority: GoogleCalendarStamp,
    pub selected: Vec<String>,
    pub calendars: Vec<GoogleCalendar>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntegrationStatus {
    pub integration: IntegrationId,
    /// An effective client ID exists (or none is needed). Never an opt-in or consent:
    /// nothing signs in or reads data until the user explicitly signs in.
    pub configured: bool,
    pub state: AccountState,
    #[serde(default)]
    pub admin_controlled_scopes: Vec<String>,
    /// Present for the system calendar (EventKit) only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_calendar: Option<SystemCalendarStatus>,
    #[serde(default)]
    pub microsoft_accounts: Vec<MicrosoftAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_microsoft: Option<LegacyMicrosoftStatus>,
    /// Google only: whether its OAuth client secret is stored (`None` elsewhere, or when
    /// the secret store could not be read). Sign-out keeps it; see
    /// [`Integrations::clear_client_secret`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_stored: Option<bool>,
}

/// What [`Integrations::retire_changed_client`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientChange {
    /// The effective client ID differs from before the edit.
    pub effective_changed: bool,
    /// A sign-in issued to the previous client was retired; only an explicit sign-in restores access.
    pub sign_in_retired: bool,
}

/// What an explicit revoking sign-out asked Google to do with the signed-out grant.
/// Google's revocation applies to every client of the OAuth project and may take a
/// while to take full effect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RevocationOutcome {
    /// Google accepted the revocation, or already held the token invalid.
    RevocationRequested,
    /// No refresh token was stored, so nothing was sent.
    NoStoredGrant,
    /// Not revoked (stable code, never provider text): the grant may still be live and
    /// the user removes it in their Google Account.
    Failed { code: String },
}

/// What [`Integrations::logout_and_revoke`] did: the local sign-out and, separately,
/// the revocation requested at Google (attempted even when local cleanup failed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleSignOut {
    pub local: std::result::Result<(), PublicError>,
    pub remote_revocation: RevocationOutcome,
}

/// How long one revocation request may take (ureq timers; see `Http`).
const REVOKE_TIMEOUT: Duration = Duration::from_secs(10);

fn client_secret_missing() -> IntegrationError {
    service_error(
        "client_secret_missing",
        "no Google OAuth client secret is stored; save it with `broscribe integrations setup google --client-secret-stdin`",
    )
}

/// Blocking library surface for CLI, settings and daemon. Use a shared daemon instance over IPC.
pub struct Integrations {
    data_dir: PathBuf,
    config: IntegrationsConfig,
    secrets: Arc<dyn SecretStore>,
    browser: Arc<dyn BrowserLauncher>,
    notify: Arc<dyn Fn(ReauthNotification) + Send + Sync>,
    reauth_notification_delivery: bool,
    endpoints: BTreeMap<IntegrationId, Endpoints>,
    coordinators: BTreeMap<IntegrationId, Arc<Coordinator>>,
    http: Http,
    /// The helper's system-calendar authority; absent in the CLI and app.
    system_calendar: Option<Arc<dyn SystemCalendar>>,
}

impl Integrations {
    pub fn new(
        data_dir: impl AsRef<Path>,
        config: IntegrationsConfig,
        secrets: Arc<dyn SecretStore>,
        browser: Arc<dyn BrowserLauncher>,
        notify: Arc<dyn Fn(ReauthNotification) + Send + Sync>,
    ) -> Result<Self> {
        let requested = data_dir.as_ref();
        let data_dir = match requested.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure_private_dir(requested)?;
                requested
                    .canonicalize()
                    .map_err(|error| IntegrationError::io(requested, error))?
            }
            Err(error) => return Err(IntegrationError::io(requested, error)),
        };
        check_profile_directories(&data_dir, &data_dir)?;
        let mut endpoints = BTreeMap::new();
        let mut coordinators = BTreeMap::new();
        for id in IntegrationId::ALL {
            // An invalid account setting must not disable unrelated integrations.
            // Missing endpoints are reported when that account is used.
            if let Ok(provider) = Endpoints::for_integration(id, &config.settings(id)) {
                endpoints.insert(id, provider);
            }
            coordinators.insert(id, coordinator(&account_dir(&data_dir, id)));
        }
        Ok(Self {
            data_dir,
            config,
            secrets,
            browser,
            notify,
            reauth_notification_delivery: true,
            endpoints,
            coordinators,
            http: Http::new(),
            system_calendar: None,
        })
    }

    /// Custom provider endpoints for local protocol tests or explicitly configured sovereign clouds.
    /// Do not accept these values from meeting content or an untrusted IPC client.
    pub fn with_endpoints(mut self, id: IntegrationId, endpoints: Endpoints) -> Self {
        self.endpoints.insert(id, endpoints);
        self
    }

    /// Uses the injected platform verifier or exact anchor snapshot. The strategy
    /// and initial availability probe are read once; a failed probe refuses
    /// provider traffic before connecting, while local calendars/sign-out remain
    /// available. Later OS verification applies current native policy, may perform
    /// synchronous OS retrieval, and fails without a weaker fallback.
    pub fn with_trust_store(mut self, store: &dyn TrustStore) -> Self {
        self.http = Http::from_trust_store(store);
        self
    }

    /// Whether the reauth callback delivers a user notification (default: `true`).
    /// Set `false` for standalone CLI callbacks that only print: they still run,
    /// but leave the durable notification pending for the daemon's `keep_alive`.
    pub fn with_reauth_notification_delivery(mut self, delivered: bool) -> Self {
        self.reauth_notification_delivery = delivered;
        self
    }

    /// Helper only: the authority allowed to read macOS calendar permission and events.
    /// Without it, system-calendar connect, configure and sync report `helper_required`;
    /// disconnect still works offline.
    pub fn with_system_calendar(mut self, calendar: Arc<dyn SystemCalendar>) -> Self {
        self.system_calendar = Some(calendar);
        self
    }

    fn native(&self) -> Result<&dyn SystemCalendar> {
        self.system_calendar.as_deref().ok_or_else(|| {
            service_error(
                "helper_required",
                "native calendars are read by the installed BroScribe agent; start it with `broscribe daemon start` and retry",
            )
        })
    }

    fn endpoints(&self, id: IntegrationId) -> Result<&Endpoints> {
        self.endpoints.get(&id).ok_or_else(|| {
            IntegrationError::NotConfigured(
                id.to_string(),
                format!(
                    "invalid provider endpoint settings; run `broscribe integrations setup {id}`"
                ),
            )
        })
    }

    /// A `state.json` transaction under the provider's thread and file locks. Credential
    /// versions left pending by an interrupted replacement are deleted first.
    fn state_tx<T>(
        &self,
        id: IntegrationId,
        f: impl FnOnce(&mut StateTx<'_>) -> Result<T>,
    ) -> Result<T> {
        let c = &self.coordinators[&id];
        let _mutex = c.state.lock().unwrap_or_else(|e| e.into_inner());
        let dir = account_dir(&self.data_dir, id);
        check_profile_directories(&self.data_dir, &dir)?;
        let _file = FileLock::acquire(&dir.join(STATE_LOCK), LOCK_WAIT)?;
        let s: AccountState = read_json_opt(&dir.join(STATE_FILE))?.unwrap_or_default();
        let mut tx = StateTx {
            dir: &dir,
            written: s.clone(),
            s,
        };
        if !tx.s.pending_secrets.is_empty() {
            self.recover_secrets(&mut tx, false)?;
        }
        let result = f(&mut tx)?;
        tx.persist()?;
        Ok(result)
    }

    fn state<T>(
        &self,
        id: IntegrationId,
        f: impl FnOnce(&Path, &mut AccountState) -> Result<T>,
    ) -> Result<T> {
        self.state_tx(id, |tx| f(tx.dir, &mut tx.s))
    }

    /// Deletes staged or superseded credential versions, never the committed one: the
    /// pointer was saved first, so deleting cannot lose the working credential. With
    /// `strict` (explicit sign-out) a failed deletion is reported; otherwise it stays
    /// pending for the next transaction.
    fn recover_secrets(&self, tx: &mut StateTx<'_>, strict: bool) -> Result<()> {
        let pending = std::mem::take(&mut tx.s.pending_secrets);
        let mut kept = Vec::new();
        let mut failure = None;
        for key in pending {
            if tx.s.secret_key.as_deref() == Some(key.as_str()) || kept.contains(&key) {
                continue;
            }
            if let Err(error) = validate_google_secret_key(&key) {
                tx.s.pending_secrets = kept;
                return Err(error);
            }
            if let Err(error) = self.secrets.delete(&key) {
                tracing::warn!(code = error.code(), "credential cleanup remains pending");
                if failure.is_none() {
                    failure = Some(error);
                }
                kept.push(key);
            }
        }
        tx.s.pending_secrets = kept;
        tx.persist()?;
        match failure {
            Some(error) if strict => Err(error.into()),
            _ => Ok(()),
        }
    }

    /// Stages `tokens` under the new credential version `key` (noted durably for
    /// recovery), then points the state at it; the previous version becomes pending
    /// deletion. A failure or crash before the caller persists the pointer leaves the
    /// previous credential committed.
    fn replace_credential(&self, tx: &mut StateTx<'_>, tokens: &Tokens, key: String) -> Result<()> {
        let json = serde_json::to_string(tokens)
            .map_err(|_| IntegrationError::Service("cannot encode credential".into()))?;
        queue_secret(&mut tx.s, key.clone());
        tx.persist()?;
        self.secrets.set(&key, &json)?;
        let previous =
            tx.s.secret_key
                .replace(key.clone())
                .unwrap_or_else(|| tokens_secret_key(IntegrationId::Google));
        tx.s.pending_secrets.retain(|pending| *pending != key);
        queue_secret(&mut tx.s, previous);
        Ok(())
    }

    fn oauth(&self, id: IntegrationId) -> Result<OAuthClient> {
        let settings = self.config.settings(id);
        let client_id = settings
            .effective_client_id(id)
            .ok_or_else(|| {
                IntegrationError::NotConfigured(
                    id.to_string(),
                    format!(
                        "set [integration.{id}] client_id; run `broscribe integrations setup {id}`"
                    ),
                )
            })?
            .to_owned();
        let secret = if id == IntegrationId::Google {
            self.secrets.get(&client_secret_key(id))?
        } else {
            None
        };
        Ok(OAuthClient::new(
            id,
            client_id,
            secret,
            self.endpoints(id)?.clone(),
            scopes(id, &settings),
            self.http.clone(),
        ))
    }

    /// The client ID this facade's configuration signs in with.
    fn effective_client_id(&self, id: IntegrationId) -> Option<String> {
        self.config
            .settings(id)
            .effective_client_id(id)
            .map(str::to_owned)
    }

    fn client_matches(state: &AccountState, expected: Option<&str>) -> bool {
        state
            .client_id
            .as_deref()
            .zip(expected)
            .is_some_and(|(issued, expected)| issued == expected)
    }

    /// The client ID the authoritative `config.toml` assigns to `id` now, not this facade's
    /// copy, which may predate an edit and its retirement. Another integration's malformed
    /// table never affects it.
    fn current_client_id(&self, id: IntegrationId) -> Result<Option<String>> {
        Ok(IntegrationsConfig::load_settings(&self.data_dir, id)?
            .effective_client_id(id)
            .map(str::to_owned))
    }

    /// A sign-in prepared with `client_id` is still the configured one; otherwise it was
    /// superseded by a client edit and must neither reserve nor publish anything.
    fn require_current_client(&self, id: IntegrationId, client_id: &str) -> Result<()> {
        if self.current_client_id(id)?.as_deref() == Some(client_id) {
            Ok(())
        } else {
            Err(IntegrationError::Superseded(id.to_string()))
        }
    }

    /// Call after `[integration.<id>]` changed, on a facade built from the new configuration,
    /// with the settings from before the edit. A sign-in issued to another client than the
    /// new effective one (or, for sign-ins without a recorded client, any effective change) is
    /// retired: the generation is bumped durably first (fencing in-flight sign-ins, refreshes
    /// and reads), then tokens, cached calendar and photos are removed. An explicit sign-in
    /// restores access. Never signs in or opens a browser; repeating an interrupted edit
    /// completes retirement. A sign-in already issued to the new client is kept.
    pub fn retire_changed_client(
        &self,
        id: IntegrationId,
        previous: &IntegrationSettings,
    ) -> Result<ClientChange> {
        if id.is_graph() {
            return self.retire_microsoft_client(id, previous);
        }
        if id.is_native_calendar() {
            return Ok(ClientChange {
                effective_changed: false,
                sign_in_retired: false,
            });
        }
        let before = previous.effective_client_id(id);
        let after = self.effective_client_id(id);
        let effective_changed = before != after.as_deref();
        let sign_in_retired = self.state_tx(id, |tx| {
            let signed_in = tx.s.status != AccountStatus::SignedOut;
            let stale = signed_in
                && match &tx.s.client_id {
                    Some(issued) => Some(issued) != after.as_ref(),
                    None => effective_changed,
                };
            if !stale && (signed_in || !effective_changed) {
                return Ok(false);
            }
            self.sign_out(
                id,
                tx,
                if stale {
                    AccountStatus::ReauthRequired
                } else {
                    AccountStatus::SignedOut
                },
            )?;
            Ok(stale)
        })?;
        if sign_in_retired {
            tracing::info!(integration = %id, "client ID changed; sign-in retired");
        }
        Ok(ClientChange {
            effective_changed,
            sign_in_retired,
        })
    }

    /// Fences durably (a new generation with `status`, credentials queued for deletion),
    /// then deletes the credentials and every copy the old generation produced.
    fn sign_out(
        &self,
        id: IntegrationId,
        tx: &mut StateTx<'_>,
        status: AccountStatus,
    ) -> Result<()> {
        check_profile_directories(&self.data_dir, tx.dir)?;
        let generation = bump(tx.s.generation)?;
        let mut next = AccountState {
            generation,
            status,
            pending_secrets: std::mem::take(&mut tx.s.pending_secrets),
            calendar_fence: tx.s.calendar_fence,
            selection_generation: tx.s.selection_generation,
            ..AccountState::default()
        };
        // Native calendars store no credentials; disconnect never needs the secret store.
        if !id.is_native_calendar() {
            if let Some(key) = tx.s.secret_key.take() {
                queue_secret(&mut next, key);
            }
            queue_secret(&mut next, tokens_secret_key(id));
        }
        tx.s = next;
        tx.persist()?;
        self.recover_secrets(tx, true)?;
        check_profile_directories(&self.data_dir, tx.dir)?;
        remove_file_if_exists(&tx.dir.join(CALENDAR_FILE))?;
        remove_cached_photos(&tx.dir.join(PHOTOS_DIR))
    }

    pub fn set_client_secret(&self, id: IntegrationId, secret: &str) -> Result<()> {
        if id != IntegrationId::Google {
            return Err(IntegrationError::Service(
                "only Google uses a configured client secret".into(),
            ));
        }
        self.state(id, |_, _| {
            self.secrets
                .set(&client_secret_key(id), secret)
                .map_err(Into::into)
        })
    }

    /// Removes the stored Google client secret and reports whether one was stored;
    /// repeating it is harmless, and account tokens are untouched. Refused with
    /// `sign_out_first` unless signed out, so an account never loses the secret its
    /// refreshes need. A sign-in already in flight then fails at its commit with
    /// `client_secret_missing`: its new tokens are dropped, not revoked at Google.
    pub fn clear_client_secret(&self, id: IntegrationId) -> Result<bool> {
        if id != IntegrationId::Google {
            return Err(IntegrationError::Service(
                "only Google uses a configured client secret".into(),
            ));
        }
        self.state(id, |_, s| {
            if s.status != AccountStatus::SignedOut {
                return Err(service_error(
                    "sign_out_first",
                    "sign out of Google before removing its client secret",
                ));
            }
            let key = client_secret_key(id);
            let stored = self.secrets.get(&key)?.is_some();
            if stored {
                self.secrets.delete(&key)?;
            }
            Ok(stored)
        })
    }

    /// Whether the Google client secret is stored; `None` for other integrations, or
    /// when the secret store cannot be read (status never fails for it).
    fn client_secret_stored(&self, id: IntegrationId) -> Option<bool> {
        if id != IntegrationId::Google {
            return None;
        }
        self.secrets
            .get(&client_secret_key(id))
            .inspect_err(|e| tracing::warn!(code = e.code(), "reading the client secret"))
            .ok()
            .map(|secret| secret.is_some())
    }

    /// The durable account state alone: no OS permission query (cheap for schedulers).
    pub fn account_state(&self, id: IntegrationId) -> Result<AccountState> {
        if id.is_graph() {
            self.legacy_microsoft_status(id)?;
        }
        self.state(id, |_, s| Ok(s.clone()))
    }

    pub fn status(&self, id: IntegrationId) -> Result<IntegrationStatus> {
        let settings = self.config.settings(id);
        let mut state = self.account_state(id)?;
        let client = settings.effective_client_id(id);
        if !id.is_native_calendar()
            && state.status == AccountStatus::Active
            && !Self::client_matches(&state, client)
        {
            state.status = AccountStatus::ReauthRequired;
        }
        // OS queries run after the state lock is released.
        let native = match id.calendar_provider() {
            Some(_) => {
                let (native, revoked) = self.native_status(&state);
                // A revocation observed by status is fenced like any other: this provider's
                // generation advances durably and its copy is dropped, so a later grant
                // never revives old caches or journal stamps. Report the fenced state.
                if revoked {
                    match self.revoke_native(id, state.generation) {
                        Ok(()) | Err(IntegrationError::Superseded(_)) => {}
                        Err(error) => return Err(error),
                    }
                    state = self.account_state(id)?;
                }
                Some(native)
            }
            None => None,
        };
        Ok(IntegrationStatus {
            integration: id,
            configured: id.is_native_calendar() || client.is_some(),
            state,
            admin_controlled_scopes: admin_controlled_scopes(id, &settings),
            system_calendar: native,
            microsoft_accounts: if id.is_graph() {
                self.list_accounts(Some(id))?
            } else {
                Vec::new()
            },
            legacy_microsoft: if id.is_graph() {
                Some(self.legacy_microsoft_status(id)?)
            } else {
                None
            },
            client_secret_stored: self.client_secret_stored(id),
        })
    }

    /// Reads permission without requesting it. Calendar labels require explicit opt-in.
    /// The flag reports observed permission revocation or a listing refused as denied.
    fn native_status(&self, state: &AccountState) -> (SystemCalendarStatus, bool) {
        let Ok(native) = self.native() else {
            return (
                SystemCalendarStatus {
                    authorization: None,
                    helper_required: true,
                    calendars: Vec::new(),
                    calendars_error: None,
                },
                false,
            );
        };
        let authorization = native
            .authorization()
            .inspect_err(|e| tracing::warn!(code = e.code(), "reading calendar permission"))
            .ok();
        let (calendars, calendars_error) = if state.status == AccountStatus::Active
            && authorization == Some(CalendarAuthorization::Granted)
        {
            match native.calendars() {
                Ok(calendars) => (calendars, None),
                Err(e) => {
                    tracing::warn!(code = e.code(), "listing native calendars");
                    (
                        Vec::new(),
                        Some(CalendarListError {
                            code: e.code().to_owned(),
                            message: e.to_string(),
                        }),
                    )
                }
            }
        } else {
            (Vec::new(), None)
        };
        let active = state.status == AccountStatus::Active;
        let revoked = active
            && (authorization.is_some_and(|a| a != CalendarAuthorization::Granted)
                || calendars_error
                    .as_ref()
                    .is_some_and(|error| is_revocation(&error.code)));
        (
            SystemCalendarStatus {
                authorization,
                helper_required: false,
                calendars,
                calendars_error,
            },
            revoked,
        )
    }

    pub fn statuses(&self) -> Result<Vec<IntegrationStatus>> {
        IntegrationId::ALL
            .into_iter()
            .map(|id| self.status(id))
            .collect()
    }

    /// Explicit user action only; background refresh and API calls never invoke this.
    /// Google signs in under the caller-named `attempt`, which any process may cancel with
    /// [`Self::cancel_login`], in three stages: admission under the state lock, the
    /// browser, token exchange and identity read with no lock held, then a commit that
    /// requires the attempt still running and the generation and configured client
    /// unchanged. Until that commit the working account, its credential, selection and
    /// copies stay valid; a cancelled, expired or failed attempt changes none of them.
    /// Its terminal result stays readable with [`Self::login_attempt`]. Native calendars
    /// connect with [`Self::connect_system_calendar`].
    pub fn login(&self, id: IntegrationId, attempt: &LoginAttemptId) -> Result<IntegrationStatus> {
        if id.is_graph() {
            self.oauth(id)?;
            return Err(service_error(
                "account_required",
                "use add_account or reconnect_account with an explicit account",
            ));
        }
        if id.calendar_provider().is_some() {
            return Err(service_error(
                "system_calendar_connect",
                "system calendars connect through the BroScribe agent's macOS permission, not an OAuth sign-in",
            ));
        }
        self.run_attempt(id, attempt, || {
            let oauth = self.oauth(id)?;
            // Google refuses a desktop client's code exchange without its secret: fail
            // before the browser opens instead of after the user consented.
            if !oauth.has_client_secret() {
                return Err(client_secret_missing());
            }
            let generation = self.state(id, |_, s| {
                self.require_current_client(id, &oauth.client_id)?;
                Ok(s.generation)
            })?;
            let tokens = self.perform_oauth(&oauth, attempt)?;
            let settings = self.config.settings(id);
            let identity = self
                .backend(id, &settings)?
                .google_identity(&tokens.access_token)?;
            let granted = granted_scopes(&tokens, &oauth.scopes);
            self.commit_login(
                id,
                attempt,
                generation,
                &tokens,
                &oauth.client_id,
                identity,
                granted,
            )?;
            self.status(id)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_login(
        &self,
        id: IntegrationId,
        attempt: &LoginAttemptId,
        generation: u64,
        tokens: &Tokens,
        client_id: &str,
        identity: AccountInfo,
        granted: Vec<String>,
    ) -> Result<()> {
        self.state_tx(id, |tx| {
            // A cancellation that won the lock decides the result before anything else.
            self.require_attempt_in(id, tx.dir, attempt)?;
            if tx.s.generation != generation {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            // An edit not yet retired: never publish tokens of the previous client.
            self.require_current_client(id, client_id)?;
            // A secret removed meanwhile: never publish an account every refresh would
            // fail. The new tokens are dropped here, not revoked at Google.
            if self.secrets.get(&client_secret_key(id))?.is_none() {
                return Err(client_secret_missing());
            }
            let next = bump(generation)?;
            let key = google_secret_version();
            let dir = tx.dir;
            self.commit_attempt(id, dir, attempt, &key, || {
                self.publish_login(tx, key.clone(), next, tokens, client_id, identity, granted)
            })
        })
    }

    /// The Google sign-in commit proper; the attempt is already marked committing `key`.
    #[allow(clippy::too_many_arguments)]
    fn publish_login(
        &self,
        tx: &mut StateTx<'_>,
        key: String,
        next: u64,
        tokens: &Tokens,
        client_id: &str,
        identity: AccountInfo,
        granted: Vec<String>,
    ) -> Result<()> {
        let id = IntegrationId::Google;
        self.replace_credential(tx, tokens, key)?;
        // The same verified account keeps its explicit calendar selection.
        let same_account = identity.email.is_some()
            && tx.s.account.as_ref().and_then(|a| a.email.as_ref()) == identity.email.as_ref();
        let selection = if same_account {
            std::mem::take(&mut tx.s.native_calendar_ids)
        } else {
            Vec::new()
        };
        let now = Utc::now();
        tx.s = AccountState {
            generation: next,
            status: AccountStatus::Active,
            account: Some(identity),
            scopes: granted,
            client_id: Some(client_id.to_owned()),
            signed_in_at: Some(now),
            last_refresh_at: Some(now),
            capabilities: Capabilities {
                calendar: Capability::Unknown,
                directory: if directory_requested(id, &self.config.settings(id)) {
                    Capability::Unknown
                } else {
                    Capability::NotRequested
                },
            },
            native_calendar_ids: selection,
            secret_key: tx.s.secret_key.take(),
            pending_secrets: std::mem::take(&mut tx.s.pending_secrets),
            calendar_fence: tx.s.calendar_fence,
            selection_generation: tx.s.selection_generation,
            ..AccountState::default()
        };
        // Committed: the new generation fences every copy of the old one, so cleanup
        // failures leave only inert files.
        tx.persist()?;
        if let Err(error) = remove_file_if_exists(&tx.dir.join(CALENDAR_FILE))
            .and_then(|()| remove_cached_photos(&tx.dir.join(PHOTOS_DIR)))
        {
            tracing::warn!(integration = %id, code = error.code(), "fenced copies remain after sign-in");
        }
        self.recover_secrets(tx, false)
    }

    /// Explicit user opt-in. Requests the provider's macOS permission only when undecided,
    /// outside every lock, and commits only if no disconnect or other connect ran meanwhile.
    /// A preexisting grant does not connect by itself, and a later grant never restores a
    /// failed connect. Connecting selects no calendars: nothing is read until
    /// [`Self::configure_native_calendars`].
    pub fn connect_system_calendar(&self, provider: CalendarProvider) -> Result<IntegrationStatus> {
        let id = IntegrationId::of_provider(provider);
        let native = self.native()?;
        let _request = NativeRequest::begin(&self.coordinators[&id].native_request)?;
        let generation = self.state(id, |_, s| Ok(s.generation))?;
        let authorization = match native.authorization()? {
            CalendarAuthorization::NotDetermined | CalendarAuthorization::WriteOnly => {
                native.request_access()?
            }
            current => current,
        };
        require_granted(authorization)?;
        self.state(id, |dir, s| {
            if s.generation != generation {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            if s.status == AccountStatus::Active {
                // Already connected: keep the selection and its cache.
                return Ok(());
            }
            let generation = bump(generation)?;
            remove_file_if_exists(&dir.join(CALENDAR_FILE))?;
            remove_cached_photos(&dir.join(PHOTOS_DIR))?;
            *s = AccountState {
                generation,
                status: AccountStatus::Active,
                signed_in_at: Some(Utc::now()),
                capabilities: Capabilities {
                    calendar: Capability::Unknown,
                    directory: Capability::NotRequested,
                },
                calendar_fence: s.calendar_fence,
                ..Default::default()
            };
            Ok(())
        })?;
        self.status(id)
    }

    /// Replaces the explicit selection after validating every ID against eligible calendars.
    /// Bumps the generation and drops its cache; empty selection never means all calendars.
    pub fn configure_native_calendars(
        &self,
        provider: CalendarProvider,
        calendar_ids: &[String],
    ) -> Result<IntegrationStatus> {
        let id = IntegrationId::of_provider(provider);
        let native = self.native()?;
        let (generation, status) = self.state(id, |_, s| Ok((s.generation, s.status)))?;
        if status != AccountStatus::Active {
            return Err(IntegrationError::SignedOut(id.to_string()));
        }
        require_granted(native.authorization()?)?;
        let selected = if calendar_ids.is_empty() {
            Vec::new()
        } else {
            let eligible = native.calendars()?;
            let mut selected: Vec<String> = Vec::with_capacity(calendar_ids.len());
            for wanted in calendar_ids {
                if !eligible.iter().any(|c| c.id == *wanted) {
                    return Err(IntegrationError::Core(broscribe_core::Error::InvalidInput(
                        format!(
                            "{wanted:?} is not an eligible calendar; see `broscribe integrations status {id}`"
                        ),
                    )));
                }
                if !selected.contains(wanted) {
                    selected.push(wanted.clone());
                }
            }
            selected
        };
        self.state_tx(id, |tx| {
            Self::fence(id, generation, &tx.s)?;
            tx.s.generation = bump(generation)?;
            tx.s.native_calendar_ids = selected;
            tx.s.last_sync = None;
            tx.s.capabilities.calendar = Capability::Unknown;
            tx.persist()?;
            remove_file_if_exists(&tx.dir.join(CALENDAR_FILE))
        })?;
        self.status(id)
    }

    /// The Google calendars the signed-in account may read, read with the existing calendar
    /// scope. Selects nothing.
    pub fn google_calendars(&self) -> Result<GoogleCalendarCatalog> {
        let id = IntegrationId::Google;
        let settings = self.config.settings(id);
        let backend = self.backend(id, &settings)?;
        let (grant, calendars) =
            self.authenticated(id, |_, token| backend.google_calendars(token))?;
        Ok(GoogleCalendarCatalog {
            authority: grant.calendar_stamp(),
            selected: grant.calendars,
            calendars: calendars?,
        })
    }

    /// Replaces the explicit Google selection after validating every ID against the catalog
    /// read under `expected`. Advances only the selection generation (fencing copies and
    /// priors of the old selection) and drops the cache; the account generation, and with it
    /// directory results and photos, stays. An empty selection reads nothing.
    pub fn configure_google_calendars(
        &self,
        expected: GoogleCalendarStamp,
        calendar_ids: &[String],
    ) -> Result<IntegrationStatus> {
        let id = IntegrationId::Google;
        let mut selected: Vec<String> = Vec::with_capacity(calendar_ids.len());
        for wanted in calendar_ids {
            if !selected.contains(wanted) {
                selected.push(wanted.clone());
            }
        }
        if !selected.is_empty() {
            let catalog = self.google_calendars()?;
            if catalog.authority != expected {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            if let Some(wanted) = selected
                .iter()
                .find(|wanted| !catalog.calendars.iter().any(|c| c.id == **wanted))
            {
                return Err(IntegrationError::Core(broscribe_core::Error::InvalidInput(
                    format!(
                        "{wanted:?} is not a readable calendar of the signed-in Google account"
                    ),
                )));
            }
        }
        self.state_tx(id, |tx| {
            Self::fence_selection(id, expected, &tx.s)?;
            if tx.s.status != AccountStatus::Active {
                return Err(IntegrationError::ReauthRequired(id.to_string()));
            }
            tx.s.selection_generation = bump(expected.selection_generation)?;
            tx.s.capabilities.calendar = if selected.is_empty() {
                Capability::NotRequested
            } else {
                Capability::Unknown
            };
            tx.s.native_calendar_ids = selected;
            tx.s.last_sync = None;
            tx.persist()?;
            remove_file_if_exists(&tx.dir.join(CALENDAR_FILE))
        })?;
        self.status(id)
    }

    /// The live generation while explicit opt-in stands and macOS still grants access.
    /// Observed revocation durably fences the generation and drops its copy. Never prompts.
    pub fn native_generation(&self, provider: CalendarProvider) -> Result<Option<u64>> {
        let id = IntegrationId::of_provider(provider);
        let read = || self.state(id, |_, s| Ok((s.generation, s.status)));
        let (generation, status) = read()?;
        // Without an opt-in the answer is known without asking macOS.
        if status != AccountStatus::Active {
            return Ok(None);
        }
        if self.native()?.authorization()? != CalendarAuthorization::Granted {
            match self.revoke_native(id, generation) {
                Ok(()) | Err(IntegrationError::Superseded(_)) => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        // A disconnect or reconfigure while macOS answered supersedes this answer.
        Ok((read()? == (generation, AccountStatus::Active)).then_some(generation))
    }

    /// The generation under which a cached native calendar may enter a recording snapshot.
    /// Google authority is a pair: see [`Self::google_authority`].
    pub fn calendar_generation(&self, id: IntegrationId) -> Result<Option<u64>> {
        if id.is_graph() {
            return Err(service_error(
                "account_required",
                "Microsoft authority requires an account handle",
            ));
        }
        match id.calendar_provider() {
            Some(provider) => self.native_generation(provider),
            None => Err(service_error(
                "google_stamp_required",
                "Google calendar authority has an account and a selection generation",
            )),
        }
    }

    /// The live Google calendar authority: the account and selection generations a cached
    /// event or prior must both carry, while the account is signed in with the configured
    /// client. `None` otherwise. Read-only.
    pub fn google_authority(&self) -> Result<Option<GoogleCalendarStamp>> {
        let id = IntegrationId::Google;
        let expected = self.effective_client_id(id);
        self.state(id, |_, s| {
            Ok(
                (s.status == AccountStatus::Active && Self::client_matches(s, expected.as_deref()))
                    .then_some(GoogleCalendarStamp {
                        generation: s.generation,
                        selection_generation: s.selection_generation,
                    }),
            )
        })
    }

    /// A native provider's permission was observed revoked or limited at `generation`:
    /// advance the generation durably, then drop its copy. The opt-in and selection stay;
    /// only a new grant and read publish again. Repeated denied polls keep the generation.
    fn revoke_native(&self, id: IntegrationId, generation: u64) -> Result<()> {
        self.state_tx(id, |tx| {
            Self::fence(id, generation, &tx.s)?;
            let cache = tx.dir.join(CALENDAR_FILE);
            if tx.s.capabilities.calendar == Capability::Denied && !cache.exists() {
                return Ok(());
            }
            tx.s.generation = bump(generation)?;
            tx.s.last_sync = None;
            tx.s.capabilities.calendar = Capability::Denied;
            // Fence pending reads durably even if removing the copy fails.
            tx.persist()?;
            remove_file_if_exists(&cache)
        })
    }

    /// Google refused the calendar read made under `grant`. Every denial advances the
    /// calendar fence, so a read that started earlier never publishes after it; when a copy
    /// may have been published the selection generation advances too, fencing it and the
    /// priors taken from it. The account generation stays: directory results and photos
    /// are not calendar data. Both are saved before the copy is removed. A denial applies
    /// only to the selection it was read under: after a newer selection, denial or sign-in
    /// it is `superseded` and changes nothing, so it can never mark that selection denied
    /// or remove its copy.
    fn deny_google_calendar(&self, id: IntegrationId, grant: &Grant) -> Result<()> {
        self.state_tx(id, |tx| {
            Self::fence_google_read(id, grant, &tx.s)?;
            let cache = tx.dir.join(CALENDAR_FILE);
            tx.s.calendar_fence = bump(tx.s.calendar_fence)?;
            if tx.s.capabilities.calendar != Capability::Denied || cache.exists() {
                tx.s.selection_generation = bump(tx.s.selection_generation)?;
            }
            tx.s.capabilities.calendar = Capability::Denied;
            tx.s.last_sync = None;
            tx.persist()?;
            remove_file_if_exists(&cache)
        })
    }

    /// Bumps the durable generation before removing credentials or caches. In-flight work cannot resurrect them.
    /// Local only: Google keeps the grant until [`Self::logout_and_revoke`] or the user removes it.
    pub fn logout(&self, id: IntegrationId) -> Result<()> {
        if id.is_graph() {
            return Err(service_error(
                "account_required",
                "remove one explicit Microsoft account",
            ));
        }
        self.state_tx(id, |tx| self.sign_out(id, tx, AccountStatus::SignedOut))
    }

    /// Google only, explicit opt-in: [`Self::logout`], then asks Google to revoke the
    /// refresh token the signed-out credential held. Google revokes every scope granted to
    /// the OAuth project, for all of its clients (other data folders, Macs or tools using
    /// the same project), and may take a while to apply it. The token is read in the
    /// sign-out's own transaction, whatever the account status, and revoked after that
    /// transaction with no lock held, even when local cleanup failed; both results are
    /// returned. When both fail the user removes access at Google themselves.
    pub fn logout_and_revoke(&self, id: IntegrationId) -> Result<GoogleSignOut> {
        if id != IntegrationId::Google {
            return Err(service_error(
                "revocation_unsupported",
                "only a Google sign-out can also revoke access at the provider",
            ));
        }
        // Outside the transaction, so it survives a failed sign-out. `None`: never read.
        let mut captured = None;
        let local = self.state_tx(id, |tx| {
            captured = Some(self.stored_refresh_token(&tx.s));
            self.sign_out(id, tx, AccountStatus::SignedOut)
        });
        let remote_revocation = match captured {
            // Missing endpoint settings never hide the local result.
            Some(Ok(Some(token))) => match self
                .endpoints(id)
                .ok()
                .and_then(|endpoints| endpoints.revoke.as_deref())
            {
                Some(url) => crate::oauth::revoke(&self.http, url, &token, REVOKE_TIMEOUT),
                None => RevocationOutcome::Failed {
                    code: "revocation_unsupported".into(),
                },
            },
            Some(Ok(None)) => RevocationOutcome::NoStoredGrant,
            Some(Err(error)) => RevocationOutcome::Failed {
                code: error.code().into(),
            },
            None => RevocationOutcome::Failed {
                code: "not_attempted".into(),
            },
        };
        match &remote_revocation {
            RevocationOutcome::Failed { code } => {
                tracing::warn!(integration = %id, code = %code, "revocation at Google failed");
            }
            outcome => tracing::info!(integration = %id, ?outcome, "revocation at Google"),
        }
        Ok(GoogleSignOut {
            local: local.map_err(|error| error.public()),
            remote_revocation,
        })
    }

    /// The refresh token of the committed credential (the versioned pointer, else the
    /// legacy slot), if any. Access tokens are never used for revocation: an expired one
    /// would be refused while the grant lives on.
    fn stored_refresh_token(&self, state: &AccountState) -> Result<Option<String>> {
        let id = IntegrationId::Google;
        let key = state
            .secret_key
            .clone()
            .unwrap_or_else(|| tokens_secret_key(id));
        validate_google_secret_key(&key)?;
        let Some(json) = self.secrets.get(&key)? else {
            return Ok(None);
        };
        let tokens: Tokens = serde_json::from_str(&json).map_err(|_| {
            IntegrationError::Service("stored credential has an invalid format".into())
        })?;
        Ok(tokens.refresh_token.filter(|token| !token.is_empty()))
    }

    fn snapshot(&self, id: IntegrationId) -> Result<Grant> {
        let expected = self.effective_client_id(id);
        self.state(id, |_, s| {
            match s.status {
                AccountStatus::SignedOut => {
                    return Err(IntegrationError::SignedOut(id.to_string()));
                }
                AccountStatus::ReauthRequired => {
                    return Err(IntegrationError::ReauthRequired(id.to_string()));
                }
                AccountStatus::Active => {}
            }
            // A legacy record without its issuing client is not evidence of a match.
            // Refuse before reading credentials; only explicit sign-in establishes it.
            if !Self::client_matches(s, expected.as_deref()) {
                return Err(IntegrationError::ReauthRequired(id.to_string()));
            }
            let key = s
                .secret_key
                .clone()
                .unwrap_or_else(|| tokens_secret_key(id));
            validate_google_secret_key(&key)?;
            let json = self
                .secrets
                .get(&key)?
                .ok_or_else(|| IntegrationError::SignedOut(id.to_string()))?;
            let tokens = serde_json::from_str(&json).map_err(|_| {
                IntegrationError::Service("stored credential has an invalid format".into())
            })?;
            Ok(Grant {
                generation: s.generation,
                selection_generation: s.selection_generation,
                tokens,
                scopes: s.scopes.clone(),
                calendars: s.native_calendar_ids.clone(),
                calendar_fence: s.calendar_fence,
            })
        })
    }

    /// Marks the account at `generation` as requiring sign-in. Rechecked under the state
    /// lock: an account issued to another client than this facade's is a newer sign-in this
    /// (stale) facade knows nothing about, and is left untouched.
    fn reauth(&self, id: IntegrationId, generation: u64) -> Result<()> {
        let expected = self.effective_client_id(id);
        let notify = self.state(id, |_, s| {
            Self::fence(id, generation, s)?;
            if s.status == AccountStatus::Active
                && s.client_id.is_some()
                && !Self::client_matches(s, expected.as_deref())
            {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            s.status = AccountStatus::ReauthRequired;
            let notify = !s.reauth_notified;
            s.reauth_notified |= self.reauth_notification_delivery;
            Ok(notify)
        })?;
        if notify {
            (self.notify)(ReauthNotification {
                integration: id,
                account: None,
                message: format!("Sign in to {} to keep calendar sync.", id.provider_name()),
            });
        }
        Ok(())
    }

    fn access(&self, id: IntegrationId, force: bool) -> Result<Grant> {
        let original = self.snapshot(id)?;
        if !force && original.tokens.is_fresh(Utc::now().timestamp()) {
            return Ok(original);
        }
        self.refresh_access(id, force, original)
    }

    fn refresh_access(&self, id: IntegrationId, force: bool, original: Grant) -> Result<Grant> {
        let c = &self.coordinators[&id];
        let _single_flight = c.refresh.lock().unwrap_or_else(|e| e.into_inner());
        let _lease = FileLock::acquire(
            &account_dir(&self.data_dir, id).join(REFRESH_LOCK),
            LOCK_WAIT,
        )?;
        let current = self.snapshot(id)?;
        if current.generation != original.generation {
            return Err(IntegrationError::Superseded(id.to_string()));
        }
        if current.tokens != original.tokens
            || (!force && current.tokens.is_fresh(Utc::now().timestamp()))
        {
            return Ok(current);
        }
        let mut oauth = self.oauth(id)?;
        // Refresh within the stored grant: a scope the configuration added after sign-in
        // needs an explicit sign-in, never a silent refresh. An unknown grant sends none.
        oauth.scopes = current.scopes.clone();
        let refresh = || match current.tokens.refresh_token.as_deref() {
            Some(r) => oauth.refresh(r),
            None => Err(IntegrationError::ReauthRequired(id.to_string())),
        };
        // No state lock is held across either HTTP attempt.
        let result = match refresh() {
            Err(IntegrationError::Transient(_)) => refresh(),
            other => other,
        };
        match result {
            Ok(tokens) => {
                self.state_tx(id, |tx| {
                    Self::fence(id, current.generation, &tx.s)?;
                    self.replace_credential(tx, &tokens, google_secret_version())?;
                    tx.s.last_refresh_at = Some(Utc::now());
                    tx.persist()?;
                    self.recover_secrets(tx, false)
                })?;
                Ok(Grant { tokens, ..current })
            }
            Err(IntegrationError::ReauthRequired(_)) => {
                self.reauth(id, current.generation)?;
                Err(IntegrationError::ReauthRequired(id.to_string()))
            }
            Err(e) => Err(e),
        }
    }

    /// An API 401 invalidates the access token, not necessarily the refresh token.
    /// Retry once with refreshed credentials before requiring interactive sign-in.
    fn authenticated<T>(
        &self,
        id: IntegrationId,
        request: impl Fn(&Grant, &str) -> Result<T>,
    ) -> Result<(Grant, Result<T>)> {
        let original = self.access(id, false)?;
        let result = request(&original, &original.tokens.access_token);
        if !matches!(result, Err(IntegrationError::ReauthRequired(_))) {
            return Ok((original, result));
        }
        // Reuse the rejected snapshot so a concurrent rotation is not refreshed twice,
        // and a replacement account cannot receive a request intended for the old one.
        let grant = self.refresh_access(id, true, original)?;
        let result = request(&grant, &grant.tokens.access_token);
        if matches!(result, Err(IntegrationError::ReauthRequired(_))) {
            self.reauth(id, grant.generation)?;
        }
        Ok((grant, result))
    }

    fn fence(id: IntegrationId, generation: u64, state: &AccountState) -> Result<()> {
        if state.generation != generation || state.status == AccountStatus::SignedOut {
            Err(IntegrationError::Superseded(id.to_string()))
        } else {
            Ok(())
        }
    }

    /// [`Self::fence`] plus the Google calendar selection the work was started under.
    fn fence_selection(
        id: IntegrationId,
        stamp: GoogleCalendarStamp,
        state: &AccountState,
    ) -> Result<()> {
        Self::fence(id, stamp.generation, state)?;
        if state.selection_generation == stamp.selection_generation {
            Ok(())
        } else {
            Err(IntegrationError::Superseded(id.to_string()))
        }
    }

    /// Call from the daemon's timer. Refreshes before expiry and at least daily while active;
    /// never opens a browser. Delivers pending reauth notifications once; signed-out accounts
    /// stay quiet. Works from the durable state, not the status projection: an account issued
    /// to another client than this facade's (a stale facade, or an edit awaiting retirement)
    /// is never refreshed or retired from here; a legacy account without a recorded client
    /// requires sign-in.
    pub fn keep_alive(&self, id: IntegrationId) -> Result<()> {
        if id.is_graph() {
            return Err(service_error(
                "account_required",
                "keep one explicit Microsoft account alive",
            ));
        }
        // No tokens, refreshes or sign-in notifications: macOS owns native permissions.
        if id.is_native_calendar() {
            return Ok(());
        }
        let state = self.account_state(id)?;
        match state.status {
            AccountStatus::SignedOut => return Ok(()),
            AccountStatus::ReauthRequired => return self.reauth(id, state.generation),
            AccountStatus::Active => {}
        }
        if !Self::client_matches(&state, self.effective_client_id(id).as_deref()) {
            return if state.client_id.is_none() {
                self.reauth(id, state.generation)
            } else {
                Ok(())
            };
        }
        let daily = state
            .last_refresh_at
            .is_none_or(|t| Utc::now().signed_duration_since(t).num_seconds() >= DAY);
        self.access(id, daily).map(|_| ())
    }

    fn backend<'a>(
        &'a self,
        id: IntegrationId,
        settings: &'a IntegrationSettings,
    ) -> Result<Backend<'a>> {
        Ok(Backend {
            id,
            settings,
            endpoints: self.endpoints(id)?,
            http: &self.http,
        })
    }

    fn capability(
        &self,
        id: IntegrationId,
        generation: u64,
        directory: bool,
        value: Capability,
    ) -> Result<()> {
        self.state(id, |_, s| {
            Self::fence(id, generation, s)?;
            if directory {
                s.capabilities.directory = value;
            } else {
                s.capabilities.calendar = value;
            }
            Ok(())
        })
    }

    pub fn sync_calendar(
        &self,
        id: IntegrationId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<CalendarCache> {
        if id.is_graph() {
            return Err(service_error(
                "account_required",
                "sync one explicit Microsoft account",
            ));
        }
        if let Some(provider) = id.calendar_provider() {
            return self.sync_native(provider, from, to);
        }
        self.sync_google(id, from, to)
    }

    /// Reads only the explicitly selected Google calendars, outside all locks. Publication
    /// requires the account generation, selection generation and calendar fence the read
    /// started under; events carry the account and selection generations. A denial of the
    /// account's calendar access is fenced durably before the copy is removed. A selected
    /// calendar the account can no longer read leaves the selection (only an explicit
    /// selection reads it again, nothing replaces it) and the selection generation
    /// advances, fencing its copy and priors, before the others are published. When
    /// another selected calendar's read failed, the losses already observed are still
    /// withdrawn durably and the copy dropped before that failure is returned; an
    /// incomplete read is never published. An API 401 is retried once with refreshed
    /// credentials; the losses observed before it are withdrawn first, so neither a failed
    /// refresh nor the retry drops them, and the retry reads only the remaining selection.
    /// Calendar status is written only under the selection the read started with.
    /// Directory results and photos are untouched.
    fn sync_google(
        &self,
        id: IntegrationId,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<CalendarCache> {
        let (selection, status, nothing_selected) = self.state(id, |_, s| {
            Ok((
                GoogleCalendarStamp {
                    generation: s.generation,
                    selection_generation: s.selection_generation,
                },
                s.status,
                s.native_calendar_ids.is_empty(),
            ))
        })?;
        match status {
            AccountStatus::SignedOut => return Err(IntegrationError::SignedOut(id.to_string())),
            AccountStatus::ReauthRequired => {
                return Err(IntegrationError::ReauthRequired(id.to_string()));
            }
            AccountStatus::Active => {}
        }
        if nothing_selected {
            // Nothing selected: no credential, request or copy. A selection saved since is
            // never marked as not requested.
            self.state(id, |_, s| {
                Self::fence_selection(id, selection, s)?;
                if !s.native_calendar_ids.is_empty() {
                    return Err(IntegrationError::Superseded(id.to_string()));
                }
                s.capabilities.calendar = Capability::NotRequested;
                Ok(())
            })?;
            return Ok(CalendarCache::default());
        }
        let settings = self.config.settings(id);
        let backend = self.backend(id, &settings)?;
        let read_selection = |grant: &Grant| {
            backend.calendar(
                &grant.tokens.access_token,
                &grant.calendars,
                grant.calendar_stamp(),
                from,
                to,
            )
        };
        let mut grant = self.access(id, false)?;
        let result = match read_selection(&grant) {
            // The access token was rejected, not necessarily the refresh token. Losses were
            // answered under an accepted token: they leave the selection before refreshing.
            Ok(first) if first.rejected() => {
                if !first.lost.is_empty() {
                    self.fail_google_read(id, &grant, &first.lost)?;
                }
                // Reuse the rejected snapshot so a concurrent rotation is not refreshed
                // twice; the refreshed grant carries the selection as it stands now.
                grant = self.refresh_access(id, true, grant)?;
                match read_selection(&grant) {
                    Ok(retried) if retried.rejected() => {
                        self.fail_google_read(id, &grant, &retried.lost)?;
                        self.reauth(id, grant.generation)?;
                        return Err(IntegrationError::ReauthRequired(id.to_string()));
                    }
                    retried => retried,
                }
            }
            first => first,
        };
        let read = match result {
            Ok(read) => read,
            Err(e @ IntegrationError::Forbidden(_)) => {
                self.deny_google_calendar(id, &grant)?;
                return Err(e);
            }
            Err(e) => {
                self.fail_google_read(id, &grant, &[])?;
                return Err(e);
            }
        };
        if let Some(error) = read.failure {
            self.fail_google_read(id, &grant, &read.lost)?;
            return Err(error);
        }
        let mut cache = CalendarCache {
            events: read.events,
        };
        cache.prune(Utc::now());
        self.state_tx(id, |tx| {
            Self::fence_google_read(id, &grant, &tx.s)?;
            if !read.lost.is_empty() {
                let live = Self::withdraw_google_calendars(tx, &grant, &read.lost)?;
                for event in &mut cache.events {
                    event.google = Some(live);
                }
            }
            write_json_private_if_changed(&tx.dir.join(CALENDAR_FILE), &cache)?;
            tx.s.capabilities.calendar = if tx.s.native_calendar_ids.is_empty() {
                Capability::NotRequested
            } else {
                Capability::Available
            };
            tx.s.last_sync = Some(Utc::now());
            Ok(())
        })?;
        Ok(cache)
    }

    /// The read under `grant` failed. While its selection and calendar fence still stand,
    /// the losses it observed leave the selection (fencing their copy and priors), the
    /// copy that mixes them with the others is dropped and the calendar status becomes
    /// `Error`. A newer selection, denial or sign-in already decided both: nothing is
    /// replaced, and the caller still reports its own failure.
    fn fail_google_read(&self, id: IntegrationId, grant: &Grant, lost: &[String]) -> Result<()> {
        let failed = self.state_tx(id, |tx| {
            Self::fence_google_read(id, grant, &tx.s)?;
            tx.s.capabilities.calendar = Capability::Error;
            if lost.is_empty() {
                return Ok(());
            }
            Self::withdraw_google_calendars(tx, grant, lost)?;
            // The copy mixes the lost calendars with the others, and this read cannot
            // replace it completely.
            remove_file_if_exists(&tx.dir.join(CALENDAR_FILE))
        });
        match failed {
            Ok(()) | Err(IntegrationError::Superseded(_)) => Ok(()),
            Err(other) => Err(other),
        }
    }

    /// A Google calendar read under `grant` may still commit: same account, same selection
    /// and no calendar denial since.
    fn fence_google_read(id: IntegrationId, grant: &Grant, state: &AccountState) -> Result<()> {
        Self::fence_selection(id, grant.calendar_stamp(), state)?;
        if state.calendar_fence != grant.calendar_fence {
            return Err(IntegrationError::Superseded(id.to_string()));
        }
        Ok(())
    }

    /// Removes `lost` calendars from the selection read under `grant` and advances the
    /// selection generation, saved before any copy changes: the lost calendars' copy and
    /// priors, and every read started under the old selection, are fenced. Returns the
    /// live authority the remaining calendars' events are restamped with.
    fn withdraw_google_calendars(
        tx: &mut StateTx<'_>,
        grant: &Grant,
        lost: &[String],
    ) -> Result<GoogleCalendarStamp> {
        tracing::warn!(
            lost = lost.len(),
            "selected Google calendars are no longer readable and leave the selection"
        );
        tx.s.native_calendar_ids
            .retain(|calendar| !lost.contains(calendar));
        tx.s.selection_generation = bump(grant.selection_generation)?;
        tx.s.last_sync = None;
        tx.persist()?;
        Ok(GoogleCalendarStamp {
            generation: grant.generation,
            selection_generation: tx.s.selection_generation,
        })
    }

    /// Reads only explicitly selected eligible EventKit calendars outside all locks,
    /// publishing a copy stamped with the generation. Revocation drops the copy.
    fn sync_native(
        &self,
        provider: CalendarProvider,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<CalendarCache> {
        let id = IntegrationId::of_provider(provider);
        let native = self.native()?;
        let (generation, status, selected) = self.state(id, |_, s| {
            Ok((s.generation, s.status, s.native_calendar_ids.clone()))
        })?;
        if status != AccountStatus::Active {
            return Err(IntegrationError::SignedOut(id.to_string()));
        }
        let require_access = || -> Result<()> {
            if let Err(error) = require_granted(native.authorization()?) {
                // Revoked or limited: drop the copy rather than serve it until a new grant.
                self.revoke_native(id, generation)?;
                return Err(error);
            }
            Ok(())
        };
        require_access()?;
        // Mark a fresh authorized read before querying the platform. A second
        // revocation must fence it even if the first already removed the
        // cache. Denied polls without an intervening read keep their generation.
        self.state(id, |_, s| {
            Self::fence(id, generation, s)?;
            if s.capabilities.calendar == Capability::Denied {
                s.capabilities.calendar = Capability::Unknown;
            }
            Ok(())
        })?;
        // The first authorization read may predate the denied-state reset.
        // Revalidate afterward, before any platform query can publish at G.
        require_access()?;
        let read = || -> Result<Vec<CalendarEvent>> {
            if selected.is_empty() {
                return Ok(Vec::new());
            }
            // Calendars that disappeared or became ineligible are dropped, never widened.
            let eligible = native.calendars()?;
            let resolved: Vec<String> = selected
                .into_iter()
                .filter(|wanted| eligible.iter().any(|c| c.id == *wanted))
                .collect();
            if resolved.is_empty() {
                return Ok(Vec::new());
            }
            Ok(native
                .events(from.timestamp(), to.timestamp(), &resolved)?
                .into_iter()
                .filter(|event| resolved.contains(&event.calendar_id))
                .filter_map(|event| CalendarEvent::from_native(id, event, generation))
                .collect())
        };
        let events = match read() {
            Ok(read) => read,
            Err(error) => {
                // A denial observed by the read itself is also a revocation.
                if is_revocation(error.code()) {
                    self.revoke_native(id, generation)?;
                } else {
                    self.capability(id, generation, false, Capability::Error)?;
                }
                return Err(error);
            }
        };
        let mut cache = CalendarCache { events };
        cache.prune(Utc::now());
        self.state(id, |dir, s| {
            Self::fence(id, generation, s)?;
            write_json_private_if_changed(&dir.join(CALENDAR_FILE), &cache)?;
            s.capabilities.calendar = Capability::Available;
            s.last_sync = Some(Utc::now());
            Ok(())
        })?;
        Ok(cache)
    }

    /// Searches the Google Workspace directory. Requires the directory scope in the stored
    /// grant; publication rechecks the current configuration and grant.
    pub fn directory(&self, id: IntegrationId, query: &str) -> Result<Vec<DirectoryPerson>> {
        if id.is_graph() {
            return Err(service_error(
                "account_required",
                "use account_directory with an explicit account",
            ));
        }
        let settings = self.config.settings(id);
        if !directory_requested(id, &settings) {
            return Ok(Vec::new());
        }
        let backend = self.backend(id, &settings)?;
        let (grant, result) = self.authenticated(id, |grant, token| {
            if !google_directory_granted(&grant.scopes) {
                return Err(service_error(
                    "insufficient_grant",
                    "directory access was not granted at sign-in; sign in again to grant it",
                ));
            }
            backend.directory(token, query)
        })?;
        let current = IntegrationsConfig::load_settings(&self.data_dir, id)?;
        self.state(id, |_, s| {
            Self::fence(id, grant.generation, s)?;
            if result.is_ok()
                && !(directory_requested(id, &current) && google_directory_granted(&s.scopes))
            {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            s.capabilities.directory = match &result {
                Ok(_) => Capability::Available,
                Err(e)
                    if matches!(e, IntegrationError::Forbidden(_))
                        || e.code() == "insufficient_grant" =>
                {
                    Capability::Denied
                }
                Err(_) => Capability::Error,
            };
            Ok(())
        })?;
        result
    }

    /// Downloads outside all locks; commits only if this account, its directory grant, the
    /// current Workspace configuration and the person's existence still match. Photos live
    /// under the account generation, so a later sign-in or sign-out fences them even if their
    /// removal fails. Caller invokes `forget_person_photos` after removing a person, so a
    /// concurrent forget either prevents this commit or removes its result under the same lock.
    pub fn cache_photo(
        &self,
        id: IntegrationId,
        person_id: &str,
        remote: &DirectoryPerson,
        person_exists: &dyn Fn(&str) -> bool,
    ) -> Result<Option<PathBuf>> {
        let name = photo_file_name(person_id)?;
        if id.is_graph() {
            return Err(service_error(
                "account_required",
                "photo downloads require an account and generation",
            ));
        }
        // Native calendars have no directory or photos; no credential is ever attempted.
        if id.is_native_calendar() || !person_exists(person_id) {
            return Ok(None);
        }
        let settings = self.config.settings(id);
        if !directory_requested(id, &settings) {
            return Ok(None);
        }
        let backend = self.backend(id, &settings)?;
        let (grant, result) = self.authenticated(id, |grant, token| {
            if !google_directory_granted(&grant.scopes) {
                return Err(service_error(
                    "insufficient_grant",
                    "directory access was not granted at sign-in",
                ));
            }
            backend.photo(token, remote)
        })?;
        let Some(photo) = result? else {
            return Ok(None);
        };
        // The configuration may have turned Workspace off while the photo downloaded.
        let current = IntegrationsConfig::load_settings(&self.data_dir, id)?;
        self.state(id, |dir, s| {
            Self::fence(id, grant.generation, s)?;
            if s.capabilities.directory == Capability::Denied
                || !google_directory_granted(&s.scopes)
                || !directory_requested(id, &current)
                || !Self::client_matches(s, current.effective_client_id(id))
            {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            if !person_exists(person_id) {
                return Ok(None);
            }
            let path = provider_photo_dir(dir, grant.generation).join(name);
            write_private(&path, &photo)?;
            Ok(Some(path))
        })
    }

    /// Runs `commit` under the account's state lock only while the account is still signed in
    /// at `generation`; `Superseded` otherwise. Sign-out and replacement take the same lock, so
    /// they either happen before (and refuse the commit) or wait until it is done. Use it to
    /// write a library change derived from this account's data (a directory result's e-mail).
    pub fn commit_for_generation<T>(
        &self,
        id: IntegrationId,
        generation: u64,
        commit: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if id.is_graph() {
            return Err(service_error(
                "account_required",
                "directory commits require an account handle",
            ));
        }
        self.state(id, |_, s| {
            Self::fence(id, generation, s)?;
            commit()
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SetupInstructions {
    pub integration: IntegrationId,
    pub config_table: String,
    pub instructions: String,
    pub scopes: Vec<String>,
}

/// Offline instructions; never opens a browser or touches another application's credentials.
pub fn setup(id: IntegrationId, settings: &IntegrationSettings) -> SetupInstructions {
    let instructions = match id {
        IntegrationId::SystemCalendar => {
            "Recommended on macOS: uses calendars already synced in the macOS Calendar app (Google, Exchange, iCloud…), with no client ID, OAuth app or password. Connect from Settings or `broscribe integrations login system-calendar`: the BroScribe agent asks macOS for Full Calendar Access (macOS offers no read-only level; BroScribe only reads). Then choose your work calendars with `broscribe integrations setup system-calendar --calendar-id <ID>`; nothing is read until you do. Titles and attendees of the selected calendars' next 30 days are copied to a private cache readable only by you and may appear as meeting metadata. Disconnect at any time with `broscribe integrations logout system-calendar`."
        }
        IntegrationId::MicrosoftGraph => {
            "Register your own Microsoft Entra application as a public/native client; BroScribe ships no client ID. Register http://127.0.0.1 and http://localhost loopback redirects (ephemeral ports), enable public client flows, and configure client_id. Request only the listed delegated read-only scopes; your tenant may require administrator consent. Verify Entra's 127.0.0.1 loopback registration in your tenant."
        }
        IntegrationId::Google => {
            "Create your own Google Cloud OAuth desktop client, enable Calendar API and (Workspace only) People API, then configure client_id. Enter the desktop client secret in Settings → Integrations, or pipe it to `broscribe integrations setup google --client-secret-stdin`; it is kept in the system secret store, never in config.toml. Register the desktop loopback flow http://127.0.0.1:<ephemeral port>. After signing in, choose the calendars to read; nothing is read until you do. Enable workspace only for Workspace directory access; administrators may restrict it."
        }
    };
    SetupInstructions {
        integration: id,
        config_table: format!("[integration.{id}]"),
        instructions: instructions.into(),
        scopes: scopes(id, settings),
    }
}
