//! Corporate Microsoft accounts. All network work runs outside registry locks.
use super::*;
use broscribe_core::calendar::{
    AccountId, AccountRef, CalendarTarget, RemoteCalendarStamp, RemoteIntegration,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedMicrosoftIdentity {
    pub authority_host: String,
    pub resource_tenant_id: String,
    pub object_id: String,
    pub client_id: String,
    pub display_name: Option<String>,
    pub tenant_display_name: Option<String>,
    pub verified_addresses: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountPermission {
    SharedCalendars,
    Microsoft365Groups,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MicrosoftAccount {
    pub account: AccountRef,
    pub identity: VerifiedMicrosoftIdentity,
    pub generation: u64,
    pub selection_generation: u64,
    pub status: AccountStatus,
    pub granted_scopes: Vec<String>,
    pub selected_calendars: Vec<CalendarTarget>,
    pub calendar: Capability,
    pub directory: Capability,
    pub shared_calendars: Capability,
    pub microsoft365_groups: Capability,
    pub last_sync: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_refresh_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub access_error: Option<String>,
    pub target_results: Vec<CalendarTargetResult>,
    /// Per selected route: the last successful read and what the cached copy holds now.
    #[serde(default)]
    pub route_reads: Vec<CalendarRouteRead>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarDescriptor {
    pub target: CalendarTarget,
    pub name: Option<String>,
    pub owner_name: Option<String>,
    pub owner_address: Option<String>,
    pub is_default: bool,
    pub can_edit: Option<bool>,
    pub access: Capability,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MicrosoftCalendarCatalog {
    pub account: AccountRef,
    pub generation: u64,
    pub selection_generation: u64,
    pub calendars: Vec<CalendarDescriptor>,
    pub groups: Capability,
    pub groups_error: Option<CalendarListError>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CalendarTargetOutcome {
    Available { event_count: usize },
    Denied { code: String },
    Error { code: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarTargetResult {
    pub target: CalendarTarget,
    pub outcome: CalendarTargetOutcome,
}

/// What an account's cached copy holds for one selected route after its latest sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteCopy {
    /// Events from this sync's successful read.
    Fresh,
    /// This sync failed transiently; events of the last successful read are kept.
    Retained,
    /// Nothing cached: denied, never read, or no event left.
    Empty,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarRouteRead {
    pub target: CalendarTarget,
    /// The last successful read of this route under its current authorization.
    pub last_success: Option<DateTime<Utc>>,
    pub copy: RouteCopy,
    pub cached_events: usize,
}

/// What [`Integrations::remove_calendar_target`] committed; the route is removed whenever
/// this is returned. `copy_error` reports that the other routes' cached events could not be
/// carried over to the new selection: their routes then read `Empty` and nothing of theirs
/// is served until the next sync reads them again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarTargetRemoval {
    #[serde(flatten)]
    pub account: MicrosoftAccount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_error: Option<PublicError>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountSyncOutcome {
    NoSelection,
    Available,
    Partial,
    Denied,
    Error,
}

/// A sync's outcome and counts. Event contents stay in the private account cache and
/// never travel in this response or its logs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCalendarSync {
    pub account: AccountRef,
    pub generation: u64,
    pub selection_generation: u64,
    pub outcome: AccountSyncOutcome,
    pub targets: Vec<CalendarTargetResult>,
    pub routes: Vec<CalendarRouteRead>,
    /// Events in the published cache, fresh and retained.
    pub event_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDirectoryResult {
    pub account: AccountRef,
    pub generation: u64,
    pub people: Vec<DirectoryPerson>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyMicrosoftStatus {
    None,
    Quarantined,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDirectorySource {
    pub account: AccountRef,
    pub generation: u64,
}

#[path = "account_store.rs"]
mod persistence;
use persistence::{Registry, StoredAccount};

/// `key` is a committed account credential version in the registry on disk. The caller
/// holds the provider state lock, which every registry writer also holds.
pub(super) fn microsoft_secret_committed(dir: &Path, key: &str) -> Result<bool> {
    let registry: Registry =
        read_json_opt(&dir.join(persistence::REGISTRY_FILE))?.unwrap_or_default();
    Ok(registry
        .accounts
        .iter()
        .any(|a| a.secret_key.as_deref() == Some(key)))
}

/// Provider ID of a remote account, shared by account and daemon consumers.
pub fn integration_id(id: RemoteIntegration) -> IntegrationId {
    match id {
        RemoteIntegration::MicrosoftGraph => IntegrationId::MicrosoftGraph,
    }
}

fn remote_integration(id: IntegrationId) -> Result<RemoteIntegration> {
    match id {
        IntegrationId::MicrosoftGraph => Ok(RemoteIntegration::MicrosoftGraph),
        _ => Err(service_error(
            "account_required",
            "this operation requires a corporate Microsoft account",
        )),
    }
}

fn has_scope(scopes: &[String], scope: &str) -> bool {
    scopes.iter().any(|s| {
        s.rsplit('/')
            .next()
            .is_some_and(|s| s.eq_ignore_ascii_case(scope))
    })
}

fn has_base_grants(scopes: &[String]) -> bool {
    has_scope(scopes, "Calendars.Read") && has_scope(scopes, "User.Read")
}

fn directory_scope(settings: &IntegrationSettings) -> Option<&'static str> {
    match settings.directory {
        GraphDirectory::PeopleRead => Some("People.Read"),
        GraphDirectory::UserReadBasicAll => Some("User.ReadBasic.All"),
        GraphDirectory::None => None,
    }
}

fn calendar_target_authorized(stored: &StoredAccount, target: &CalendarTarget) -> bool {
    let (enabled, required) = match target {
        CalendarTarget::Own { .. } => return true,
        CalendarTarget::RecipientShared { .. } | CalendarTarget::Delegated { .. } => {
            (stored.shared_enabled, "Calendars.Read.Shared")
        }
        CalendarTarget::Group { .. } => (stored.groups_enabled, "Group.Read.All"),
    };
    enabled
        && has_scope(&stored.view.granted_scopes, required)
        && has_scope(&stored.authorized_scopes, required)
}

fn actual_scopes(tokens: &Tokens) -> Vec<String> {
    tokens
        .scope
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}
fn configured_tenant(settings: &IntegrationSettings) -> String {
    settings
        .tenant
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("common")
        .to_ascii_lowercase()
}

fn authority_host(endpoints: &Endpoints) -> Result<String> {
    let uri: ureq::http::Uri = endpoints
        .token
        .parse()
        .map_err(|_| service_error("account_identity_unavailable", "invalid authority"))?;
    uri.host()
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| service_error("account_identity_unavailable", "missing authority host"))
}

fn same_identity(a: &VerifiedMicrosoftIdentity, b: &VerifiedMicrosoftIdentity) -> bool {
    a.authority_host == b.authority_host
        && a.resource_tenant_id == b.resource_tenant_id
        && a.object_id == b.object_id
}

fn bind_account_endpoint(
    url: &str,
    tenant: &str,
    expected_host: &str,
    action: &str,
) -> Result<String> {
    let invalid = || {
        service_error(
            "account_authority_invalid",
            "account OAuth authority cannot be safely bound to its verified resource tenant",
        )
    };
    if tenant.trim().is_empty()
        || matches!(tenant, "." | "..")
        || ["common", "organizations", "consumers"]
            .iter()
            .any(|selector| tenant.eq_ignore_ascii_case(selector))
        || tenant.len() > 2048
        || tenant.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let uri: ureq::http::Uri = url.parse().map_err(|_| invalid())?;
    let host = uri.host().ok_or_else(invalid)?.to_ascii_lowercase();
    if host != expected_host
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
    {
        return Err(invalid());
    }
    let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]");
    let microsoft = matches!(
        host.as_str(),
        "login.microsoftonline.com"
            | "login.microsoftonline.us"
            | "login.chinacloudapi.cn"
            | "login.microsoftonline.de"
    );
    if !((uri.scheme_str() == Some("https") && microsoft)
        || (loopback && matches!(uri.scheme_str(), Some("http" | "https"))))
    {
        return Err(invalid());
    }
    let suffix = format!("/oauth2/v2.0/{action}");
    let Some(prefix) = uri.path().strip_suffix(&suffix) else {
        // Explicit opaque loopback fixtures keep their exact origin/path. Never turn a
        // synthetic endpoint into a production request or guess a production authority.
        return if loopback {
            Ok(url.to_owned())
        } else {
            Err(invalid())
        };
    };
    let (head, original_tenant) = prefix.rsplit_once('/').ok_or_else(invalid)?;
    if original_tenant.is_empty() {
        return Err(invalid());
    }
    let mut path = format!("{head}/{}{suffix}", crate::encode::pct_encode(tenant));
    if let Some(query) = uri.query() {
        path.push('?');
        path.push_str(query);
    }
    let mut parts = uri.into_parts();
    parts.path_and_query = Some(path.parse().map_err(|_| invalid())?);
    ureq::http::Uri::from_parts(parts)
        .map(|uri| uri.to_string())
        .map_err(|_| invalid())
}

impl Integrations {
    /// One read of the authoritative settings for `id`; checks derived from it below never
    /// reread `config.toml`.
    fn authoritative(&self, id: IntegrationId) -> Result<IntegrationSettings> {
        IntegrationsConfig::load_settings(&self.data_dir, id)
    }

    /// `current` still names `client` and this facade's tenant.
    fn check_microsoft_config(
        &self,
        id: IntegrationId,
        client: &str,
        current: &IntegrationSettings,
    ) -> Result<()> {
        if current.effective_client_id(id) != Some(client)
            || configured_tenant(current) != configured_tenant(&self.config.settings(id))
        {
            return Err(IntegrationError::Superseded(id.to_string()));
        }
        Ok(())
    }

    fn require_microsoft_config(
        &self,
        id: IntegrationId,
        client: &str,
    ) -> Result<IntegrationSettings> {
        let current = self.authoritative(id)?;
        self.check_microsoft_config(id, client, &current)?;
        Ok(current)
    }

    /// The directory scope `current` configures, equal to this facade's.
    fn directory_scope_of(
        &self,
        id: IntegrationId,
        current: &IntegrationSettings,
    ) -> Result<&'static str> {
        let required = directory_scope(current).ok_or_else(|| {
            IntegrationError::NotConfigured(
                id.to_string(),
                "configure a Microsoft directory permission before granting directory access"
                    .into(),
            )
        })?;
        if Some(required) != directory_scope(&self.config.settings(id)) {
            return Err(IntegrationError::Superseded(id.to_string()));
        }
        Ok(required)
    }

    fn require_directory_scope(&self, id: IntegrationId) -> Result<&'static str> {
        self.directory_scope_of(id, &self.authoritative(id)?)
    }

    fn account_oauth(&self, stored: &StoredAccount) -> Result<OAuthClient> {
        let id = integration_id(stored.view.account.integration);
        let mut oauth = self.oauth(id)?;
        self.require_microsoft_config(id, &oauth.client_id)?;
        let identity = &stored.view.identity;
        oauth.endpoints.token = bind_account_endpoint(
            &oauth.endpoints.token,
            &identity.resource_tenant_id,
            &identity.authority_host,
            "token",
        )?;
        oauth.endpoints.authorize = bind_account_endpoint(
            &oauth.endpoints.authorize,
            &identity.resource_tenant_id,
            &identity.authority_host,
            "authorize",
        )?;
        Ok(oauth)
    }
    fn registry<T>(
        &self,
        id: IntegrationId,
        f: impl FnOnce(&Path, &mut Registry) -> Result<T>,
    ) -> Result<T> {
        self.registry_access(id, true, false, f)
    }

    fn registry_read<T>(
        &self,
        id: IntegrationId,
        f: impl FnOnce(&Path, &Registry) -> Result<T>,
    ) -> Result<T> {
        self.registry_access(id, false, false, |dir, registry| f(dir, registry))
    }

    fn registry_access<T>(
        &self,
        id: IntegrationId,
        write: bool,
        require_legacy_recovery: bool,
        f: impl FnOnce(&Path, &mut Registry) -> Result<T>,
    ) -> Result<T> {
        remote_integration(id)?;
        let dir = account_dir(&self.data_dir, id);
        check_profile_directories(&self.data_dir, &dir)?;
        // Legacy recovery can fence state.json. All paths, including photo forget,
        // acquire provider state before the account registry, never in reverse.
        let provider = &self.coordinators[&id];
        let _provider_mutex = provider.state.lock().unwrap_or_else(|e| e.into_inner());
        let _provider_lock = FileLock::acquire(&dir.join(STATE_LOCK), LOCK_WAIT)?;
        let coordinator = coordinator(&dir.join("accounts"));
        let _mutex = coordinator.state.lock().unwrap_or_else(|e| e.into_inner());
        let _lock = FileLock::acquire(&dir.join("accounts.lock"), LOCK_WAIT)?;
        let mut registry: Registry =
            read_json_opt(&dir.join(persistence::REGISTRY_FILE))?.unwrap_or_default();
        if registry.version > 1 {
            return Err(service_error(
                "account_registry_version",
                "account registry version is unsupported",
            ));
        }
        registry.version = 1;
        if let Err(error) =
            persistence::recover_legacy_discard(&dir, &mut registry, self.secrets.as_ref())
        {
            if require_legacy_recovery {
                return Err(error);
            }
            // A failure to delete the unrelated legacy slot must not disable registered
            // accounts. Its durable intent/quarantine remains for an explicit retry.
            tracing::warn!(integration = %id, code = error.code(), "legacy discard cleanup remains pending");
        }
        persistence::recover(&dir, &mut registry, self.secrets.as_ref())?;
        if !registry.legacy_checked && registry.legacy_discard.is_none() {
            let mut legacy: AccountState =
                read_json_opt(&dir.join(STATE_FILE))?.unwrap_or_default();
            if legacy.status != AccountStatus::SignedOut {
                legacy.generation = persistence::bump(legacy.generation)?;
                legacy.status = AccountStatus::ReauthRequired;
                write_json_private(&dir.join(STATE_FILE), &legacy)?;
                registry.legacy_quarantined = true;
            }
            registry.legacy_checked = true;
            persistence::save(&dir, &registry)?;
        }
        if !write {
            return f(&dir, &mut registry);
        }
        let before = registry.clone();
        let value = f(&dir, &mut registry)?;
        if registry != before {
            persistence::save(&dir, &registry)?;
        }
        Ok(value)
    }

    fn account_current_in(
        &self,
        stored: &StoredAccount,
        current: &IntegrationSettings,
    ) -> Result<()> {
        let id = integration_id(stored.view.account.integration);
        self.check_microsoft_config(id, &stored.view.identity.client_id, current)?;
        if self.effective_client_id(id).as_deref() != Some(&stored.view.identity.client_id)
            || authority_host(self.endpoints(id)?)? != stored.view.identity.authority_host
        {
            return Err(IntegrationError::Superseded(id.to_string()));
        }
        Ok(())
    }

    fn require_account_current(&self, stored: &StoredAccount) -> Result<()> {
        self.account_current_in(
            stored,
            &self.authoritative(integration_id(stored.view.account.integration))?,
        )
    }

    fn stored_account(&self, account: &AccountRef) -> Result<StoredAccount> {
        self.registry_read(integration_id(account.integration), |_, r| {
            r.accounts
                .iter()
                .find(|a| &a.view.account == account)
                .cloned()
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))
        })
    }

    fn fence_account(&self, stored: &StoredAccount, generation: u64) -> Result<()> {
        self.fence_account_in(
            stored,
            generation,
            &self.authoritative(integration_id(stored.view.account.integration))?,
        )
    }

    fn fence_account_in(
        &self,
        stored: &StoredAccount,
        generation: u64,
        current: &IntegrationSettings,
    ) -> Result<()> {
        self.account_current_in(stored, current)?;
        if stored.view.access_error.as_deref() == Some("conditional_access_blocked") {
            return Err(IntegrationError::ConditionalAccessBlocked);
        }
        if stored.view.generation != generation || stored.view.status != AccountStatus::Active {
            return Err(IntegrationError::Superseded(
                integration_id(stored.view.account.integration).to_string(),
            ));
        }
        Ok(())
    }

    pub fn list_accounts(&self, selected: Option<IntegrationId>) -> Result<Vec<MicrosoftAccount>> {
        let mut accounts = Vec::new();
        for id in [IntegrationId::MicrosoftGraph] {
            if selected.is_some_and(|selected| selected != id) {
                continue;
            }
            // One authoritative read for every account; a malformed table reads as stale.
            let current = self.authoritative(id).ok();
            self.registry_read(id, |_, r| {
                accounts.extend(
                    r.accounts
                        .iter()
                        .filter(|a| a.view.status != AccountStatus::SignedOut)
                        .map(|a| {
                            let mut view = a.view.clone();
                            if current
                                .as_ref()
                                .is_none_or(|current| self.account_current_in(a, current).is_err())
                            {
                                view.status = AccountStatus::ReauthRequired;
                            }
                            view
                        }),
                );
                Ok(())
            })?;
        }
        Ok(accounts)
    }

    pub fn legacy_microsoft_status(&self, id: IntegrationId) -> Result<LegacyMicrosoftStatus> {
        self.registry_read(id, |_, r| {
            Ok(if r.legacy_quarantined || r.legacy_discard.is_some() {
                LegacyMicrosoftStatus::Quarantined
            } else {
                LegacyMicrosoftStatus::None
            })
        })
    }

    /// Explicitly discards only the quarantined, unverified provider-wide Microsoft
    /// sign-in and its legacy caches. Registered accounts are never selected implicitly.
    pub fn discard_legacy_microsoft(
        &self,
        id: IntegrationId,
        expected_generation: u64,
    ) -> Result<LegacyMicrosoftStatus> {
        // registry_access recovers an existing intent before testing the no-slot case:
        // an interrupted discard may already have fenced legacy state SignedOut.
        self.registry_access(id, false, true, |dir, registry| {
            if !registry.legacy_quarantined {
                return Ok(LegacyMicrosoftStatus::None);
            }
            let legacy: AccountState = read_json_opt(&dir.join(STATE_FILE))?.unwrap_or_default();
            if legacy.generation != expected_generation {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            registry.legacy_discard = Some(persistence::LegacyDiscardIntent {
                from_generation: legacy.generation,
                fenced_generation: persistence::bump(legacy.generation)?,
            });
            persistence::save(dir, registry)?;
            persistence::recover_legacy_discard(dir, registry, self.secrets.as_ref())?;
            Ok(LegacyMicrosoftStatus::None)
        })
    }

    /// Signs in a new account under the caller-named `attempt` (see
    /// [`Self::cancel_login`]).
    pub fn add_account(
        &self,
        id: IntegrationId,
        attempt: &LoginAttemptId,
    ) -> Result<MicrosoftAccount> {
        self.microsoft_login(id, None, None, attempt)
    }

    pub fn reconnect_account(
        &self,
        account: &AccountRef,
        expected_generation: u64,
        attempt: &LoginAttemptId,
    ) -> Result<MicrosoftAccount> {
        self.microsoft_login(
            integration_id(account.integration),
            Some((account, expected_generation)),
            None,
            attempt,
        )
    }

    pub fn grant_account_permission(
        &self,
        account: &AccountRef,
        expected_generation: u64,
        permission: AccountPermission,
        attempt: &LoginAttemptId,
    ) -> Result<MicrosoftAccount> {
        self.microsoft_login(
            integration_id(account.integration),
            Some((account, expected_generation)),
            Some(permission),
            attempt,
        )
    }

    /// Sign-in under `attempt` in three stages: admission under the registry lock, the
    /// browser and verification requests with no lock held, then a commit under the
    /// registry lock that requires the attempt still running and the client, tenant and
    /// account generation unchanged. A cancelled, expired or failed attempt leaves the
    /// account, its credential, selection, routes and cache as they were.
    fn microsoft_login(
        &self,
        id: IntegrationId,
        target: Option<(&AccountRef, u64)>,
        permission: Option<AccountPermission>,
        attempt: &LoginAttemptId,
    ) -> Result<MicrosoftAccount> {
        remote_integration(id)?;
        self.run_attempt(id, attempt, || {
            let (previous, oauth) = self.prepare_microsoft_login(id, target, permission)?;
            self.require_microsoft_config(id, &oauth.client_id)?;
            let authority = authority_host(&oauth.endpoints)?;
            let (tokens, identity, grants) = self.authorize_microsoft_login(
                id,
                previous.as_ref(),
                permission,
                &oauth,
                &authority,
                attempt,
            )?;
            self.commit_microsoft_login(
                id,
                target,
                permission,
                attempt,
                &authority,
                MicrosoftGrant {
                    tokens,
                    identity,
                    grants,
                    authorized: oauth.scopes.clone(),
                },
            )
        })
    }

    /// Admission: the previous account and the scopes this sign-in may request.
    fn prepare_microsoft_login(
        &self,
        id: IntegrationId,
        target: Option<(&AccountRef, u64)>,
        permission: Option<AccountPermission>,
    ) -> Result<(Option<StoredAccount>, OAuthClient)> {
        remote_integration(id)?;
        if target.is_none()
            && self
                .config
                .settings(id)
                .tenant
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("consumers"))
        {
            return Err(service_error(
                "personal_account_unsupported",
                "personal Microsoft accounts are unsupported by this corporate connector",
            ));
        }
        let directory = if permission == Some(AccountPermission::Directory) {
            Some(self.require_directory_scope(id)?)
        } else {
            None
        };
        let previous = target.map(|(a, _)| self.stored_account(a)).transpose()?;
        if previous
            .as_ref()
            .is_some_and(|stored| stored.view.status == AccountStatus::SignedOut)
        {
            return Err(service_error("account_not_found", "account was removed"));
        }
        let mut oauth = match &previous {
            Some(stored) => self.account_oauth(stored)?,
            None => self.oauth(id)?,
        };
        if let (Some(previous), Some((_, expected))) = (&previous, target) {
            if previous.view.generation != expected {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            // Reconnect repairs only base access: revoked optional permissions return only
            // through their explicit incremental-consent action. An incremental grant keeps
            // the optional scopes still both authorized and granted.
            oauth.scopes = BASE_SCOPES.iter().map(|s| (*s).to_owned()).collect();
            if permission.is_some() {
                // Reauthentication may change the configured client, never the account.
                self.fence_account(previous, expected)?;
                for scope in &previous.authorized_scopes {
                    if has_scope(&previous.view.granted_scopes, scope_name(scope))
                        && !oauth.scopes.contains(scope)
                    {
                        oauth.scopes.push(scope.clone());
                    }
                }
            }
        }
        if let Some(permission) = permission {
            oauth
                .scopes
                .push(permission_scope(id, permission, directory)?.into());
        }
        Ok((previous, oauth))
    }

    /// Browser sign-in and verification. No lock is held; nothing durable changes.
    fn authorize_microsoft_login(
        &self,
        id: IntegrationId,
        previous: Option<&StoredAccount>,
        permission: Option<AccountPermission>,
        oauth: &OAuthClient,
        authority: &str,
        attempt: &LoginAttemptId,
    ) -> Result<(Tokens, VerifiedMicrosoftIdentity, Vec<String>)> {
        let tokens = self.perform_oauth(oauth, attempt)?;
        let current = self.require_microsoft_config(id, &oauth.client_id)?;
        let grants = actual_scopes(&tokens);
        if !has_base_grants(&grants) {
            return Err(service_error(
                "insufficient_grant",
                "base calendar and identity permissions were not granted; working credentials were preserved",
            ));
        }
        let settings = self.config.settings(id);
        let backend = self.backend(id, &settings)?;
        let identity =
            backend.microsoft_identity(&tokens.access_token, &oauth.client_id, authority)?;
        if previous.is_some_and(|old| !same_identity(&old.view.identity, &identity)) {
            return Err(service_error(
                "account_identity_mismatch",
                "sign-in returned another account; working credentials were preserved",
            ));
        }
        if let Some(permission) = permission {
            let directory = if permission == AccountPermission::Directory {
                Some(self.directory_scope_of(id, &current)?)
            } else {
                None
            };
            let required = permission_scope(id, permission, directory)?;
            let lost_optional = previous.is_some_and(|old| {
                old.view
                    .granted_scopes
                    .iter()
                    .filter(|scope| has_scope(&old.authorized_scopes, scope_name(scope)))
                    .any(|scope| !has_scope(&grants, scope_name(scope)))
            });
            if !has_scope(&grants, required) || lost_optional {
                return Err(service_error(
                    "insufficient_grant",
                    "added permission was not granted; working credentials were preserved",
                ));
            }
            self.ensure_attempt(id, attempt)?;
            backend.verify_incremental(&tokens.access_token, permission)?;
        }
        Ok((tokens, identity, grants))
    }

    /// Commit under the registry lock: the attempt must still be running; it is marked
    /// committing the new credential version, which is published first, then copies that
    /// the bumped generation already fences are removed best-effort in the same critical
    /// section, before any sync or photo can publish under the new generation.
    fn commit_microsoft_login(
        &self,
        id: IntegrationId,
        target: Option<(&AccountRef, u64)>,
        permission: Option<AccountPermission>,
        attempt: &LoginAttemptId,
        authority: &str,
        grant: MicrosoftGrant,
    ) -> Result<MicrosoftAccount> {
        let provider = remote_integration(id)?;
        let MicrosoftGrant {
            tokens,
            identity,
            grants,
            authorized,
        } = grant;
        let settings = self.config.settings(id);
        self.registry(id, |dir, r| {
            // A cancellation that won the lock decides the result before anything else.
            self.require_attempt_in(id, dir, attempt)?;
            let current = self.require_microsoft_config(id, &identity.client_id)?;
            if permission == Some(AccountPermission::Directory) {
                self.directory_scope_of(id, &current)?;
            }
            if authority_host(self.endpoints(id)?)? != authority {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            let existing = r.accounts.iter().find(|a| {
                a.view.status != AccountStatus::SignedOut
                    && same_identity(&a.view.identity, &identity)
            });
            if target.is_none()
                && let Some(existing) = existing
            {
                return Err(IntegrationError::AccountExists {
                    account_id: existing.view.account.account_id.clone(),
                });
            }
            let mut stored = if let Some((account, generation)) = target {
                let mut old = r
                    .accounts
                    .iter()
                    .find(|a| &a.view.account == account)
                    .cloned()
                    .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
                if old.view.status == AccountStatus::SignedOut {
                    return Err(service_error("account_not_found", "account was removed"));
                }
                if old.view.generation != generation {
                    return Err(IntegrationError::Superseded(id.to_string()));
                }
                if !same_identity(&old.view.identity, &identity) {
                    return Err(service_error(
                        "account_identity_mismatch",
                        "sign-in returned another account; existing credentials were preserved",
                    ));
                }
                old.view.generation = persistence::bump(generation)?;
                old
            } else {
                let account_id = loop {
                    let candidate = AccountId::parse(&persistence::random_id())
                        .map_err(IntegrationError::Core)?;
                    if !r
                        .accounts
                        .iter()
                        .any(|account| account.view.account.account_id == candidate)
                    {
                        break candidate;
                    }
                };
                StoredAccount {
                    secret_key: None,
                    reauth_notified: false,
                    readable_targets: Vec::new(),
                    shared_enabled: false,
                    groups_enabled: false,
                    authorized_scopes: Vec::new(),
                    denial_fence: 0,
                    view: MicrosoftAccount {
                        account: AccountRef {
                            integration: provider,
                            account_id,
                        },
                        identity: identity.clone(),
                        generation: 1,
                        selection_generation: 0,
                        status: AccountStatus::Active,
                        granted_scopes: Vec::new(),
                        selected_calendars: Vec::new(),
                        calendar: Capability::Unknown,
                        directory: Capability::NotRequested,
                        shared_calendars: Capability::NotRequested,
                        microsoft365_groups: Capability::NotRequested,
                        last_sync: None,
                        target_results: Vec::new(),
                        last_refresh_at: None,
                        access_error: None,
                        route_reads: Vec::new(),
                    },
                }
            };
            stored.view.identity = identity;
            stored.reauth_notified = false;
            stored.view.access_error = None;
            if permission.is_none() {
                // Reconnect: optional routes return only through explicit consent.
                stored.shared_enabled = false;
                stored.groups_enabled = false;
                stored.readable_targets.clear();
                stored.view.target_results.clear();
                stored.view.route_reads.clear();
                stored.view.calendar = Capability::Unknown;
                stored.view.last_sync = None;
                stored.view.shared_calendars = Capability::NotRequested;
                stored.view.microsoft365_groups = Capability::NotRequested;
            }
            stored.view.status = AccountStatus::Active;
            stored.view.granted_scopes = grants;
            stored.authorized_scopes = authorized;
            stored.view.last_refresh_at = Some(Utc::now());
            stored.view.directory = match directory_scope(&settings) {
                Some(required)
                    if has_scope(&stored.authorized_scopes, required)
                        && has_scope(&stored.view.granted_scopes, required) =>
                {
                    if permission == Some(AccountPermission::Directory) {
                        Capability::Available
                    } else {
                        Capability::Unknown
                    }
                }
                Some(required) if has_scope(&stored.authorized_scopes, required) => {
                    Capability::Denied
                }
                _ => Capability::NotRequested,
            };
            if permission == Some(AccountPermission::SharedCalendars) {
                stored.shared_enabled = true;
                stored.view.shared_calendars = Capability::Unknown;
            }
            if permission == Some(AccountPermission::Microsoft365Groups) {
                stored.groups_enabled = true;
                stored.view.microsoft365_groups = Capability::Available;
            }
            let secret_key = persistence::secret_version(&stored.view.account);
            stored.secret_key = Some(secret_key.clone());
            let view = stored.view.clone();
            self.commit_attempt(id, dir, attempt, &secret_key, || {
                persistence::publish(dir, r, self.secrets.as_ref(), stored, &tokens)
            })?;
            // Still under the commit's lock: a sync or photo commit under this generation
            // waits for the lock, so this cleanup can never remove its copy. A cleanup failure
            // never undoes the committed sign-in.
            let account_dir = persistence::account_path(&self.data_dir, &view.account);
            if let Err(error) =
                remove_fenced_copies(&account_dir, view.generation, permission.is_none())
            {
                tracing::warn!(integration = %id, code = error.code(), "fenced account copies remain after sign-in");
            }
            Ok(view)
        })
    }
}

/// Removes the account copies a committed sign-in's bumped generation already fences:
/// photos live under their generation and cached events carry it. A reconnect drops the
/// cached calendar; an incremental grant restamps the retained copy under `generation`.
fn remove_fenced_copies(account_dir: &Path, generation: u64, reconnect: bool) -> Result<()> {
    remove_cached_photos(&account_dir.join(PHOTOS_DIR))?;
    let cache_path = account_dir.join(CALENDAR_FILE);
    if reconnect {
        return remove_file_if_exists(&cache_path);
    }
    if let Some(mut cache) = read_json_opt::<CalendarCache>(&cache_path)? {
        for event in &mut cache.events {
            if let Some(stamp) = &mut event.remote {
                stamp.generation = generation;
            }
        }
        write_json_private(&cache_path, &cache)?;
    }
    Ok(())
}

/// What a verified sign-in brings to its commit.
struct MicrosoftGrant {
    tokens: Tokens,
    identity: VerifiedMicrosoftIdentity,
    grants: Vec<String>,
    authorized: Vec<String>,
}

const BASE_SCOPES: [&str; 3] = ["offline_access", "Calendars.Read", "User.Read"];

/// `https://graph.microsoft.com/Calendars.Read` → `Calendars.Read`.
fn scope_name(scope: &str) -> &str {
    scope.rsplit('/').next().unwrap_or(scope)
}

fn permission_scope(
    id: IntegrationId,
    permission: AccountPermission,
    directory: Option<&'static str>,
) -> Result<&'static str> {
    match permission {
        AccountPermission::SharedCalendars => Ok("Calendars.Read.Shared"),
        AccountPermission::Microsoft365Groups => Ok("Group.Read.All"),
        AccountPermission::Directory => directory.ok_or_else(|| {
            IntegrationError::NotConfigured(
                id.to_string(),
                "configure a Microsoft directory permission before granting directory access"
                    .into(),
            )
        }),
    }
}

#[path = "account_operations.rs"]
mod operations;

impl Integrations {
    pub(super) fn retire_microsoft_client(
        &self,
        id: IntegrationId,
        previous: &IntegrationSettings,
    ) -> Result<ClientChange> {
        let before = previous.effective_client_id(id);
        let after = self.effective_client_id(id);
        let effective_changed = before != after.as_deref();
        let retired = self.registry(id, |dir, r| {
            if self.current_client_id(id)? != after {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            let mut retired = false;
            for stored in &mut r.accounts {
                if stored.view.status != AccountStatus::SignedOut
                    && Some(stored.view.identity.client_id.as_str()) != after.as_deref()
                    && (stored.view.status != AccountStatus::ReauthRequired
                        || stored.secret_key.is_some())
                {
                    if let Some(key) = &stored.secret_key {
                        persistence::validate_secret_key(
                            dir,
                            &stored.view.account.account_id,
                            key,
                        )?;
                    }
                    stored.view.generation = persistence::bump(stored.view.generation)?;
                    stored.view.status = AccountStatus::ReauthRequired;
                    stored.view.target_results.clear();
                    stored.view.route_reads.clear();
                    stored.readable_targets.clear();
                    if let Some(key) = stored.secret_key.take() {
                        r.pending_secrets.push(persistence::PendingSecret {
                            account_id: stored.view.account.account_id.clone(),
                            key,
                            obsolete_key: None,
                        });
                    }
                    retired = true;
                }
            }
            // Reconnect must retain verified identity; only authorization and derived copies retire.
            persistence::save(dir, r)?;
            persistence::recover(dir, r, self.secrets.as_ref())?;
            for stored in &r.accounts {
                if stored.view.status == AccountStatus::ReauthRequired {
                    let dir = persistence::account_path(&self.data_dir, &stored.view.account);
                    remove_file_if_exists(&dir.join(CALENDAR_FILE))?;
                    remove_cached_photos(&dir.join(PHOTOS_DIR))?;
                }
            }
            Ok(retired)
        })?;
        Ok(ClientChange {
            effective_changed,
            sign_in_retired: retired,
        })
    }
}

/// Each Microsoft account's photo directory at its current generation, only while the
/// account is active, unblocked, holds a credential issued to the configured client, and
/// both requested and was granted the basic-profile directory the configuration asks for.
pub(crate) fn microsoft_photo_roots(data_dir: &Path, id: IntegrationId) -> Result<Vec<PathBuf>> {
    let registry: Registry =
        read_json_opt(&account_dir(data_dir, id).join(persistence::REGISTRY_FILE))?
            .unwrap_or_default();
    let settings = IntegrationsConfig::load_settings(data_dir, id)?;
    // Photos come only from the basic-profile directory the current configuration requests.
    if directory_scope(&settings) != Some("User.ReadBasic.All") {
        return Ok(Vec::new());
    }
    let client = settings.effective_client_id(id).map(str::to_owned);
    Ok(registry
        .accounts
        .iter()
        .filter(|stored| {
            stored.view.status == AccountStatus::Active
                && stored.view.access_error.is_none()
                && stored.view.directory != Capability::Denied
                && stored.secret_key.is_some()
                && client.as_deref() == Some(stored.view.identity.client_id.as_str())
                && has_scope(&stored.authorized_scopes, "User.ReadBasic.All")
                && has_scope(&stored.view.granted_scopes, "User.ReadBasic.All")
        })
        .map(|stored| {
            persistence::account_path(data_dir, &stored.view.account)
                .join(PHOTOS_DIR)
                .join(stored.view.generation.to_string())
        })
        .collect())
}
