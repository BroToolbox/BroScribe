//! Public synthetic regressions for tombstones and independently revocable optional grants.
use super::*;

const BASE_ONLY: &str = "Calendars.Read User.Read";
const ALL_GRANTS: &str =
    "Calendars.Read User.Read User.ReadBasic.All Calendars.Read.Shared Group.Read.All";

fn group() -> CalendarTarget {
    CalendarTarget::Group {
        group_id: "same-group".into(),
    }
}
fn credentials(h: &Accounts, account: &AccountRef) -> BTreeMap<String, String> {
    let prefix = format!(
        "integration/{}/accounts/{}/tokens/",
        account.integration.as_str(),
        account.account_id
    );
    h.secrets
        .secrets
        .lock()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}
fn current(h: &Accounts, account: &AccountRef) -> MicrosoftAccount {
    h.service
        .list_accounts(None)
        .unwrap()
        .into_iter()
        .find(|found| &found.account == account)
        .unwrap()
}
fn enable_optional(h: &Accounts, account: MicrosoftAccount) -> MicrosoftAccount {
    *h.cloud.signin.lock().unwrap() = "a".into();
    *h.cloud.grants.lock().unwrap() = ALL_GRANTS.into();
    let account = h
        .service
        .grant_account_permission(
            &account.account,
            account.generation,
            AccountPermission::SharedCalendars,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    h.service
        .grant_account_permission(
            &account.account,
            account.generation,
            AccountPermission::Microsoft365Groups,
            &LoginAttemptId::generate(),
        )
        .unwrap()
}

#[test]
fn removed_identity_adds_a_fresh_handle_and_never_revives_old_secret_or_authority() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let old = h.add("a");
    let old = h.select(&old, &[own()]);
    h.sync(&old.account);
    let b = h.add("b");
    let b = h.select(&b, &[own()]);
    h.sync(&b.account);
    let b_view = current(&h, &b.account);
    let b_authority = h.service.account_authority(&b.account).unwrap();
    let b_cache = h.service.cached_account_calendar(&b.account).unwrap();
    let b_credentials = credentials(&h, &b.account);
    let old_authority = h.service.account_authority(&old.account).unwrap().unwrap();
    let old_credentials = credentials(&h, &old.account);
    h.service
        .remove_account(&old.account, old.generation)
        .unwrap();
    assert!(h.service.account_authority(&old.account).unwrap().is_none());
    assert!(credentials(&h, &old.account).is_empty());
    let before_reconnect = h.server.requests.lock().unwrap().len();
    for generation in [old.generation, old.generation + 1] {
        assert_eq!(
            h.service
                .reconnect_account(&old.account, generation, &LoginAttemptId::generate())
                .unwrap_err()
                .code(),
            "account_not_found"
        );
    }
    assert_eq!(h.server.requests.lock().unwrap().len(), before_reconnect);

    // An interrupted old-slot secret cleanup must remain in the old namespace and
    // cannot be adopted by the subsequent new sign-in of the same verified identity.
    let orphan = format!(
        "integration/microsoft-graph/accounts/{}/tokens/55555555555555555555555555555555",
        old.account.account_id
    );
    h.secrets
        .set(&orphan, "synthetic uncommitted old-slot credential")
        .unwrap();
    let registry_path =
        account_dir(h.tmp.path(), IntegrationId::MicrosoftGraph).join("accounts.json");
    let mut registry: Value =
        serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
    registry["pending_secrets"] =
        json!([{"account_id":old.account.account_id,"key":orphan,"obsolete_key":null}]);
    write_json_private(&registry_path, &registry).unwrap();
    let added = h.add("a");
    assert_ne!(added.account, old.account);
    let before_reconnect = h.server.requests.lock().unwrap().len();
    assert_eq!(
        h.service
            .reconnect_account(
                &old.account,
                old.generation + 1,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "account_not_found"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), before_reconnect);
    assert_eq!(
        added.identity.resource_tenant_id,
        old.identity.resource_tenant_id
    );
    assert_eq!(added.identity.object_id, old.identity.object_id);
    assert!(added.selected_calendars.is_empty());
    assert_eq!(added.selection_generation, 0);
    assert!(h.secrets.get(&orphan).unwrap().is_none());
    assert!(
        old_credentials
            .keys()
            .all(|key| h.secrets.get(key).unwrap().is_none())
    );
    assert!(credentials(&h, &old.account).is_empty());
    assert_eq!(credentials(&h, &added.account).len(), 1);
    assert!(h.service.account_authority(&old.account).unwrap().is_none());
    assert!(
        h.service
            .cached_account_calendar(&old.account)
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(
        h.service
            .commit_for_account_generation(&old.account, old_authority.generation, || Ok(()))
            .unwrap_err()
            .code(),
        "superseded"
    );
    let selected = h.select(&added, &[own()]);
    h.sync(&selected.account);
    let new_authority = h
        .service
        .account_authority(&selected.account)
        .unwrap()
        .unwrap();
    assert_ne!(new_authority.account, old_authority.account);
    let before = current(&h, &selected.account);
    let before_credentials = credentials(&h, &selected.account);
    let duplicate = LoginAttemptId::generate();
    match h
        .service
        .add_account(IntegrationId::MicrosoftGraph, &duplicate)
        .unwrap_err()
    {
        IntegrationError::AccountExists { account_id } => {
            assert_eq!(account_id, added.account.account_id)
        }
        error => panic!("unexpected error {}", error.code()),
    }
    assert_eq!(current(&h, &selected.account), before);
    assert_eq!(credentials(&h, &selected.account), before_credentials);
    assert_eq!(current(&h, &b.account), b_view);
    assert_eq!(
        h.service.account_authority(&b.account).unwrap(),
        b_authority
    );
    assert_eq!(
        h.service.cached_account_calendar(&b.account).unwrap(),
        b_cache
    );
    assert_eq!(credentials(&h, &b.account), b_credentials);
    let registry: Value = serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
    assert!(registry["pending_secrets"].as_array().unwrap().is_empty());
    // The refused attempt keeps its sanitized terminal result.
    assert_eq!(
        h.service
            .login_attempt(IntegrationId::MicrosoftGraph, &duplicate)
            .unwrap()
            .state,
        LoginAttemptState::Failed {
            code: "account_exists".into(),
            message: IntegrationError::AccountExists {
                account_id: added.account.account_id.clone()
            }
            .to_string(),
        }
    );
    let tombstone = registry["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|slot| slot["view"]["account"]["account_id"] == json!(old.account.account_id))
        .unwrap();
    assert_eq!(tombstone["view"]["status"], json!("signed_out"));
    assert!(tombstone["secret_key"].is_null());
}

#[test]
fn failed_fresh_add_after_removal_recovers_its_version_without_resurrecting_tombstone() {
    let h = Accounts::new();
    let old = h.add("a");
    h.service
        .remove_account(&old.account, old.generation)
        .unwrap();
    let store = Arc::new(FailingSetStore {
        inner: h.secrets.clone(),
        fail_after_write: AtomicBool::new(true),
        registry: account_dir(h.tmp.path(), IntegrationId::MicrosoftGraph).join("accounts.json"),
    });
    let service = facade(
        h.tmp.path(),
        IntegrationId::MicrosoftGraph,
        &h.server,
        store,
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    );
    assert!(
        service
            .add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
            .is_err()
    );
    assert!(service.list_accounts(None).unwrap().is_empty());
    assert!(h.secrets.secrets.lock().unwrap().is_empty());
    assert!(service.account_authority(&old.account).unwrap().is_none());
    let added = h.add("a");
    assert_ne!(added.account, old.account);
}

#[test]
fn narrower_refresh_preserves_own_authority_and_cache_but_revokes_only_optional_access() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = enable_optional(&h, a);
    let a = h.select(&a, &[own(), shared(), group()]);
    h.sync(&a.account);
    let person = h
        .service
        .account_directory(&a.account, "Example")
        .unwrap()
        .people
        .remove(0);
    let photo = h
        .service
        .cache_account_photo(&a.account, a.generation, "p-abc123", &person, &|_| true)
        .unwrap()
        .unwrap();
    let b = h.add("b");
    let b = h.select(&b, &[own()]);
    h.sync(&b.account);
    let b_view = current(&h, &b.account);
    let b_credentials = credentials(&h, &b.account);
    let b_cache = h.service.cached_account_calendar(&b.account).unwrap();
    let old_authority = h.service.account_authority(&a.account).unwrap().unwrap();
    let old_cache = h.service.cached_account_calendar(&a.account).unwrap();
    let own_event = old_cache
        .events
        .iter()
        .find(|event| event.remote.as_ref().unwrap().target == own())
        .unwrap()
        .clone();
    let old_credentials = credentials(&h, &a.account);
    h.expire(&a.account);
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    h.service.keep_account_alive(&a.account).unwrap();
    let narrowed = current(&h, &a.account);
    assert_eq!(narrowed.granted_scopes, vec!["Calendars.Read", "User.Read"]);
    assert_eq!(narrowed.status, AccountStatus::Active);
    assert_eq!(narrowed.selected_calendars, a.selected_calendars);
    assert_eq!(narrowed.shared_calendars, Capability::Denied);
    assert_eq!(narrowed.microsoft365_groups, Capability::Denied);
    assert_eq!(narrowed.directory, Capability::Denied);
    let authority = h.service.account_authority(&a.account).unwrap().unwrap();
    assert_eq!(authority.generation, old_authority.generation);
    assert_eq!(
        authority.selection_generation,
        old_authority.selection_generation
    );
    assert_eq!(authority.selected_calendars, vec![own()]);
    assert_eq!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events,
        vec![own_event]
    );
    assert!(photo.exists());
    assert!(cached_photo(h.tmp.path(), "p-abc123").is_none());
    assert!(
        old_credentials
            .keys()
            .all(|key| h.secrets.get(key).unwrap().is_none())
    );
    assert_eq!(credentials(&h, &a.account).len(), 1);
    let before = h.server.requests.lock().unwrap().len();
    assert_eq!(
        h.service
            .account_directory(&a.account, "Example")
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    assert_eq!(
        h.service
            .cache_account_photo(&a.account, a.generation, "p-abc123", &person, &|_| true)
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    let committed = AtomicBool::new(false);
    assert_eq!(
        h.service
            .commit_for_account_generation(&a.account, a.generation, || {
                committed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    assert!(!committed.load(Ordering::SeqCst));
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
    let read = h.sync(&a.account);
    assert_eq!(read.outcome, AccountSyncOutcome::Partial);
    assert_eq!(read.cache.events.len(), 1);
    assert!(matches!(
        read.targets[0].outcome,
        CalendarTargetOutcome::Available { .. }
    ));
    assert!(
        read.targets[1..]
            .iter()
            .all(|result| matches!(result.outcome, CalendarTargetOutcome::Denied { .. }))
    );
    assert!(
        !h.server.requests.lock().unwrap()[before..]
            .iter()
            .any(
                |request| request.target.contains("shared-copy/calendarView")
                    || request.target.contains("/groups/")
                    || request.target.starts_with("/directory")
            )
    );

    // A broad provider reply to a later base-only refresh is not a new optional opt-in.
    h.expire(&a.account);
    *h.cloud.grants.lock().unwrap() = ALL_GRANTS.into();
    h.service.keep_account_alive(&a.account).unwrap();
    let read = h.sync(&a.account);
    assert_eq!(read.outcome, AccountSyncOutcome::Partial);
    assert_eq!(
        h.service
            .account_authority(&a.account)
            .unwrap()
            .unwrap()
            .selected_calendars,
        vec![own()]
    );
    assert_eq!(
        h.service
            .account_directory(&a.account, "Example")
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    let last_refresh = h
        .server
        .requests
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|request| {
            request
                .form()
                .get("grant_type")
                .is_some_and(|grant| grant == "refresh_token")
        })
        .cloned()
        .unwrap();
    // The synthetic token responses never echo `offline_access`; the refresh still asks for
    // it (part of the original request, so within the documented subset rule) because
    // Microsoft returns a new refresh token only when `offline_access` is requested.
    // Nothing outside the admitted grant is requested.
    assert_eq!(
        last_refresh.form()["scope"],
        format!("offline_access {BASE_ONLY}")
    );
    assert_eq!(current(&h, &b.account), b_view);
    assert_eq!(credentials(&h, &b.account), b_credentials);
    assert_eq!(
        h.service.cached_account_calendar(&b.account).unwrap(),
        b_cache
    );
}

#[test]
fn expiry_sync_admits_reduced_base_token_instead_of_classifying_own_calendar_denied() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = enable_optional(&h, a);
    let a = h.select(&a, &[own(), shared(), group()]);
    h.sync(&a.account);
    h.expire(&a.account);
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    let result = h.sync(&a.account);
    assert_eq!(result.outcome, AccountSyncOutcome::Partial);
    assert!(matches!(
        result.targets[0].outcome,
        CalendarTargetOutcome::Available { event_count: 1 }
    ));
    assert!(
        result.targets[1..]
            .iter()
            .all(|target| matches!(target.outcome, CalendarTargetOutcome::Denied { .. }))
    );
    assert_eq!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events
            .len(),
        1
    );
}

#[test]
fn optional_refresh_rejection_requires_explicit_base_reconnect_without_hidden_scope_fallback() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = enable_optional(&h, a);
    let a = h.select(&a, &[own(), shared(), group()]);
    h.sync(&a.account);
    let b = h.add("b");
    let b = h.select(&b, &[own()]);
    h.sync(&b.account);
    let b_view = current(&h, &b.account);
    let b_credentials = credentials(&h, &b.account);
    let b_cache = h.service.cached_account_calendar(&b.account).unwrap();
    let cache_path = account_dir(h.tmp.path(), IntegrationId::MicrosoftGraph)
        .join("accounts")
        .join(a.account.account_id.as_str())
        .join(CALENDAR_FILE);
    let private_cache = std::fs::read(&cache_path).unwrap();
    h.expire(&a.account);
    let before_credentials = credentials(&h, &a.account);
    *h.cloud.oauth_error.lock().unwrap() = Some(json!({"error":"invalid_scope"}));
    let before = h.server.requests.lock().unwrap().len();
    assert_eq!(
        h.service.keep_account_alive(&a.account).unwrap_err().code(),
        "oauth"
    );
    assert_eq!(
        current(&h, &a.account).status,
        AccountStatus::ReauthRequired
    );
    assert_eq!(credentials(&h, &a.account), before_credentials);
    assert_eq!(std::fs::read(&cache_path).unwrap(), private_cache);
    assert_eq!(
        current(&h, &a.account).selected_calendars,
        a.selected_calendars
    );
    let after = h.server.requests.lock().unwrap().len();
    assert_eq!(after, before + 1);
    h.service.keep_account_alive(&a.account).unwrap();
    assert_eq!(h.server.requests.lock().unwrap().len(), after);
    *h.cloud.oauth_error.lock().unwrap() = None;
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    *h.cloud.signin.lock().unwrap() = "a".into();
    let restored = h
        .service
        .reconnect_account(&a.account, a.generation, &LoginAttemptId::generate())
        .unwrap();
    assert_eq!(restored.account, a.account);
    assert!(restored.generation > a.generation);
    let authorize = h.server.requests.lock().unwrap()[after..]
        .iter()
        .find(|request| request.target.starts_with("/authorize?"))
        .cloned()
        .unwrap();
    assert_eq!(
        authorize.query()["scope"],
        "offline_access Calendars.Read User.Read"
    );
    let result = h.sync(&restored.account);
    assert_eq!(result.outcome, AccountSyncOutcome::Partial);
    assert_eq!(result.cache.events.len(), 1);
    assert_eq!(
        h.service
            .account_authority(&restored.account)
            .unwrap()
            .unwrap()
            .selected_calendars,
        vec![own()]
    );
    assert_eq!(current(&h, &b.account), b_view);
    assert_eq!(credentials(&h, &b.account), b_credentials);
    assert_eq!(
        h.service.cached_account_calendar(&b.account).unwrap(),
        b_cache
    );
}

#[test]
fn missing_base_refresh_grant_is_an_account_failure_and_never_publishes_own_target_denial() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = h.select(&a, &[own()]);
    h.sync(&a.account);
    let prior = current(&h, &a.account);
    h.expire(&a.account);
    let prior_credentials = credentials(&h, &a.account);
    *h.cloud.grants.lock().unwrap() = "User.Read".into();
    assert_eq!(
        h.service
            .sync_account(&a.account, Utc::now(), Utc::now() + Delta::days(1))
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    let failed = current(&h, &a.account);
    assert_eq!(failed.status, AccountStatus::ReauthRequired);
    assert_eq!(failed.target_results, prior.target_results);
    assert_eq!(credentials(&h, &a.account), prior_credentials);
    assert!(h.service.account_authority(&a.account).unwrap().is_none());
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    match h
        .service
        .add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
        .unwrap_err()
    {
        IntegrationError::AccountExists { account_id } => {
            assert_eq!(account_id, a.account.account_id)
        }
        error => panic!("unexpected error {}", error.code()),
    }
    assert_eq!(current(&h, &a.account), failed);
    assert_eq!(credentials(&h, &a.account), prior_credentials);
}

#[test]
fn directory_regrant_is_explicit_staged_and_configuration_off_never_silently_enables_it() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = h.select(&a, &[own()]);
    h.sync(&a.account);
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    let a = h
        .service
        .reconnect_account(&a.account, a.generation, &LoginAttemptId::generate())
        .unwrap();
    h.sync(&a.account);
    let base_cache = h.service.cached_account_calendar(&a.account).unwrap();
    let base_credentials = credentials(&h, &a.account);
    let base_view = current(&h, &a.account);
    assert_eq!(
        h.service
            .grant_account_permission(
                &a.account,
                a.generation,
                AccountPermission::Directory,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    *h.cloud.grants.lock().unwrap() = BASE_GRANTS.into();
    h.cloud.denied.lock().unwrap().push("/directory".into());
    assert_eq!(
        h.service
            .grant_account_permission(
                &a.account,
                a.generation,
                AccountPermission::Directory,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "forbidden"
    );
    assert_eq!(current(&h, &a.account), base_view);
    assert_eq!(credentials(&h, &a.account), base_credentials);
    assert_eq!(
        h.service.cached_account_calendar(&a.account).unwrap(),
        base_cache
    );
    h.cloud.denied.lock().unwrap().clear();
    let granted = h
        .service
        .grant_account_permission(
            &a.account,
            a.generation,
            AccountPermission::Directory,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    assert_eq!(granted.directory, Capability::Available);
    assert!(
        !h.service
            .account_directory(&a.account, "Example")
            .unwrap()
            .people
            .is_empty()
    );
    let before = h.server.requests.lock().unwrap().len();
    let mut config = IntegrationsConfig::load(h.tmp.path()).unwrap();
    config
        .integration
        .get_mut("microsoft-graph")
        .unwrap()
        .directory = GraphDirectory::None;
    let service = Integrations::new(
        h.tmp.path(),
        authoritative(h.tmp.path(), config),
        h.secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    )
    .unwrap()
    .with_endpoints(IntegrationId::MicrosoftGraph, h.server.endpoints());
    assert_eq!(
        service
            .grant_account_permission(
                &granted.account,
                granted.generation,
                AccountPermission::Directory,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "not_configured"
    );
    assert_eq!(
        h.service
            .grant_account_permission(
                &granted.account,
                granted.generation,
                AccountPermission::Directory,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "not_configured"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
    assert_eq!(
        h.service
            .cached_account_calendar(&granted.account)
            .unwrap()
            .events
            .len(),
        1
    );
}

fn concurrent_accounts() -> Accounts {
    let cloud = Arc::new(Cloud::default());
    *cloud.signin.lock().unwrap() = "a".into();
    *cloud.grants.lock().unwrap() = BASE_GRANTS.into();
    let state = cloud.clone();
    let handler = Arc::new(move |request: &Request| {
        if request.target.starts_with("/authorize") {
            return reply(200, json!({}));
        }
        if request.target == "/token" {
            let tenant = if request
                .form()
                .get("grant_type")
                .is_some_and(|grant| grant == "refresh_token")
            {
                request.form()["refresh_token"]
                    .trim_start_matches("refresh-")
                    .to_owned()
            } else {
                state.signin.lock().unwrap().clone()
            };
            return reply(
                200,
                json!({"access_token":format!("access-{tenant}"),"refresh_token":format!("refresh-{tenant}"),"expires_in":3600,"scope":state.grants.lock().unwrap().clone()}),
            );
        }
        let tenant = request
            .authorization
            .as_deref()
            .unwrap_or("")
            .trim_start_matches("Bearer access-");
        if request.target.starts_with("/api/me?") {
            return reply(
                200,
                json!({"id":"same-object","mail":format!("reader@{tenant}.example.invalid")}),
            );
        }
        if request.target.starts_with("/api/organization?") {
            return reply(200, json!({"value":[{"id":format!("tenant-{tenant}")}]}));
        }
        if request.target.starts_with("/api/me/calendars?") {
            return reply(
                200,
                json!({"value":[
                    {"id":"same-calendar","owner":{"address":format!("reader@{tenant}.example.invalid")}},
                    {"id":"shared-copy","owner":{"address":"owner@example.invalid"}}
                ]}),
            );
        }
        if request.target.contains("/memberOf/") {
            if let Some((entered, release)) = state.pause.lock().unwrap().take() {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            return reply(
                200,
                json!({"value":[{
                    "id":"same-group","displayName":"Synthetic group","groupTypes":["Unified"]
                }]}),
            );
        }
        if request.target.contains("calendarView") {
            if let Some((entered, release)) = state.pause.lock().unwrap().take() {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            return reply(200, json!({"value":state.events.lock().unwrap().clone()}));
        }
        reply(200, json!({"id":"same-calendar","value":[]}))
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let server_thread = thread::spawn(move || {
        let mut workers = Vec::new();
        while !done.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let handler = handler.clone();
                    let recorded = recorded.clone();
                    workers.push(thread::spawn(move||{
                        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                        let request=read_request(&mut stream);recorded.lock().unwrap().push(request.clone());
                        let response=handler(&request);
                        write!(stream,"HTTP/1.1 {} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",response.status,response.body.len()).unwrap();
                        stream.write_all(&response.body).unwrap();
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("synthetic listener failure: {error}"),
            }
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    let server = Server {
        base,
        stop,
        thread: Some(server_thread),
        requests,
    };
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let service = Arc::new(facade(
        tmp.path(),
        IntegrationId::MicrosoftGraph,
        &server,
        secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    ));
    Accounts {
        tmp,
        secrets,
        cloud,
        service,
        server,
    }
}

#[test]
fn base_reconnect_fences_old_inflight_read_preserves_selection_and_leaves_other_tenant_unchanged() {
    let h = concurrent_accounts();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    *h.cloud.grants.lock().unwrap() = format!("{BASE_GRANTS} Calendars.Read.Shared");
    let a = h
        .service
        .grant_account_permission(
            &a.account,
            a.generation,
            AccountPermission::SharedCalendars,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    let a = h.select(&a, &[own(), shared()]);
    h.sync(&a.account);
    let b = h.add("b");
    let b = h.select(&b, &[own()]);
    h.sync(&b.account);
    let b_view = current(&h, &b.account);
    let b_credentials = credentials(&h, &b.account);
    let b_authority = h.service.account_authority(&b.account).unwrap();
    let b_cache = h.service.cached_account_calendar(&b.account).unwrap();
    let prior = h.service.account_authority(&a.account).unwrap().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *h.cloud.pause.lock().unwrap() = Some((entered_tx, release_rx));
    let service = h.service.clone();
    let account = a.account.clone();
    let old_read = thread::spawn(move || {
        service.sync_account(&account, Utc::now(), Utc::now() + Delta::days(1))
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    *h.cloud.signin.lock().unwrap() = "a".into();
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    let restored = h
        .service
        .reconnect_account(&a.account, a.generation, &LoginAttemptId::generate())
        .unwrap();
    assert_eq!(restored.selected_calendars, a.selected_calendars);
    assert!(restored.generation > prior.generation);
    release_tx.send(()).unwrap();
    assert_eq!(old_read.join().unwrap().unwrap_err().code(), "superseded");
    assert!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events
            .is_empty()
    );
    let read = h.sync(&restored.account);
    assert_eq!(read.outcome, AccountSyncOutcome::Partial);
    assert_eq!(read.cache.events.len(), 1);
    let authority = h
        .service
        .account_authority(&restored.account)
        .unwrap()
        .unwrap();
    assert_eq!(authority.generation, restored.generation);
    assert_eq!(authority.selected_calendars, vec![own()]);
    assert_eq!(current(&h, &b.account), b_view);
    assert_eq!(credentials(&h, &b.account), b_credentials);
    assert_eq!(
        h.service.account_authority(&b.account).unwrap(),
        b_authority
    );
    assert_eq!(
        h.service.cached_account_calendar(&b.account).unwrap(),
        b_cache
    );
}

#[test]
fn reduced_refresh_cannot_publish_an_optional_response_started_under_old_permissions() {
    let h = concurrent_accounts();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    *h.cloud.grants.lock().unwrap() = format!("{BASE_GRANTS} Calendars.Read.Shared");
    let a = h
        .service
        .grant_account_permission(
            &a.account,
            a.generation,
            AccountPermission::SharedCalendars,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    let a = h.select(&a, &[shared(), own()]);
    h.sync(&a.account);
    let base = h
        .service
        .cached_account_calendar(&a.account)
        .unwrap()
        .events
        .into_iter()
        .find(|event| event.remote.as_ref().unwrap().target == own())
        .unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *h.cloud.pause.lock().unwrap() = Some((entered_tx, release_rx));
    let service = h.service.clone();
    let account = a.account.clone();
    let old_read = thread::spawn(move || {
        service.sync_account(&account, Utc::now(), Utc::now() + Delta::days(1))
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    h.expire(&a.account);
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    h.service.keep_account_alive(&a.account).unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(old_read.join().unwrap().unwrap_err().code(), "superseded");
    assert_eq!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events,
        vec![base]
    );
    let read = h.sync(&a.account);
    assert_eq!(read.outcome, AccountSyncOutcome::Partial);
    assert!(matches!(
        read.targets[0].outcome,
        CalendarTargetOutcome::Denied { .. }
    ));
    assert!(matches!(
        read.targets[1].outcome,
        CalendarTargetOutcome::Available { event_count: 1 }
    ));
}

#[test]
fn explicit_directory_regrant_uses_configured_people_scope_without_basic_directory_fallback() {
    let h = Accounts::new();
    let a = h.add("a");
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    let a = h
        .service
        .reconnect_account(&a.account, a.generation, &LoginAttemptId::generate())
        .unwrap();
    let mut config = IntegrationsConfig::load(h.tmp.path()).unwrap();
    config
        .integration
        .get_mut("microsoft-graph")
        .unwrap()
        .directory = GraphDirectory::PeopleRead;
    let service = Integrations::new(
        h.tmp.path(),
        authoritative(h.tmp.path(), config),
        h.secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    )
    .unwrap()
    .with_endpoints(IntegrationId::MicrosoftGraph, h.server.endpoints());
    *h.cloud.grants.lock().unwrap() = "Calendars.Read User.Read People.Read".into();
    let before = h.server.requests.lock().unwrap().len();
    let granted = service
        .grant_account_permission(
            &a.account,
            a.generation,
            AccountPermission::Directory,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    assert_eq!(granted.directory, Capability::Available);
    let people = service
        .account_directory(&a.account, "Example")
        .unwrap()
        .people;
    assert_eq!(people.len(), 1);
    assert_eq!(
        people[0].email.as_deref(),
        Some("colleague@a.example.invalid")
    );
    assert!(people[0].photo_url.is_none());
    let requests = h.server.requests.lock().unwrap();
    let authorize = requests[before..]
        .iter()
        .find(|request| request.target.starts_with("/authorize?"))
        .unwrap();
    assert_eq!(
        authorize.query()["scope"],
        "offline_access Calendars.Read User.Read People.Read"
    );
    assert!(
        requests[before..]
            .iter()
            .any(|request| request.target.starts_with("/directory/me/people?"))
    );
    assert!(
        !requests[before..]
            .iter()
            .any(|request| request.target.starts_with("/directory/users?"))
    );
}

#[test]
fn add_account_requires_the_same_base_grants_as_refresh_and_reconnect() {
    let h = Accounts::new();
    for grants in ["User.Read", "User.Read Calendars.ReadWrite", ""] {
        *h.cloud.grants.lock().unwrap() = grants.into();
        assert_eq!(
            h.service
                .add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
                .unwrap_err()
                .code(),
            "insufficient_grant"
        );
        assert!(h.service.list_accounts(None).unwrap().is_empty());
        assert!(h.secrets.secrets.lock().unwrap().is_empty());
    }
    assert!(!h.server.requests.lock().unwrap().iter().any(|request| {
        request.target.starts_with("/api/me?") || request.target.starts_with("/api/organization?")
    }));
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    let account = h.add("a");
    assert_eq!(account.status, AccountStatus::Active);
}

#[test]
fn reduced_refresh_fences_a_group_catalog_fetched_under_the_old_grant() {
    let h = concurrent_accounts();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    *h.cloud.grants.lock().unwrap() = format!("{BASE_GRANTS} Group.Read.All");
    let a = h
        .service
        .grant_account_permission(
            &a.account,
            a.generation,
            AccountPermission::Microsoft365Groups,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    let a = h.select(&a, &[own()]);
    h.sync(&a.account);
    let authority = h.service.account_authority(&a.account).unwrap();
    let cache = h.service.cached_account_calendar(&a.account).unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *h.cloud.pause.lock().unwrap() = Some((entered_tx, release_rx));
    let service = h.service.clone();
    let account = a.account.clone();
    let old_catalog = thread::spawn(move || service.list_calendars(&account));
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    h.expire(&a.account);
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    h.service.keep_account_alive(&a.account).unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(
        old_catalog.join().unwrap().unwrap_err().code(),
        "superseded"
    );
    assert_eq!(
        current(&h, &a.account).microsoft365_groups,
        Capability::Denied
    );
    assert_eq!(h.service.account_authority(&a.account).unwrap(), authority);
    assert_eq!(
        h.service.cached_account_calendar(&a.account).unwrap(),
        cache
    );
}

#[test]
fn removing_one_route_keeps_the_others_and_route_reads_report_retained_copies() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = enable_optional(&h, a);
    let a = h.select(&a, &[own(), shared(), group()]);
    let read = h.sync(&a.account);
    assert_eq!(read.outcome, AccountSyncOutcome::Available);
    assert_eq!(read.event_count, 3);
    assert!(
        read.routes
            .iter()
            .all(|route| route.copy == RouteCopy::Fresh
                && route.cached_events == 1
                && route.last_success.is_some())
    );
    let first_success = read
        .routes
        .iter()
        .find(|route| route.target == shared())
        .unwrap()
        .last_success;
    // A transient failure keeps that route's last successful read and reports it retained.
    h.cloud
        .transient
        .lock()
        .unwrap()
        .push("/api/me/calendars/shared-copy/calendarView".into());
    let read = h.sync(&a.account);
    assert_eq!(read.outcome, AccountSyncOutcome::Partial);
    let shared_read = read
        .routes
        .iter()
        .find(|route| route.target == shared())
        .unwrap();
    assert_eq!(shared_read.copy, RouteCopy::Retained);
    assert_eq!(shared_read.cached_events, 1);
    assert_eq!(shared_read.last_success, first_success);
    let account = current(&h, &a.account);
    assert_eq!(account.route_reads, read.routes);
    // Removing one route leaves the other routes' selection, readability and cached events.
    let removed = h
        .service
        .remove_calendar_target(
            &a.account,
            account.generation,
            account.selection_generation,
            &shared(),
        )
        .unwrap();
    assert!(removed.copy_error.is_none());
    let removed = removed.account;
    assert_eq!(removed.selected_calendars, vec![own(), group()]);
    assert!(removed.selection_generation > account.selection_generation);
    assert!(
        removed
            .route_reads
            .iter()
            .all(|route| route.target != shared())
    );
    assert_eq!(
        h.service
            .account_authority(&a.account)
            .unwrap()
            .unwrap()
            .selected_calendars,
        vec![own(), group()]
    );
    let cached = h.service.cached_account_calendar(&a.account).unwrap();
    assert_eq!(cached.events.len(), 2);
    assert!(
        cached
            .events
            .iter()
            .all(|event| event.remote.as_ref().unwrap().target != shared())
    );
    // A stale handle or a route that is not selected is refused without changes.
    assert_eq!(
        h.service
            .remove_calendar_target(
                &a.account,
                account.generation,
                account.selection_generation,
                &own(),
            )
            .unwrap_err()
            .code(),
        "superseded"
    );
    assert_eq!(
        h.service
            .remove_calendar_target(
                &a.account,
                removed.generation,
                removed.selection_generation,
                &shared(),
            )
            .unwrap_err()
            .code(),
        "invalid_calendar_target"
    );
    assert_eq!(current(&h, &a.account), removed);
}

#[test]
fn a_removal_whose_copy_cannot_be_carried_over_is_still_reported_as_committed() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = enable_optional(&h, a);
    let a = h.select(&a, &[own(), group()]);
    assert_eq!(h.sync(&a.account).event_count, 2);
    let account = current(&h, &a.account);
    // Synthetic obstruction: the copy cannot be read back or rewritten after the removal.
    let cache = account_dir(h.tmp.path(), IntegrationId::MicrosoftGraph)
        .join("accounts")
        .join(a.account.account_id.as_str())
        .join(CALENDAR_FILE);
    std::fs::remove_file(&cache).unwrap();
    std::fs::create_dir_all(cache.join("blocker")).unwrap();
    let removed = h
        .service
        .remove_calendar_target(
            &a.account,
            account.generation,
            account.selection_generation,
            &group(),
        )
        .unwrap();
    let error = removed.copy_error.clone().unwrap();
    assert_eq!(error.code, "io");
    let root = h.tmp.path().to_string_lossy();
    assert!(!error.message.contains(root.as_ref()), "{error:?}");
    // The removal is saved, and the remaining route says truthfully that nothing is cached.
    assert_eq!(removed.account.selected_calendars, vec![own()]);
    assert!(removed.account.selection_generation > account.selection_generation);
    assert_eq!(removed.account.route_reads.len(), 1);
    assert_eq!(removed.account.route_reads[0].copy, RouteCopy::Empty);
    assert_eq!(removed.account.route_reads[0].cached_events, 0);
    assert_eq!(current(&h, &a.account), removed.account);
    // The flattened answer still reads as the account view.
    let wire = serde_json::to_value(&removed).unwrap();
    assert_eq!(
        serde_json::from_value::<MicrosoftAccount>(wire.clone()).unwrap(),
        removed.account
    );
    assert_eq!(wire["copy_error"]["code"], "io");
    // Once the copy is writable again, the next sync reads the remaining route afresh.
    std::fs::remove_dir_all(&cache).unwrap();
    let read = h.sync(&a.account);
    assert_eq!(read.outcome, AccountSyncOutcome::Available);
    assert_eq!(read.routes[0].copy, RouteCopy::Fresh);
    assert_eq!(read.event_count, 1);
}

#[test]
fn a_narrowing_refresh_stops_claiming_copies_of_the_routes_it_withdrew() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = enable_optional(&h, a);
    let a = h.select(&a, &[own(), shared(), group()]);
    assert!(
        h.sync(&a.account)
            .routes
            .iter()
            .all(|route| route.copy == RouteCopy::Fresh && route.cached_events == 1)
    );
    h.expire(&a.account);
    *h.cloud.grants.lock().unwrap() = BASE_ONLY.into();
    h.service.keep_account_alive(&a.account).unwrap();
    let account = current(&h, &a.account);
    for route in &account.route_reads {
        if route.target == own() {
            assert_eq!(route.copy, RouteCopy::Fresh);
            assert_eq!(route.cached_events, 1);
            assert!(route.last_success.is_some());
        } else {
            assert_eq!(route.copy, RouteCopy::Empty, "{route:?}");
            assert_eq!(route.cached_events, 0);
            assert!(route.last_success.is_none());
        }
    }
    // What the routes claim matches what is served.
    assert_eq!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events
            .len(),
        1
    );
}

/// A second facade on the same data directory, as another process (the app, a second CLI)
/// cancelling the sign-in. It needs no provider configuration.
fn canceller(h: &Accounts) -> Integrations {
    Integrations::new(
        h.tmp.path(),
        IntegrationsConfig::default(),
        h.secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
    .unwrap()
}

#[test]
fn a_reconnect_cancelled_during_the_code_exchange_keeps_the_working_account_and_its_copies() {
    let h = Accounts::new();
    let a = h.add("a");
    let a = h.select(&a, &[own()]);
    h.cloud.events.lock().unwrap().push(Accounts::event());
    h.sync(&a.account);
    let view = current(&h, &a.account);
    let before_credentials = credentials(&h, &a.account);
    let cache = h.service.cached_account_calendar(&a.account).unwrap();
    assert!(!cache.events.is_empty());
    let authority = h.service.account_authority(&a.account).unwrap();
    assert!(authority.is_some());

    let attempt = LoginAttemptId::parse("reconnect-1").unwrap();
    let other = canceller(&h);
    let cancelling = attempt.clone();
    *h.cloud.on_exchange.lock().unwrap() = Some(Box::new(move || {
        let status = other
            .cancel_login(IntegrationId::MicrosoftGraph, &cancelling)
            .unwrap();
        assert_eq!(status.state, LoginAttemptState::Cancelled);
    }));
    let before_requests = h.server.requests.lock().unwrap().len();
    let error = h
        .service
        .reconnect_account(&a.account, a.generation, &attempt)
        .unwrap_err();
    assert_eq!(error.code(), "login_cancelled");
    // The late token is dropped: no credential version, generation, selection, route
    // state, cache or authority changed.
    assert_eq!(current(&h, &a.account), view);
    assert_eq!(credentials(&h, &a.account), before_credentials);
    assert_eq!(
        h.service.cached_account_calendar(&a.account).unwrap(),
        cache
    );
    assert_eq!(h.service.account_authority(&a.account).unwrap(), authority);
    // Nothing was read with the late token.
    assert!(
        h.server.requests.lock().unwrap()[before_requests..]
            .iter()
            .all(|r| !r.target.starts_with("/api/"))
    );
    assert_eq!(
        h.service
            .login_attempt(IntegrationId::MicrosoftGraph, &attempt)
            .unwrap()
            .state,
        LoginAttemptState::Cancelled
    );
    // The attempt is single use: repeating it neither opens a browser nor retries.
    let requests = h.server.requests.lock().unwrap().len();
    assert_eq!(
        h.service
            .reconnect_account(&a.account, a.generation, &attempt)
            .unwrap_err()
            .code(),
        "login_cancelled"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), requests);
    // A fresh, uncancelled attempt still works afterwards.
    let reconnected = h
        .service
        .reconnect_account(&a.account, a.generation, &LoginAttemptId::generate())
        .unwrap();
    assert_eq!(reconnected.generation, a.generation + 1);
}

#[test]
fn a_new_account_cancelled_during_its_sign_in_is_never_created_beside_the_working_one() {
    let h = Accounts::new();
    let a = h.add("a");
    let view = current(&h, &a.account);
    let attempt = LoginAttemptId::generate();
    let other = canceller(&h);
    let cancelling = attempt.clone();
    *h.cloud.signin.lock().unwrap() = "b".into();
    *h.cloud.on_exchange.lock().unwrap() = Some(Box::new(move || {
        other
            .cancel_login(IntegrationId::MicrosoftGraph, &cancelling)
            .unwrap();
    }));
    let secrets = h.secrets.secrets.lock().unwrap().clone();
    assert_eq!(
        h.service
            .add_account(IntegrationId::MicrosoftGraph, &attempt)
            .unwrap_err()
            .code(),
        "login_cancelled"
    );
    assert_eq!(h.service.list_accounts(None).unwrap(), vec![view]);
    assert_eq!(*h.secrets.secrets.lock().unwrap(), secrets);
}

#[test]
fn a_cancel_after_the_commit_reports_the_committed_result_unchanged() {
    let h = Accounts::new();
    let attempt = LoginAttemptId::generate();
    *h.cloud.signin.lock().unwrap() = "a".into();
    let added = h
        .service
        .add_account(IntegrationId::MicrosoftGraph, &attempt)
        .unwrap();
    let status = canceller(&h)
        .cancel_login(IntegrationId::MicrosoftGraph, &attempt)
        .unwrap();
    assert_eq!(status.state, LoginAttemptState::Succeeded);
    assert_eq!(status.attempt, attempt);
    assert_eq!(current(&h, &added.account), added);
}

/// Runs its hook once, right after the next credential write: inside a sign-in's commit,
/// while that commit holds the account registry lock.
struct CommitHook {
    inner: Arc<FakeSecretStore>,
    hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl SecretStore for CommitHook {
    fn get(&self, key: &str) -> broscribe_platform::Result<Option<String>> {
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &str) -> broscribe_platform::Result<()> {
        self.inner.set(key, value)?;
        if let Some(hook) = self.hook.lock().unwrap().take() {
            hook();
        }
        Ok(())
    }
    fn delete(&self, key: &str) -> broscribe_platform::Result<()> {
        self.inner.delete(key)
    }
}

/// The account's directory as the service resolves it (its data directory is canonical).
fn account_files(h: &Accounts, account: &AccountRef) -> std::path::PathBuf {
    account_dir(
        &std::fs::canonicalize(h.tmp.path()).unwrap(),
        IntegrationId::MicrosoftGraph,
    )
    .join("accounts")
    .join(account.account_id.as_str())
}

/// The account's own calendar synced and a directory photo cached under its generation,
/// with the directory entry it came from.
fn with_copies(h: &Accounts) -> (MicrosoftAccount, DirectoryPerson, std::path::PathBuf) {
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = h.select(&a, &[own()]);
    assert!(!h.sync(&a.account).cache.events.is_empty());
    let person = h
        .service
        .account_directory(&a.account, "Example")
        .unwrap()
        .people
        .remove(0);
    let photo = h
        .service
        .cache_account_photo(&a.account, a.generation, "p-abc123", &person, &|_| true)
        .unwrap()
        .unwrap();
    (a, person, photo)
}

#[test]
fn a_sync_and_photo_admitted_right_after_a_sign_in_commit_survive_its_fenced_copy_cleanup() {
    // A base reconnect returns only base access: the directory photo comes back only through
    // its explicit consent, so after it the queued photo request must be refused. The
    // incremental grants keep (shared calendars) or explicitly regrant (directory) it.
    for permission in [
        None,
        Some(AccountPermission::SharedCalendars),
        Some(AccountPermission::Directory),
    ] {
        let h = Accounts::new();
        let (a, _, old_photo) = with_copies(&h);
        if permission.is_some() {
            *h.cloud.grants.lock().unwrap() = ALL_GRANTS.into();
        }
        // The sign-in commits through a second facade whose credential write releases the
        // sync and the photo while the commit still holds the registry lock: each is
        // admitted at the first moment the committed generation can be observed.
        let (sync_go, sync_start) = std::sync::mpsc::channel();
        let (photo_go, photo_start) = std::sync::mpsc::channel();
        let hook = Arc::new(CommitHook {
            inner: h.secrets.clone(),
            hook: Mutex::new(Some(Box::new(move || {
                sync_go.send(()).unwrap();
                photo_go.send(()).unwrap();
            }))),
        });
        let hooked = facade(
            h.tmp.path(),
            IntegrationId::MicrosoftGraph,
            &h.server,
            hook.clone(),
            Arc::new(CallbackBrowser::default()),
            Arc::new(|_| {}),
        );
        let attempt = LoginAttemptId::generate();
        let (service, account) = (&h.service, &a.account);
        let (committed, synced, photo) = thread::scope(|s| {
            let sync = s.spawn(move || {
                sync_start
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the sign-in reached its commit");
                service
                    .sync_account(
                        account,
                        Utc::now() - Delta::days(1),
                        Utc::now() + Delta::days(1),
                    )
                    .unwrap()
            });
            let photo = s.spawn(move || {
                photo_start
                    .recv_timeout(Duration::from_secs(10))
                    .expect("the sign-in reached its commit");
                let directory = service.account_directory(account, "Example");
                if permission.is_none() {
                    assert_eq!(directory.unwrap_err().code(), "insufficient_grant");
                    return None;
                }
                let person = directory.unwrap().people.remove(0);
                let generation = person.source.as_ref().unwrap().generation;
                service
                    .cache_account_photo(account, generation, "p-abc123", &person, &|_| true)
                    .unwrap()
            });
            let committed = match permission {
                None => hooked.reconnect_account(account, a.generation, &attempt),
                Some(permission) => {
                    hooked.grant_account_permission(account, a.generation, permission, &attempt)
                }
            };
            // A sign-in that never reached its commit releases nobody: fail with its error.
            drop(hook.hook.lock().unwrap().take());
            let committed = committed.unwrap();
            (committed, sync.join().unwrap(), photo.join().unwrap())
        });
        let generation = a.generation + 1;
        assert_eq!(committed.generation, generation);
        // The sync published after the commit; the committed account is the current one.
        let now = current(&h, &a.account);
        assert_eq!(
            (now.generation, now.status, now.granted_scopes),
            (generation, AccountStatus::Active, committed.granted_scopes)
        );
        assert_eq!(
            h.service
                .login_attempt(IntegrationId::MicrosoftGraph, &attempt)
                .unwrap()
                .state,
            LoginAttemptState::Succeeded
        );
        // The copies the bumped generation fences are gone...
        assert!(!old_photo.exists());
        // ...and nothing published under the committed generation went with them.
        assert_eq!(synced.generation, generation);
        assert_eq!(synced.outcome, AccountSyncOutcome::Available);
        let cache = h.service.cached_account_calendar(&a.account).unwrap();
        assert!(!cache.events.is_empty());
        assert_eq!(synced.event_count, cache.events.len());
        assert!(cache.events.iter().all(|event| {
            event
                .remote
                .as_ref()
                .is_some_and(|stamp| stamp.generation == generation)
        }));
        let Some(photo) = photo else {
            // Without directory consent nothing serves the person's photo any more.
            assert!(cached_photo(h.tmp.path(), "p-abc123").is_none());
            continue;
        };
        assert!(
            photo.starts_with(
                account_files(&h, &a.account)
                    .join(crate::store::PHOTOS_DIR)
                    .join(generation.to_string())
            )
        );
        assert_eq!(std::fs::read(&photo).unwrap(), b"photo-a");
        assert_eq!(
            std::fs::canonicalize(cached_photo(h.tmp.path(), "p-abc123").unwrap()).unwrap(),
            std::fs::canonicalize(&photo).unwrap()
        );
    }
}

#[test]
fn a_sign_in_whose_fenced_copies_cannot_be_removed_stays_committed() {
    let h = Accounts::new();
    let (a, old_person, old_photo) = with_copies(&h);
    let old_credentials = credentials(&h, &a.account);
    // A cached calendar that cannot be removed: the cleanup after the commit fails.
    let cache_path = account_files(&h, &a.account).join(CALENDAR_FILE);
    std::fs::remove_file(&cache_path).unwrap();
    std::fs::create_dir(&cache_path).unwrap();
    let attempt = LoginAttemptId::generate();
    let reconnected = h
        .service
        .reconnect_account(&a.account, a.generation, &attempt)
        .unwrap();
    assert_eq!(reconnected.generation, a.generation + 1);
    assert_eq!(reconnected.status, AccountStatus::Active);
    assert_eq!(current(&h, &a.account), reconnected);
    assert_eq!(
        h.service
            .login_attempt(IntegrationId::MicrosoftGraph, &attempt)
            .unwrap()
            .state,
        LoginAttemptState::Succeeded
    );
    let new_credentials = credentials(&h, &a.account);
    assert_eq!(new_credentials.len(), 1);
    assert!(
        new_credentials
            .keys()
            .all(|key| !old_credentials.contains_key(key))
    );
    // The cleanup stopped at the copy it could not remove; nothing was rolled back.
    assert!(!old_photo.exists());
    assert!(cache_path.is_dir());
    // The committed generation is the working one: the base reconnect withdrew directory
    // consent, and only an explicit grant from that generation restores the photo. That
    // grant's own cleanup meets the same stuck copy and still commits.
    assert_eq!(
        h.service
            .account_directory(&a.account, "Example")
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    let granted = h
        .service
        .grant_account_permission(
            &a.account,
            reconnected.generation,
            AccountPermission::Directory,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    assert_eq!(granted.generation, reconnected.generation + 1);
    assert_eq!(granted.directory, Capability::Available);
    assert!(cache_path.is_dir());
    let person = h
        .service
        .account_directory(&a.account, "Example")
        .unwrap()
        .people
        .remove(0);
    let photo = h
        .service
        .cache_account_photo(&a.account, granted.generation, "p-abc123", &person, &|_| {
            true
        })
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(&photo).unwrap(), b"photo-a");
    assert_eq!(
        h.service
            .cache_account_photo(&a.account, a.generation, "p-abc123", &old_person, &|_| true)
            .unwrap_err()
            .code(),
        "superseded"
    );
}
