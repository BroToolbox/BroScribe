//! Public synthetic regressions of cancellable Google sign-ins. A second facade on the same
//! data directory plays the cancelling process (the app or another CLI). No real browser,
//! provider, network or keychain is used.
use super::*;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Instant;

const PHOTO: &str = "photos/3/p-123abc.img";

/// A browser the user left open: it hands the authorization URL to the test and never
/// calls back by itself.
struct WaitingBrowser(Mutex<mpsc::Sender<String>>);
impl BrowserLauncher for WaitingBrowser {
    fn open(&self, url: &str) -> broscribe_platform::Result<()> {
        self.0.lock().unwrap().send(url.to_owned()).unwrap();
        Ok(())
    }
}

/// Another process's view of the same data directory; cancelling needs no configuration.
fn other_process(dir: &std::path::Path, secrets: Arc<FakeSecretStore>) -> Integrations {
    Integrations::new(
        dir,
        IntegrationsConfig::default(),
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
    .unwrap()
}

/// A working Google account with a versioned credential, an explicit selection, a cached
/// calendar and a cached photo.
fn working_google(dir: &std::path::Path, secrets: &FakeSecretStore) -> String {
    let key = format!("{}/{:032x}", tokens_secret_key(IntegrationId::Google), 7);
    let tokens = Tokens {
        access_token: "synthetic-working-access".into(),
        refresh_token: Some("synthetic-working-refresh".into()),
        expires_at: Utc::now().timestamp() + 3600,
        scope: None,
    };
    secrets
        .set(&key, &serde_json::to_string(&tokens).unwrap())
        .unwrap();
    let google = account_dir(dir, IntegrationId::Google);
    write_json_private(
        &google.join(STATE_FILE),
        &AccountState {
            generation: 3,
            status: AccountStatus::Active,
            account: Some(AccountInfo {
                name: None,
                email: Some(GOOGLE_CALENDAR.into()),
            }),
            capabilities: Capabilities {
                calendar: Capability::Available,
                directory: Capability::Available,
            },
            client_id: settings().client_id,
            scopes: google_grant(),
            native_calendar_ids: vec![GOOGLE_CALENDAR.into()],
            secret_key: Some(key.clone()),
            last_sync: Some(Utc::now()),
            ..Default::default()
        },
    )
    .unwrap();
    write_json_private(&google.join(CALENDAR_FILE), &json!({"events":[]})).unwrap();
    crate::store::write_private(&google.join(PHOTO), b"working-photo").unwrap();
    key
}

/// Everything a cancelled sign-in must leave exactly as it was.
#[derive(Debug, PartialEq)]
struct Working {
    state: AccountState,
    secrets: HashMap<String, String>,
    cache: Vec<u8>,
    photo: Vec<u8>,
}
fn working(i: &Integrations, dir: &std::path::Path, secrets: &FakeSecretStore) -> Working {
    let google = account_dir(dir, IntegrationId::Google);
    Working {
        state: i.account_state(IntegrationId::Google).unwrap(),
        secrets: secrets.secrets.lock().unwrap().clone(),
        cache: std::fs::read(google.join(CALENDAR_FILE)).unwrap(),
        photo: std::fs::read(google.join(PHOTO)).unwrap(),
    }
}

fn identity_and_token(r: &Request) -> Response {
    google_identity_fixture(r).unwrap_or_else(|| reply(200, google_token()))
}

#[test]
fn a_sign_in_cancelled_before_its_callback_closes_the_listener_and_keeps_the_working_account() {
    let id = IntegrationId::Google;
    let server = Server::new(identity_and_token);
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    working_google(tmp.path(), &secrets);
    let (opened, browser_url) = mpsc::channel();
    let i = Arc::new(facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(WaitingBrowser(Mutex::new(opened))),
        Arc::new(|_| panic!("a cancelled sign-in sends no notification")),
    ));
    let before = working(&i, tmp.path(), &secrets);
    let attempt = LoginAttemptId::generate();
    let signing_in = {
        let (i, attempt) = (i.clone(), attempt.clone());
        thread::spawn(move || i.login(id, &attempt))
    };
    let url = browser_url.recv_timeout(Duration::from_secs(10)).unwrap();
    let other = other_process(tmp.path(), secrets.clone());
    assert_eq!(
        other.login_attempt(id, &attempt).unwrap().state,
        LoginAttemptState::Running
    );
    let cancelled_at = Instant::now();
    let status = other.cancel_login(id, &attempt).unwrap();
    assert_eq!(status.state, LoginAttemptState::Cancelled);
    assert_eq!(
        signing_in.join().unwrap().unwrap_err().code(),
        "login_cancelled"
    );
    assert!(cancelled_at.elapsed() < Duration::from_secs(5));

    // The listener is gone: the browser's late callback reaches nothing.
    let query: BTreeMap<_, _> = parse_query(url.split_once('?').unwrap().1)
        .into_iter()
        .collect();
    let late = format!(
        "{}?{}",
        query["redirect_uri"],
        build_query(&[("state", &query["state"]), ("code", "synthetic-late-code")])
    );
    assert!(Http::new().get(&late, None, &[], &[]).is_err());
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| !r.target.starts_with("/token"))
    );
    assert_eq!(working(&i, tmp.path(), &secrets), before);

    // Single use and no automatic retry: the same attempt opens no browser again.
    assert_eq!(i.login(id, &attempt).unwrap_err().code(), "login_cancelled");
    assert!(browser_url.try_recv().is_err());
    assert_eq!(
        i.login_attempt(id, &attempt).unwrap().state,
        LoginAttemptState::Cancelled
    );
}

#[test]
fn a_cancel_during_the_code_exchange_drops_the_late_token_without_storing_it() {
    let id = IntegrationId::Google;
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    working_google(tmp.path(), &secrets);
    let attempt = LoginAttemptId::generate();
    let other = Arc::new(other_process(tmp.path(), secrets.clone()));
    let server = {
        let (other, attempt) = (other.clone(), attempt.clone());
        Server::new(move |r| {
            if r.target == "/token" {
                let status = other.cancel_login(id, &attempt).unwrap();
                assert_eq!(status.state, LoginAttemptState::Cancelled);
            }
            identity_and_token(r)
        })
    };
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| panic!("a cancelled sign-in sends no notification")),
    );
    let before = working(&i, tmp.path(), &secrets);
    assert_eq!(i.login(id, &attempt).unwrap_err().code(), "login_cancelled");
    // Not even the identity was read with the late token.
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| !r.target.starts_with("/api/"))
    );
    assert_eq!(working(&i, tmp.path(), &secrets), before);
    assert_eq!(
        other.login_attempt(id, &attempt).unwrap().state,
        LoginAttemptState::Cancelled
    );
}

#[test]
fn a_cancel_that_wins_the_lock_just_before_the_commit_keeps_the_working_account() {
    let id = IntegrationId::Google;
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    working_google(tmp.path(), &secrets);
    let attempt = LoginAttemptId::generate();
    let other = Arc::new(other_process(tmp.path(), secrets.clone()));
    let server = {
        let (other, attempt) = (other.clone(), attempt.clone());
        // The identity read is the last step before the commit takes the lock.
        Server::new(move |r| {
            if r.target.starts_with("/api/calendars/primary?") {
                other.cancel_login(id, &attempt).unwrap();
            }
            identity_and_token(r)
        })
    };
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| panic!("a cancelled sign-in sends no notification")),
    );
    let before = working(&i, tmp.path(), &secrets);
    assert_eq!(i.login(id, &attempt).unwrap_err().code(), "login_cancelled");
    // No staged credential version, generation, selection, cache or photo changed.
    assert_eq!(working(&i, tmp.path(), &secrets), before);
    assert_eq!(
        i.google_authority().unwrap(),
        Some(GoogleCalendarStamp {
            generation: before.state.generation,
            selection_generation: before.state.selection_generation,
        })
    );
    assert_eq!(
        other.login_attempt(id, &attempt).unwrap().state,
        LoginAttemptState::Cancelled
    );
}

#[test]
fn a_commit_that_won_keeps_its_result_and_a_late_cancel_reports_it() {
    let id = IntegrationId::Google;
    let server = Server::new(identity_and_token);
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    working_google(tmp.path(), &secrets);
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    );
    let attempt = LoginAttemptId::generate();
    let signed_in = i.login(id, &attempt).unwrap().state;
    assert_eq!(signed_in.generation, 4);
    // The same verified account keeps its explicit selection.
    assert_eq!(signed_in.native_calendar_ids, [GOOGLE_CALENDAR]);
    let status = other_process(tmp.path(), secrets.clone())
        .cancel_login(id, &attempt)
        .unwrap();
    assert_eq!(status.state, LoginAttemptState::Succeeded);
    assert_eq!(i.account_state(id).unwrap(), signed_in);
}

#[test]
fn a_failed_sign_in_keeps_a_sanitized_result_and_is_not_retried() {
    let id = IntegrationId::Google;
    let tokens = Arc::new(AtomicUsize::new(0));
    let count = tokens.clone();
    let server = Server::new(move |r| {
        if r.target == "/token" {
            count.fetch_add(1, Ordering::SeqCst);
            return reply(
                400,
                json!({"error":"invalid_grant","error_description":"synthetic-secret-detail"}),
            );
        }
        identity_and_token(r)
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    working_google(tmp.path(), &secrets);
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    );
    let before = working(&i, tmp.path(), &secrets);
    let attempt = LoginAttemptId::generate();
    assert_eq!(i.login(id, &attempt).unwrap_err().code(), "oauth");
    let status = i.login_attempt(id, &attempt).unwrap();
    let LoginAttemptState::Failed { code, message } = &status.state else {
        panic!("unexpected state {:?}", status.state);
    };
    assert_eq!(code, "oauth");
    assert!(!message.contains("synthetic-secret-detail"), "{message}");
    assert_eq!(tokens.load(Ordering::SeqCst), 1);
    assert_eq!(working(&i, tmp.path(), &secrets), before);
    // The public result shape: integration, attempt and a flattened status.
    let value = serde_json::to_value(&status).unwrap();
    assert_eq!(value["integration"], json!("google"));
    assert_eq!(value["attempt"], json!(attempt.as_str()));
    assert_eq!(value["status"], json!("failed"));
    assert_eq!(value["code"], json!("oauth"));
    // A late cancel cannot rewrite the terminal result.
    assert_eq!(i.cancel_login(id, &attempt).unwrap(), status);
}

#[test]
fn a_dead_process_is_not_presumed_cancelled_and_half_done_commits_resolve_exactly() {
    let id = IntegrationId::Google;
    let server = Server::new(|_| panic!("reading attempts never reaches the provider"));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let committed = working_google(tmp.path(), &secrets);
    let browser = Arc::new(FakeBrowserLauncher::default());
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        browser.clone(),
        Arc::new(|_| {}),
    );
    let now = Utc::now();
    let entry = |id: &str, started: chrono::DateTime<Utc>, phase: Value| json!({"id":id,"started_at":started,"updated_at":started,"phase":phase});
    let mut attempts = vec![
        // Its process died during the browser wait an hour ago.
        entry("crashed", now - Delta::hours(1), json!({"phase":"running"})),
        // Its process died while saving: one commit landed, the other did not.
        entry(
            "saved",
            now - Delta::minutes(2),
            json!({"phase":"committing","secret_key":committed}),
        ),
        entry(
            "unsaved",
            now - Delta::minutes(2),
            json!({"phase":"committing","secret_key":format!("{}/{:032x}", tokens_secret_key(id), 9)}),
        ),
        // A result older than the retention is forgotten.
        entry(
            "old",
            now - Delta::hours(3),
            json!({"phase":"finished","outcome":{"status":"succeeded"}}),
        ),
    ];
    // Seven sign-ins still running elsewhere.
    for n in 0..7 {
        attempts.push(entry(&format!("live-{n}"), now, json!({"phase":"running"})));
    }
    write_json_private(
        &account_dir(tmp.path(), id).join("login-attempts.json"),
        &json!({"version":1,"attempts":attempts}),
    )
    .unwrap();
    let state = |name: &str| {
        i.login_attempt(id, &LoginAttemptId::parse(name).unwrap())
            .unwrap()
            .state
    };
    assert_eq!(
        state("crashed"),
        LoginAttemptState::Failed {
            code: "login_abandoned".into(),
            message: "the sign-in stopped without finishing and saved nothing".into(),
        }
    );
    assert_eq!(state("saved"), LoginAttemptState::Succeeded);
    assert_eq!(state("unsaved"), LoginAttemptState::Interrupted);
    assert_eq!(state("old"), LoginAttemptState::NotFound);
    assert_eq!(state("live-0"), LoginAttemptState::Running);
    // An abandoned attempt can never commit late.
    assert_eq!(
        i.cancel_login(id, &LoginAttemptId::parse("crashed").unwrap())
            .unwrap()
            .state,
        state("crashed")
    );
    // At most eight sign-ins run at once: with an eighth running elsewhere, another is
    // refused before any browser opens.
    let journal = account_dir(tmp.path(), id).join("login-attempts.json");
    let mut value: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    value["attempts"].as_array_mut().unwrap().push(entry(
        "live-7",
        Utc::now(),
        json!({"phase":"running"}),
    ));
    write_json_private(&journal, &value).unwrap();
    assert_eq!(
        i.login(id, &LoginAttemptId::generate()).unwrap_err().code(),
        "busy"
    );
    assert!(browser.opened.lock().unwrap().is_empty());
}

#[test]
fn cancelling_an_attempt_before_it_starts_refuses_that_sign_in() {
    let id = IntegrationId::Google;
    let server = Server::new(|_| panic!("a sign-in cancelled in advance reaches nothing"));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let browser = Arc::new(FakeBrowserLauncher::default());
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        browser.clone(),
        Arc::new(|_| {}),
    );
    let attempt = LoginAttemptId::parse("app-attempt-1").unwrap();
    // The app cancels before its sign-in child got to register the attempt.
    assert_eq!(
        other_process(tmp.path(), secrets)
            .cancel_login(id, &attempt)
            .unwrap()
            .state,
        LoginAttemptState::Cancelled
    );
    assert_eq!(i.login(id, &attempt).unwrap_err().code(), "login_cancelled");
    assert!(browser.opened.lock().unwrap().is_empty());
    assert_eq!(
        i.account_state(id).unwrap().status,
        AccountStatus::SignedOut
    );
    // Native calendars have no OAuth attempt to cancel.
    assert_eq!(
        i.cancel_login(IntegrationId::SystemCalendar, &attempt)
            .unwrap_err()
            .code(),
        "login_unsupported"
    );
    for invalid in ["", "a/b", "spaced id", &"x".repeat(65)] {
        assert!(LoginAttemptId::parse(invalid).is_err(), "{invalid:?}");
    }
    assert!(serde_json::from_value::<LoginAttemptId>(json!("../escape")).is_err());
}
