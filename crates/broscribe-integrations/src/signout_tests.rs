//! Synthetic regressions of Google sign-out, explicit revocation at Google and removal of
//! the stored client secret. Every request goes to the local mock; no real provider,
//! network or keychain is used.
use super::*;

const REFRESH: &str = "synthetic-old-refresh";

fn google_state(dir: &std::path::Path) -> AccountState {
    crate::store::read_json_opt(&account_dir(dir, IntegrationId::Google).join(STATE_FILE))
        .unwrap()
        .unwrap_or_default()
}

fn revokes(server: &Server) -> Vec<Request> {
    server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.target == "/revoke")
        .cloned()
        .collect()
}

/// A signed-in Google account under the mock, with its refresh token stored.
fn signed_in(
    server: &Server,
    secrets: Arc<FakeSecretStore>,
    dir: &std::path::Path,
) -> Integrations {
    let id = IntegrationId::Google;
    seed(dir, id, secrets.as_ref(), Utc::now().timestamp() + 3600);
    facade(
        dir,
        id,
        server,
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
}

/// Another process on the same data directory; secret removal needs no configuration.
fn other_process(dir: &std::path::Path, secrets: Arc<dyn SecretStore>) -> Integrations {
    Integrations::new(
        dir,
        IntegrationsConfig::default(),
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
    .unwrap()
}

#[test]
fn revoking_sign_out_sends_the_stored_refresh_token_once_after_the_local_sign_out() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let secrets = Arc::new(FakeSecretStore::default());
    let observed = Arc::new(Mutex::new(None));
    let server = {
        let (root, secrets, observed) = (root.clone(), secrets.clone(), observed.clone());
        Server::new(move |r| {
            if r.target == "/revoke" {
                // Signed out locally, credential gone, before Google is asked.
                *observed.lock().unwrap() = Some((
                    google_state(&root).status,
                    google_credential(&root, secrets.as_ref()).is_some(),
                ));
                return reply(200, json!({}));
            }
            reply(404, json!({}))
        })
    };
    let i = signed_in(&server, secrets.clone(), tmp.path());
    let done = i.logout_and_revoke(IntegrationId::Google).unwrap();
    assert_eq!(done.local, Ok(()));
    assert_eq!(
        done.remote_revocation,
        RevocationOutcome::RevocationRequested
    );
    let sent = revokes(&server);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "POST");
    assert_eq!(
        sent[0].form(),
        BTreeMap::from([("token".to_owned(), REFRESH.to_owned())]),
        "only the refresh token: no access token, client ID or secret"
    );
    assert_eq!(
        *observed.lock().unwrap(),
        Some((AccountStatus::SignedOut, false))
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    // Sign-out keeps the configured client secret.
    assert_eq!(
        secrets
            .get(&client_secret_key(IntegrationId::Google))
            .unwrap()
            .as_deref(),
        Some(GOOGLE_CLIENT_SECRET)
    );
    // Nothing left to revoke: a repeat sends nothing.
    let again = i.logout_and_revoke(IntegrationId::Google).unwrap();
    assert_eq!(again.local, Ok(()));
    assert_eq!(again.remote_revocation, RevocationOutcome::NoStoredGrant);
    assert_eq!(revokes(&server).len(), 1);
}

#[test]
fn plain_sign_out_never_contacts_the_provider() {
    let server = Server::new(|_| reply(200, json!({})));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let i = signed_in(&server, secrets.clone(), tmp.path());
    i.logout(IntegrationId::Google).unwrap();
    assert_eq!(google_state(tmp.path()).status, AccountStatus::SignedOut);
    assert!(google_credential(tmp.path(), secrets.as_ref()).is_none());
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn an_account_needing_sign_in_or_issued_to_another_client_is_still_revoked() {
    for (status, client) in [
        (AccountStatus::ReauthRequired, settings().client_id),
        (
            AccountStatus::Active,
            Some("synthetic-previous-client".to_owned()),
        ),
    ] {
        let server = Server::new(|_| reply(200, json!({})));
        let tmp = tempfile::tempdir().unwrap();
        let secrets = Arc::new(FakeSecretStore::default());
        let i = signed_in(&server, secrets.clone(), tmp.path());
        let mut state = google_state(tmp.path());
        state.status = status;
        state.client_id = client;
        write_json_private(
            &account_dir(tmp.path(), IntegrationId::Google).join(STATE_FILE),
            &state,
        )
        .unwrap();
        let done = i.logout_and_revoke(IntegrationId::Google).unwrap();
        assert_eq!(done.local, Ok(()));
        assert_eq!(
            done.remote_revocation,
            RevocationOutcome::RevocationRequested
        );
        assert_eq!(revokes(&server)[0].form()["token"], REFRESH);
        assert_eq!(google_state(tmp.path()).status, AccountStatus::SignedOut);
    }
}

/// Deleting the stored credential fails; everything else works.
struct FailingDelete(Arc<FakeSecretStore>);
impl SecretStore for FailingDelete {
    fn get(&self, key: &str) -> broscribe_platform::Result<Option<String>> {
        self.0.get(key)
    }
    fn set(&self, key: &str, value: &str) -> broscribe_platform::Result<()> {
        self.0.set(key, value)
    }
    fn delete(&self, key: &str) -> broscribe_platform::Result<()> {
        if key.starts_with(&tokens_secret_key(IntegrationId::Google)) {
            return Err(broscribe_platform::Error::Service {
                code: "synthetic_delete_failed",
                message: "synthetic credential deletion failure".into(),
            });
        }
        self.0.delete(key)
    }
}

#[test]
fn a_failed_local_deletion_still_revokes_and_reports_both() {
    let server = Server::new(|_| reply(200, json!({})));
    let tmp = tempfile::tempdir().unwrap();
    let inner = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        IntegrationId::Google,
        inner.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let i = facade(
        tmp.path(),
        IntegrationId::Google,
        &server,
        Arc::new(FailingDelete(inner.clone())),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    );
    let done = i.logout_and_revoke(IntegrationId::Google).unwrap();
    assert_eq!(done.local.unwrap_err().code, "synthetic_delete_failed");
    assert_eq!(
        done.remote_revocation,
        RevocationOutcome::RevocationRequested
    );
    assert_eq!(revokes(&server)[0].form()["token"], REFRESH);
    // The durable fence was saved before the deletion failed.
    let state = google_state(tmp.path());
    assert_eq!(state.status, AccountStatus::SignedOut);
    assert!(!state.pending_secrets.is_empty());
}

#[test]
fn revocation_replies_and_transport_failures_have_stable_outcomes() {
    let requested = RevocationOutcome::RevocationRequested;
    let failed = |code: &str| RevocationOutcome::Failed { code: code.into() };
    for (status, body, expected) in [
        (200, json!({}), requested.clone()),
        // Google already holds the token invalid: nothing left to revoke.
        (400, json!({"error":"invalid_token"}), requested.clone()),
        (
            400,
            json!({"error":"invalid_request","error_description":"synthetic-detail"}),
            failed("revocation_rejected"),
        ),
        (401, json!({}), failed("revocation_rejected")),
        (429, json!({}), failed("rate_limited")),
        (503, json!({}), failed("provider_unavailable")),
    ] {
        let server = Server::new(move |_| reply(status, body.clone()));
        let tmp = tempfile::tempdir().unwrap();
        let secrets = Arc::new(FakeSecretStore::default());
        let i = signed_in(&server, secrets.clone(), tmp.path());
        let done = i.logout_and_revoke(IntegrationId::Google).unwrap();
        assert_eq!(done.local, Ok(()), "HTTP {status}");
        assert_eq!(done.remote_revocation, expected, "HTTP {status}");
        // One request, never retried.
        assert_eq!(revokes(&server).len(), 1, "HTTP {status}");
        assert!(google_credential(tmp.path(), secrets.as_ref()).is_none());
    }

    // A network failure: nothing listens on the revocation endpoint.
    let closed = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        format!("http://{}/revoke", listener.local_addr().unwrap())
    };
    let server = Server::new(|_| reply(200, json!({})));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let i = signed_in(&server, secrets.clone(), tmp.path()).with_endpoints(
        IntegrationId::Google,
        Endpoints {
            revoke: Some(closed),
            ..server.endpoints()
        },
    );
    let done = i.logout_and_revoke(IntegrationId::Google).unwrap();
    assert_eq!(done.local, Ok(()));
    assert_eq!(done.remote_revocation, failed("transient"));
    assert_eq!(google_state(tmp.path()).status, AccountStatus::SignedOut);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn an_unreadable_trust_store_signs_out_and_reports_the_revocation_as_failed() {
    use broscribe_platform::fakes::FakeTrustStore;
    let server = Server::new(|_| reply(200, json!({})));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let i = signed_in(&server, secrets.clone(), tmp.path())
        .with_trust_store(&FakeTrustStore::default());
    let done = i.logout_and_revoke(IntegrationId::Google).unwrap();
    assert_eq!(done.local, Ok(()));
    assert_eq!(
        done.remote_revocation,
        RevocationOutcome::Failed {
            code: "trust_store_unavailable".into()
        }
    );
    assert!(google_credential(tmp.path(), secrets.as_ref()).is_none());
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn without_a_refresh_token_nothing_is_sent() {
    let server = Server::new(|_| reply(200, json!({})));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let i = signed_in(&server, secrets.clone(), tmp.path());
    // Only an access token: it is never used to revoke.
    let tokens = Tokens {
        access_token: "synthetic-access-only".into(),
        refresh_token: None,
        expires_at: Utc::now().timestamp() + 3600,
        scope: None,
    };
    secrets
        .set(
            &tokens_secret_key(IntegrationId::Google),
            &serde_json::to_string(&tokens).unwrap(),
        )
        .unwrap();
    let done = i.logout_and_revoke(IntegrationId::Google).unwrap();
    assert_eq!(done.local, Ok(()));
    assert_eq!(done.remote_revocation, RevocationOutcome::NoStoredGrant);
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn only_google_offers_revocation() {
    let graph = Endpoints::for_integration(
        IntegrationId::MicrosoftGraph,
        &IntegrationSettings::default(),
    )
    .unwrap();
    assert_eq!(graph.revoke, None);
    let google =
        Endpoints::for_integration(IntegrationId::Google, &IntegrationSettings::default()).unwrap();
    assert_eq!(
        google.revoke.as_deref(),
        Some("https://oauth2.googleapis.com/revoke")
    );
    let tmp = tempfile::tempdir().unwrap();
    let i = other_process(tmp.path(), Arc::new(FakeSecretStore::default()));
    for id in [IntegrationId::MicrosoftGraph, IntegrationId::SystemCalendar] {
        assert_eq!(
            i.logout_and_revoke(id).unwrap_err().code(),
            "revocation_unsupported"
        );
        assert_eq!(
            i.clear_client_secret(id).unwrap_err().code(),
            "service",
            "only Google has a client secret"
        );
    }
    // The wire shape the CLI reports.
    assert_eq!(
        serde_json::to_value(RevocationOutcome::Failed {
            code: "transient".into()
        })
        .unwrap(),
        json!({"outcome":"failed","code":"transient"})
    );
    assert_eq!(
        serde_json::to_value(RevocationOutcome::RevocationRequested).unwrap(),
        json!({"outcome":"revocation_requested"})
    );
}

#[test]
fn the_client_secret_is_removed_only_when_signed_out_and_only_it() {
    let id = IntegrationId::Google;
    let server = Server::new(|_| reply(200, json!({})));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let i = signed_in(&server, secrets.clone(), tmp.path());
    assert_eq!(i.status(id).unwrap().client_secret_stored, Some(true));
    // Signed in, or needing sign-in: refused, nothing removed.
    for status in [AccountStatus::Active, AccountStatus::ReauthRequired] {
        let mut state = google_state(tmp.path());
        state.status = status;
        write_json_private(&account_dir(tmp.path(), id).join(STATE_FILE), &state).unwrap();
        let before = secrets.secrets.lock().unwrap().clone();
        assert_eq!(
            i.clear_client_secret(id).unwrap_err().code(),
            "sign_out_first"
        );
        assert_eq!(*secrets.secrets.lock().unwrap(), before);
    }
    i.logout(id).unwrap();
    // An unrelated stored credential stays untouched.
    secrets
        .set(&tokens_secret_key(id), "synthetic-unrelated")
        .unwrap();
    let mut expected = secrets.secrets.lock().unwrap().clone();
    expected.remove(&client_secret_key(id));
    assert!(i.clear_client_secret(id).unwrap());
    assert_eq!(*secrets.secrets.lock().unwrap(), expected);
    // Idempotent.
    assert!(!i.clear_client_secret(id).unwrap());
    assert_eq!(*secrets.secrets.lock().unwrap(), expected);

    let status = i.status(id).unwrap();
    assert_eq!(status.client_secret_stored, Some(false));
    let wire = serde_json::to_value(&status).unwrap();
    assert_eq!(wire["client_secret_stored"], false);
    let back: IntegrationStatus = serde_json::from_value(wire).unwrap();
    assert_eq!(back.client_secret_stored, Some(false));
    // Older payloads without the field still parse.
    let mut legacy = serde_json::to_value(&status).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("client_secret_stored");
    let legacy: IntegrationStatus = serde_json::from_value(legacy).unwrap();
    assert_eq!(legacy.client_secret_stored, None);
    // Other integrations carry no client secret field.
    let graph = serde_json::to_value(i.status(IntegrationId::MicrosoftGraph).unwrap()).unwrap();
    assert!(graph.get("client_secret_stored").is_none());
    assert!(server.requests.lock().unwrap().is_empty());
}

#[test]
fn sign_in_without_a_stored_client_secret_fails_before_the_browser_opens() {
    let id = IntegrationId::Google;
    let server = Server::new(|_| reply(200, google_token()));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let browser = Arc::new(CallbackBrowser::default());
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        browser.clone(),
        Arc::new(|_| panic!("no notification")),
    );
    assert!(i.clear_client_secret(id).unwrap());
    let attempt = LoginAttemptId::generate();
    assert_eq!(
        i.login(id, &attempt).unwrap_err().code(),
        "client_secret_missing"
    );
    assert_eq!(browser.opens.load(Ordering::SeqCst), 0);
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(matches!(
        i.login_attempt(id, &attempt).unwrap().state,
        LoginAttemptState::Failed { code, .. } if code == "client_secret_missing"
    ));
    assert_eq!(google_state(tmp.path()).status, AccountStatus::SignedOut);
}

#[test]
fn a_client_secret_removed_during_a_sign_in_fails_its_commit_and_stores_nothing() {
    let id = IntegrationId::Google;
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let other = Arc::new(other_process(tmp.path(), secrets.clone()));
    let removed = Arc::new(AtomicBool::new(false));
    let server = {
        let (other, removed) = (other.clone(), removed.clone());
        // The identity read is the last step before the commit takes the lock.
        Server::new(move |r| {
            if r.target.starts_with("/api/calendars/primary?") {
                removed.store(other.clear_client_secret(id).unwrap(), Ordering::SeqCst);
            }
            google_identity_fixture(r).unwrap_or_else(|| reply(200, google_token()))
        })
    };
    let browser = Arc::new(CallbackBrowser::default());
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        browser.clone(),
        Arc::new(|_| panic!("no notification")),
    );
    let before = google_state(tmp.path());
    let attempt = LoginAttemptId::generate();
    assert_eq!(
        i.login(id, &attempt).unwrap_err().code(),
        "client_secret_missing"
    );
    assert!(removed.load(Ordering::SeqCst));
    assert_eq!(browser.opens.load(Ordering::SeqCst), 1);
    // The new tokens were dropped: no account, credential or staged version, and nothing
    // was sent to the revocation endpoint.
    assert_eq!(google_state(tmp.path()), before);
    assert!(secrets.secrets.lock().unwrap().is_empty());
    assert!(revokes(&server).is_empty());
    assert!(matches!(
        i.login_attempt(id, &attempt).unwrap().state,
        LoginAttemptState::Failed { code, .. } if code == "client_secret_missing"
    ));
}
