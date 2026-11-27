//! Synthetic authority peers model refresh tokens usable across resource tenants.
use super::*;

const BASE: &str = "Calendars.Read User.Read User.ReadBasic.All";

struct AuthorityFixture {
    tmp: tempfile::TempDir,
    secrets: Arc<FakeSecretStore>,
    signin: Arc<AtomicUsize>,
    grants: Arc<Mutex<String>>,
    server: Server,
    id: IntegrationId,
}

impl AuthorityFixture {
    fn new(id: IntegrationId) -> Self {
        let signin = Arc::new(AtomicUsize::new(1));
        let grants = Arc::new(Mutex::new(BASE.to_owned()));
        let current = signin.clone();
        let scope = grants.clone();
        let server = Server::new(move |request| {
            if request.target.contains("/oauth2/v2.0/authorize?") {
                return reply(200, json!({}));
            }
            if request.target.ends_with("/oauth2/v2.0/token") {
                let authority = request.target.split('/').nth(1).unwrap();
                let resource = match authority {
                    "tenant-a" => "tenant-a",
                    "tenant-b" => "tenant-b",
                    _ if request
                        .form()
                        .get("grant_type")
                        .is_some_and(|grant| grant == "refresh_token") =>
                    {
                        "tenant-a"
                    }
                    _ if current.load(Ordering::SeqCst) == 2 => "tenant-b",
                    _ => "tenant-a",
                };
                // Deliberately honor the requested resource tenant even if the refresh
                // token originally came from the other tenant, as Microsoft permits.
                return reply(
                    200,
                    json!({"access_token":format!("resource-{resource}"),"refresh_token":format!("refresh-{resource}"),"expires_in":3600,"scope":scope.lock().unwrap().clone()}),
                );
            }
            let resource = request
                .authorization
                .as_deref()
                .unwrap_or("")
                .trim_start_matches("Bearer resource-");
            if request.target.starts_with("/api/me?") {
                return reply(
                    200,
                    json!({"id":"same-object","mail":format!("reader@{resource}.example.invalid")}),
                );
            }
            if request.target.starts_with("/api/organization?") {
                return reply(200, json!({"value":[{"id":resource}]}));
            }
            reply(200, json!({"value":[]}))
        });
        Self {
            tmp: tempfile::tempdir().unwrap(),
            secrets: Arc::new(FakeSecretStore::default()),
            signin,
            grants,
            server,
            id,
        }
    }

    fn endpoints(&self, tenant: &str) -> Endpoints {
        let mut endpoints = self.server.endpoints();
        endpoints.authorize = format!("{}/{tenant}/oauth2/v2.0/authorize", self.server.base);
        endpoints.token = format!("{}/{tenant}/oauth2/v2.0/token", self.server.base);
        endpoints
    }

    fn facade(&self, tenant: &str) -> Integrations {
        let mut settings = settings();
        settings.tenant = Some(tenant.into());
        let config = IntegrationsConfig {
            integration: [(self.id.to_string(), settings)].into_iter().collect(),
        };
        let browser: Arc<dyn BrowserLauncher> = Arc::new(CallbackBrowser::default());
        Integrations::new(
            self.tmp.path(),
            authoritative(self.tmp.path(), config),
            self.secrets.clone(),
            browser,
            Arc::new(|_| {}),
        )
        .unwrap()
        .with_endpoints(self.id, self.endpoints(tenant))
    }

    fn expire(&self, account: &AccountRef) {
        let prefix = format!(
            "integration/{}/accounts/{}/tokens/",
            self.id, account.account_id
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

#[test]
fn refresh_reconnect_and_incremental_auth_use_verified_resource_tenant_not_provider_settings() {
    let h = AuthorityFixture::new(IntegrationId::MicrosoftGraph);
    let initial = h.facade("organizations");
    let a = initial
        .add_account(h.id, &LoginAttemptId::generate())
        .unwrap();
    h.signin.store(2, Ordering::SeqCst);
    let b = initial
        .add_account(h.id, &LoginAttemptId::generate())
        .unwrap();
    assert_eq!(a.identity.resource_tenant_id, "tenant-a");
    assert_eq!(b.identity.resource_tenant_id, "tenant-b");
    let a_prefix = format!(
        "integration/{}/accounts/{}/tokens/",
        h.id, a.account.account_id
    );
    let a_secrets: BTreeMap<_, _> = h
        .secrets
        .secrets
        .lock()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.starts_with(&a_prefix))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    h.expire(&b.account);
    let current = h.facade("tenant-a");
    let before = h.server.requests.lock().unwrap().len();
    assert_eq!(
        initial.keep_account_alive(&b.account).unwrap_err().code(),
        "superseded"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
    current.keep_account_alive(&b.account).unwrap();
    let refreshed = h
        .server
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|request| {
            request
                .form()
                .get("grant_type")
                .is_some_and(|grant| grant == "refresh_token")
        })
        .cloned()
        .unwrap();
    assert_eq!(refreshed.target, "/tenant-b/oauth2/v2.0/token");
    assert_eq!(refreshed.form()["refresh_token"], "refresh-tenant-b");
    let b = current
        .reconnect_account(&b.account, b.generation, &LoginAttemptId::generate())
        .unwrap();
    *h.grants.lock().unwrap() = format!("{BASE} Calendars.Read.Shared");
    let b = current
        .grant_account_permission(
            &b.account,
            b.generation,
            AccountPermission::SharedCalendars,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    *h.grants.lock().unwrap() = format!("{BASE} Calendars.Read.Shared Group.Read.All");
    let b = current
        .grant_account_permission(
            &b.account,
            b.generation,
            AccountPermission::Microsoft365Groups,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    assert_eq!(b.identity.resource_tenant_id, "tenant-b");
    let requests = h.server.requests.lock().unwrap();
    assert!(
        requests[before..]
            .iter()
            .filter(|request| request.target.contains("/oauth2/v2.0/"))
            .all(|request| request.target.starts_with("/tenant-b/"))
    );
    assert!(
        requests[before..]
            .iter()
            .filter(|request| request.target.ends_with("/token"))
            .all(|request| request.form()["client_id"] == "synthetic-client")
    );
    drop(requests);
    let after: BTreeMap<_, _> = h
        .secrets
        .secrets
        .lock()
        .unwrap()
        .iter()
        .filter(|(key, _)| key.starts_with(&a_prefix))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(after, a_secrets);
}

#[test]
fn untrusted_or_changed_authority_hosts_cannot_redirect_existing_account_credentials() {
    let h = AuthorityFixture::new(IntegrationId::MicrosoftGraph);
    let service = h.facade("organizations");
    let account = service
        .add_account(h.id, &LoginAttemptId::generate())
        .unwrap();
    let mut endpoints = h.endpoints("organizations");
    endpoints.token = "https://untrusted.example.invalid/organizations/oauth2/v2.0/token".into();
    let service = h.facade("organizations").with_endpoints(h.id, endpoints);
    let before = h.server.requests.lock().unwrap().len();
    let credentials = h.secrets.secrets.lock().unwrap().clone();
    assert_eq!(
        service
            .reconnect_account(
                &account.account,
                account.generation,
                &LoginAttemptId::generate(),
            )
            .unwrap_err()
            .code(),
        "account_authority_invalid"
    );
    assert_eq!(h.server.requests.lock().unwrap().len(), before);
    assert_eq!(*h.secrets.secrets.lock().unwrap(), credentials);
}
