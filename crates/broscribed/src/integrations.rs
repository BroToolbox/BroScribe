//! Account maintenance never holds a library lock and never starts sign-in.
//! EventKit calendars are read only here, in the helper that owns their macOS permission.
use crate::{
    Daemon,
    daemon::{Shared, lock},
};
use broscribe_core::calendar::{AccountAuthority, AccountRef, GoogleCalendarStamp};
use broscribe_integrations::{
    IntegrationId, IntegrationStatus, Integrations, IntegrationsConfig, PublicError, SyncRow,
};
use broscribe_ipc::{IpcError, NotificationRequest, SystemCalendarAction};
use broscribe_platform::{BrowserLauncher, CalendarProvider, SecretStore, SystemCalendar};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// System calendars are local and cheap to read: refreshed on the one-minute maintenance
/// tick (with margin for its own duration), not on the remote providers' daily schedule.
const NATIVE_SYNC_EVERY: chrono::TimeDelta = chrono::TimeDelta::seconds(55);

/// Maintenance seam; production delegates expiry/daily refresh policy to the
/// integration service. Tests provide a local fake, never remote accounts.
pub trait IntegrationMaintenance: Send + Sync {
    fn reload(&self) -> Result<(), IpcError>;
    fn keep_alive(&self) -> Result<Vec<NotificationRequest>, IpcError>;
    /// Every report row is a [`SyncRow`]: counts and sanitized failures only.
    fn sync(&self, integration: Option<&str>, force: bool) -> Result<Vec<SyncRow>, IpcError>;
    /// An explicit native-calendar action of one provider; only `Connect` may ask macOS
    /// for that provider's permission.
    fn native_calendar(
        &self,
        provider: CalendarProvider,
        action: SystemCalendarAction,
    ) -> Result<IntegrationStatus, IpcError>;
    /// The live generation of one provider's opt-in while macOS grants it, checked by
    /// every consumer of that provider's snapshot events. Never requests permission.
    fn native_generation(&self, provider: CalendarProvider) -> Result<Option<u64>, IpcError>;
    /// The generation under which a native provider's cached calendar may enter a
    /// recording snapshot: that provider's live authority. Google answers only
    /// [`Self::google_authority`]; Microsoft only [`Self::remote_authority`].
    fn calendar_generation(&self, id: IntegrationId) -> Result<Option<u64>, IpcError>;
    /// Google's live account and calendar-selection generations, only while signed in with
    /// the configured client (never a legacy, unknown or other client's cache). Read-only.
    fn google_authority(&self) -> Result<Option<GoogleCalendarStamp>, IpcError>;
    fn remote_authority(&self, account: &AccountRef) -> Result<Option<AccountAuthority>, IpcError>;
    fn remote_calendar_events(
        &self,
    ) -> Result<Vec<broscribe_integrations::CalendarEvent>, IpcError>;
    fn list_accounts(&self) -> Result<Vec<broscribe_integrations::MicrosoftAccount>, IpcError>;
}

/// Account I/O boundary: tests drive the real maintenance scheduler without
/// credentials, provider HTTP or wall-clock delays.
trait AccountService: Send + Sync {
    fn keep_alive(&self, id: IntegrationId) -> broscribe_integrations::Result<()>;
    fn state(
        &self,
        id: IntegrationId,
    ) -> broscribe_integrations::Result<broscribe_integrations::AccountState>;
    /// Reads one provider-wide calendar; the events its published copy holds.
    fn sync_calendar(
        &self,
        id: IntegrationId,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
    ) -> broscribe_integrations::Result<usize>;
    fn native_calendar(
        &self,
        provider: CalendarProvider,
        action: SystemCalendarAction,
    ) -> broscribe_integrations::Result<IntegrationStatus>;
    fn native_generation(
        &self,
        provider: CalendarProvider,
    ) -> broscribe_integrations::Result<Option<u64>>;
    fn calendar_generation(&self, id: IntegrationId)
    -> broscribe_integrations::Result<Option<u64>>;
    fn google_authority(&self) -> broscribe_integrations::Result<Option<GoogleCalendarStamp>>;
    fn list_accounts(
        &self,
    ) -> broscribe_integrations::Result<Vec<broscribe_integrations::MicrosoftAccount>>;
    fn keep_account_alive(&self, account: &AccountRef) -> broscribe_integrations::Result<()>;
    fn sync_account(
        &self,
        account: &AccountRef,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
    ) -> broscribe_integrations::Result<broscribe_integrations::AccountCalendarSync>;
    fn account_authority(
        &self,
        account: &AccountRef,
    ) -> broscribe_integrations::Result<Option<AccountAuthority>>;
    fn cached_account_calendar(
        &self,
        account: &AccountRef,
    ) -> broscribe_integrations::Result<broscribe_integrations::CalendarCache>;
}
impl AccountService for Integrations {
    fn keep_alive(&self, id: IntegrationId) -> broscribe_integrations::Result<()> {
        Integrations::keep_alive(self, id)
    }
    fn state(
        &self,
        id: IntegrationId,
    ) -> broscribe_integrations::Result<broscribe_integrations::AccountState> {
        self.account_state(id)
    }
    fn sync_calendar(
        &self,
        id: IntegrationId,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
    ) -> broscribe_integrations::Result<usize> {
        Integrations::sync_calendar(self, id, from, to).map(|cache| cache.events.len())
    }
    fn native_calendar(
        &self,
        provider: CalendarProvider,
        action: SystemCalendarAction,
    ) -> broscribe_integrations::Result<IntegrationStatus> {
        let id = IntegrationId::of_provider(provider);
        match action {
            SystemCalendarAction::Status => self.status(id),
            SystemCalendarAction::Connect => self.connect_system_calendar(provider),
            SystemCalendarAction::Configure { calendar_ids } => {
                self.configure_native_calendars(provider, &calendar_ids)
            }
            SystemCalendarAction::Disconnect => {
                self.logout(id)?;
                self.status(id)
            }
        }
    }
    fn native_generation(
        &self,
        provider: CalendarProvider,
    ) -> broscribe_integrations::Result<Option<u64>> {
        Integrations::native_generation(self, provider)
    }
    fn calendar_generation(
        &self,
        id: IntegrationId,
    ) -> broscribe_integrations::Result<Option<u64>> {
        Integrations::calendar_generation(self, id)
    }
    fn google_authority(&self) -> broscribe_integrations::Result<Option<GoogleCalendarStamp>> {
        Integrations::google_authority(self)
    }
    fn list_accounts(
        &self,
    ) -> broscribe_integrations::Result<Vec<broscribe_integrations::MicrosoftAccount>> {
        Integrations::list_accounts(self, None)
    }
    fn keep_account_alive(&self, account: &AccountRef) -> broscribe_integrations::Result<()> {
        Integrations::keep_account_alive(self, account)
    }
    fn sync_account(
        &self,
        account: &AccountRef,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
    ) -> broscribe_integrations::Result<broscribe_integrations::AccountCalendarSync> {
        Integrations::sync_account(self, account, from, to)
    }
    fn account_authority(
        &self,
        account: &AccountRef,
    ) -> broscribe_integrations::Result<Option<AccountAuthority>> {
        Integrations::account_authority(self, account)
    }
    fn cached_account_calendar(
        &self,
        account: &AccountRef,
    ) -> broscribe_integrations::Result<broscribe_integrations::CalendarCache> {
        Integrations::cached_account_calendar(self, account)
    }
}
pub struct AccountMaintenance {
    data_dir: PathBuf,
    secrets: Arc<dyn SecretStore>,
    browser: Arc<dyn BrowserLauncher>,
    service: Mutex<Option<Arc<dyn AccountService>>>,
    notifications: Arc<Mutex<Vec<NotificationRequest>>>,
    now: Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>,
    /// Failed automatic calendar syncs per account, for exponential backoff.
    sync_failures: Mutex<std::collections::HashMap<SyncContext, SyncFailure>>,
    /// The helper's system-calendar authority; `None` reports `helper_required`.
    system_calendar: Option<Arc<dyn SystemCalendar>>,
    /// The platform TLS strategy; `None` retains the constructor's public-root
    /// transport. Production entry points inject their native trust store.
    trust_store: Option<Arc<dyn broscribe_platform::TrustStore>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SyncContext {
    Provider(IntegrationId),
    Account(AccountRef),
}

fn provider_contexts() -> impl Iterator<Item = IntegrationId> {
    IntegrationId::ALL
        .into_iter()
        .filter(|id| id.is_native_calendar() || *id == IntegrationId::Google)
}

/// A failing account is retried after 2, 4, 8 … minutes, at most hourly, rather
/// than on every one-minute maintenance tick; a forced (user) sync ignores it.
/// The backoff belongs to one sign-in: a new account generation (re-sign-in or
/// replacement) or a configuration reload starts afresh.
#[derive(Clone, Copy)]
struct SyncFailure {
    generation: u64,
    selection_generation: u64,
    last_sync_attempt: chrono::DateTime<chrono::Utc>,
    failures: u32,
}
impl SyncFailure {
    fn retry_at(self) -> chrono::DateTime<chrono::Utc> {
        let minutes = 1i64 << self.failures.min(6);
        self.last_sync_attempt + chrono::Duration::minutes(minutes.min(60))
    }
}

fn sign_in_notice(request: broscribe_integrations::ReauthNotification) -> NotificationRequest {
    NotificationRequest {
        body: request.message,
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Sign-In Required",
            "Sign-in required",
        ),
        id: match &request.account {
            Some(account) => format!(
                "integration-reauth-{}-{}",
                request.integration.as_str(),
                account.account_id.as_str()
            ),
            None => format!("integration-reauth-{}", request.integration.as_str()),
        },
        actions: vec![broscribe_ipc::NotificationButton {
            id: "open".into(),
            label: "Sign in".into(),
        }],
        meeting_id: None,
    }
}

impl AccountMaintenance {
    pub fn new(
        data_dir: PathBuf,
        secrets: Arc<dyn SecretStore>,
        browser: Arc<dyn BrowserLauncher>,
    ) -> Self {
        Self {
            data_dir,
            secrets,
            browser,
            service: Mutex::new(None),
            notifications: Arc::new(Mutex::new(Vec::new())),
            now: Arc::new(chrono::Utc::now),
            sync_failures: Mutex::new(std::collections::HashMap::new()),
            system_calendar: None,
            trust_store: None,
        }
    }

    /// Injects the helper's macOS calendar authority (the only process that has it).
    pub fn with_system_calendar(mut self, calendar: Arc<dyn SystemCalendar>) -> Self {
        self.system_calendar = Some(calendar);
        self
    }

    /// Injects the platform TLS strategy. An unavailable probe refuses provider
    /// traffic before connecting, without affecting reload, local calendars,
    /// status, sign-out or removal. Later native verification failures never
    /// substitute another root set.
    pub fn with_trust_store(mut self, store: Arc<dyn broscribe_platform::TrustStore>) -> Self {
        self.trust_store = Some(store);
        self
    }

    fn integrations(&self, config: IntegrationsConfig) -> Result<Integrations, IpcError> {
        let events = self.notifications.clone();
        let service = Integrations::new(
            &self.data_dir,
            config,
            self.secrets.clone(),
            self.browser.clone(),
            Arc::new(move |request| {
                lock(&events).push(sign_in_notice(request));
            }),
        )
        .map_err(public_error)?;
        let service = match &self.trust_store {
            Some(store) => service.with_trust_store(store.as_ref()),
            None => service,
        };
        let service = match &self.system_calendar {
            Some(calendar) => service.with_system_calendar(calendar.clone()),
            None => service,
        };
        Ok(service)
    }

    /// Native calendars need no `config.toml` settings: when no configuration is loaded (for
    /// example a broken file), a configuration-free service still serves them, so disconnect
    /// always works.
    fn native_service(&self) -> Result<Arc<dyn AccountService>, IpcError> {
        if let Some(service) = lock(&self.service).clone() {
            return Ok(service);
        }
        Ok(Arc::new(self.integrations(IntegrationsConfig::default())?))
    }
}
impl IntegrationMaintenance for AccountMaintenance {
    fn reload(&self) -> Result<(), IpcError> {
        // One malformed `[integration.<name>]` table disables only that
        // integration; the others and the native calendars keep working.
        // The document's own parse error may quote setting values: only its code travels.
        let (config, invalid) = IntegrationsConfig::load_isolated(&self.data_dir).map_err(|e| {
            IpcError::new(e.code(), "integration configuration could not be loaded")
        })?;
        if !invalid.is_empty() {
            // Table names only: the messages may quote setting values.
            tracing::warn!(
                tables = ?invalid.keys().collect::<Vec<_>>(),
                "invalid integration settings ignored"
            );
        }
        let service = self.integrations(config)?;
        *lock(&self.service) = Some(Arc::new(service));
        lock(&self.sync_failures).clear();
        Ok(())
    }
    fn keep_alive(&self) -> Result<Vec<NotificationRequest>, IpcError> {
        let service = lock(&self.service).clone().ok_or_else(|| {
            IpcError::new(
                "not_configured",
                "integration maintenance has not been loaded",
            )
        })?;
        for id in provider_contexts() {
            // One account failing never prevents the others from refreshing.
            if let Err(error) = service.keep_alive(id) {
                tracing::warn!(code = error.code(), "integration maintenance failed");
            }
        }
        let accounts = service.list_accounts().unwrap_or_else(|error| {
            tracing::warn!(
                code = error.code(),
                "account registry maintenance unavailable"
            );
            Vec::new()
        });
        for account in accounts {
            if account.access_error.as_deref() == Some("conditional_access_blocked") {
                continue;
            }
            if let Err(error) = service.keep_account_alive(&account.account) {
                tracing::warn!(code = error.code(), "account maintenance failed");
            }
        }
        Ok(std::mem::take(&mut *lock(&self.notifications)))
    }

    fn sync(&self, integration: Option<&str>, force: bool) -> Result<Vec<SyncRow>, IpcError> {
        let service = lock(&self.service).clone().ok_or_else(|| {
            IpcError::new(
                "not_configured",
                "integration maintenance has not been loaded",
            )
        })?;
        let selected = integration
            .map(IntegrationId::parse)
            .transpose()
            .map_err(public_error)?;
        let now = (self.now)();
        let (from, to) = (
            now - chrono::Duration::days(1),
            now + chrono::Duration::days(30),
        );
        let report_requested = force || selected.is_some();
        let mut results = Vec::new();
        let mut contexts = Vec::new();
        for id in provider_contexts() {
            if selected.is_some_and(|selected| selected != id) {
                continue;
            }
            let state = match service.state(id) {
                Ok(state) => state,
                // One account failing never prevents the others from syncing.
                Err(error) if selected.is_none() => {
                    tracing::warn!(code = error.code(), "reading integration state failed");
                    continue;
                }
                Err(error) => return Err(public_error(error)),
            };
            if state.status != broscribe_integrations::AccountStatus::Active {
                // Background maintenance skips an inactive source. A request naming it gets
                // the service's own direct answer (`signed_out`, `reauth_required`), given
                // from durable state before any credential or provider use, as without the
                // agent; it never reads as an empty success.
                if selected.is_some() {
                    let events = service.sync_calendar(id, from, to).map_err(public_error)?;
                    results.push(SyncRow::provider(id, events));
                }
                continue;
            }
            // Google's calendar reselection, like a new sign-in, starts its backoff afresh.
            let selection_generation = state.selection_generation;
            contexts.push((
                SyncContext::Provider(id),
                id,
                state,
                selection_generation,
                None,
            ));
        }
        let accounts = match service.list_accounts() {
            Ok(accounts) => accounts,
            Err(error) if selected == Some(IntegrationId::MicrosoftGraph) => {
                return Err(public_error(error));
            }
            Err(error) => {
                tracing::warn!(code = error.code(), "account registry sync unavailable");
                Vec::new()
            }
        };
        for account in accounts {
            let id = broscribe_integrations::integration_id(account.account.integration);
            if selected.is_some_and(|selected| selected != id) {
                continue;
            }
            let unavailable =
                if account.access_error.as_deref() == Some("conditional_access_blocked") {
                    Some("conditional_access_blocked")
                } else if account.status != broscribe_integrations::AccountStatus::Active {
                    Some("reauth_required")
                } else {
                    None
                };
            if unavailable.is_some() && !report_requested {
                continue;
            }
            contexts.push((
                SyncContext::Account(account.account),
                id,
                broscribe_integrations::AccountState {
                    status: account.status,
                    generation: account.generation,
                    last_sync: account.last_sync,
                    ..Default::default()
                },
                account.selection_generation,
                unavailable,
            ));
        }
        let mut requested_provider_error = None;
        for (context, id, state, selection_generation, unavailable) in contexts {
            if let Some(code) = unavailable {
                if let SyncContext::Account(account) = context {
                    let message = if code == "conditional_access_blocked" {
                        "Microsoft Conditional Access blocked this account; ask your organisation's administrator to inspect the sign-in logs."
                    } else {
                        "Reconnect this Microsoft account explicitly to restore access."
                    };
                    results.push(SyncRow::AccountFailed {
                        account,
                        error: PublicError {
                            code: code.to_owned(),
                            message: message.to_owned(),
                        },
                    });
                }
                continue;
            }
            let every = match id.calendar_provider() {
                Some(CalendarProvider::EventKit) => NATIVE_SYNC_EVERY,
                None => chrono::Duration::days(1),
            };
            let stale = state.last_sync.is_none_or(|at| now - at >= every);
            let (needs_retry, backing_off) = {
                let mut failures = lock(&self.sync_failures);
                if failures.get(&context).is_some_and(|failure| {
                    failure.generation != state.generation
                        || failure.selection_generation != selection_generation
                }) {
                    failures.remove(&context);
                }
                let failure = failures.get(&context);
                (
                    failure.is_some(),
                    failure.is_some_and(|failure| now < failure.retry_at()),
                )
            };
            if force || ((stale || needs_retry) && !backing_off) {
                let result = match &context {
                    SyncContext::Provider(id) => service
                        .sync_calendar(*id, from, to)
                        .map(|events| (SyncRow::provider(*id, events), true)),
                    SyncContext::Account(account) => {
                        service.sync_account(account, from, to).map(|outcome| {
                            use broscribe_integrations::{
                                AccountSyncOutcome, CalendarTargetOutcome,
                            };
                            for target in &outcome.targets {
                                if let CalendarTargetOutcome::Denied { code }
                                | CalendarTargetOutcome::Error { code } = &target.outcome
                                {
                                    tracing::warn!(
                                        code = code.as_str(),
                                        "selected calendar read incomplete"
                                    );
                                }
                            }
                            let complete = matches!(
                                outcome.outcome,
                                AccountSyncOutcome::Available | AccountSyncOutcome::NoSelection
                            );
                            (SyncRow::Account(outcome), complete)
                        })
                    }
                };
                let complete = match result {
                    Ok((row, complete)) => {
                        if report_requested {
                            results.push(row);
                        }
                        complete
                    }
                    Err(error) => {
                        if report_requested {
                            results.push(match &context {
                                SyncContext::Account(account) => {
                                    SyncRow::account_failed(account.clone(), &error)
                                }
                                SyncContext::Provider(id) => SyncRow::provider_failed(*id, &error),
                            });
                        }
                        tracing::warn!(code = error.code(), "calendar sync failed");
                        if selected.is_some() && id != IntegrationId::MicrosoftGraph {
                            requested_provider_error = Some(public_error(error));
                        }
                        false
                    }
                };
                if complete {
                    lock(&self.sync_failures).remove(&context);
                } else {
                    let mut failures = lock(&self.sync_failures);
                    let failure = failures.entry(context).or_insert(SyncFailure {
                        generation: state.generation,
                        selection_generation,
                        last_sync_attempt: now,
                        failures: 0,
                    });
                    failure.last_sync_attempt = now;
                    failure.failures = failure.failures.saturating_add(1);
                }
            }
        }
        requested_provider_error.map_or(Ok(results), Err)
    }

    fn native_calendar(
        &self,
        provider: CalendarProvider,
        action: SystemCalendarAction,
    ) -> Result<IntegrationStatus, IpcError> {
        self.native_service()?
            .native_calendar(provider, action)
            .map_err(public_error)
    }

    fn native_generation(&self, provider: CalendarProvider) -> Result<Option<u64>, IpcError> {
        self.native_service()?
            .native_generation(provider)
            .map_err(public_error)
    }

    fn calendar_generation(&self, id: IntegrationId) -> Result<Option<u64>, IpcError> {
        // The service itself refuses Google (`google_stamp_required`) and Microsoft
        // (`account_required`): their authority is never a single scalar.
        self.native_service()?
            .calendar_generation(id)
            .map_err(public_error)
    }
    fn google_authority(&self) -> Result<Option<GoogleCalendarStamp>, IpcError> {
        // Without a loaded configuration the configured client is unknown: nothing remote
        // is published.
        let Some(service) = lock(&self.service).clone() else {
            return Ok(None);
        };
        service.google_authority().map_err(public_error)
    }
    fn remote_authority(&self, account: &AccountRef) -> Result<Option<AccountAuthority>, IpcError> {
        let Some(service) = lock(&self.service).clone() else {
            return Ok(None);
        };
        let authority = service.account_authority(account).map_err(public_error)?;
        if authority
            .as_ref()
            .is_some_and(|live| live.account != *account)
        {
            return Err(IpcError::new(
                "calendar_provenance_unverified",
                "calendar authority belongs to another account",
            ));
        }
        Ok(authority)
    }
    fn list_accounts(&self) -> Result<Vec<broscribe_integrations::MicrosoftAccount>, IpcError> {
        let Some(service) = lock(&self.service).clone() else {
            return Ok(Vec::new());
        };
        service.list_accounts().map_err(public_error)
    }

    /// Each active account's cached events, as `cached_account_calendar` publishes them:
    /// filtered by the account's live authority and fenced by a second authority read after
    /// the file read, so one pass reads each account's authority exactly where it is
    /// checked. Generation, selection and route are rechecked again before publication.
    fn remote_calendar_events(
        &self,
    ) -> Result<Vec<broscribe_integrations::CalendarEvent>, IpcError> {
        let Some(service) = lock(&self.service).clone() else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        for account in service.list_accounts().map_err(public_error)? {
            if account.status != broscribe_integrations::AccountStatus::Active
                || account.access_error.as_deref() == Some("conditional_access_blocked")
            {
                continue;
            }
            match service.cached_account_calendar(&account.account) {
                Ok(cache) => events.extend(cache.events),
                Err(error) => {
                    tracing::warn!(code = error.code(), "account calendar snapshot unavailable")
                }
            }
        }
        Ok(events)
    }
}

/// The failure's stable code and sanitized message ([`PublicError`]): never raw provider
/// text, tokens or local paths.
fn public_error(error: broscribe_integrations::IntegrationError) -> IpcError {
    let PublicError { code, message } = error.public();
    IpcError::new(code, message)
}

impl Daemon {
    pub fn set_integrations(&self, maintenance: Arc<dyn IntegrationMaintenance>) {
        *lock(&self.shared.integrations) = Some(maintenance);
    }
}

pub(crate) async fn run(shared: Arc<Shared>) {
    loop {
        let s = shared.clone();
        let notifications = tokio::task::spawn_blocking(move || {
            let service = lock(&s.integrations).clone();
            service
                .map(|service| {
                    // A failed sync never stops token refreshes.
                    if let Err(error) = service.sync(None, false) {
                        tracing::warn!(code = error.code, "calendar sync failed");
                    }
                    service.keep_alive()
                })
                .transpose()
        })
        .await;
        shared.reconcile_integration_prompts();
        // The live calendar authority query stays off the async runtime.
        let s = shared.clone();
        match tokio::task::spawn_blocking(move || snapshot_calendar(&s)).await {
            Ok(Err(error)) => tracing::warn!(code = error.code, "caching recording calendar prior"),
            Err(error) => tracing::warn!(%error, "caching recording calendar prior"),
            Ok(Ok(())) => {}
        }
        match notifications {
            Ok(Ok(Some(notifications))) => {
                for notification in notifications {
                    shared.notify(notification);
                }
            }
            Ok(Err(error)) => tracing::warn!(code = error.code, "integration maintenance failed"),
            _ => {}
        }
        tokio::select! {
            () = shared.shutdown.wait() => return,
            () = shared.integration_kick.notified() => {},
            // Service refreshes within its five-minute expiry margin and daily.
            () = tokio::time::sleep(shared.cfg.integrations_every) => {},
        }
    }
}

/// Snapshot cached, signed-in accounts into the durable session. No network;
/// worker jobs consume this private copy even after a helper restart. Blocking.
pub(crate) fn snapshot_calendar(shared: &Shared) -> Result<(), IpcError> {
    if lock(&shared.session).is_none() {
        return Ok(());
    }
    // Asked before taking the session lock: a slow permission check never holds capture.
    let service = lock(&shared.integrations).clone();
    let live = service.as_deref().map(live_calendars).unwrap_or_default();
    let remote = service
        .map(|service| service.remote_calendar_events())
        .transpose()
        .unwrap_or_else(|error| {
            tracing::warn!(code = error.code, "remote calendar snapshot unavailable");
            None
        })
        .unwrap_or_default();
    let session = lock(&shared.session);
    let Some(active) = session.as_ref() else {
        return Ok(());
    };
    write_session_calendar(
        &shared.cfg.data_dir,
        &shared.cfg.journal_root().join(active.handle.session_id()),
        chrono::Utc::now(),
        &live,
        remote,
    )
}

/// The authorities under which the provider-wide calendar caches may enter a session
/// snapshot. Native providers carry their scalar opt-in generation; Google carries its
/// account AND calendar-selection generations, never one standing in for the other.
#[derive(Debug, Default, PartialEq, Eq)]
struct LiveCalendars {
    native: BTreeMap<IntegrationId, u64>,
    google: Option<GoogleCalendarStamp>,
}

/// Each provider's publishable authority; one the maintenance cannot vouch for (no live
/// native authority, signed out, or not signed in with the configured client) is absent
/// and its events are left out. Providers are asked independently: one failing never
/// removes another's events.
fn live_calendars(service: &dyn IntegrationMaintenance) -> LiveCalendars {
    let unavailable = |integration: IntegrationId| {
        move |error: &IpcError| {
            tracing::warn!(
                code = error.code,
                integration = integration.as_str(),
                "calendar authority unavailable; leaving its events out"
            )
        }
    };
    LiveCalendars {
        native: provider_contexts()
            .filter(|id| id.is_native_calendar())
            .filter_map(|id| {
                service
                    .calendar_generation(id)
                    .inspect_err(unavailable(id))
                    .ok()
                    .flatten()
                    .map(|generation| (id, generation))
            })
            .collect(),
        google: service
            .google_authority()
            .inspect_err(unavailable(IntegrationId::Google))
            .ok()
            .flatten(),
    }
}

/// Merges the publishable accounts' calendar caches into `<session>/calendar.json`. Every
/// provider enters only under its own live authority and only while its `state.json` is
/// still at that authority: a native provider at its generation, Google at both its account
/// and calendar-selection generations. Its events must also carry that provider and stamp
/// (native `account_generation`, Google `google`), so legacy scalar-stamped, unstamped,
/// older-account or older-selection priors fail closed. A disconnect, revocation, client
/// change, reselection or unknown authority leaves that provider out instead of trusting
/// `state.json`, without touching the others.
fn write_session_calendar(
    data_dir: &std::path::Path,
    session_dir: &std::path::Path,
    now: chrono::DateTime<chrono::Utc>,
    live: &LiveCalendars,
    remote: Vec<broscribe_integrations::CalendarEvent>,
) -> Result<(), IpcError> {
    let mut cache = broscribe_integrations::CalendarCache { events: remote };
    for id in provider_contexts() {
        let google = id == IntegrationId::Google;
        let (generation, selection_generation) = if google {
            let Some(stamp) = live.google else {
                continue;
            };
            (stamp.generation, Some(stamp.selection_generation))
        } else {
            let Some(generation) = live.native.get(&id).copied() else {
                continue;
            };
            (generation, None)
        };
        let dir = broscribe_integrations::account_dir(data_dir, id);
        let state = dir.join("state.json");
        if !state.exists() {
            continue;
        }
        let state: broscribe_integrations::AccountState =
            broscribe_core::fsutil::read_json(&state).map_err(IpcError::from)?;
        if state.status != broscribe_integrations::AccountStatus::Active
            || state.generation != generation
            || selection_generation.is_some_and(|selection| state.selection_generation != selection)
        {
            continue;
        }
        let path = dir.join("calendar.json");
        if path.exists() {
            let account: broscribe_integrations::CalendarCache =
                broscribe_core::fsutil::read_json(&path).map_err(IpcError::from)?;
            cache
                .events
                .extend(account.events.into_iter().filter(|event| {
                    event.integration == id
                        && if google {
                            event.google == live.google
                        } else {
                            event.account_generation == Some(generation)
                        }
                }));
        }
    }
    cache.prune(now);
    // Every pass recomputes the admitted events from the live authorities above; only a
    // changed result is written (each write syncs the file and its directory).
    let path = session_dir.join("calendar.json");
    if path.exists()
        && broscribe_core::fsutil::read_json::<broscribe_integrations::CalendarCache>(&path)
            .is_ok_and(|current| current == cache)
    {
        return Ok(());
    }
    // Attendee names and e-mails: never readable by other users (mode 0600).
    cache.save_private(&path).map_err(public_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    // These fixtures model only the provider-wide Google/native contexts.
    macro_rules! no_remote_accounts {
        () => {
            fn list_accounts(
                &self,
            ) -> broscribe_integrations::Result<Vec<broscribe_integrations::MicrosoftAccount>> {
                Ok(Vec::new())
            }
            fn keep_account_alive(&self, _: &AccountRef) -> broscribe_integrations::Result<()> {
                panic!("no remote accounts in this fixture")
            }
            fn sync_account(
                &self,
                _: &AccountRef,
                _: chrono::DateTime<chrono::Utc>,
                _: chrono::DateTime<chrono::Utc>,
            ) -> broscribe_integrations::Result<broscribe_integrations::AccountCalendarSync> {
                panic!("no remote accounts in this fixture")
            }
            fn account_authority(
                &self,
                _: &AccountRef,
            ) -> broscribe_integrations::Result<Option<AccountAuthority>> {
                panic!("no remote accounts in this fixture")
            }
            fn cached_account_calendar(
                &self,
                _: &AccountRef,
            ) -> broscribe_integrations::Result<broscribe_integrations::CalendarCache> {
                panic!("no remote accounts in this fixture")
            }
        };
    }
    fn no_system_calendar<T>() -> broscribe_integrations::Result<T> {
        Err(broscribe_integrations::IntegrationError::Service(
            "synthetic accounts have no system calendar".into(),
        ))
    }
    #[test]
    fn reloads_configuration_with_fake_credentials_and_no_sign_in() {
        let tmp = tempfile::tempdir().unwrap();
        let secrets = Arc::new(broscribe_platform::fakes::FakeSecretStore::default());
        let browser = Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default());
        let service =
            AccountMaintenance::new(tmp.path().to_path_buf(), secrets.clone(), browser.clone());
        std::fs::write(
            tmp.path().join("config.toml"),
            "[integration.google]\nclient_id=\"synthetic-client\"\n",
        )
        .unwrap();
        service.reload().unwrap();
        assert!(service.keep_alive().unwrap().is_empty());
        assert!(browser.opened.lock().unwrap().is_empty());
        assert!(secrets.secrets.lock().unwrap().is_empty());
        // IN10: a malformed `[integration.google]` table disables Google only;
        // reload succeeds, so the other integrations and native calendars stay
        // served instead of the whole service going away.
        std::fs::write(
            tmp.path().join("config.toml"),
            "[integration.google]\nclient_id=123\n[integration.microsoft-graph]\nclient_id=\"synthetic-client\"\n",
        )
        .unwrap();
        service.reload().unwrap();
        assert!(lock(&service.service).is_some());
        assert!(service.keep_alive().unwrap().is_empty());
        assert!(browser.opened.lock().unwrap().is_empty());
        // A document that is not TOML at all still fails the reload.
        std::fs::write(tmp.path().join("config.toml"), "[integration.google\n").unwrap();
        assert!(service.reload().is_err());
    }

    struct FakeAccounts {
        google: Mutex<broscribe_integrations::AccountState>,
        syncs: std::sync::atomic::AtomicUsize,
        keeps: std::sync::atomic::AtomicUsize,
        /// The service's live Google pair, or a transient failure answering it.
        google_authority: Mutex<Option<GoogleCalendarStamp>>,
        google_authority_fails: std::sync::atomic::AtomicBool,
    }
    impl AccountService for FakeAccounts {
        no_remote_accounts!();
        fn keep_alive(&self, id: IntegrationId) -> broscribe_integrations::Result<()> {
            if id == IntegrationId::Google {
                self.keeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        }
        fn state(
            &self,
            id: IntegrationId,
        ) -> broscribe_integrations::Result<broscribe_integrations::AccountState> {
            Ok(if id == IntegrationId::Google {
                lock(&self.google).clone()
            } else {
                Default::default()
            })
        }
        fn sync_calendar(
            &self,
            id: IntegrationId,
            from: chrono::DateTime<chrono::Utc>,
            to: chrono::DateTime<chrono::Utc>,
        ) -> broscribe_integrations::Result<usize> {
            assert_eq!(id, IntegrationId::Google);
            assert_eq!(to - from, chrono::Duration::days(31));
            lock(&self.google).last_sync = Some(from + chrono::Duration::days(1));
            self.syncs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(0)
        }
        fn native_calendar(
            &self,
            _: CalendarProvider,
            _: SystemCalendarAction,
        ) -> broscribe_integrations::Result<IntegrationStatus> {
            no_system_calendar()
        }
        fn native_generation(
            &self,
            _: CalendarProvider,
        ) -> broscribe_integrations::Result<Option<u64>> {
            no_system_calendar()
        }
        fn calendar_generation(
            &self,
            id: IntegrationId,
        ) -> broscribe_integrations::Result<Option<u64>> {
            assert!(
                id.is_native_calendar(),
                "Google authority is never asked as a scalar generation"
            );
            no_system_calendar()
        }
        fn google_authority(&self) -> broscribe_integrations::Result<Option<GoogleCalendarStamp>> {
            if self
                .google_authority_fails
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(broscribe_integrations::IntegrationError::Transient(
                    "synthetic busy account lock".into(),
                ));
            }
            Ok(*lock(&self.google_authority))
        }
    }
    struct Engine;
    impl broscribe_ipc::CaptureEngine for Engine {
        fn start(
            &self,
            _: broscribe_ipc::SessionConfig,
            _: broscribe_ipc::CaptureOutputs,
        ) -> broscribe_core::Result<Box<dyn broscribe_ipc::SessionHandle>> {
            Err(broscribe_core::Error::Unsupported(
                "synthetic scheduler has no capture devices".into(),
            ))
        }
        fn recover_orphans(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
        ) -> broscribe_core::Result<broscribe_ipc::OrphanRecovery> {
            Ok(broscribe_ipc::OrphanRecovery::default())
        }
    }

    #[tokio::test]
    async fn record_calendar_scheduler_syncs_stale_daily_but_not_fresh_or_signed_out_accounts() {
        use std::sync::atomic::Ordering;
        let tmp = tempfile::tempdir().unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-14T12:00:00Z")
            .unwrap()
            .to_utc();
        let clock = Arc::new(Mutex::new(now));
        let accounts = Arc::new(FakeAccounts {
            google: Mutex::new(broscribe_integrations::AccountState {
                status: broscribe_integrations::AccountStatus::Active,
                ..Default::default()
            }),
            syncs: Default::default(),
            keeps: Default::default(),
            google_authority: Default::default(),
            google_authority_fails: Default::default(),
        });
        let mut maintenance = AccountMaintenance::new(
            tmp.path().join("data"),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        );
        *lock(&maintenance.service) = Some(accounts.clone());
        maintenance.now = Arc::new({
            let clock = clock.clone();
            move || *lock(&clock)
        });
        let maintenance = Arc::new(maintenance);
        let mut cfg = crate::DaemonConfig::new(
            tmp.path().join("library"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            crate::WorkerCommand::new("/bin/false"),
        );
        cfg.integrations_every = std::time::Duration::from_millis(10);
        let daemon = Daemon::new(
            cfg,
            Arc::new(Engine),
            Arc::new(broscribe_platform::fakes::FakeProcessProbe::new(
                std::process::id(),
            )),
            Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.path().join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
        )
        .unwrap();
        daemon.set_integrations(maintenance.clone());
        let task = tokio::spawn(run(daemon.shared.clone()));
        let await_polls = |target| {
            let accounts = accounts.clone();
            async move {
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while accounts.keeps.load(Ordering::SeqCst) < target {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
            }
        };
        await_polls(2).await;
        assert_eq!(accounts.syncs.load(Ordering::SeqCst), 1);
        *lock(&clock) = now + chrono::Duration::hours(23) + chrono::Duration::minutes(59);
        await_polls(accounts.keeps.load(Ordering::SeqCst) + 2).await;
        assert_eq!(accounts.syncs.load(Ordering::SeqCst), 1);
        *lock(&clock) = now + chrono::Duration::days(1);
        await_polls(accounts.keeps.load(Ordering::SeqCst) + 2).await;
        assert_eq!(accounts.syncs.load(Ordering::SeqCst), 2);
        lock(&accounts.google).status = broscribe_integrations::AccountStatus::SignedOut;
        *lock(&clock) = now + chrono::Duration::days(3);
        await_polls(accounts.keeps.load(Ordering::SeqCst) + 2).await;
        assert_eq!(accounts.syncs.load(Ordering::SeqCst), 2);
        daemon.trigger_shutdown();
        task.await.unwrap();
        lock(&accounts.google).status = broscribe_integrations::AccountStatus::Active;
        maintenance.sync(Some("google"), true).unwrap();
        assert_eq!(accounts.syncs.load(Ordering::SeqCst), 3);
    }

    /// A recording snapshot's Google authority is the service's live account and
    /// calendar-selection pair through `google_authority`, never a scalar generation;
    /// without a loaded configuration or with an unavailable answer only Google is left out.
    #[test]
    fn record_snapshot_authority_asks_google_for_its_live_pair_only() {
        let tmp = tempfile::tempdir().unwrap();
        let accounts = Arc::new(FakeAccounts {
            google: Mutex::new(broscribe_integrations::AccountState {
                status: broscribe_integrations::AccountStatus::Active,
                ..Default::default()
            }),
            syncs: Default::default(),
            keeps: Default::default(),
            google_authority: Default::default(),
            google_authority_fails: Default::default(),
        });
        let maintenance = AccountMaintenance::new(
            tmp.path().to_path_buf(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        );
        assert_eq!(maintenance.google_authority().unwrap(), None);
        assert_eq!(live_calendars(&maintenance).google, None);
        *lock(&maintenance.service) = Some(accounts.clone());
        assert_eq!(live_calendars(&maintenance), LiveCalendars::default());
        let live = GoogleCalendarStamp {
            generation: 4,
            selection_generation: 9,
        };
        *lock(&accounts.google_authority) = Some(live);
        assert_eq!(maintenance.google_authority().unwrap(), Some(live));
        assert_eq!(
            live_calendars(&maintenance),
            LiveCalendars {
                native: BTreeMap::new(),
                google: Some(live),
            }
        );
        accounts
            .google_authority_fails
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            maintenance.google_authority().unwrap_err().code,
            "transient"
        );
        assert_eq!(live_calendars(&maintenance), LiveCalendars::default());
        // A configuration-free service never answers Google with a scalar generation.
        *lock(&maintenance.service) = None;
        assert_eq!(
            maintenance
                .calendar_generation(IntegrationId::Google)
                .unwrap_err()
                .code,
            "google_stamp_required"
        );
    }

    struct FailingAccounts(
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicU64,
        std::sync::atomic::AtomicU64,
    );
    impl AccountService for FailingAccounts {
        no_remote_accounts!();
        fn keep_alive(&self, _: IntegrationId) -> broscribe_integrations::Result<()> {
            Ok(())
        }
        fn state(
            &self,
            id: IntegrationId,
        ) -> broscribe_integrations::Result<broscribe_integrations::AccountState> {
            Ok(broscribe_integrations::AccountState {
                status: if id == IntegrationId::Google {
                    broscribe_integrations::AccountStatus::Active
                } else {
                    broscribe_integrations::AccountStatus::SignedOut
                },
                generation: self.1.load(std::sync::atomic::Ordering::SeqCst),
                selection_generation: self.2.load(std::sync::atomic::Ordering::SeqCst),
                ..Default::default()
            })
        }
        fn sync_calendar(
            &self,
            _: IntegrationId,
            _: chrono::DateTime<chrono::Utc>,
            _: chrono::DateTime<chrono::Utc>,
        ) -> broscribe_integrations::Result<usize> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(broscribe_integrations::IntegrationError::Transient(
                "synthetic outage".into(),
            ))
        }
        fn native_calendar(
            &self,
            _: CalendarProvider,
            _: SystemCalendarAction,
        ) -> broscribe_integrations::Result<IntegrationStatus> {
            no_system_calendar()
        }
        fn native_generation(
            &self,
            _: CalendarProvider,
        ) -> broscribe_integrations::Result<Option<u64>> {
            no_system_calendar()
        }
        fn calendar_generation(
            &self,
            _: IntegrationId,
        ) -> broscribe_integrations::Result<Option<u64>> {
            no_system_calendar()
        }
        fn google_authority(&self) -> broscribe_integrations::Result<Option<GoogleCalendarStamp>> {
            panic!("sync maintenance never asks for the Google snapshot authority")
        }
    }

    #[test]
    fn record_calendar_sync_failures_back_off_exponentially_up_to_an_hour() {
        use std::sync::atomic::Ordering;
        let tmp = tempfile::tempdir().unwrap();
        let start = chrono::DateTime::parse_from_rfc3339("2026-11-14T12:00:00Z")
            .unwrap()
            .to_utc();
        let clock = Arc::new(Mutex::new(start));
        let accounts = Arc::new(FailingAccounts(
            Default::default(),
            Default::default(),
            Default::default(),
        ));
        let mut maintenance = AccountMaintenance::new(
            tmp.path().to_path_buf(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        );
        *lock(&maintenance.service) = Some(accounts.clone());
        maintenance.now = Arc::new({
            let clock = clock.clone();
            move || *lock(&clock)
        });
        let at = |minutes: i64| {
            *lock(&clock) = start + chrono::Duration::minutes(minutes);
            maintenance.sync(None, false).unwrap();
            accounts.0.load(Ordering::SeqCst)
        };
        assert_eq!(at(0), 1);
        assert_eq!(at(1), 1, "one minute later is still backing off");
        assert_eq!(at(2), 2);
        assert_eq!(at(5), 2);
        assert_eq!(at(6), 3);
        // 8, 16, 32, then capped at 60 minutes between attempts.
        assert_eq!(at(14), 4);
        assert_eq!(at(30), 5);
        assert_eq!(at(62), 6);
        assert_eq!(at(121), 6);
        assert_eq!(at(122), 7);
        assert_eq!(at(181), 7);
        assert_eq!(at(182), 8);
        // A user-requested sync is never held back.
        assert!(maintenance.sync(Some("google"), true).is_err());
        assert_eq!(accounts.0.load(Ordering::SeqCst), 9);
    }

    #[test]
    fn record_calendar_sync_backoff_resets_on_re_sign_in_and_reload() {
        use std::sync::atomic::Ordering;
        let tmp = tempfile::tempdir().unwrap();
        let start = chrono::DateTime::parse_from_rfc3339("2026-11-14T12:00:00Z")
            .unwrap()
            .to_utc();
        let clock = Arc::new(Mutex::new(start));
        let accounts = Arc::new(FailingAccounts(
            Default::default(),
            1.into(),
            Default::default(),
        ));
        let mut maintenance = AccountMaintenance::new(
            tmp.path().to_path_buf(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        );
        *lock(&maintenance.service) = Some(accounts.clone());
        maintenance.now = Arc::new({
            let clock = clock.clone();
            move || *lock(&clock)
        });
        let at = |minutes: i64| {
            *lock(&clock) = start + chrono::Duration::minutes(minutes);
            maintenance.sync(None, false).unwrap();
            accounts.0.load(Ordering::SeqCst)
        };
        assert_eq!(at(0), 1);
        assert_eq!(at(1), 1, "the first sign-in is backing off");
        // Signing in again (a new generation) retries at the next tick.
        accounts.1.store(2, Ordering::SeqCst);
        assert_eq!(at(1), 2);
        assert_eq!(at(2), 2, "the new sign-in's own failure backs off");
        // Reselecting Google calendars (a new selection fence under the same sign-in)
        // retries at the next tick too, then backs off on its own failure.
        accounts.2.store(1, Ordering::SeqCst);
        assert_eq!(at(2), 3);
        assert_eq!(at(3), 3, "the new selection's own failure backs off");
        // Reloading the configuration forgets every backoff.
        maintenance.reload().unwrap();
        assert!(lock(&maintenance.sync_failures).is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn record_session_calendar_snapshot_and_journal_are_private() {
        use broscribe_integrations::{AccountState, AccountStatus, CalendarCache, CalendarEvent};
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let now = chrono::Utc::now();
        let account = broscribe_integrations::account_dir(&data, IntegrationId::Google);
        std::fs::create_dir_all(&account).unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &account.join("state.json"),
            &AccountState {
                status: AccountStatus::Active,
                generation: 2,
                selection_generation: 5,
                ..Default::default()
            },
        )
        .unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &account.join("calendar.json"),
            &CalendarCache {
                events: vec![CalendarEvent {
                    id: "synthetic".into(),
                    integration: IntegrationId::Google,
                    title: Some("Synthetic planning".into()),
                    start: now,
                    end: now + chrono::Duration::hours(1),
                    join_links: Vec::new(),
                    app: None,
                    attendees: vec![broscribe_integrations::Attendee {
                        name: Some("Casey Example".into()),
                        email: Some("casey@example.invalid".into()),
                        is_self: false,
                    }],
                    account_generation: None,
                    google: Some(GoogleCalendarStamp {
                        generation: 2,
                        selection_generation: 5,
                    }),
                    remote: None,
                    ical_uid: None,
                    occurrence_id: None,
                }],
            },
        )
        .unwrap();
        let root = data.join("journal");
        // An older journal root left world-readable is tightened.
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let journal = broscribe_capture::journal::JournalWriter::create(
            &root,
            "synthetic-session",
            Default::default(),
            2.0,
            std::process::id(),
        )
        .unwrap();
        let live = LiveCalendars {
            native: BTreeMap::new(),
            google: Some(GoogleCalendarStamp {
                generation: 2,
                selection_generation: 5,
            }),
        };
        write_session_calendar(&data, journal.dir(), now, &live, Vec::new()).unwrap();
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let snapshot = journal.dir().join("calendar.json");
        assert_eq!(mode(&snapshot), 0o600);
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(journal.dir()), 0o700);
        for track in ["mic", "system"] {
            assert_eq!(mode(&journal.dir().join(track)), 0o700);
        }
        let cache: CalendarCache = broscribe_core::fsutil::read_json(&snapshot).unwrap();
        assert_eq!(cache.events.len(), 1);
        let leftovers = std::fs::read_dir(journal.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    /// Google's state cannot be read (e.g. its lock is busy); the others work.
    #[derive(Default)]
    struct OneBrokenAccount {
        synced: Mutex<Vec<IntegrationId>>,
        kept: Mutex<Vec<IntegrationId>>,
    }
    impl AccountService for OneBrokenAccount {
        no_remote_accounts!();
        fn keep_alive(&self, id: IntegrationId) -> broscribe_integrations::Result<()> {
            lock(&self.kept).push(id);
            Ok(())
        }
        fn state(
            &self,
            id: IntegrationId,
        ) -> broscribe_integrations::Result<broscribe_integrations::AccountState> {
            if id == IntegrationId::Google {
                return Err(broscribe_integrations::IntegrationError::Transient(
                    "synthetic busy account lock".into(),
                ));
            }
            Ok(broscribe_integrations::AccountState {
                status: broscribe_integrations::AccountStatus::Active,
                ..Default::default()
            })
        }
        fn sync_calendar(
            &self,
            id: IntegrationId,
            _: chrono::DateTime<chrono::Utc>,
            _: chrono::DateTime<chrono::Utc>,
        ) -> broscribe_integrations::Result<usize> {
            lock(&self.synced).push(id);
            Ok(0)
        }
        fn native_calendar(
            &self,
            _: CalendarProvider,
            _: SystemCalendarAction,
        ) -> broscribe_integrations::Result<IntegrationStatus> {
            no_system_calendar()
        }
        fn native_generation(
            &self,
            _: CalendarProvider,
        ) -> broscribe_integrations::Result<Option<u64>> {
            no_system_calendar()
        }
        fn calendar_generation(
            &self,
            _: IntegrationId,
        ) -> broscribe_integrations::Result<Option<u64>> {
            no_system_calendar()
        }
        fn google_authority(&self) -> broscribe_integrations::Result<Option<GoogleCalendarStamp>> {
            panic!("sync maintenance never asks for the Google snapshot authority")
        }
    }

    #[test]
    fn one_unreadable_account_neither_stops_other_syncs_nor_token_refreshes() {
        let tmp = tempfile::tempdir().unwrap();
        let accounts = Arc::new(OneBrokenAccount::default());
        let maintenance = AccountMaintenance::new(
            tmp.path().to_path_buf(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        );
        *lock(&maintenance.service) = Some(accounts.clone());
        maintenance.sync(None, false).unwrap();
        assert!(!lock(&accounts.synced).contains(&IntegrationId::Google));
        assert_eq!(*lock(&accounts.synced), vec![IntegrationId::SystemCalendar]);
        // An explicitly selected account still reports its own failure.
        assert!(maintenance.sync(Some("google"), true).is_err());
        maintenance.keep_alive().unwrap();
        assert_eq!(
            *lock(&accounts.kept),
            vec![IntegrationId::SystemCalendar, IntegrationId::Google]
        );
    }

    /// Maintenance whose calendar sync always fails.
    #[derive(Default)]
    struct SyncFails {
        keeps: std::sync::atomic::AtomicUsize,
    }
    impl IntegrationMaintenance for SyncFails {
        fn list_accounts(&self) -> Result<Vec<broscribe_integrations::MicrosoftAccount>, IpcError> {
            Ok(Vec::new())
        }
        fn remote_authority(&self, _: &AccountRef) -> Result<Option<AccountAuthority>, IpcError> {
            Ok(None)
        }
        fn remote_calendar_events(
            &self,
        ) -> Result<Vec<broscribe_integrations::CalendarEvent>, IpcError> {
            Ok(Vec::new())
        }
        fn reload(&self) -> Result<(), IpcError> {
            Ok(())
        }
        fn keep_alive(&self) -> Result<Vec<NotificationRequest>, IpcError> {
            self.keeps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Vec::new())
        }
        fn sync(&self, _: Option<&str>, _: bool) -> Result<Vec<SyncRow>, IpcError> {
            Err(IpcError::new("transient", "synthetic sync failure"))
        }
        fn native_calendar(
            &self,
            _: CalendarProvider,
            _: SystemCalendarAction,
        ) -> Result<IntegrationStatus, IpcError> {
            Err(IpcError::new("not_configured", "synthetic maintenance"))
        }
        fn native_generation(&self, _: CalendarProvider) -> Result<Option<u64>, IpcError> {
            Err(IpcError::new("not_configured", "synthetic maintenance"))
        }
        fn calendar_generation(&self, _: IntegrationId) -> Result<Option<u64>, IpcError> {
            Err(IpcError::new("not_configured", "synthetic maintenance"))
        }
        fn google_authority(&self) -> Result<Option<GoogleCalendarStamp>, IpcError> {
            Err(IpcError::new("not_configured", "synthetic maintenance"))
        }
    }

    #[tokio::test]
    async fn scheduler_refreshes_tokens_even_when_calendar_sync_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = crate::DaemonConfig::new(
            tmp.path().join("library"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            crate::WorkerCommand::new("/bin/false"),
        );
        cfg.integrations_every = std::time::Duration::from_millis(10);
        let daemon = Daemon::new(
            cfg,
            Arc::new(Engine),
            Arc::new(broscribe_platform::fakes::FakeProcessProbe::new(
                std::process::id(),
            )),
            Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.path().join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
        )
        .unwrap();
        let maintenance = Arc::new(SyncFails::default());
        daemon.set_integrations(maintenance.clone());
        let task = tokio::spawn(run(daemon.shared.clone()));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while maintenance.keeps.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        daemon.trigger_shutdown();
        task.await.unwrap();
    }

    /// Native events enter the session snapshot only stamped with their provider's live
    /// generation; Google events only stamped with the live account AND calendar-selection
    /// pair while `state.json` is at both. Legacy scalar-stamped, unstamped, older-account
    /// and older-selection Google priors fail closed, and neither provider's stamp stands
    /// in for the other's.
    #[test]
    fn record_session_snapshot_admits_events_only_under_their_live_authority() {
        use broscribe_integrations::{AccountState, AccountStatus, CalendarCache, CalendarEvent};
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let now = chrono::Utc::now();
        let event = |id: &str, integration, account_generation, google| CalendarEvent {
            id: id.into(),
            integration,
            title: Some("Synthetic sync".into()),
            start: now,
            end: now + chrono::Duration::hours(1),
            join_links: Vec::new(),
            app: None,
            attendees: Vec::new(),
            account_generation,
            google,
            remote: None,
            ical_uid: None,
            occurrence_id: None,
        };
        let seed = |id: IntegrationId,
                    generation: u64,
                    selection_generation: u64,
                    events: Vec<CalendarEvent>| {
            let dir = broscribe_integrations::account_dir(&data, id);
            std::fs::create_dir_all(&dir).unwrap();
            broscribe_core::fsutil::write_json_atomic(
                &dir.join("state.json"),
                &AccountState {
                    generation,
                    selection_generation,
                    status: AccountStatus::Active,
                    ..Default::default()
                },
            )
            .unwrap();
            broscribe_core::fsutil::write_json_atomic(
                &dir.join("calendar.json"),
                &CalendarCache { events },
            )
            .unwrap();
        };
        let pair = |generation, selection_generation| GoogleCalendarStamp {
            generation,
            selection_generation,
        };
        let native = IntegrationId::SystemCalendar;
        let google = IntegrationId::Google;
        let current = pair(1, 5);
        seed(
            native,
            3,
            0,
            vec![
                event("live", native, Some(3), None),
                event("previous-selection", native, Some(2), None),
                event("unstamped", native, None, None),
                event("native-with-google-pair", native, None, Some(pair(3, 0))),
            ],
        );
        seed(
            google,
            current.generation,
            current.selection_generation,
            vec![
                event("google", google, None, Some(current)),
                event("google-unstamped", google, None, None),
                // A pre-pair prior stamped with the account generation only.
                event("google-legacy-scalar", google, Some(1), None),
                event("google-previous-selection", google, None, Some(pair(1, 4))),
                event("google-previous-account", google, None, Some(pair(0, 5))),
                event("native-in-google-cache", native, Some(3), None),
            ],
        );
        let session = tmp.path().join("session");
        let snapshot = |native_live: Option<u64>, google_live: Option<GoogleCalendarStamp>| {
            let live = LiveCalendars {
                native: native_live.map(|live| (native, live)).into_iter().collect(),
                google: google_live,
            };
            write_session_calendar(&data, &session, now, &live, Vec::new()).unwrap();
            let cache: CalendarCache =
                broscribe_core::fsutil::read_json(&session.join("calendar.json")).unwrap();
            cache.events.into_iter().map(|e| e.id).collect::<Vec<_>>()
        };
        assert_eq!(snapshot(Some(3), Some(current)), ["live", "google"]);
        // Revoked, disconnected or unknown authority: state.json alone never admits them.
        assert!(snapshot(None, None).is_empty());
        // A live generation the cached account no longer matches admits nothing either.
        assert_eq!(snapshot(Some(4), Some(current)), ["google"]);
        // Google reselected its calendars after this cache, or signed in again: the old
        // pair's priors stay out while native ones are untouched.
        assert_eq!(snapshot(Some(3), Some(pair(1, 6))), ["live"]);
        assert_eq!(snapshot(Some(3), Some(pair(2, 5))), ["live"]);
        // A live pair older than state.json (reselected after the answer) admits nothing,
        // not even the events stamped with that older pair.
        assert_eq!(snapshot(Some(3), Some(pair(1, 4))), ["live"]);
        // Google not signed in with the configured client: left out although its
        // state.json says active.
        assert_eq!(snapshot(Some(3), None), ["live"]);
    }

    /// An opted-in system calendar; every other account is signed out.
    struct NativeAccounts {
        state: Mutex<broscribe_integrations::AccountState>,
        synced: Mutex<Vec<IntegrationId>>,
    }
    impl AccountService for NativeAccounts {
        no_remote_accounts!();
        fn keep_alive(&self, _: IntegrationId) -> broscribe_integrations::Result<()> {
            Ok(())
        }
        fn state(
            &self,
            id: IntegrationId,
        ) -> broscribe_integrations::Result<broscribe_integrations::AccountState> {
            Ok(if id == IntegrationId::SystemCalendar {
                lock(&self.state).clone()
            } else {
                Default::default()
            })
        }
        fn sync_calendar(
            &self,
            id: IntegrationId,
            _: chrono::DateTime<chrono::Utc>,
            to: chrono::DateTime<chrono::Utc>,
        ) -> broscribe_integrations::Result<usize> {
            lock(&self.synced).push(id);
            lock(&self.state).last_sync = Some(to - chrono::Duration::days(30));
            Ok(0)
        }
        fn native_calendar(
            &self,
            _: CalendarProvider,
            _: SystemCalendarAction,
        ) -> broscribe_integrations::Result<IntegrationStatus> {
            no_system_calendar()
        }
        fn native_generation(
            &self,
            _: CalendarProvider,
        ) -> broscribe_integrations::Result<Option<u64>> {
            no_system_calendar()
        }
        fn calendar_generation(
            &self,
            _: IntegrationId,
        ) -> broscribe_integrations::Result<Option<u64>> {
            no_system_calendar()
        }
        fn google_authority(&self) -> broscribe_integrations::Result<Option<GoogleCalendarStamp>> {
            panic!("sync maintenance never asks for the Google snapshot authority")
        }
    }

    #[test]
    fn system_calendar_refreshes_on_the_minute_tick_not_daily() {
        let tmp = tempfile::tempdir().unwrap();
        let start = chrono::DateTime::parse_from_rfc3339("2026-11-14T12:00:00Z")
            .unwrap()
            .to_utc();
        let clock = Arc::new(Mutex::new(start));
        let accounts = Arc::new(NativeAccounts {
            state: Mutex::new(broscribe_integrations::AccountState {
                status: broscribe_integrations::AccountStatus::Active,
                ..Default::default()
            }),
            synced: Mutex::default(),
        });
        let mut maintenance = AccountMaintenance::new(
            tmp.path().to_path_buf(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        );
        *lock(&maintenance.service) = Some(accounts.clone());
        maintenance.now = Arc::new({
            let clock = clock.clone();
            move || *lock(&clock)
        });
        let at = |seconds: i64| {
            *lock(&clock) = start + chrono::Duration::seconds(seconds);
            maintenance.sync(None, false).unwrap();
            lock(&accounts.synced).len()
        };
        assert_eq!(at(0), 1);
        assert_eq!(at(54), 1);
        assert_eq!(at(55), 2);
        assert_eq!(at(110), 3);
        assert_eq!(
            *lock(&accounts.synced),
            [IntegrationId::SystemCalendar; 3],
            "signed-out remote accounts are never synced"
        );
    }

    #[test]
    fn system_calendar_disconnects_without_configuration_or_helper_authority() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "[integration.google\n").unwrap();
        let maintenance = AccountMaintenance::new(
            tmp.path().to_path_buf(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        );
        assert!(maintenance.reload().is_err());
        let account =
            broscribe_integrations::account_dir(tmp.path(), IntegrationId::SystemCalendar);
        std::fs::create_dir_all(&account).unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &account.join("state.json"),
            &broscribe_integrations::AccountState {
                generation: 2,
                status: broscribe_integrations::AccountStatus::Active,
                native_calendar_ids: vec!["synthetic-work".into()],
                ..Default::default()
            },
        )
        .unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &account.join("calendar.json"),
            &broscribe_integrations::CalendarCache::default(),
        )
        .unwrap();
        let error = maintenance
            .native_calendar(CalendarProvider::EventKit, SystemCalendarAction::Connect)
            .unwrap_err();
        assert_eq!(error.code, "helper_required");
        assert_eq!(
            maintenance
                .native_generation(CalendarProvider::EventKit)
                .unwrap_err()
                .code,
            "helper_required"
        );
        let status = maintenance
            .native_calendar(CalendarProvider::EventKit, SystemCalendarAction::Disconnect)
            .unwrap();
        assert_eq!(status.state.generation, 3);
        assert_eq!(
            status.state.status,
            broscribe_integrations::AccountStatus::SignedOut
        );
        assert!(status.state.native_calendar_ids.is_empty());
        assert!(status.system_calendar.unwrap().helper_required);
        assert!(!account.join("calendar.json").exists());
        assert_eq!(
            maintenance
                .native_generation(CalendarProvider::EventKit)
                .unwrap(),
            None
        );
    }

    /// An unreadable OS trust store fails only provider traffic: the daemon's
    /// reload still installs the service, so maintenance runs and the native
    /// calendar still disconnects offline; nothing opens a browser.
    #[test]
    fn an_unreadable_trust_store_keeps_reload_and_native_disconnect_working() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[integration.google]\nclient_id=\"synthetic-client\"\n",
        )
        .unwrap();
        let browser = Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default());
        let store = Arc::new(broscribe_platform::fakes::FakeTrustStore::default());
        let maintenance = AccountMaintenance::new(
            tmp.path().to_path_buf(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            browser.clone(),
        )
        .with_trust_store(store);
        maintenance.reload().unwrap();
        assert!(lock(&maintenance.service).is_some());
        maintenance.keep_alive().unwrap();
        let account =
            broscribe_integrations::account_dir(tmp.path(), IntegrationId::SystemCalendar);
        std::fs::create_dir_all(&account).unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &account.join("state.json"),
            &broscribe_integrations::AccountState {
                generation: 2,
                status: broscribe_integrations::AccountStatus::Active,
                native_calendar_ids: vec!["synthetic-work".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let status = maintenance
            .native_calendar(CalendarProvider::EventKit, SystemCalendarAction::Disconnect)
            .unwrap();
        assert_eq!(
            status.state.status,
            broscribe_integrations::AccountStatus::SignedOut
        );
        assert!(browser.opened.lock().unwrap().is_empty());
    }
    struct RemoteAccounts {
        accounts: Mutex<BTreeMap<AccountRef, broscribe_integrations::MicrosoftAccount>>,
        authorities: Mutex<BTreeMap<AccountRef, AccountAuthority>>,
        caches: BTreeMap<AccountRef, broscribe_integrations::CalendarCache>,
        attempts: Mutex<Vec<SyncContext>>,
        kept: Mutex<Vec<AccountRef>>,
        fail: AccountRef,
        now: chrono::DateTime<chrono::Utc>,
    }

    fn remote_ref(handle: char) -> AccountRef {
        AccountRef {
            integration: broscribe_core::calendar::RemoteIntegration::MicrosoftGraph,
            account_id: broscribe_core::calendar::AccountId::parse(&handle.to_string().repeat(32))
                .unwrap(),
        }
    }

    fn own_target(id: &str) -> broscribe_core::calendar::CalendarTarget {
        broscribe_core::calendar::CalendarTarget::Own {
            calendar_id: id.into(),
        }
    }

    fn remote_fixture(now: chrono::DateTime<chrono::Utc>) -> RemoteAccounts {
        use broscribe_integrations::{
            CalendarCache, CalendarEvent, Capability, MicrosoftAccount, VerifiedMicrosoftIdentity,
        };
        let mut accounts = BTreeMap::new();
        let mut authorities = BTreeMap::new();
        let mut caches = BTreeMap::new();
        for handle in ['a', 'b', 'c'] {
            let account = remote_ref(handle);
            let selected_calendars = vec![
                own_target("same-id"),
                own_target("denied-id"),
                own_target("never-readable-id"),
            ];
            let state = MicrosoftAccount {
                account: account.clone(),
                identity: VerifiedMicrosoftIdentity {
                    authority_host: "login.example.invalid".into(),
                    resource_tenant_id: format!("tenant-{handle}"),
                    object_id: format!("object-{handle}"),
                    client_id: "synthetic-client".into(),
                    display_name: None,
                    tenant_display_name: None,
                    verified_addresses: Vec::new(),
                },
                generation: 7,
                selection_generation: 11,
                status: broscribe_integrations::AccountStatus::Active,
                granted_scopes: Vec::new(),
                selected_calendars,
                calendar: Capability::Available,
                directory: Capability::Unknown,
                shared_calendars: Capability::Unknown,
                microsoft365_groups: Capability::Unknown,
                last_sync: None,
                last_refresh_at: None,
                access_error: (handle == 'c').then(|| "conditional_access_blocked".into()),
                target_results: vec![
                    broscribe_integrations::CalendarTargetResult {
                        target: own_target("same-id"),
                        outcome: broscribe_integrations::CalendarTargetOutcome::Available {
                            event_count: 1,
                        },
                    },
                    broscribe_integrations::CalendarTargetResult {
                        target: own_target("denied-id"),
                        outcome: broscribe_integrations::CalendarTargetOutcome::Denied {
                            code: "forbidden".into(),
                        },
                    },
                    broscribe_integrations::CalendarTargetResult {
                        target: own_target("never-readable-id"),
                        outcome: broscribe_integrations::CalendarTargetOutcome::Error {
                            code: "transient".into(),
                        },
                    },
                ],
                route_reads: Vec::new(),
            };
            authorities.insert(
                account.clone(),
                AccountAuthority {
                    account: account.clone(),
                    generation: 7,
                    selection_generation: 11,
                    selected_calendars: vec![own_target("same-id")],
                },
            );
            let valid = CalendarEvent {
                id: format!("event-{handle}"),
                integration: IntegrationId::MicrosoftGraph,
                title: Some("Synthetic remote meeting".into()),
                start: now,
                end: now + chrono::Duration::hours(1),
                join_links: Vec::new(),
                app: None,
                attendees: Vec::new(),
                account_generation: None,
                google: None,
                remote: Some(broscribe_core::calendar::RemoteCalendarStamp {
                    account: account.clone(),
                    generation: 7,
                    selection_generation: 11,
                    target: own_target("same-id"),
                }),
                ical_uid: Some("same-uid".into()),
                occurrence_id: None,
            };
            let mut missing = valid.clone();
            missing.id = "missing-stamp".into();
            missing.remote = None;
            let mut denied = valid.clone();
            denied.id = "denied-route".into();
            denied.remote.as_mut().unwrap().target = own_target("denied-id");
            let mut unread = valid.clone();
            unread.id = "never-readable-route".into();
            unread.remote.as_mut().unwrap().target = own_target("never-readable-id");
            let mut wrong_route = valid.clone();
            wrong_route.id = "wrong-routing-kind".into();
            wrong_route.remote.as_mut().unwrap().target =
                broscribe_core::calendar::CalendarTarget::RecipientShared {
                    calendar_id: "same-id".into(),
                };
            let mut other = valid.clone();
            other.id = "another-account".into();
            other.remote.as_mut().unwrap().account =
                remote_ref(if handle == 'a' { 'b' } else { 'a' });
            caches.insert(
                account.clone(),
                CalendarCache {
                    events: vec![valid, missing, denied, unread, wrong_route, other],
                },
            );
            accounts.insert(account, state);
        }
        RemoteAccounts {
            accounts: Mutex::new(accounts),
            authorities: Mutex::new(authorities),
            caches,
            attempts: Mutex::new(Vec::new()),
            kept: Mutex::new(Vec::new()),
            fail: remote_ref('a'),
            now,
        }
    }

    impl AccountService for RemoteAccounts {
        fn list_accounts(
            &self,
        ) -> broscribe_integrations::Result<Vec<broscribe_integrations::MicrosoftAccount>> {
            Ok(lock(&self.accounts).values().cloned().collect())
        }
        fn keep_account_alive(&self, account: &AccountRef) -> broscribe_integrations::Result<()> {
            lock(&self.kept).push(account.clone());
            Ok(())
        }
        fn account_authority(
            &self,
            account: &AccountRef,
        ) -> broscribe_integrations::Result<Option<AccountAuthority>> {
            if lock(&self.accounts).get(account).is_some_and(|state| {
                state.access_error.as_deref() == Some("conditional_access_blocked")
            }) {
                return Ok(None);
            }
            Ok(lock(&self.authorities).get(account).cloned())
        }
        /// The service's contract: only events under the account's live authority.
        fn cached_account_calendar(
            &self,
            account: &AccountRef,
        ) -> broscribe_integrations::Result<broscribe_integrations::CalendarCache> {
            let Some(authority) = self.account_authority(account)? else {
                return Ok(Default::default());
            };
            let mut cache = self.caches.get(account).unwrap().clone();
            cache.events.retain(|event| {
                event.remote.as_ref().is_some_and(|stamp| {
                    stamp.account == authority.account
                        && stamp.generation == authority.generation
                        && stamp.selection_generation == authority.selection_generation
                        && authority.selected_calendars.contains(&stamp.target)
                })
            });
            Ok(cache)
        }
        fn sync_account(
            &self,
            account: &AccountRef,
            _: chrono::DateTime<chrono::Utc>,
            _: chrono::DateTime<chrono::Utc>,
        ) -> broscribe_integrations::Result<broscribe_integrations::AccountCalendarSync> {
            lock(&self.attempts).push(SyncContext::Account(account.clone()));
            if *account == self.fail {
                return Err(broscribe_integrations::IntegrationError::Transient(
                    "synthetic account outage".into(),
                ));
            }
            let mut accounts = lock(&self.accounts);
            let state = accounts.get_mut(account).unwrap();
            state.last_sync = Some(self.now);
            Ok(broscribe_integrations::AccountCalendarSync {
                account: account.clone(),
                generation: state.generation,
                selection_generation: state.selection_generation,
                outcome: broscribe_integrations::AccountSyncOutcome::Partial,
                targets: state.target_results.clone(),
                routes: state.route_reads.clone(),
                event_count: self.caches.get(account).unwrap().events.len(),
            })
        }
        fn keep_alive(&self, _: IntegrationId) -> broscribe_integrations::Result<()> {
            Ok(())
        }
        fn state(
            &self,
            _: IntegrationId,
        ) -> broscribe_integrations::Result<broscribe_integrations::AccountState> {
            Ok(broscribe_integrations::AccountState {
                status: broscribe_integrations::AccountStatus::Active,
                ..Default::default()
            })
        }
        fn sync_calendar(
            &self,
            id: IntegrationId,
            _: chrono::DateTime<chrono::Utc>,
            _: chrono::DateTime<chrono::Utc>,
        ) -> broscribe_integrations::Result<usize> {
            lock(&self.attempts).push(SyncContext::Provider(id));
            Ok(0)
        }
        fn native_calendar(
            &self,
            _: CalendarProvider,
            _: SystemCalendarAction,
        ) -> broscribe_integrations::Result<IntegrationStatus> {
            no_system_calendar()
        }
        fn native_generation(
            &self,
            _: CalendarProvider,
        ) -> broscribe_integrations::Result<Option<u64>> {
            Ok(Some(7))
        }
        fn calendar_generation(
            &self,
            _: IntegrationId,
        ) -> broscribe_integrations::Result<Option<u64>> {
            Ok(Some(7))
        }
        fn google_authority(&self) -> broscribe_integrations::Result<Option<GoogleCalendarStamp>> {
            panic!("account maintenance never asks for the Google snapshot authority")
        }
    }

    #[test]
    fn account_maintenance_scopes_backoff_and_blocked_attempts_without_stopping_native_or_google() {
        let tmp = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let accounts = Arc::new(remote_fixture(now));
        let mut maintenance = AccountMaintenance::new(
            tmp.path().to_owned(),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        );
        let clock = Arc::new(Mutex::new(now));
        maintenance.now = Arc::new({
            let clock = clock.clone();
            move || *lock(&clock)
        });
        *lock(&maintenance.service) = Some(accounts.clone());
        maintenance.keep_alive().unwrap();
        assert_eq!(
            *lock(&accounts.kept),
            vec![remote_ref('a'), remote_ref('b')]
        );
        maintenance.sync(None, false).unwrap();
        assert_eq!(
            *lock(&accounts.attempts),
            vec![
                SyncContext::Provider(IntegrationId::SystemCalendar),
                SyncContext::Provider(IntegrationId::Google),
                SyncContext::Account(remote_ref('a')),
                SyncContext::Account(remote_ref('b')),
            ]
        );
        lock(&accounts.attempts).clear();
        maintenance.sync(None, false).unwrap();
        assert_eq!(
            *lock(&accounts.attempts),
            vec![
                SyncContext::Provider(IntegrationId::SystemCalendar),
                SyncContext::Provider(IntegrationId::Google),
            ]
        );
        lock(&accounts.accounts)
            .get_mut(&remote_ref('a'))
            .unwrap()
            .selection_generation += 1;
        lock(&accounts.attempts).clear();
        maintenance.sync(None, false).unwrap();
        assert!(lock(&accounts.attempts).contains(&SyncContext::Account(remote_ref('a'))));
        assert!(!lock(&accounts.attempts).contains(&SyncContext::Account(remote_ref('b'))));
        assert!(!lock(&accounts.attempts).contains(&SyncContext::Account(remote_ref('c'))));
        *lock(&clock) = now + chrono::Duration::minutes(2);
        lock(&accounts.attempts).clear();
        maintenance.sync(None, false).unwrap();
        assert!(lock(&accounts.attempts).contains(&SyncContext::Account(remote_ref('a'))));
        // B's partial read retries on its own backoff despite a fresh last_sync.
        assert!(lock(&accounts.attempts).contains(&SyncContext::Account(remote_ref('b'))));
        assert!(!lock(&accounts.attempts).contains(&SyncContext::Account(remote_ref('c'))));
        // A provider is an aggregate, not an implicit first-account request.
        lock(&accounts.attempts).clear();
        let report =
            serde_json::to_value(maintenance.sync(Some("microsoft-graph"), true).unwrap()).unwrap();
        assert_eq!(
            *lock(&accounts.attempts),
            vec![
                SyncContext::Account(remote_ref('a')),
                SyncContext::Account(remote_ref('b')),
            ]
        );
        assert_eq!(report[0]["account"], serde_json::json!(remote_ref('a')));
        assert_eq!(report[0]["error"]["code"], "transient");
        assert_eq!(report[1]["account"], serde_json::json!(remote_ref('b')));
        assert_eq!(report[1]["outcome"], "partial");
        // IN12: counts only; event contents never travel in the report.
        assert_eq!(report[1]["event_count"], 6);
        assert!(report[1].get("cache").is_none() && report[1].get("events").is_none());
        assert!(!report[1].to_string().contains("Synthetic remote meeting"));
        assert_eq!(report[2]["account"], serde_json::json!(remote_ref('c')));
        assert_eq!(report[2]["error"]["code"], "conditional_access_blocked");

        lock(&accounts.accounts)
            .get_mut(&remote_ref('b'))
            .unwrap()
            .status = broscribe_integrations::AccountStatus::ReauthRequired;
        lock(&accounts.attempts).clear();
        let report =
            serde_json::to_value(maintenance.sync(Some("microsoft-graph"), true).unwrap()).unwrap();
        assert_eq!(
            *lock(&accounts.attempts),
            vec![SyncContext::Account(remote_ref('a'))]
        );
        assert_eq!(report[1]["account"], serde_json::json!(remote_ref('b')));
        assert_eq!(report[1]["error"]["code"], "reauth_required");
        assert_eq!(report[2]["account"], serde_json::json!(remote_ref('c')));
        assert_eq!(report[2]["error"]["code"], "conditional_access_blocked");
    }

    #[tokio::test]
    async fn daemon_account_authority_and_snapshots_never_swap_equal_account_calendar_generations()
    {
        use broscribe_ipc::{Call, Request};
        let tmp = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now();
        let accounts = Arc::new(remote_fixture(now));
        let maintenance = Arc::new(AccountMaintenance::new(
            tmp.path().join("data"),
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        ));
        *lock(&maintenance.service) = Some(accounts.clone());
        let daemon = Daemon::new(
            crate::DaemonConfig::new(
                tmp.path().join("library"),
                tmp.path().join("data"),
                tmp.path().join("cache"),
                crate::WorkerCommand::new("/bin/false"),
            ),
            Arc::new(Engine),
            Arc::new(broscribe_platform::fakes::FakeProcessProbe::new(
                std::process::id(),
            )),
            Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.path().join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
        )
        .unwrap();
        daemon.set_integrations(maintenance.clone());
        for handle in ['a', 'b'] {
            let result = daemon
                .handle(Request::new(Call::MicrosoftCalendarAuthority {
                    account: remote_ref(handle),
                }))
                .await
                .outcome
                .unwrap();
            assert_eq!(
                result["authority"]["account"]["account_id"],
                remote_ref(handle).account_id.as_str()
            );
            assert_eq!(
                result["authority"]["selected_calendars"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
        let ids = || {
            maintenance
                .remote_calendar_events()
                .unwrap()
                .into_iter()
                .map(|event| event.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(), ["event-a", "event-b"]);
        // Latest transient outcomes do not revoke the last successful read.
        lock(&accounts.accounts)
            .get_mut(&remote_ref('a'))
            .unwrap()
            .target_results[0]
            .outcome = broscribe_integrations::CalendarTargetOutcome::Error {
            code: "transient".into(),
        };
        assert_eq!(ids(), ["event-a", "event-b"]);
        lock(&accounts.authorities).remove(&remote_ref('a'));
        assert_eq!(ids(), ["event-b"]);
        lock(&accounts.authorities).insert(
            remote_ref('a'),
            AccountAuthority {
                account: remote_ref('a'),
                generation: 7,
                selection_generation: 12,
                selected_calendars: vec![own_target("same-id")],
            },
        );
        assert_eq!(ids(), ["event-b"]);
        let missing = daemon
            .handle(Request::new(Call::MicrosoftCalendarAuthority {
                account: remote_ref('d'),
            }))
            .await
            .outcome
            .unwrap();
        assert!(missing["authority"].is_null());
        let blocked = daemon
            .handle(Request::new(Call::MicrosoftCalendarAuthority {
                account: remote_ref('c'),
            }))
            .await
            .outcome
            .unwrap();
        assert!(blocked["authority"].is_null());
        let notification_id = format!(
            "integration-reauth-microsoft-graph-{}",
            remote_ref('b').account_id
        );
        daemon.shared.notify(NotificationRequest {
            id: notification_id.clone(),
            title: "Sign-in required".into(),
            body: "Synthetic account".into(),
            actions: vec![broscribe_ipc::NotificationButton {
                id: "open".into(),
                label: "Sign in".into(),
            }],
            meeting_id: None,
        });
        let result = daemon
            .handle(Request::new(Call::NotificationAction {
                notification_id,
                action: "open".into(),
            }))
            .await
            .outcome
            .unwrap();
        assert_eq!(
            result["integration"],
            format!("microsoft-graph/{}", remote_ref('b').account_id)
        );
    }

    /// IN09: a requested Google sync through the agent keeps the service's own code and
    /// fixed guidance (here `trust_store_unavailable` from the real service with an
    /// unreadable trust store), never a configuration message, a token or a browser.
    #[test]
    fn a_requested_sync_keeps_the_services_sanitized_code_and_message() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[integration.google]\nclient_id=\"synthetic-client\"\n",
        )
        .unwrap();
        let account = broscribe_integrations::account_dir(tmp.path(), IntegrationId::Google);
        std::fs::create_dir_all(&account).unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &account.join("state.json"),
            &broscribe_integrations::AccountState {
                generation: 1,
                status: broscribe_integrations::AccountStatus::Active,
                client_id: Some("synthetic-client".into()),
                native_calendar_ids: vec!["synthetic-work".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let secrets = Arc::new(broscribe_platform::fakes::FakeSecretStore::default());
        secrets
            .set(
                &broscribe_integrations::tokens_secret_key(IntegrationId::Google),
                r#"{"access_token":"synthetic-token-marker","expires_at":4102444800}"#,
            )
            .unwrap();
        let browser = Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default());
        let maintenance =
            AccountMaintenance::new(tmp.path().to_path_buf(), secrets, browser.clone())
                .with_trust_store(Arc::new(
                    broscribe_platform::fakes::FakeTrustStore::default(),
                ));
        maintenance.reload().unwrap();
        let error = maintenance.sync(Some("google"), true).unwrap_err();
        assert_eq!(error.code, "trust_store_unavailable");
        assert!(error.message.contains("trust store"), "{error}");
        assert!(!error.message.contains("configuration"), "{error}");
        assert!(!error.message.contains("synthetic-token-marker"), "{error}");
        assert!(browser.opened.lock().unwrap().is_empty());
    }

    #[test]
    fn calendar_sign_in_notice_uses_the_calendar_sentence() {
        let notice = sign_in_notice(broscribe_integrations::ReauthNotification {
            integration: IntegrationId::Google,
            message: "Sign in to Google to keep calendar sync.".into(),
            account: None,
        });
        let title = if broscribe_platform_native::NativePlatform::host_platform()
            == broscribe_platform::HostPlatform::MacOs
        {
            "Sign-In Required"
        } else {
            "Sign-in required"
        };
        assert_eq!(notice.title, title);
        assert_eq!(notice.body, "Sign in to Google to keep calendar sync.");
    }

    /// An explicit Google sync through the agent says that Google needs sign-in, with the
    /// code the service gives without the agent, instead of an empty success. Unqualified
    /// maintenance, forced or not, still skips the inactive source. No browser opens.
    #[test]
    fn an_explicit_google_sync_reports_sign_in_but_maintenance_skips_it() {
        use broscribe_integrations::AccountStatus;
        for (status, code) in [
            (AccountStatus::ReauthRequired, "reauth_required"),
            (AccountStatus::SignedOut, "signed_out"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("config.toml"),
                "[integration.google]\nclient_id=\"synthetic-client\"\n",
            )
            .unwrap();
            let account = broscribe_integrations::account_dir(tmp.path(), IntegrationId::Google);
            std::fs::create_dir_all(&account).unwrap();
            broscribe_core::fsutil::write_json_atomic(
                &account.join("state.json"),
                &broscribe_integrations::AccountState {
                    generation: 2,
                    status,
                    client_id: Some("synthetic-client".into()),
                    native_calendar_ids: vec!["synthetic-work".into()],
                    ..Default::default()
                },
            )
            .unwrap();
            let browser = Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default());
            let maintenance = AccountMaintenance::new(
                tmp.path().to_path_buf(),
                Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
                browser.clone(),
            );
            maintenance.reload().unwrap();
            let error = maintenance.sync(Some("google"), true).unwrap_err();
            assert_eq!(error.code, code);
            assert!(maintenance.sync(None, false).unwrap().is_empty());
            assert!(maintenance.sync(None, true).unwrap().is_empty());
            assert!(browser.opened.lock().unwrap().is_empty());
        }
    }

    /// One provider-wide account whose every read fails with `error`.
    struct ErringAccounts(fn() -> broscribe_integrations::IntegrationError);
    impl AccountService for ErringAccounts {
        no_remote_accounts!();
        fn keep_alive(&self, _: IntegrationId) -> broscribe_integrations::Result<()> {
            Ok(())
        }
        fn state(
            &self,
            id: IntegrationId,
        ) -> broscribe_integrations::Result<broscribe_integrations::AccountState> {
            Ok(broscribe_integrations::AccountState {
                status: if id == IntegrationId::Google {
                    broscribe_integrations::AccountStatus::Active
                } else {
                    broscribe_integrations::AccountStatus::SignedOut
                },
                ..Default::default()
            })
        }
        fn sync_calendar(
            &self,
            _: IntegrationId,
            _: chrono::DateTime<chrono::Utc>,
            _: chrono::DateTime<chrono::Utc>,
        ) -> broscribe_integrations::Result<usize> {
            Err((self.0)())
        }
        fn native_calendar(
            &self,
            _: CalendarProvider,
            _: SystemCalendarAction,
        ) -> broscribe_integrations::Result<IntegrationStatus> {
            no_system_calendar()
        }
        fn native_generation(
            &self,
            _: CalendarProvider,
        ) -> broscribe_integrations::Result<Option<u64>> {
            no_system_calendar()
        }
        fn calendar_generation(
            &self,
            _: IntegrationId,
        ) -> broscribe_integrations::Result<Option<u64>> {
            no_system_calendar()
        }
        fn google_authority(&self) -> broscribe_integrations::Result<Option<GoogleCalendarStamp>> {
            panic!("sync maintenance never asks for the Google snapshot authority")
        }
    }

    #[test]
    fn sync_failures_report_their_own_code_without_local_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let maintenance = |error: fn() -> broscribe_integrations::IntegrationError| {
            let maintenance = AccountMaintenance::new(
                tmp.path().to_path_buf(),
                Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
                Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
            );
            *lock(&maintenance.service) = Some(Arc::new(ErringAccounts(error)));
            maintenance
        };
        let reauth = maintenance(|| {
            broscribe_integrations::IntegrationError::ReauthRequired("google".into())
        })
        .sync(Some("google"), true)
        .unwrap_err();
        assert_eq!(reauth.code, "reauth_required");
        assert!(reauth.message.contains("sign-in required"), "{reauth}");
        // A forced report keeps every row; a storage failure's row names no file.
        let rows = maintenance(|| {
            broscribe_integrations::IntegrationError::Core(broscribe_core::Error::io_at(
                std::path::Path::new("/synthetic/private/google/state.json"),
                std::io::Error::other("synthetic"),
            ))
        })
        .sync(None, true)
        .unwrap();
        let rows = serde_json::to_value(rows).unwrap();
        assert_eq!(rows[0]["integration"], "google");
        assert_eq!(rows[0]["error"]["code"], "io");
        assert!(!rows.to_string().contains("/synthetic/private"), "{rows}");
    }

    /// IN19: an unchanged session snapshot is not rewritten every maintenance pass (no
    /// replacement file, no sync); a changed one is replaced at once.
    #[test]
    #[cfg(unix)]
    fn an_unchanged_session_snapshot_is_not_rewritten() {
        use std::os::unix::fs::MetadataExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("session");
        std::fs::create_dir_all(&session).unwrap();
        let now = chrono::Utc::now();
        let event = |id: &str| broscribe_integrations::CalendarEvent {
            id: id.into(),
            integration: IntegrationId::MicrosoftGraph,
            title: Some("Synthetic sync".into()),
            start: now,
            end: now + chrono::Duration::hours(1),
            join_links: Vec::new(),
            app: None,
            attendees: Vec::new(),
            account_generation: None,
            google: None,
            remote: None,
            ical_uid: None,
            occurrence_id: None,
        };
        let live = LiveCalendars::default();
        let write = |events| {
            write_session_calendar(tmp.path(), &session, now, &live, events).unwrap();
            std::fs::metadata(session.join("calendar.json"))
                .unwrap()
                .ino()
        };
        let first = write(vec![event("a")]);
        assert_eq!(write(vec![event("a")]), first);
        assert_ne!(write(vec![event("a"), event("b")]), first);
    }
}
