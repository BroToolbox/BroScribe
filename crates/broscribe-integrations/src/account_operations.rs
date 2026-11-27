//! Account-qualified operations; network calls never hold the registry lock.
use super::*;

impl Integrations {
    fn account_tokens(&self, account: &AccountRef) -> Result<(StoredAccount, Tokens)> {
        self.registry_read(integration_id(account.integration), |dir, registry| {
            let stored = registry
                .accounts
                .iter()
                .find(|a| &a.view.account == account)
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            if stored.view.access_error.as_deref() == Some("conditional_access_blocked") {
                return Err(IntegrationError::ConditionalAccessBlocked);
            }
            match stored.view.status {
                AccountStatus::SignedOut => {
                    return Err(IntegrationError::SignedOut(
                        integration_id(account.integration).to_string(),
                    ));
                }
                AccountStatus::ReauthRequired => {
                    return Err(IntegrationError::ReauthRequired(
                        integration_id(account.integration).to_string(),
                    ));
                }
                AccountStatus::Active => {}
            }
            self.fence_account(stored, stored.view.generation)?;
            let key = stored.secret_key.as_deref().ok_or_else(|| {
                IntegrationError::ReauthRequired(integration_id(account.integration).to_string())
            })?;
            persistence::validate_secret_key(dir, &account.account_id, key)?;
            let json = self.secrets.get(key)?.ok_or_else(|| {
                IntegrationError::ReauthRequired(integration_id(account.integration).to_string())
            })?;
            let tokens = serde_json::from_str(&json)
                .map_err(|_| IntegrationError::Service("invalid stored credential".into()))?;
            Ok((stored.clone(), tokens))
        })
    }

    fn account_access(
        &self,
        account: &AccountRef,
        force: bool,
        rejected: Option<&Tokens>,
    ) -> Result<(StoredAccount, Tokens)> {
        let (original, tokens) = self.account_tokens(account)?;
        if !force && tokens.is_fresh(Utc::now().timestamp()) {
            return Ok((original, tokens));
        }
        let dir = persistence::account_path(&self.data_dir, account);
        let c = coordinator(&dir);
        let _mutex = c.refresh.lock().unwrap_or_else(|e| e.into_inner());
        let _lease = FileLock::acquire(&dir.join(REFRESH_LOCK), LOCK_WAIT)?;
        let (current, old_tokens) = self.account_tokens(account)?;
        if current.view.generation != original.view.generation {
            return Err(IntegrationError::Superseded(
                integration_id(account.integration).to_string(),
            ));
        }
        if current.secret_key != original.secret_key
            || (!force && old_tokens.is_fresh(Utc::now().timestamp()))
            || rejected.is_some_and(|rejected| rejected != &old_tokens)
        {
            return Ok((current, old_tokens));
        }
        let id = integration_id(account.integration);
        let mut oauth = self.account_oauth(&current)?;
        oauth.scopes = refresh_scopes(&current);
        let refresh = old_tokens
            .refresh_token
            .as_deref()
            .ok_or_else(|| IntegrationError::ReauthRequired(id.to_string()));
        let result = refresh.and_then(|token| match oauth.refresh(token) {
            Err(IntegrationError::Transient(_)) => oauth.refresh(token),
            result => result,
        });
        let mut tokens = match result {
            Ok(tokens) => tokens,
            Err(error @ IntegrationError::ReauthRequired(_)) => {
                self.account_reauth(account, current.view.generation)?;
                return Err(error);
            }
            Err(error @ IntegrationError::ConditionalAccessBlocked) => {
                self.registry(id, |_, r| {
                    let stored = r
                        .accounts
                        .iter_mut()
                        .find(|a| &a.view.account == account)
                        .ok_or_else(|| {
                            service_error("account_not_found", "account does not exist")
                        })?;
                    self.fence_account(stored, current.view.generation)?;
                    stored.view.access_error = Some("conditional_access_blocked".into());
                    stored.view.calendar = Capability::Denied;
                    if stored.view.directory != Capability::NotRequested {
                        stored.view.directory = Capability::Denied;
                    }
                    if stored.view.shared_calendars != Capability::NotRequested {
                        stored.view.shared_calendars = Capability::Denied;
                    }
                    if stored.view.microsoft365_groups != Capability::NotRequested {
                        stored.view.microsoft365_groups = Capability::Denied;
                    }
                    Ok(())
                })?;
                return Err(error);
            }
            Err(
                error @ (IntegrationError::OAuth(_)
                | IntegrationError::Denied(_)
                | IntegrationError::Forbidden(_)),
            ) => {
                // A provider rejection of the old optional scope set requires explicit
                // base reconnect, not a hidden narrower-scope refresh or repeated retry.
                self.account_reauth(account, current.view.generation)?;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if tokens.scope.is_none() {
            tokens.scope = old_tokens.scope.clone();
        }
        let settings = self.config.settings(id);
        let identity = self.backend(id, &settings)?.microsoft_identity(
            &tokens.access_token,
            &current.view.identity.client_id,
            &current.view.identity.authority_host,
        )?;
        if !same_identity(&identity, &current.view.identity) {
            return Err(service_error(
                "account_identity_mismatch",
                "refresh returned another identity",
            ));
        }
        let grants = actual_scopes(&tokens);
        if !has_base_grants(&grants) {
            self.account_reauth(account, current.view.generation)?;
            return Err(service_error(
                "insufficient_grant",
                "refresh did not retain base calendar and identity permissions; reconnect is required",
            ));
        }
        self.registry(id, |dir, r| {
            let mut stored = r
                .accounts
                .iter()
                .find(|a| &a.view.account == account)
                .cloned()
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            self.fence_account(&stored, current.view.generation)?;
            if stored.secret_key != current.secret_key {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            stored.secret_key = Some(persistence::secret_version(account));
            stored.view.granted_scopes = grants;
            // Lost optional consent cannot revive through an unexpectedly broader token:
            // only an explicit incremental grant restores its authorization/opt-in.
            let directory_was_authorized = directory_scope(&settings)
                .is_some_and(|required| has_scope(&stored.authorized_scopes, required));
            let grants = &stored.view.granted_scopes;
            stored.authorized_scopes.retain(|scope| {
                scope == "offline_access"
                    || has_scope(grants, scope.rsplit('/').next().unwrap_or(scope))
            });
            if stored.shared_enabled
                && !(has_scope(grants, "Calendars.Read.Shared")
                    && has_scope(&stored.authorized_scopes, "Calendars.Read.Shared"))
            {
                stored.shared_enabled = false;
                stored.view.shared_calendars = Capability::Denied;
            }
            if stored.groups_enabled
                && !(has_scope(grants, "Group.Read.All")
                    && has_scope(&stored.authorized_scopes, "Group.Read.All"))
            {
                stored.groups_enabled = false;
                stored.view.microsoft365_groups = Capability::Denied;
            }
            if directory_was_authorized
                && directory_scope(&settings).is_some_and(|required| {
                    !has_scope(grants, required) || !has_scope(&stored.authorized_scopes, required)
                })
            {
                stored.view.directory = Capability::Denied;
            }
            let shared = stored.shared_enabled;
            let groups = stored.groups_enabled;
            let route_authorized = |target: &CalendarTarget| match target {
                CalendarTarget::Own { .. } => true,
                CalendarTarget::Group { .. } => groups,
                _ => shared,
            };
            stored.readable_targets.retain(&route_authorized);
            for result in &mut stored.view.target_results {
                if !route_authorized(&result.target) {
                    result.outcome = CalendarTargetOutcome::Denied {
                        code: "insufficient_grant".into(),
                    };
                }
            }
            // A withdrawn route's copy is no longer served, and its earlier reads were
            // under an authorization it no longer has.
            for read in &mut stored.view.route_reads {
                if !route_authorized(&read.target) {
                    read.last_success = None;
                    read.copy = RouteCopy::Empty;
                    read.cached_events = 0;
                }
            }
            if stored
                .view
                .target_results
                .iter()
                .any(|result| matches!(result.outcome, CalendarTargetOutcome::Denied { .. }))
            {
                stored.view.calendar =
                    if stored.view.target_results.iter().any(|result| {
                        matches!(result.outcome, CalendarTargetOutcome::Available { .. })
                    }) {
                        Capability::Partial
                    } else {
                        Capability::Denied
                    };
            }
            stored.view.last_refresh_at = Some(Utc::now());
            persistence::publish(dir, r, self.secrets.as_ref(), stored.clone(), &tokens)?;
            Ok((stored, tokens))
        })
    }

    fn account_authenticated<T>(
        &self,
        account: &AccountRef,
        request: impl Fn(&StoredAccount, &str) -> Result<T>,
    ) -> Result<(StoredAccount, Result<T>)> {
        let (stored, tokens) = self.account_access(account, false, None)?;
        let result = request(&stored, &tokens.access_token);
        if !matches!(result, Err(IntegrationError::ReauthRequired(_))) {
            return Ok((stored, result));
        }
        let (refreshed, tokens) = self.account_access(account, true, Some(&tokens))?;
        if refreshed.view.generation != stored.view.generation {
            return Err(IntegrationError::Superseded(
                integration_id(account.integration).to_string(),
            ));
        }
        let result = request(&refreshed, &tokens.access_token);
        if matches!(result, Err(IntegrationError::ReauthRequired(_))) {
            self.account_reauth(account, refreshed.view.generation)?;
        }
        Ok((refreshed, result))
    }

    pub fn keep_account_alive(&self, account: &AccountRef) -> Result<()> {
        let stored = self.stored_account(account)?;
        match stored.view.status {
            AccountStatus::SignedOut => return Ok(()),
            AccountStatus::ReauthRequired => {
                return self.account_reauth(account, stored.view.generation);
            }
            AccountStatus::Active => {}
        }
        let daily = stored
            .view
            .last_refresh_at
            .is_none_or(|at| Utc::now().signed_duration_since(at).num_seconds() >= DAY);
        self.account_access(account, daily, None).map(|_| ())
    }

    fn account_reauth(&self, account: &AccountRef, generation: u64) -> Result<()> {
        let notify = self.registry(integration_id(account.integration), |_, r| {
            let stored = r
                .accounts
                .iter_mut()
                .find(|a| &a.view.account == account)
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            self.require_account_current(stored)?;
            if stored.view.generation != generation
                || stored.view.status == AccountStatus::SignedOut
            {
                return Err(IntegrationError::Superseded(
                    integration_id(account.integration).to_string(),
                ));
            }
            stored.view.status = AccountStatus::ReauthRequired;
            let notify = !stored.reauth_notified;
            stored.reauth_notified |= self.reauth_notification_delivery;
            Ok(notify)
        })?;
        if notify {
            (self.notify)(ReauthNotification {
                integration: integration_id(account.integration),
                account: Some(account.clone()),
                message: "Sign in to restore this Microsoft account's access.".into(),
            });
        }
        Ok(())
    }

    pub fn remove_account(&self, account: &AccountRef, expected_generation: u64) -> Result<()> {
        let account_dir = persistence::account_path(&self.data_dir, account);
        check_profile_directories(&self.data_dir, &account_dir)?;
        self.registry(integration_id(account.integration), |dir, r| {
            let stored = r
                .accounts
                .iter_mut()
                .find(|a| &a.view.account == account)
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            if stored.view.generation != expected_generation {
                return Err(IntegrationError::Superseded(
                    integration_id(account.integration).to_string(),
                ));
            }
            if let Some(key) = &stored.secret_key {
                persistence::validate_secret_key(dir, &account.account_id, key)?;
            }
            stored.view.generation = persistence::bump(stored.view.generation)?;
            stored.view.status = AccountStatus::SignedOut;
            stored.view.selected_calendars.clear();
            stored.view.target_results.clear();
            stored.view.route_reads.clear();
            stored.readable_targets.clear();
            if let Some(key) = stored.secret_key.take() {
                r.pending_secrets.push(persistence::PendingSecret {
                    account_id: account.account_id.clone(),
                    key,
                    obsolete_key: None,
                });
            }
            persistence::save(dir, r)?;
            persistence::recover(dir, r, self.secrets.as_ref())?;
            check_profile_directories(&self.data_dir, &account_dir)?;
            remove_file_if_exists(&account_dir.join(CALENDAR_FILE))?;
            remove_cached_photos(&account_dir.join(PHOTOS_DIR))
        })
    }

    pub fn list_calendars(&self, account: &AccountRef) -> Result<MicrosoftCalendarCatalog> {
        let id = integration_id(account.integration);
        let settings = self.config.settings(id);
        let backend = self.backend(id, &settings)?;
        let (stored, result) = self.account_authenticated(account, |stored, token| {
            let mut calendars =
                backend.microsoft_catalog(token, &stored.view.identity.verified_addresses)?;
            let mut groups = Capability::NotRequested;
            let mut groups_error = None;
            if stored.groups_enabled
                && has_scope(&stored.view.granted_scopes, "Group.Read.All")
                && has_scope(&stored.authorized_scopes, "Group.Read.All")
            {
                match backend.microsoft_groups(token) {
                    Ok(found) => {
                        calendars.extend(found);
                        groups = Capability::Available;
                    }
                    Err(error) => {
                        groups = if matches!(error, IntegrationError::Forbidden(_)) {
                            Capability::Denied
                        } else {
                            Capability::Error
                        };
                        groups_error = Some(CalendarListError {
                            code: error.code().into(),
                            message: error.to_string(),
                        });
                    }
                }
            }
            Ok((calendars, groups, groups_error))
        })?;
        let (calendars, groups, groups_error) = result?;
        self.registry(id, |_, r| {
            let current = r
                .accounts
                .iter_mut()
                .find(|a| &a.view.account == account)
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            self.fence_account(current, stored.view.generation)?;
            if current.view.selection_generation != stored.view.selection_generation {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            if groups != Capability::NotRequested
                && !(current.groups_enabled
                    && has_scope(&current.view.granted_scopes, "Group.Read.All")
                    && has_scope(&current.authorized_scopes, "Group.Read.All"))
            {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            current.view.microsoft365_groups = groups;
            Ok(())
        })?;
        Ok(MicrosoftCalendarCatalog {
            account: account.clone(),
            generation: stored.view.generation,
            selection_generation: stored.view.selection_generation,
            calendars,
            groups,
            groups_error,
        })
    }

    pub fn configure_calendars(
        &self,
        account: &AccountRef,
        expected_generation: u64,
        expected_selection_generation: u64,
        targets: &[CalendarTarget],
    ) -> Result<MicrosoftAccount> {
        let id = integration_id(account.integration);
        for target in targets {
            target.validate().map_err(IntegrationError::Core)?;
        }
        if targets.is_empty() {
            return self.commit_calendar_selection(
                account,
                expected_generation,
                expected_selection_generation,
                targets,
            );
        }
        let catalog = self.list_calendars(account)?;
        if catalog.generation != expected_generation
            || catalog.selection_generation != expected_selection_generation
        {
            return Err(IntegrationError::Superseded(id.to_string()));
        }
        let settings = self.config.settings(id);
        let backend = self.backend(id, &settings)?;
        let (snapshot, result) = self.account_authenticated(account, |stored, token| {
            for target in targets {
                match target {
                    CalendarTarget::Own { .. }
                    | CalendarTarget::RecipientShared { .. }
                    | CalendarTarget::Group { .. } => {
                        if !catalog.calendars.iter().any(|c| &c.target == target) {
                            return Err(service_error(
                                "invalid_calendar_target",
                                "calendar target is not in this account's catalog",
                            ));
                        }
                    }
                    CalendarTarget::Delegated { .. } => {}
                }
                if !calendar_target_authorized(stored, target) {
                    return Err(service_error(
                        "insufficient_grant",
                        "selected route's permission is not explicitly enabled and granted",
                    ));
                }
                backend.verify_target(token, target)?;
            }
            Ok(())
        })?;
        result?;
        if snapshot.view.generation != expected_generation
            || snapshot.view.selection_generation != expected_selection_generation
        {
            return Err(IntegrationError::Superseded(id.to_string()));
        }
        self.commit_calendar_selection(
            account,
            expected_generation,
            expected_selection_generation,
            targets,
        )
    }

    fn commit_calendar_selection(
        &self,
        account: &AccountRef,
        expected_generation: u64,
        expected_selection_generation: u64,
        targets: &[CalendarTarget],
    ) -> Result<MicrosoftAccount> {
        let id = integration_id(account.integration);
        let mut selected = Vec::new();
        for target in targets {
            if !selected.contains(target) {
                selected.push(target.clone());
            }
        }
        self.registry(id, |dir, r| {
            let stored = r
                .accounts
                .iter_mut()
                .find(|a| &a.view.account == account)
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            self.fence_account(stored, expected_generation)?;
            if stored.view.selection_generation != expected_selection_generation {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            stored.view.selection_generation = persistence::bump(expected_selection_generation)?;
            stored.view.selected_calendars = selected;
            stored.view.target_results.clear();
            stored.view.route_reads.clear();
            stored.readable_targets.clear();
            stored.view.calendar = Capability::Unknown;
            stored.view.last_sync = None;
            let view = stored.view.clone();
            persistence::save(dir, r)?;
            remove_file_if_exists(
                &persistence::account_path(&self.data_dir, account).join(CALENDAR_FILE),
            )?;
            Ok(view)
        })
    }

    /// Reads the selected routes and publishes the account's private cache. The response
    /// carries outcomes and counts only, never events. Three stages: an admission snapshot,
    /// route reads with no lock held, then one commit under the registry lock. An account
    /// already removed, blocked or needing reconnect is refused at admission with its own
    /// code, before any credential or network use; `superseded` means only that the account
    /// changed while this request ran.
    pub fn sync_account(
        &self,
        account: &AccountRef,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<AccountCalendarSync> {
        let initial = self.stored_account(account)?;
        self.admit_account(&initial)?;
        self.fence_account(&initial, initial.view.generation)?;
        if to <= from || to.signed_duration_since(from) > chrono::Duration::days(366) {
            return Err(service_error(
                "invalid_calendar_interval",
                "calendar interval must be positive and at most 366 days",
            ));
        }
        for target in &initial.view.selected_calendars {
            target.validate().map_err(IntegrationError::Core)?;
        }
        let (targets, events) = self.read_selected_routes(account, &initial, from, to)?;
        self.commit_account_sync(account, &initial, targets, events)
    }

    /// What the account's durable state already refuses, before a request starts: a removed
    /// account (`account_not_found`, as reconnect reports it), a Conditional Access block,
    /// or a sign-in that needs an explicit reconnect (`reauth_required`). None of these is
    /// a change while the request ran, so none is reported as `superseded`.
    fn admit_account(&self, stored: &StoredAccount) -> Result<()> {
        let id = integration_id(stored.view.account.integration);
        if stored.view.access_error.as_deref() == Some("conditional_access_blocked") {
            return Err(IntegrationError::ConditionalAccessBlocked);
        }
        match stored.view.status {
            AccountStatus::SignedOut => {
                Err(service_error("account_not_found", "account was removed"))
            }
            AccountStatus::ReauthRequired => Err(IntegrationError::ReauthRequired(id.to_string())),
            AccountStatus::Active => {
                let current = self.authoritative(id)?;
                self.account_current_in(stored, &current)
                    .map_err(|error| match error {
                        IntegrationError::Superseded(_) => {
                            IntegrationError::ReauthRequired(id.to_string())
                        }
                        error => error,
                    })
            }
        }
    }

    /// Network stage: each selected route's outcome and events. No lock is held.
    fn read_selected_routes(
        &self,
        account: &AccountRef,
        initial: &StoredAccount,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<(Vec<CalendarTargetResult>, Vec<CalendarEvent>)> {
        let id = integration_id(account.integration);
        let settings = self.config.settings(id);
        let backend = self.backend(id, &settings)?;
        let mut targets = Vec::new();
        let mut events = Vec::new();
        for target in &initial.view.selected_calendars {
            let read = self.account_authenticated(account, |stored, token| {
                if stored.view.generation != initial.view.generation
                    || stored.view.selection_generation != initial.view.selection_generation
                {
                    return Err(IntegrationError::Superseded(id.to_string()));
                }
                if !calendar_target_authorized(stored, target) {
                    return Err(service_error(
                        "insufficient_grant",
                        "selected route's permission is not explicitly enabled and granted",
                    ));
                }
                backend.microsoft_calendar(
                    token,
                    &stored.view.identity,
                    RemoteCalendarStamp {
                        account: account.clone(),
                        generation: initial.view.generation,
                        selection_generation: initial.view.selection_generation,
                        target: target.clone(),
                    },
                    from,
                    to,
                )
            });
            let outcome = match read? {
                (_, Ok(found)) => {
                    let count = found.len();
                    events.extend(found);
                    CalendarTargetOutcome::Available { event_count: count }
                }
                (_, Err(error @ IntegrationError::Superseded(_))) => return Err(error),
                (_, Err(error))
                    if matches!(error, IntegrationError::Forbidden(_))
                        || error.code() == "insufficient_grant" =>
                {
                    CalendarTargetOutcome::Denied {
                        code: error.code().into(),
                    }
                }
                (_, Err(error)) => CalendarTargetOutcome::Error {
                    code: error.code().into(),
                },
            };
            targets.push(CalendarTargetResult {
                target: target.clone(),
                outcome,
            });
        }
        Ok((targets, events))
    }

    /// Commit stage, under the registry lock. Any observed denial advances the account's
    /// denial fence (and, for a route that was readable, its selection generation) and is
    /// saved before the fallible cache replacement: a failed cache write can never leave a
    /// withdrawn route published, and a read that started before the denial never commits.
    fn commit_account_sync(
        &self,
        account: &AccountRef,
        initial: &StoredAccount,
        targets: Vec<CalendarTargetResult>,
        events: Vec<CalendarEvent>,
    ) -> Result<AccountCalendarSync> {
        let id = integration_id(account.integration);
        let outcome = sync_outcome(&targets);
        let cache_path = persistence::account_path(&self.data_dir, account).join(CALENDAR_FILE);
        let current = self.authoritative(id)?;
        self.registry(id, |dir, r| {
            let index = r
                .accounts
                .iter()
                .position(|a| &a.view.account == account)
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            let denied: Vec<&CalendarTarget> = targets
                .iter()
                .filter(|t| matches!(t.outcome, CalendarTargetOutcome::Denied { .. }))
                .map(|t| &t.target)
                .collect();
            {
                let stored = &mut r.accounts[index];
                self.fence_account_in(stored, initial.view.generation, &current)?;
                if stored.view.selection_generation != initial.view.selection_generation
                    || stored.denial_fence != initial.denial_fence
                    || targets.iter().any(|result| {
                        matches!(result.outcome, CalendarTargetOutcome::Available { .. })
                            && !calendar_target_authorized(stored, &result.target)
                    })
                {
                    return Err(IntegrationError::Superseded(id.to_string()));
                }
                if !denied.is_empty() {
                    stored.denial_fence = persistence::bump(stored.denial_fence)?;
                    if denied.iter().any(|t| stored.readable_targets.contains(t)) {
                        stored.view.selection_generation =
                            persistence::bump(stored.view.selection_generation)?;
                    }
                    stored.readable_targets.retain(|t| !denied.contains(&t));
                }
            }
            if !denied.is_empty() {
                persistence::save(dir, r)?;
            }
            let stored = &mut r.accounts[index];
            let now = Utc::now();
            let mut cache = CalendarCache { events };
            let previous = read_json_opt::<CalendarCache>(&cache_path).unwrap_or_else(|error| {
                tracing::warn!(
                    code = error.code(),
                    "unreadable account calendar copy is replaced"
                );
                None
            });
            if let Some(previous) = previous {
                // A transient failure keeps the route's last successful read.
                cache
                    .events
                    .extend(previous.events.into_iter().filter(|event| {
                        event.remote.as_ref().is_some_and(|stamp| {
                            stamp.account == *account
                                && stamp.generation == initial.view.generation
                                && stamp.selection_generation == initial.view.selection_generation
                                && stored.readable_targets.contains(&stamp.target)
                                && targets.iter().any(|result| {
                                    result.target == stamp.target
                                        && matches!(
                                            result.outcome,
                                            CalendarTargetOutcome::Error { .. }
                                        )
                                })
                        })
                    }));
            }
            cache.prune(now);
            let selection_generation = stored.view.selection_generation;
            if selection_generation != initial.view.selection_generation {
                for event in &mut cache.events {
                    if let Some(stamp) = &mut event.remote {
                        stamp.selection_generation = selection_generation;
                    }
                }
            }
            write_json_private_if_changed(&cache_path, &cache)?;
            stored.view.target_results = targets.clone();
            for result in &targets {
                if matches!(result.outcome, CalendarTargetOutcome::Available { .. })
                    && !stored.readable_targets.contains(&result.target)
                {
                    stored.readable_targets.push(result.target.clone());
                }
            }
            stored.view.last_sync = Some(now);
            stored.view.calendar = match outcome {
                AccountSyncOutcome::Available => Capability::Available,
                AccountSyncOutcome::NoSelection => Capability::NotRequested,
                AccountSyncOutcome::Partial => Capability::Partial,
                AccountSyncOutcome::Denied => Capability::Denied,
                AccountSyncOutcome::Error => Capability::Error,
            };
            stored.view.shared_calendars =
                route_capability(&targets, false, stored.view.shared_calendars);
            stored.view.microsoft365_groups =
                route_capability(&targets, true, stored.view.microsoft365_groups);
            stored.view.route_reads = route_reads(
                &stored.view.selected_calendars,
                &targets,
                &cache,
                &stored.view.route_reads,
                now,
            );
            Ok(AccountCalendarSync {
                account: account.clone(),
                generation: initial.view.generation,
                selection_generation,
                outcome,
                targets,
                routes: stored.view.route_reads.clone(),
                event_count: cache.events.len(),
            })
        })
    }

    /// Removes one selected route without touching the others. The removal is saved before
    /// the cache is rewritten; the remaining routes' cached events stay valid under the new
    /// selection generation, and the removed route's events are dropped. When that rewrite
    /// fails the removal still stands: the result reports the failure and the remaining
    /// routes as holding nothing, since their old copy is fenced until the next sync.
    pub fn remove_calendar_target(
        &self,
        account: &AccountRef,
        expected_generation: u64,
        expected_selection_generation: u64,
        target: &CalendarTarget,
    ) -> Result<CalendarTargetRemoval> {
        let id = integration_id(account.integration);
        let cache_path = persistence::account_path(&self.data_dir, account).join(CALENDAR_FILE);
        // Saved explicitly below: the removal before the copy, and a best-effort route
        // status afterwards that never turns the committed removal into a failure.
        self.registry_access(id, false, false, |dir, r| {
            let index = r
                .accounts
                .iter()
                .position(|a| &a.view.account == account)
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            let stored = &mut r.accounts[index];
            self.fence_account(stored, expected_generation)?;
            if stored.view.selection_generation != expected_selection_generation {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            if !stored.view.selected_calendars.contains(target) {
                return Err(service_error(
                    "invalid_calendar_target",
                    "calendar target is not selected for this account",
                ));
            }
            let selection_generation = persistence::bump(expected_selection_generation)?;
            stored.view.selection_generation = selection_generation;
            stored.view.selected_calendars.retain(|t| t != target);
            stored.view.target_results.retain(|t| &t.target != target);
            stored.view.route_reads.retain(|t| &t.target != target);
            stored.readable_targets.retain(|t| t != target);
            if stored.view.selected_calendars.is_empty() {
                stored.view.calendar = Capability::NotRequested;
            }
            persistence::save(dir, r)?;
            let carried = read_json_opt::<CalendarCache>(&cache_path).and_then(|cache| {
                let Some(mut cache) = cache else {
                    return Ok(());
                };
                cache.events.retain_mut(|event| {
                    let Some(stamp) = &mut event.remote else {
                        return false;
                    };
                    if stamp.target == *target
                        || stamp.generation != expected_generation
                        || stamp.selection_generation != expected_selection_generation
                    {
                        return false;
                    }
                    stamp.selection_generation = selection_generation;
                    true
                });
                write_json_private(&cache_path, &cache)
            });
            let Err(error) = carried else {
                return Ok(CalendarTargetRemoval {
                    account: r.accounts[index].view.clone(),
                    copy_error: None,
                });
            };
            tracing::warn!(
                code = error.code(),
                "calendar route removed; the other routes' copy waits for the next sync"
            );
            let stored = &mut r.accounts[index];
            for read in &mut stored.view.route_reads {
                read.copy = RouteCopy::Empty;
                read.cached_events = 0;
            }
            let account = stored.view.clone();
            if let Err(error) = persistence::save(dir, r) {
                tracing::warn!(
                    code = error.code(),
                    "route status stays stale until the next sync"
                );
            }
            Ok(CalendarTargetRemoval {
                account,
                copy_error: Some(error.public()),
            })
        })
    }

    pub fn account_authority(&self, account: &AccountRef) -> Result<Option<AccountAuthority>> {
        let stored = self.stored_account(account)?;
        if stored.view.status != AccountStatus::Active
            || stored.view.access_error.is_some()
            || self.require_account_current(&stored).is_err()
        {
            return Ok(None);
        }
        let selected_calendars = stored
            .view
            .selected_calendars
            .iter()
            .filter(|target| {
                target.validate().is_ok()
                    && stored.readable_targets.contains(target)
                    && calendar_target_authorized(&stored, target)
            })
            .cloned()
            .collect();
        Ok(Some(AccountAuthority {
            account: account.clone(),
            generation: stored.view.generation,
            selection_generation: stored.view.selection_generation,
            selected_calendars,
        }))
    }

    pub fn cached_account_calendar(&self, account: &AccountRef) -> Result<CalendarCache> {
        let Some(authority) = self.account_authority(account)? else {
            return Ok(CalendarCache::default());
        };
        let mut cache: CalendarCache =
            read_json_opt(&persistence::account_path(&self.data_dir, account).join(CALENDAR_FILE))?
                .unwrap_or_default();
        cache.events.retain(|e| {
            e.remote.as_ref().is_some_and(|s| {
                s.account == authority.account
                    && s.generation == authority.generation
                    && s.selection_generation == authority.selection_generation
                    && authority.selected_calendars.contains(&s.target)
            })
        });
        cache.prune(Utc::now());
        if self.account_authority(account)? != Some(authority) {
            return Err(IntegrationError::Superseded(
                integration_id(account.integration).to_string(),
            ));
        }
        Ok(cache)
    }

    pub fn account_directory(
        &self,
        account: &AccountRef,
        query: &str,
    ) -> Result<AccountDirectoryResult> {
        let id = integration_id(account.integration);
        let settings = self.config.settings(id);
        if !directory_requested(id, &settings) {
            let stored = self.stored_account(account)?;
            self.fence_account(&stored, stored.view.generation)?;
            return Ok(AccountDirectoryResult {
                account: account.clone(),
                generation: stored.view.generation,
                people: Vec::new(),
            });
        }
        let required = self.require_directory_scope(id)?;
        let backend = self.backend(id, &settings)?;
        let (stored, result) = self.account_authenticated(account, |stored, token| {
            if !has_scope(&stored.view.granted_scopes, required) || !has_scope(&stored.authorized_scopes, required) { return Err(service_error("insufficient_grant", "directory permission has not been explicitly requested and granted for this account")); }
            backend.directory(token, query)
        })?;
        // Publication rechecks the current configuration and this account's grants: a
        // refresh or retirement that withdrew the directory permission meanwhile wins.
        let current_settings = self.authoritative(id)?;
        self.registry(id, |_, r| {
            let current = r
                .accounts
                .iter_mut()
                .find(|a| &a.view.account == account)
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            self.fence_account_in(current, stored.view.generation, &current_settings)?;
            let required = self.directory_scope_of(id, &current_settings)?;
            if result.is_ok()
                && !(has_scope(&current.view.granted_scopes, required)
                    && has_scope(&current.authorized_scopes, required))
            {
                return Err(IntegrationError::Superseded(id.to_string()));
            }
            current.view.directory = match &result {
                Ok(_) => Capability::Available,
                Err(error)
                    if matches!(error, IntegrationError::Forbidden(_))
                        || error.code() == "insufficient_grant" =>
                {
                    Capability::Denied
                }
                Err(_) => Capability::Error,
            };
            Ok(())
        })?;
        let mut people = result?;
        for person in &mut people {
            person.source = Some(AccountDirectorySource {
                account: account.clone(),
                generation: stored.view.generation,
            });
        }
        Ok(AccountDirectoryResult {
            account: account.clone(),
            generation: stored.view.generation,
            people,
        })
    }

    pub fn cache_account_photo(
        &self,
        account: &AccountRef,
        expected_generation: u64,
        person_id: &str,
        remote: &DirectoryPerson,
        person_exists: &dyn Fn(&str) -> bool,
    ) -> Result<Option<PathBuf>> {
        if remote.source.as_ref()
            != Some(&AccountDirectorySource {
                account: account.clone(),
                generation: expected_generation,
            })
        {
            return Err(service_error(
                "account_identity_mismatch",
                "directory result belongs to another account or generation",
            ));
        }
        let name = photo_file_name(person_id)?;
        if !person_exists(person_id) {
            return Ok(None);
        }
        if remote.photo_url.is_none() {
            return Ok(None);
        }
        let id = integration_id(account.integration);
        let settings = self.config.settings(id);
        if self.require_directory_scope(id)? != "User.ReadBasic.All" {
            return Err(service_error(
                "insufficient_grant",
                "directory photo permission is not configured",
            ));
        }
        let backend = self.backend(id, &settings)?;
        let (stored, result) = self.account_authenticated(account, |stored, token| {
            if stored.view.generation != expected_generation { return Err(IntegrationError::Superseded(id.to_string())); }
            if !has_scope(&stored.view.granted_scopes, "User.ReadBasic.All") || !has_scope(&stored.authorized_scopes, "User.ReadBasic.All") {
                return Err(service_error("insufficient_grant", "directory photo permission has not been explicitly requested and granted for this account"));
            }
            backend.photo(token, remote)
        })?;
        let Some(photo) = result? else {
            return Ok(None);
        };
        self.commit_for_account_generation(account, stored.view.generation, || {
            if !person_exists(person_id) {
                return Ok(None);
            }
            let path = persistence::account_path(&self.data_dir, account)
                .join(PHOTOS_DIR)
                .join(stored.view.generation.to_string())
                .join(name);
            write_private(&path, &photo)?;
            Ok(Some(path))
        })
    }

    pub fn commit_for_account_generation<T>(
        &self,
        account: &AccountRef,
        generation: u64,
        commit: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let id = integration_id(account.integration);
        let current = self.authoritative(id)?;
        self.registry_read(id, |_, r| {
            let stored = r
                .accounts
                .iter()
                .find(|a| &a.view.account == account)
                .ok_or_else(|| service_error("account_not_found", "account does not exist"))?;
            self.fence_account_in(stored, generation, &current)?;
            let required = self.directory_scope_of(id, &current)?;
            if !has_scope(&stored.view.granted_scopes, required)
                || !has_scope(&stored.authorized_scopes, required)
            {
                return Err(service_error(
                    "insufficient_grant",
                    "directory permission is no longer authorized and granted",
                ));
            }
            commit()
        })
    }
}

/// The scopes a refresh asks for: only grants the account both requested and received, so a
/// configuration change never adds one (refresh scopes must be a subset of the original
/// authorization). `offline_access` was part of every original request and is kept even
/// when a token response does not echo it: Microsoft documents that a refresh token is
/// returned only when `offline_access` is requested.
pub(super) fn refresh_scopes(stored: &StoredAccount) -> Vec<String> {
    let mut scopes: Vec<String> = stored
        .view
        .granted_scopes
        .iter()
        .filter(|scope| has_scope(&stored.authorized_scopes, scope_name(scope)))
        .cloned()
        .collect();
    if has_scope(&stored.authorized_scopes, "offline_access")
        && !has_scope(&scopes, "offline_access")
    {
        scopes.insert(0, "offline_access".into());
    }
    scopes
}

fn sync_outcome(targets: &[CalendarTargetResult]) -> AccountSyncOutcome {
    let succeeded = targets
        .iter()
        .filter(|t| matches!(t.outcome, CalendarTargetOutcome::Available { .. }))
        .count();
    if targets.is_empty() {
        AccountSyncOutcome::NoSelection
    } else if succeeded == targets.len() {
        AccountSyncOutcome::Available
    } else if succeeded != 0 {
        AccountSyncOutcome::Partial
    } else if targets
        .iter()
        .all(|t| matches!(t.outcome, CalendarTargetOutcome::Denied { .. }))
    {
        AccountSyncOutcome::Denied
    } else {
        AccountSyncOutcome::Error
    }
}

/// The shared (`group == false`) or group route capability after a sync; `previous` when
/// the sync read no such route.
fn route_capability(
    targets: &[CalendarTargetResult],
    group: bool,
    previous: Capability,
) -> Capability {
    let (total, succeeded, denied) = targets
        .iter()
        .filter(|r| {
            if group {
                matches!(r.target, CalendarTarget::Group { .. })
            } else {
                matches!(
                    r.target,
                    CalendarTarget::RecipientShared { .. } | CalendarTarget::Delegated { .. }
                )
            }
        })
        .fold((0, 0, 0), |(total, succeeded, denied), result| {
            (
                total + 1,
                succeeded
                    + usize::from(matches!(
                        result.outcome,
                        CalendarTargetOutcome::Available { .. }
                    )),
                denied
                    + usize::from(matches!(
                        result.outcome,
                        CalendarTargetOutcome::Denied { .. }
                    )),
            )
        });
    if total == 0 {
        previous
    } else if succeeded == total {
        Capability::Available
    } else if succeeded != 0 {
        Capability::Partial
    } else if denied == total {
        Capability::Denied
    } else {
        Capability::Error
    }
}

/// Per selected route: when it was last read successfully and whether the published cache
/// holds this sync's events, the last successful read's (transient failure), or nothing.
fn route_reads(
    selected: &[CalendarTarget],
    targets: &[CalendarTargetResult],
    cache: &CalendarCache,
    previous: &[CalendarRouteRead],
    now: DateTime<Utc>,
) -> Vec<CalendarRouteRead> {
    selected
        .iter()
        .map(|target| {
            let cached_events = cache
                .events
                .iter()
                .filter(|e| e.remote.as_ref().is_some_and(|s| &s.target == target))
                .count();
            let last_success = previous
                .iter()
                .find(|p| &p.target == target)
                .and_then(|p| p.last_success);
            let outcome = targets
                .iter()
                .find(|r| &r.target == target)
                .map(|r| &r.outcome);
            let (last_success, copy) = match outcome {
                Some(CalendarTargetOutcome::Available { .. }) => (Some(now), RouteCopy::Fresh),
                Some(CalendarTargetOutcome::Error { .. }) if cached_events != 0 => {
                    (last_success, RouteCopy::Retained)
                }
                // Earlier reads were under an authorization the route no longer has.
                Some(CalendarTargetOutcome::Denied { .. }) => (None, RouteCopy::Empty),
                _ => (last_success, RouteCopy::Empty),
            };
            CalendarRouteRead {
                target: target.clone(),
                last_success,
                copy,
                cached_events,
            }
        })
        .collect()
}
