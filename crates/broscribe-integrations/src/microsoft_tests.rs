//! Synthetic HTTP + SecretStore tests of the public account API. No production endpoints.
use super::*;

const BASE_GRANTS: &str = "Calendars.Read User.Read User.ReadBasic.All";
#[derive(Default)]
struct Cloud {
    signin: Mutex<String>,
    grants: Mutex<String>,
    oauth_error: Mutex<Option<Value>>,
    denied: Mutex<Vec<String>>,
    transient: Mutex<Vec<String>>,
    events: Mutex<Vec<Value>>,
    pagination: Mutex<Option<String>>,
    identity_missing: AtomicBool,
    pause: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    refresh_pause: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    /// Runs once while the next authorization code is being exchanged.
    on_exchange: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
struct Accounts {
    tmp: tempfile::TempDir,
    secrets: Arc<FakeSecretStore>,
    cloud: Arc<Cloud>,
    service: Arc<Integrations>,
    server: Server,
}
/// A public sync answer and the copy the account serves after it.
struct Synced {
    sync: AccountCalendarSync,
    cache: CalendarCache,
}
impl std::ops::Deref for Synced {
    type Target = AccountCalendarSync;
    fn deref(&self) -> &AccountCalendarSync {
        &self.sync
    }
}
impl Accounts {
    fn new() -> Self {
        let cloud = Arc::new(Cloud::default());
        *cloud.signin.lock().unwrap() = "a".into();
        *cloud.grants.lock().unwrap() = BASE_GRANTS.into();
        let state = cloud.clone();
        let server = Server::new(move |r| {
            if r.target == "/authorize" || r.target.starts_with("/authorize?") {
                return reply(200, json!({}));
            }
            if r.target == "/token" {
                if r.form()
                    .get("grant_type")
                    .is_some_and(|g| g == "refresh_token")
                    && let Some((entered, release)) = state.refresh_pause.lock().unwrap().take()
                {
                    entered.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                if r.form()
                    .get("grant_type")
                    .is_some_and(|g| g == "authorization_code")
                    && let Some(hook) = state.on_exchange.lock().unwrap().take()
                {
                    hook();
                }
                if let Some(error) = state.oauth_error.lock().unwrap().clone() {
                    return reply(400, error);
                }
                let tenant = if r
                    .form()
                    .get("grant_type")
                    .is_some_and(|g| g == "refresh_token")
                {
                    r.form()
                        .get("refresh_token")
                        .unwrap()
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
            let tenant = r
                .authorization
                .as_deref()
                .unwrap_or("")
                .trim_start_matches("Bearer access-");
            if r.target.starts_with("/api/me?") {
                return reply(
                    200,
                    json!({"id":"same-object","displayName":"Example Reader","mail":format!("reader@{tenant}.example.invalid")}),
                );
            }
            if r.target.starts_with("/api/organization?") {
                return if state.identity_missing.load(Ordering::SeqCst) {
                    reply(403, json!({}))
                } else {
                    reply(
                        200,
                        json!({"value":[{"id":format!("tenant-{tenant}"),"displayName":"Synthetic Organisation"}]}),
                    )
                };
            }
            if state
                .denied
                .lock()
                .unwrap()
                .iter()
                .any(|path| r.target.starts_with(path))
            {
                return reply(403, json!({}));
            }
            if state
                .transient
                .lock()
                .unwrap()
                .iter()
                .any(|path| r.target.starts_with(path))
            {
                return reply(503, json!({}));
            }
            if r.target.starts_with("/api/me/calendars?") {
                if let Some(next) = state.pagination.lock().unwrap().clone() {
                    return reply(200, json!({"value":[],"@odata.nextLink":next}));
                }
                return reply(
                    200,
                    json!({"value":[
                        {"id":"same-calendar","name":"Work","isDefaultCalendar":true,"owner":{"address":format!("reader@{tenant}.example.invalid")}},
                        {"id":"shared-copy","name":"Shared","owner":{"address":"owner@example.invalid"}}
                    ]}),
                );
            }
            if r.target.starts_with("/api/me/memberOf/") {
                return reply(
                    200,
                    json!({"value":[{"id":"same-group","groupTypes":["Unified"],"displayName":"Synthetic Group"}]}),
                );
            }
            if r.target.contains("calendarView") {
                if let Some((entered, release)) = state.pause.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                return reply(200, json!({"value":state.events.lock().unwrap().clone()}));
            }
            if r.target.ends_with("photo/$value") {
                return Response {
                    status: 200,
                    body: format!("photo-{tenant}").into_bytes(),
                };
            }
            if r.target.starts_with("/directory/me/people?") {
                return reply(
                    200,
                    json!({"value":[{"id":"same-person","displayName":"Example Colleague","scoredEmailAddresses":[{"address":format!("colleague@{tenant}.example.invalid")}]}]}),
                );
            }
            if r.target.starts_with("/directory/users?") {
                return reply(
                    200,
                    json!({"value":[{"id":"same-person","displayName":"Example Colleague","mail":format!("colleague@{tenant}.example.invalid")}]}),
                );
            }
            reply(200, json!({"id":"same-calendar"}))
        });
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
        Self {
            tmp,
            secrets,
            cloud,
            service,
            server,
        }
    }
    fn add(&self, tenant: &str) -> MicrosoftAccount {
        *self.cloud.signin.lock().unwrap() = tenant.into();
        self.service
            .add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
            .unwrap()
    }
    fn select(&self, account: &MicrosoftAccount, targets: &[CalendarTarget]) -> MicrosoftAccount {
        self.service
            .configure_calendars(
                &account.account,
                account.generation,
                account.selection_generation,
                targets,
            )
            .unwrap()
    }
    /// A sync through the public API, with the copy it published as the account serves it.
    fn sync(&self, account: &AccountRef) -> Synced {
        let sync = self
            .service
            .sync_account(
                account,
                Utc::now() - Delta::days(1),
                Utc::now() + Delta::days(1),
            )
            .unwrap();
        let cache = self.service.cached_account_calendar(account).unwrap();
        assert_eq!(sync.event_count, cache.events.len());
        Synced { sync, cache }
    }
    fn event() -> Value {
        let start = Utc::now();
        json!({"id":"same-event","iCalUId":"same-meeting","subject":"Synthetic planning","start":{"dateTime":start.to_rfc3339()},"end":{"dateTime":(start+Delta::hours(1)).to_rfc3339()},"attendees":[{"type":"required","emailAddress":{"name":"Example Reader","address":"reader@a.example.invalid"},"status":{"response":"accepted"}}]})
    }
    fn expire(&self, account: &AccountRef) {
        let prefix = format!(
            "integration/microsoft-graph/accounts/{}/tokens/",
            account.account_id
        );
        let mut secrets = self.secrets.secrets.lock().unwrap();
        let (_, value) = secrets
            .iter_mut()
            .find(|(key, _)| key.starts_with(&prefix))
            .unwrap();
        let mut token: Value = serde_json::from_str(value).unwrap();
        token["expires_at"] = json!(0);
        *value = token.to_string();
    }
}
fn own() -> CalendarTarget {
    CalendarTarget::Own {
        calendar_id: "same-calendar".into(),
    }
}
fn shared() -> CalendarTarget {
    CalendarTarget::RecipientShared {
        calendar_id: "shared-copy".into(),
    }
}

#[test]
fn verified_two_tenant_accounts_never_share_ids_credentials_selection_or_photos() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let b = h.add("b");
    assert_ne!(a.account, b.account);
    assert_eq!(a.identity.object_id, b.identity.object_id);
    assert_ne!(a.identity.resource_tenant_id, b.identity.resource_tenant_id);
    assert_eq!(
        a.identity.verified_addresses,
        vec!["reader@a.example.invalid"]
    );
    assert_eq!(
        b.identity.verified_addresses,
        vec!["reader@b.example.invalid"]
    );
    let a = h.select(&a, &[own()]);
    let b = h.select(&b, &[own()]);
    let ea = h.sync(&a.account).cache.events.remove(0);
    let eb = h.sync(&b.account).cache.events.remove(0);
    assert_eq!(ea.id, eb.id);
    assert_ne!(ea.remote, eb.remote);
    assert!(ea.attendees[0].is_self);
    assert!(!eb.attendees[0].is_self);
    let pa = h
        .service
        .account_directory(&a.account, "Example")
        .unwrap()
        .people
        .remove(0);
    let pb = h
        .service
        .account_directory(&b.account, "Example")
        .unwrap()
        .people
        .remove(0);
    assert_eq!(pa.remote_id, pb.remote_id);
    assert_ne!(pa.source, pb.source);
    assert_eq!(
        h.service
            .cache_account_photo(&a.account, a.generation, "p-abc123", &pb, &|_| true)
            .unwrap_err()
            .code(),
        "account_identity_mismatch"
    );
    let photoa = h
        .service
        .cache_account_photo(&a.account, a.generation, "p-abc123", &pa, &|_| true)
        .unwrap()
        .unwrap();
    let photob = h
        .service
        .cache_account_photo(&b.account, b.generation, "p-abc123", &pb, &|_| true)
        .unwrap()
        .unwrap();
    assert_ne!(photoa, photob);
    assert_eq!(std::fs::read(&photoa).unwrap(), b"photo-a");
    assert_eq!(std::fs::read(&photob).unwrap(), b"photo-b");
    h.service.remove_account(&a.account, a.generation).unwrap();
    assert!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(
        h.service
            .cached_account_calendar(&b.account)
            .unwrap()
            .events
            .len(),
        1
    );
    assert!(!photoa.exists());
    assert!(photob.exists());
    forget_person_photos(h.tmp.path(), "p-abc123").unwrap();
    assert!(!photob.exists());
}

#[test]
fn new_accounts_select_nothing_and_duplicate_add_never_replaces_working_slot() {
    let h = Accounts::new();
    let a = h.add("a");
    let before = h.server.requests.lock().unwrap().len();
    let no_selection = h.sync(&a.account);
    assert_eq!(no_selection.outcome, AccountSyncOutcome::NoSelection);
    assert!(no_selection.cache.events.is_empty());
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
    let a = h.select(&a, &[own()]);
    let secrets = h.secrets.secrets.lock().unwrap().clone();
    let error = h
        .service
        .add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
        .unwrap_err();
    match error {
        IntegrationError::AccountExists { account_id } => {
            assert_eq!(account_id, a.account.account_id)
        }
        error => panic!("unexpected code {}", error.code()),
    }
    assert_eq!(*h.secrets.secrets.lock().unwrap(), secrets);
    assert_eq!(h.service.list_accounts(None).unwrap(), vec![a]);
}

#[test]
fn mismatched_reconnect_and_denied_or_insufficient_incremental_grants_preserve_base() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = h.select(&a, &[own()]);
    h.sync(&a.account);
    let before = h.service.cached_account_calendar(&a.account).unwrap();
    let secrets = h.secrets.secrets.lock().unwrap().clone();
    *h.cloud.signin.lock().unwrap() = "b".into();
    assert_eq!(
        h.service
            .reconnect_account(&a.account, a.generation, &LoginAttemptId::generate())
            .unwrap_err()
            .code(),
        "account_identity_mismatch"
    );
    *h.cloud.signin.lock().unwrap() = "a".into();
    assert_eq!(
        h.service
            .grant_account_permission(
                &a.account,
                a.generation,
                AccountPermission::SharedCalendars,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    *h.cloud.oauth_error.lock().unwrap() = Some(
        json!({"error":"access_denied","error_codes":[53003],"error_description":"synthetic policy refusal"}),
    );
    assert_eq!(
        h.service
            .grant_account_permission(
                &a.account,
                a.generation,
                AccountPermission::Microsoft365Groups,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "conditional_access_blocked"
    );
    *h.cloud.oauth_error.lock().unwrap() = None;
    *h.cloud.grants.lock().unwrap() = format!("{BASE_GRANTS} Group.Read.All");
    h.cloud
        .denied
        .lock()
        .unwrap()
        .push("/api/me/memberOf/".into());
    assert_eq!(
        h.service
            .grant_account_permission(
                &a.account,
                a.generation,
                AccountPermission::Microsoft365Groups,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "forbidden"
    );
    assert_eq!(*h.secrets.secrets.lock().unwrap(), secrets);
    assert_eq!(
        h.service.cached_account_calendar(&a.account).unwrap(),
        before
    );
    assert_eq!(
        h.service.list_accounts(None).unwrap()[0].generation,
        a.generation
    );
}

#[test]
fn route_filters_do_not_confuse_reader_owner_organizer_and_resource_attendees() {
    let h = Accounts::new();
    let mut a = h.add("a");
    *h.cloud.grants.lock().unwrap() = format!("{BASE_GRANTS} Calendars.Read.Shared Group.Read.All");
    a = h
        .service
        .grant_account_permission(
            &a.account,
            a.generation,
            AccountPermission::SharedCalendars,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    a = h
        .service
        .grant_account_permission(
            &a.account,
            a.generation,
            AccountPermission::Microsoft365Groups,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    let routes = vec![
        own(),
        shared(),
        CalendarTarget::Delegated {
            mailbox: "owner@example.invalid".into(),
            calendar_id: None,
        },
        CalendarTarget::Delegated {
            mailbox: "owner@example.invalid".into(),
            calendar_id: Some("owner-relative".into()),
        },
        CalendarTarget::Group {
            group_id: "same-group".into(),
        },
    ];
    a = h.select(&a, &routes);
    let base = Accounts::event();
    let mut fixtures = vec![base.clone()];
    for (field, value) in [
        ("isCancelled", json!(true)),
        ("isAllDay", json!(true)),
        ("sensitivity", json!("private")),
        ("sensitivity", json!("confidential")),
    ] {
        let mut event = base.clone();
        event[field] = value.clone();
        event["id"] = json!(format!("excluded-{field}-{value}"));
        fixtures.push(event);
    }
    let mut owner_declined = base.clone();
    owner_declined["id"] = json!("owner-declined");
    owner_declined["responseStatus"] = json!({"response":"declined"});
    owner_declined["attendees"] = json!([
        {"type":"required","emailAddress":{"address":"owner@example.invalid"},"status":{"response":"declined"}},
        {"type":"resource","emailAddress":{"address":"room@example.invalid"}},
        {"type":"required","emailAddress":{"name":"Guest Example","address":"guest@example.invalid"},"status":{"response":"accepted"}}
    ]);
    fixtures.push(owner_declined);
    let mut self_declined = base.clone();
    self_declined["id"] = json!("reader-declined");
    self_declined["attendees"][0]["status"]["response"] = json!("declined");
    fixtures.push(self_declined);
    *h.cloud.events.lock().unwrap() = fixtures;
    let read = h.sync(&a.account);
    assert_eq!(read.outcome, AccountSyncOutcome::Available);
    assert_eq!(read.cache.events.len(), 9); // own:1; each other route:2
    for event in &read.cache.events {
        assert!(!event.id.starts_with("excluded"));
        assert_ne!(event.id, "reader-declined");
        assert!(
            event
                .attendees
                .iter()
                .all(|a| a.email.as_deref() != Some("room@example.invalid")
                    && a.email.as_deref() != Some("owner@example.invalid"))
        );
    }
    let requests = h.server.requests.lock().unwrap();
    assert!(requests.iter().any(|r| {
        r.target
            .starts_with("/api/me/calendars/shared-copy/calendarView")
    }));
    assert!(requests.iter().any(|r| {
        r.target
            .starts_with("/api/users/owner%40example.invalid/calendarView")
    }));
    assert!(
        requests.iter().any(|r| r.target.starts_with(
            "/api/users/owner%40example.invalid/calendars/owner-relative/calendarView"
        ))
    );
    assert!(
        requests
            .iter()
            .any(|r| r.target.starts_with("/api/groups/same-group/calendarView"))
    );
}

#[test]
fn partial_target_denial_fences_only_that_account_and_transient_errors_preserve_last_success() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let b = h.add("b");
    let b = h.select(&b, &[own()]);
    h.sync(&b.account);
    *h.cloud.signin.lock().unwrap() = "a".into();
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
    let old = h.service.account_authority(&a.account).unwrap().unwrap();
    h.cloud
        .transient
        .lock()
        .unwrap()
        .push("/api/me/calendars/shared-copy/calendarView".into());
    let transient = h.sync(&a.account);
    assert_eq!(transient.outcome, AccountSyncOutcome::Partial);
    assert_eq!(transient.cache.events.len(), 2);
    assert_eq!(
        h.service.account_authority(&a.account).unwrap(),
        Some(old.clone())
    );
    h.cloud.transient.lock().unwrap().clear();
    h.cloud
        .denied
        .lock()
        .unwrap()
        .push("/api/me/calendars/shared-copy/calendarView".into());
    let denied = h.sync(&a.account);
    assert_eq!(denied.outcome, AccountSyncOutcome::Partial);
    assert_eq!(denied.cache.events.len(), 1);
    let current = h.service.account_authority(&a.account).unwrap().unwrap();
    assert!(current.selection_generation > old.selection_generation);
    assert_eq!(current.selected_calendars, vec![own()]);
    assert_eq!(
        h.service
            .cached_account_calendar(&b.account)
            .unwrap()
            .events
            .len(),
        1
    );
    h.cloud.denied.lock().unwrap().clear();
    h.sync(&a.account);
    assert_ne!(
        h.service
            .account_authority(&a.account)
            .unwrap()
            .unwrap()
            .selection_generation,
        old.selection_generation
    );
}

#[test]
fn blocked_refresh_preserves_private_state_but_cannot_authorize_cached_evidence_or_retry() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let b = h.add("b");
    let a = h.select(&a, &[own()]);
    let b = h.select(&b, &[own()]);
    h.sync(&a.account);
    h.sync(&b.account);
    h.expire(&a.account);
    let secrets = h.secrets.secrets.lock().unwrap().clone();
    *h.cloud.oauth_error.lock().unwrap() =
        Some(json!({"error":"invalid_grant","error_codes":[53003]}));
    assert_eq!(
        h.service.keep_account_alive(&a.account).unwrap_err().code(),
        "conditional_access_blocked"
    );
    let requests = h.server.requests.lock().unwrap().len();
    assert_eq!(
        h.service.keep_account_alive(&a.account).unwrap_err().code(),
        "conditional_access_blocked"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), requests);
    assert!(h.service.account_authority(&a.account).unwrap().is_none());
    assert!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(*h.secrets.secrets.lock().unwrap(), secrets);
    let account = h
        .service
        .list_accounts(None)
        .unwrap()
        .into_iter()
        .find(|x| x.account == a.account)
        .unwrap();
    assert_eq!(account.selected_calendars, vec![own()]);
    assert_eq!(
        account.access_error.as_deref(),
        Some("conditional_access_blocked")
    );
    assert_eq!(
        h.service
            .cached_account_calendar(&b.account)
            .unwrap()
            .events
            .len(),
        1
    );
    let (from, to) = (Utc::now(), Utc::now() + Delta::days(1));
    assert_eq!(
        h.service
            .sync_account(&a.account, from, to)
            .unwrap_err()
            .code(),
        "conditional_access_blocked"
    );
    assert_eq!(
        h.service
            .sync_account(&b.account, from, to)
            .unwrap()
            .outcome,
        AccountSyncOutcome::Available
    );
    assert!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(
        h.service
            .cached_account_calendar(&b.account)
            .unwrap()
            .events
            .len(),
        1
    );
    *h.cloud.oauth_error.lock().unwrap() = None;
    *h.cloud.signin.lock().unwrap() = "a".into();
    let restored = h
        .service
        .reconnect_account(&a.account, a.generation, &LoginAttemptId::generate())
        .unwrap();
    assert!(restored.access_error.is_none());
    assert!(restored.generation > a.generation);
}

#[test]
fn selection_revocation_during_http_discards_results_without_reviving_cache() {
    for remove in [false, true] {
        let h = Accounts::new();
        *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
        let a = h.add("a");
        let a = h.select(&a, &[own()]);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *h.cloud.pause.lock().unwrap() = Some((entered_tx, release_rx));
        let service = h.service.clone();
        let account = a.account.clone();
        let worker = thread::spawn(move || {
            service.sync_account(&account, Utc::now(), Utc::now() + Delta::days(1))
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if remove {
            h.service.remove_account(&a.account, a.generation).unwrap();
        } else {
            h.service
                .configure_calendars(&a.account, a.generation, a.selection_generation, &[])
                .unwrap();
        }
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
        assert!(
            h.service
                .cached_account_calendar(&a.account)
                .unwrap()
                .events
                .is_empty()
        );
    }
}

/// A direct sync of an account that already needs reconnecting, or was removed, names that
/// with its own code before any credential or network use, while the other account keeps
/// syncing. Only a change while a sync runs is `superseded` (test above).
#[test]
fn a_direct_sync_of_an_account_needing_reconnect_or_removed_is_not_superseded() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let b = h.add("b");
    let a = h.select(&a, &[own()]);
    let b = h.select(&b, &[own()]);
    h.sync(&a.account);
    h.sync(&b.account);
    h.expire(&a.account);
    *h.cloud.oauth_error.lock().unwrap() = Some(json!({"error":"invalid_grant"}));
    assert!(h.service.keep_account_alive(&a.account).is_err());
    *h.cloud.oauth_error.lock().unwrap() = None;
    let status = |account: &AccountRef| {
        h.service
            .list_accounts(None)
            .unwrap()
            .into_iter()
            .find(|x| &x.account == account)
            .map(|x| x.status)
    };
    assert_eq!(status(&a.account), Some(AccountStatus::ReauthRequired));
    let (from, to) = (Utc::now(), Utc::now() + Delta::days(1));

    let requests = h.server.requests.lock().unwrap().len();
    let secrets = h.secrets.secrets.lock().unwrap().clone();
    assert_eq!(
        h.service
            .sync_account(&a.account, from, to)
            .unwrap_err()
            .code(),
        "reauth_required"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), requests);
    assert_eq!(*h.secrets.secrets.lock().unwrap(), secrets);
    assert_eq!(status(&a.account), Some(AccountStatus::ReauthRequired));
    assert_eq!(
        h.service
            .sync_account(&b.account, from, to)
            .unwrap()
            .outcome,
        AccountSyncOutcome::Available
    );

    h.service.remove_account(&a.account, a.generation).unwrap();
    let requests = h.server.requests.lock().unwrap().len();
    assert_eq!(
        h.service
            .sync_account(&a.account, from, to)
            .unwrap_err()
            .code(),
        "account_not_found"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), requests);
    assert_eq!(
        h.service
            .sync_account(&b.account, from, to)
            .unwrap()
            .outcome,
        AccountSyncOutcome::Available
    );
}

#[test]
fn a_client_replaced_before_sync_requires_reauthentication_without_sending_credentials() {
    let h = Accounts::new();
    let account = h.add("a");
    let requests = h.server.requests.lock().unwrap().len();
    let secrets = h.secrets.secrets.lock().unwrap().clone();
    std::fs::write(
        h.tmp.path().join("config.toml"),
        "[integration.microsoft-graph]\nclient_id = 'another-synthetic-client'\n",
    )
    .unwrap();
    let listed = h.service.list_accounts(None).unwrap();
    assert_eq!(listed[0].status, AccountStatus::ReauthRequired);
    let from = Utc::now();
    assert_eq!(
        h.service
            .sync_account(&account.account, from, from + Delta::days(1))
            .unwrap_err()
            .code(),
        "reauth_required",
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), requests);
    assert_eq!(*h.secrets.secrets.lock().unwrap(), secrets);
}

#[test]
fn unknown_profile_is_not_guessed_personal_and_bounded_pagination_never_leaks_bearer() {
    let h = Accounts::new();
    h.cloud.identity_missing.store(true, Ordering::SeqCst);
    assert_eq!(
        h.service
            .add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
            .unwrap_err()
            .code(),
        "account_identity_unavailable"
    );
    assert!(h.service.list_accounts(None).unwrap().is_empty());
    assert!(h.secrets.secrets.lock().unwrap().is_empty());
    h.cloud.identity_missing.store(false, Ordering::SeqCst);
    let a = h.add("a");
    let destination = Server::new(|_| reply(200, json!({"value":[]})));
    *h.cloud.pagination.lock().unwrap() = Some(format!("{}/steal", destination.base));
    assert_eq!(
        h.service.list_calendars(&a.account).unwrap_err().code(),
        "invalid_pagination"
    );
    assert!(destination.requests.lock().unwrap().is_empty());
    *h.cloud.pagination.lock().unwrap() =
        Some(format!("{}/api/me/calendars?$top=100", h.server.base));
    assert_eq!(
        h.service.list_calendars(&a.account).unwrap_err().code(),
        "invalid_pagination"
    );
}

struct QueuedBrowser {
    ready: std::sync::mpsc::Sender<String>,
}
impl BrowserLauncher for QueuedBrowser {
    fn open(&self, url: &str) -> broscribe_platform::Result<()> {
        self.ready.send(url.into()).unwrap();
        Ok(())
    }
}
fn finish_callback(url: &str) {
    let query: BTreeMap<_, _> = parse_query(url.split_once('?').unwrap().1)
        .into_iter()
        .collect();
    let callback = format!(
        "{}?{}",
        query["redirect_uri"],
        build_query(&[("state", &query["state"]), ("code", "synthetic-code")])
    );
    Http::new().get(&callback, None, &[], &[]).unwrap();
}

#[test]
fn overlapping_signins_have_independent_attempts_and_reverse_completion_keeps_both_tenants() {
    let h = Accounts::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let service = Arc::new(facade(
        h.tmp.path(),
        IntegrationId::MicrosoftGraph,
        &h.server,
        h.secrets.clone(),
        Arc::new(QueuedBrowser { ready: ready_tx }),
        Arc::new(|_| {}),
    ));
    let first = {
        let service = service.clone();
        thread::spawn(move || {
            service.add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
        })
    };
    let first_url = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let second = {
        let service = service.clone();
        thread::spawn(move || {
            service.add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
        })
    };
    let second_url = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    *h.cloud.signin.lock().unwrap() = "b".into();
    finish_callback(&second_url);
    let b = second.join().unwrap().unwrap();
    *h.cloud.signin.lock().unwrap() = "a".into();
    finish_callback(&first_url);
    let a = first.join().unwrap().unwrap();
    assert_ne!(a.account, b.account);
    assert_eq!(service.list_accounts(None).unwrap().len(), 2);
}

#[test]
fn expired_refresh_is_single_flight_and_stale_facades_never_borrow_new_client_credentials() {
    let h = Accounts::new();
    let a = h.add("a");
    h.expire(&a.account);
    let other = Arc::new(facade(
        h.tmp.path(),
        IntegrationId::MicrosoftGraph,
        &h.server,
        h.secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    ));
    let before = h
        .server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.target == "/token")
        .count();
    let barrier = Arc::new(std::sync::Barrier::new(6));
    let workers: Vec<_> = (0..6)
        .map(|n| {
            let service = if n % 2 == 0 {
                h.service.clone()
            } else {
                other.clone()
            };
            let account = a.account.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                service.keep_account_alive(&account)
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    assert_eq!(
        h.server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target == "/token")
            .count(),
        before + 1
    );
    let mut config = IntegrationsConfig::load(h.tmp.path()).unwrap();
    config
        .integration
        .get_mut("microsoft-graph")
        .unwrap()
        .client_id = Some("new-client".into());
    authoritative(h.tmp.path(), config.clone());
    let fresh = Integrations::new(
        h.tmp.path(),
        config,
        h.secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    )
    .unwrap()
    .with_endpoints(IntegrationId::MicrosoftGraph, h.server.endpoints());
    let before = h.server.requests.lock().unwrap().len();
    assert_eq!(
        h.service.keep_account_alive(&a.account).unwrap_err().code(),
        "superseded"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
    assert!(
        fresh
            .retire_changed_client(IntegrationId::MicrosoftGraph, &settings())
            .unwrap()
            .sign_in_retired
    );
    let retired = fresh.list_accounts(None).unwrap().remove(0);
    let restored = fresh
        .reconnect_account(&a.account, retired.generation, &LoginAttemptId::generate())
        .unwrap();
    assert_eq!(restored.account, a.account);
    assert_eq!(restored.identity.client_id, "new-client");
    assert_eq!(
        h.service.keep_account_alive(&a.account).unwrap_err().code(),
        "superseded"
    );
}

#[test]
fn crash_recovery_removes_only_uncommitted_versions_and_finishes_committed_cleanup() {
    for step in 0..6 {
        let h = Accounts::new();
        let a = h.add("a");
        let b = h.add("b");
        let registry_path =
            account_dir(h.tmp.path(), IntegrationId::MicrosoftGraph).join("accounts.json");
        let mut registry: Value =
            serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
        let a_slot = registry["accounts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|slot| slot["view"]["account"]["account_id"] == json!(a.account.account_id))
            .unwrap();
        let old = a_slot["secret_key"].as_str().unwrap().to_owned();
        let old_value = h.secrets.get(&old).unwrap().unwrap();
        let new = format!(
            "integration/microsoft-graph/accounts/{}/tokens/22222222222222222222222222222222",
            a.account.account_id
        );
        // Public persistent protocol snapshots: before prepare; after prepare; after secret
        // write; after pointer commit; after obsolete-secret deletion.
        if step >= 3 {
            a_slot["secret_key"] = json!(new);
        }
        if (1..5).contains(&step) {
            registry["pending_secrets"] =
                json!([{"account_id":a.account.account_id,"key":new,"obsolete_key":old}]);
        }
        if step >= 2 {
            h.secrets.set(&new, &old_value).unwrap();
        }
        if step >= 4 {
            h.secrets.delete(&old).unwrap();
        }
        write_json_private(&registry_path, &registry).unwrap();
        let recovered = h.service.list_accounts(None).unwrap();
        assert_eq!(recovered.len(), 2);
        assert!(recovered.iter().any(|account| account.account == b.account));
        assert_eq!(h.secrets.get(&new).unwrap().is_some(), step >= 3);
        assert_eq!(h.secrets.get(&old).unwrap().is_some(), step < 3);
        // Repeating recovery is idempotent and never enumerates SecretStore keys.
        assert_eq!(h.service.list_accounts(None).unwrap(), recovered);
        assert!(h.service.account_directory(&b.account, "Example").is_ok());
    }
}

struct FailingSetStore {
    inner: Arc<FakeSecretStore>,
    fail_after_write: AtomicBool,
    registry: std::path::PathBuf,
}
impl SecretStore for FailingSetStore {
    fn get(&self, key: &str) -> broscribe_platform::Result<Option<String>> {
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &str) -> broscribe_platform::Result<()> {
        let registry: Value =
            serde_json::from_slice(&std::fs::read(&self.registry).unwrap()).unwrap();
        assert!(
            registry["pending_secrets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|pending| pending["key"] == key)
        );
        self.inner.set(key, value)?;
        if self.fail_after_write.swap(false, Ordering::SeqCst) {
            return Err(broscribe_platform::Error::Service {
                code: "synthetic_crash",
                message: "synthetic failure after secret write".into(),
            });
        }
        Ok(())
    }
    fn delete(&self, key: &str) -> broscribe_platform::Result<()> {
        self.inner.delete(key)
    }
}

#[test]
fn secret_store_failure_after_write_recovers_without_overwriting_a_working_secret() {
    let h = Accounts::new();
    let a = h.add("a");
    let before = h.secrets.secrets.lock().unwrap().clone();
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
            .reconnect_account(&a.account, a.generation, &LoginAttemptId::generate())
            .is_err()
    );
    assert_eq!(service.list_accounts(None).unwrap(), vec![a]);
    assert_eq!(*h.secrets.secrets.lock().unwrap(), before);
}

#[test]
fn paginated_catalog_has_a_finite_page_bound() {
    let server = Server::new(|r| {
        if let Some(identity) = microsoft_identity_fixture(r) {
            return identity;
        }
        if r.target == "/token" {
            return reply(200, token());
        }
        if r.target.starts_with("/authorize") {
            return reply(200, json!({}));
        }
        let page = r
            .query()
            .get("page")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        reply(
            200,
            json!({"value":[],"@odata.nextLink":format!("http://{}/api/me/calendars?page={}",r.host,page+1)}),
        )
    });
    let tmp = tempfile::tempdir().unwrap();
    let service = facade(
        tmp.path(),
        IntegrationId::MicrosoftGraph,
        &server,
        Arc::new(FakeSecretStore::default()),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    );
    let account = service
        .add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
        .unwrap();
    assert_eq!(
        service.list_calendars(&account.account).unwrap_err().code(),
        "pagination_limit"
    );
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.target.starts_with("/api/me/calendars?"))
            .count(),
        100
    );
}

#[test]
fn failed_only_legacy_attempt_is_not_a_quarantined_account_but_unbound_working_slot_is() {
    for working in [false, true] {
        let h = Accounts::new();
        let id = IntegrationId::MicrosoftGraph;
        write_json_private(
            &account_dir(h.tmp.path(), id).join(STATE_FILE),
            &AccountState {
                generation: 7,
                status: if working {
                    AccountStatus::Active
                } else {
                    AccountStatus::SignedOut
                },
                ..Default::default()
            },
        )
        .unwrap();
        if working {
            h.secrets
                .set(&tokens_secret_key(id), "synthetic-unbound-credential")
                .unwrap();
        }
        assert_eq!(
            h.service.legacy_microsoft_status(id).unwrap(),
            if working {
                LegacyMicrosoftStatus::Quarantined
            } else {
                LegacyMicrosoftStatus::None
            }
        );
        assert!(h.service.list_accounts(Some(id)).unwrap().is_empty());
        assert_eq!(
            h.service
                .sync_calendar(id, Utc::now(), Utc::now() + Delta::days(1))
                .unwrap_err()
                .code(),
            "account_required"
        );
        assert_eq!(
            h.secrets.get(&tokens_secret_key(id)).unwrap().is_some(),
            working
        );
    }
}

#[test]
fn dot_segment_routes_are_refused_before_any_http_or_evidence_authorization() {
    let h = Accounts::new();
    let a = h.add("a");
    let invalid = vec![
        CalendarTarget::Own {
            calendar_id: ".".into(),
        },
        CalendarTarget::RecipientShared {
            calendar_id: "..".into(),
        },
        CalendarTarget::Delegated {
            mailbox: "..".into(),
            calendar_id: None,
        },
        CalendarTarget::Delegated {
            mailbox: "owner@example.invalid".into(),
            calendar_id: Some(".".into()),
        },
        CalendarTarget::Group {
            group_id: "..".into(),
        },
    ];
    let before = h.server.requests.lock().unwrap().len();
    for target in &invalid {
        assert_eq!(
            h.service
                .configure_calendars(
                    &a.account,
                    a.generation,
                    a.selection_generation,
                    std::slice::from_ref(target)
                )
                .unwrap_err()
                .code(),
            "invalid_calendar_target"
        );
    }
    let registry_path =
        account_dir(h.tmp.path(), IntegrationId::MicrosoftGraph).join("accounts.json");
    let mut registry: Value =
        serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
    registry["accounts"][0]["view"]["selected_calendars"] = json!(invalid);
    registry["accounts"][0]["readable_targets"] = json!(invalid);
    write_json_private(&registry_path, &registry).unwrap();
    assert_eq!(
        h.service
            .sync_account(&a.account, Utc::now(), Utc::now() + Delta::days(1))
            .unwrap_err()
            .code(),
        "invalid_calendar_target"
    );
    assert!(
        h.service
            .account_authority(&a.account)
            .unwrap()
            .unwrap()
            .selected_calendars
            .is_empty()
    );
    assert!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
    CalendarTarget::Delegated {
        mailbox: "owner+alias@example.invalid".into(),
        calendar_id: Some("opaque/with+punctuation=".into()),
    }
    .validate()
    .unwrap();
}

#[test]
fn http_redirects_cannot_forward_account_bearer_to_another_origin() {
    let h = Accounts::new();
    let a = h.add("a");
    let destination = Server::new(|_| reply(200, json!({"value":[]})));
    let location = format!("{}/steal", destination.base);
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let redirect = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = read_request(&mut stream);
        assert_eq!(request.authorization.as_deref(), Some("Bearer access-a"));
        write!(stream,"HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let mut endpoints = h.server.endpoints();
    endpoints.api = format!("http://{address}/api");
    let service = facade(
        h.tmp.path(),
        IntegrationId::MicrosoftGraph,
        &h.server,
        h.secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    )
    .with_endpoints(IntegrationId::MicrosoftGraph, endpoints);
    assert!(service.list_calendars(&a.account).is_err());
    redirect.join().unwrap();
    assert!(destination.requests.lock().unwrap().is_empty());
}

#[test]
fn first_account_secret_failure_recovers_without_an_orphan_account_or_secret() {
    let h = Accounts::new();
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
}

#[test]
fn requested_permissions_do_not_become_granted_capabilities_without_evidence() {
    let h = Accounts::new();
    *h.cloud.grants.lock().unwrap() = "Calendars.Read User.Read".into();
    let account = h.add("a");
    assert_eq!(account.granted_scopes, vec!["Calendars.Read", "User.Read"]);
    assert_eq!(account.calendar, Capability::Unknown);
    let requests = h.server.requests.lock().unwrap().len();
    assert_eq!(
        h.service
            .account_directory(&account.account, "Example")
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), requests);
    assert_eq!(
        h.service.list_accounts(None).unwrap()[0].directory,
        Capability::Denied
    );
    let account = h.select(&account, &[own()]);
    assert_eq!(
        h.sync(&account.account).outcome,
        AccountSyncOutcome::Available
    );
}

#[test]
fn directory_and_photo_generations_survive_failed_grant_but_not_successful_reconnect() {
    let h = Accounts::new();
    let a = h.add("a");
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
    let cached = cached_photo(h.tmp.path(), "p-abc123").unwrap();
    assert_eq!(
        std::fs::canonicalize(cached).unwrap(),
        std::fs::canonicalize(&photo).unwrap()
    );
    assert_eq!(
        h.service
            .grant_account_permission(
                &a.account,
                a.generation,
                AccountPermission::SharedCalendars,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    assert!(photo.exists());
    assert_eq!(
        std::fs::canonicalize(cached_photo(h.tmp.path(), "p-abc123").unwrap()).unwrap(),
        std::fs::canonicalize(&photo).unwrap()
    );
    let restored = h
        .service
        .reconnect_account(&a.account, a.generation, &LoginAttemptId::generate())
        .unwrap();
    assert!(!photo.exists());
    assert!(cached_photo(h.tmp.path(), "p-abc123").is_none());
    let before = h.server.requests.lock().unwrap().len();
    assert_eq!(
        h.service
            .cache_account_photo(&a.account, a.generation, "p-abc123", &person, &|_| true)
            .unwrap_err()
            .code(),
        "superseded"
    );
    assert_eq!(
        h.service
            .cache_account_photo(
                &a.account,
                restored.generation,
                "p-abc123",
                &person,
                &|_| { true }
            )
            .unwrap_err()
            .code(),
        "account_identity_mismatch"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
}

/// The people gallery's one batched lookup per load serves each person's photo from the
/// account that cached it, Google and every Microsoft account alike. People without a
/// photo or with an invalid ID are left out; Workspace switched off hides only Google's
/// photos; an account removed while the photos are being read never has its bytes
/// returned, while the other account's still are.
#[test]
fn batched_photos_follow_each_accounts_authority_even_when_it_changes_during_the_load() {
    let h = Accounts::new();
    let a = h.add("a");
    let b = h.add("b");
    let cache = |account: &MicrosoftAccount, person_id: &str| {
        let person = h
            .service
            .account_directory(&account.account, "Example")
            .unwrap()
            .people
            .remove(0);
        h.service
            .cache_account_photo(
                &account.account,
                account.generation,
                person_id,
                &person,
                &|_| true,
            )
            .unwrap()
            .unwrap();
    };
    cache(&a, "p-aaaaaa");
    cache(&b, "p-bbbbbb");
    let configure = |google: IntegrationSettings| {
        authoritative(
            h.tmp.path(),
            IntegrationsConfig {
                integration: [
                    (IntegrationId::MicrosoftGraph.to_string(), settings()),
                    (IntegrationId::Google.to_string(), google),
                ]
                .into_iter()
                .collect(),
            },
        );
    };
    configure(settings());
    let google = account_dir(h.tmp.path(), IntegrationId::Google);
    write_json_private(
        &google.join(STATE_FILE),
        &AccountState {
            generation: 1,
            status: AccountStatus::Active,
            client_id: settings().client_id,
            scopes: google_grant(),
            ..Default::default()
        },
    )
    .unwrap();
    crate::store::write_private(
        &crate::store::provider_photo_dir(&google, 1).join("p-cccccc.img"),
        b"photo-g",
    )
    .unwrap();
    let everyone = ["p-cccccc", "p-aaaaaa", "p-000def", "../escape", "p-bbbbbb"];
    let shown = || {
        cached_photos(h.tmp.path(), everyone.map(|id| (id, id)), |path| {
            std::fs::read(path).ok()
        })
    };
    let photo = |id: &'static str, bytes: &[u8]| (id, bytes.to_vec());
    assert_eq!(
        shown(),
        vec![
            photo("p-cccccc", b"photo-g"),
            photo("p-aaaaaa", b"photo-a"),
            photo("p-bbbbbb", b"photo-b"),
        ]
    );

    configure(IntegrationSettings {
        workspace: false,
        ..settings()
    });
    assert_eq!(
        shown(),
        vec![photo("p-aaaaaa", b"photo-a"), photo("p-bbbbbb", b"photo-b")]
    );

    // Account A is removed right after its photo was read, before B's is.
    let mut removed = false;
    let read = cached_photos(
        h.tmp.path(),
        [("p-aaaaaa", "p-aaaaaa"), ("p-bbbbbb", "p-bbbbbb")],
        |path| {
            let bytes = std::fs::read(path).ok();
            if !removed {
                assert_eq!(bytes.as_deref(), Some(b"photo-a".as_slice()));
                h.service.remove_account(&a.account, a.generation).unwrap();
                removed = true;
            }
            bytes
        },
    );
    assert!(removed);
    assert_eq!(read, vec![photo("p-bbbbbb", b"photo-b")]);
    assert_eq!(shown(), vec![photo("p-bbbbbb", b"photo-b")]);
}

#[test]
fn a_delayed_refresh_cannot_resurrect_a_removed_account_or_touch_another_tenant() {
    let h = Accounts::new();
    let a = h.add("a");
    let b = h.add("b");
    h.expire(&a.account);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *h.cloud.refresh_pause.lock().unwrap() = Some((entered_tx, release_rx));
    let service = h.service.clone();
    let account = a.account.clone();
    let worker = thread::spawn(move || service.keep_account_alive(&account));
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    h.service.remove_account(&a.account, a.generation).unwrap();
    let b_credentials = h.secrets.secrets.lock().unwrap().clone();
    release_tx.send(()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
    assert_eq!(*h.secrets.secrets.lock().unwrap(), b_credentials);
    assert_eq!(h.service.list_accounts(None).unwrap(), vec![b]);
    assert!(h.service.account_authority(&a.account).unwrap().is_none());
}

#[test]
fn corrupt_recovery_references_never_delete_another_provider_or_account_secret() {
    for foreign_old in [false, true] {
        let h = Accounts::new();
        let a = h.add("a");
        let foreign = "integration/google/tokens";
        h.secrets
            .set(foreign, "synthetic-unrelated-secret")
            .unwrap();
        let registry_path =
            account_dir(h.tmp.path(), IntegrationId::MicrosoftGraph).join("accounts.json");
        let mut registry: Value =
            serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
        let own_version = format!(
            "integration/microsoft-graph/accounts/{}/tokens/33333333333333333333333333333333",
            a.account.account_id
        );
        registry["pending_secrets"] = if foreign_old {
            json!([{"account_id":a.account.account_id,"key":own_version,"obsolete_key":foreign}])
        } else {
            json!([{"account_id":a.account.account_id,"key":foreign,"obsolete_key":null}])
        };
        write_json_private(&registry_path, &registry).unwrap();
        let before = h.secrets.secrets.lock().unwrap().clone();
        assert_eq!(
            h.service.list_accounts(None).unwrap_err().code(),
            "invalid_secret_reference"
        );
        assert_eq!(*h.secrets.secrets.lock().unwrap(), before);
    }
    let h = Accounts::new();
    let a = h.add("a");
    let b = h.add("b");
    let foreign = format!(
        "integration/microsoft-graph/accounts/{}/tokens/44444444444444444444444444444444",
        b.account.account_id
    );
    h.secrets
        .set(&foreign, "synthetic-unreferenced-other-account-secret")
        .unwrap();
    let registry_path =
        account_dir(h.tmp.path(), IntegrationId::MicrosoftGraph).join("accounts.json");
    let mut registry: Value =
        serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
    registry["pending_secrets"] =
        json!([{"account_id":a.account.account_id,"key":foreign,"obsolete_key":null}]);
    write_json_private(&registry_path, &registry).unwrap();
    assert_eq!(
        h.service.list_accounts(None).unwrap_err().code(),
        "invalid_secret_reference"
    );
    assert_eq!(
        h.secrets.get(&foreign).unwrap().as_deref(),
        Some("synthetic-unreferenced-other-account-secret")
    );
}

#[test]
fn broader_returned_tokens_do_not_silently_enable_shared_or_group_reads() {
    let h = Accounts::new();
    *h.cloud.grants.lock().unwrap() = format!("{BASE_GRANTS} Calendars.Read.Shared Group.Read.All");
    let a = h.add("a");
    let catalog = h.service.list_calendars(&a.account).unwrap();
    assert_eq!(catalog.groups, Capability::NotRequested);
    assert!(
        !catalog
            .calendars
            .iter()
            .any(|calendar| matches!(calendar.target, CalendarTarget::Group { .. }))
    );
    assert_eq!(
        h.service
            .configure_calendars(
                &a.account,
                a.generation,
                a.selection_generation,
                &[shared()]
            )
            .unwrap_err()
            .code(),
        "insufficient_grant"
    );
    assert!(!h.server.requests.lock().unwrap().iter().any(|r|r.target.starts_with("/api/me/memberOf/") || r.target.contains("calendarView")));
}

#[path = "account_lifecycle_tests.rs"]
mod account_lifecycle_tests;

#[test]
fn route_denials_fence_stale_successes_even_on_first_read_and_before_cache_replacement() {
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = h.select(&a, &[own()]);
    // A second facade whose provider refuses the route, sharing the account and its locks.
    let refusing = Server::new(|r| {
        if r.target.contains("calendarView") {
            reply(403, json!({}))
        } else {
            reply(200, json!({"id":"same-calendar"}))
        }
    });
    let denying = facade(
        h.tmp.path(),
        IntegrationId::MicrosoftGraph,
        &refusing,
        h.secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    );
    let (from, to) = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));

    // First read of a never-readable route: a success read before the denial committed
    // must not publish after it.
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *h.cloud.pause.lock().unwrap() = Some((entered_tx, release_rx));
    let stale = {
        let service = h.service.clone();
        let account = a.account.clone();
        thread::spawn(move || service.sync_account(&account, from, to))
    };
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let denied = denying.sync_account(&a.account, from, to).unwrap();
    assert_eq!(denied.outcome, AccountSyncOutcome::Denied);
    assert_eq!(denied.event_count, 0);
    assert_eq!(denied.routes[0].copy, RouteCopy::Empty);
    assert!(denied.routes[0].last_success.is_none());
    release_tx.send(()).unwrap();
    assert_eq!(stale.join().unwrap().unwrap_err().code(), "superseded");
    assert!(
        h.service
            .account_authority(&a.account)
            .unwrap()
            .unwrap()
            .selected_calendars
            .is_empty()
    );
    assert!(
        h.service
            .cached_account_calendar(&a.account)
            .unwrap()
            .events
            .is_empty()
    );
    // Sync responses carry outcomes and counts, never the private event copy.
    assert!(
        serde_json::to_value(&denied)
            .unwrap()
            .get("cache")
            .is_none()
    );

    // A later successful read publishes; the response still holds no event content.
    let read = h.sync(&a.account);
    assert_eq!(read.outcome, AccountSyncOutcome::Available);
    assert_eq!(read.routes[0].copy, RouteCopy::Fresh);
    assert!(read.routes[0].last_success.is_some());
    assert!(
        !serde_json::to_string(&read.sync)
            .unwrap()
            .contains("Synthetic planning")
    );
    let before = h.service.account_authority(&a.account).unwrap().unwrap();
    assert_eq!(before.selected_calendars, vec![own()]);

    // Revoking the now-readable route is saved before the cache is replaced: even when the
    // cache cannot be written, the route is withdrawn and its copy no longer matches.
    let cache = account_dir(h.tmp.path(), IntegrationId::MicrosoftGraph)
        .join("accounts")
        .join(a.account.account_id.as_str())
        .join(CALENDAR_FILE);
    std::fs::remove_file(&cache).unwrap();
    std::fs::create_dir_all(cache.join("blocker")).unwrap();
    assert!(denying.sync_account(&a.account, from, to).is_err());
    let after = h.service.account_authority(&a.account).unwrap().unwrap();
    assert!(after.selection_generation > before.selection_generation);
    assert!(after.selected_calendars.is_empty());
}

#[test]
fn an_unreadable_trust_store_fails_microsoft_reads_closed_and_keeps_removal_local() {
    use broscribe_platform::fakes::FakeTrustStore;
    let h = Accounts::new();
    *h.cloud.events.lock().unwrap() = vec![Accounts::event()];
    let a = h.add("a");
    let a = h.select(&a, &[own()]);
    h.sync(&a.account);
    let authority = h.service.account_authority(&a.account).unwrap();
    let broken = facade(
        h.tmp.path(),
        IntegrationId::MicrosoftGraph,
        &h.server,
        h.secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| panic!("an unavailable transport is not a sign-in failure")),
    )
    .with_trust_store(&FakeTrustStore::default());
    let before = h.server.requests.lock().unwrap().len();
    let (from, to) = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
    // Reported as a failed read, never as success or as a denial that withdraws routes.
    let read = broken.sync_account(&a.account, from, to).unwrap();
    assert_eq!(read.outcome, AccountSyncOutcome::Error);
    assert_eq!(
        read.targets[0].outcome,
        CalendarTargetOutcome::Error {
            code: "trust_store_unavailable".into()
        }
    );
    assert_eq!(read.routes[0].copy, RouteCopy::Retained);
    assert_eq!(
        broken
            .add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
            .unwrap_err()
            .code(),
        "trust_store_unavailable"
    );
    assert_eq!(
        broken
            .account_directory(&a.account, "Example")
            .unwrap_err()
            .code(),
        "trust_store_unavailable"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
    assert_eq!(broken.account_authority(&a.account).unwrap(), authority);
    // Removal needs no provider and still happens.
    let account = broken.list_accounts(None).unwrap().remove(0);
    broken
        .remove_account(&a.account, account.generation)
        .unwrap();
    assert!(broken.list_accounts(None).unwrap().is_empty());
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
}
