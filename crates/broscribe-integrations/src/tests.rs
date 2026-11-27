//! All data is synthetic. Local TCP peers exercise the real HTTP/OAuth boundary.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;

use broscribe_platform::{
    BrowserLauncher, SecretStore,
    fakes::{FakeBrowserLauncher, FakeSecretStore},
};
use chrono::{Duration as Delta, Utc};
use serde_json::{Value, json};

use crate::encode::{build_query, parse_query};
use crate::http::Http;
use crate::oauth::Tokens;
use crate::store::{CALENDAR_FILE, STATE_FILE, write_json_private};
use crate::*;

#[derive(Clone, Debug)]
struct Request {
    method: String,
    target: String,
    body: String,
    authorization: Option<String>,
    host: String,
}
impl Request {
    fn form(&self) -> BTreeMap<String, String> {
        parse_query(&self.body).into_iter().collect()
    }
    fn query(&self) -> BTreeMap<String, String> {
        parse_query(self.target.split_once('?').map(|(_, q)| q).unwrap_or(""))
            .into_iter()
            .collect()
    }
}
struct Response {
    status: u16,
    body: Vec<u8>,
}
fn reply(status: u16, body: Value) -> Response {
    Response {
        status,
        body: serde_json::to_vec(&body).unwrap(),
    }
}

struct Server {
    base: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    requests: Arc<Mutex<Vec<Request>>>,
}
impl Server {
    fn new(handler: impl Fn(&Request) -> Response + Send + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let thread = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let r = read_request(&mut stream);
                        recorded.lock().unwrap().push(r.clone());
                        let response = handler(&r);
                        write!(stream,"HTTP/1.1 {} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",response.status,response.body.len()).unwrap();
                        stream.write_all(&response.body).unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("mock listener: {e}"),
                }
            }
        });
        Self {
            base,
            stop,
            thread: Some(thread),
            requests,
        }
    }
    fn endpoints(&self) -> Endpoints {
        Endpoints {
            authorize: format!("{}/authorize", self.base),
            token: format!("{}/token", self.base),
            api: format!("{}/api", self.base),
            directory_api: format!("{}/directory", self.base),
            revoke: Some(format!("{}/revoke", self.base)),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}
fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let header = String::from_utf8(bytes).unwrap();
    let mut first = header.lines().next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let target = first.next().unwrap().to_owned();
    let mut length = 0;
    let mut authorization = None;
    let mut host = String::new();
    for line in header.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("Content-Length") {
                length = v.trim().parse().unwrap();
            }
            if k.eq_ignore_ascii_case("Authorization") {
                authorization = Some(v.trim().into());
            }
            if k.eq_ignore_ascii_case("Host") {
                host = v.trim().into();
            }
        }
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    Request {
        method,
        target,
        body: String::from_utf8(body).unwrap(),
        authorization,
        host,
    }
}

#[derive(Default)]
struct CallbackBrowser {
    bad_state_status: Arc<AtomicUsize>,
    opens: AtomicUsize,
}
impl BrowserLauncher for CallbackBrowser {
    fn open(&self, url: &str) -> broscribe_platform::Result<()> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let url = url.to_owned();
        let status = self.bad_state_status.clone();
        thread::spawn(move || {
            let query: BTreeMap<_, _> = parse_query(url.split_once('?').unwrap().1)
                .into_iter()
                .collect();
            Http::new().get(&url, None, &[], &[]).unwrap();
            let wrong = format!(
                "{}?{}",
                query["redirect_uri"],
                build_query(&[("state", "wrong"), ("code", "ignored")])
            );
            status.store(
                Http::new().get(&wrong, None, &[], &[]).unwrap().status as usize,
                Ordering::SeqCst,
            );
            let valid = format!(
                "{}?{}",
                query["redirect_uri"],
                build_query(&[("state", &query["state"]), ("code", "synthetic-code")])
            );
            Http::new().get(&valid, None, &[], &[]).unwrap();
        });
        Ok(())
    }
}

fn settings() -> IntegrationSettings {
    IntegrationSettings {
        client_id: Some("synthetic-client".into()),
        workspace: true,
        ..Default::default()
    }
}
/// Writes `config` as the authoritative `config.toml` (sign-in admission and commit check
/// the client against it, not the facade's copy) and returns it for the facade.
fn authoritative(dir: &std::path::Path, config: IntegrationsConfig) -> IntegrationsConfig {
    std::fs::write(
        broscribe_core::config::Config::path(dir),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();
    config
}
fn facade(
    dir: &std::path::Path,
    id: IntegrationId,
    server: &Server,
    secrets: Arc<dyn SecretStore>,
    browser: Arc<dyn BrowserLauncher>,
    notify: Arc<dyn Fn(ReauthNotification) + Send + Sync>,
) -> Integrations {
    let config = IntegrationsConfig {
        integration: [(id.to_string(), settings())].into_iter().collect(),
    };
    // Google desktop clients sign in only with their stored client secret.
    if id == IntegrationId::Google {
        secrets
            .set(&client_secret_key(id), GOOGLE_CLIENT_SECRET)
            .unwrap();
    }
    Integrations::new(dir, authoritative(dir, config), secrets, browser, notify)
        .unwrap()
        .with_endpoints(id, server.endpoints())
}
const GOOGLE_CLIENT_SECRET: &str = "synthetic-desktop-secret";
fn seed(dir: &std::path::Path, id: IntegrationId, secrets: &dyn SecretStore, expiry: i64) {
    let tokens = Tokens {
        access_token: "synthetic-old-access".into(),
        refresh_token: Some("synthetic-old-refresh".into()),
        expires_at: expiry,
        scope: None,
    };
    secrets
        .set(
            &tokens_secret_key(id),
            &serde_json::to_string(&tokens).unwrap(),
        )
        .unwrap();
    let google = id == IntegrationId::Google;
    write_json_private(
        &account_dir(dir, id).join(STATE_FILE),
        &AccountState {
            generation: 1,
            status: AccountStatus::Active,
            client_id: settings().client_id,
            scopes: if google { google_grant() } else { Vec::new() },
            native_calendar_ids: if google {
                vec![GOOGLE_CALENDAR.into()]
            } else {
                Vec::new()
            },
            ..Default::default()
        },
    )
    .unwrap();
}
fn token() -> Value {
    json!({"access_token":"synthetic-new-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600,"scope":"Calendars.Read User.Read User.ReadBasic.All"})
}

/// The synthetic Google account's primary calendar ID (its address).
const GOOGLE_CALENDAR: &str = "reader@example.invalid";
fn google_grant() -> Vec<String> {
    vec![
        crate::config::GOOGLE_CALENDAR_READONLY.into(),
        crate::config::GOOGLE_DIRECTORY_READONLY.into(),
    ]
}
fn google_token() -> Value {
    json!({"access_token":"synthetic-new-access","refresh_token":"synthetic-rotated-refresh","expires_in":3600,"scope":google_grant().join(" ")})
}
/// Primary calendar identity and calendar list of the synthetic Google account.
fn google_identity_fixture(request: &Request) -> Option<Response> {
    if request.target.starts_with("/api/calendars/primary?") {
        return Some(reply(
            200,
            json!({"id":GOOGLE_CALENDAR,"summary":"Reader Example"}),
        ));
    }
    if request.target.starts_with("/api/users/me/calendarList?") {
        return Some(reply(
            200,
            json!({"items":[
                {"id":GOOGLE_CALENDAR,"summary":"Reader Example","primary":true,"accessRole":"owner"},
                {"id":"busy@example.invalid","summary":"Free/busy only","accessRole":"freeBusyReader"}
            ]}),
        ));
    }
    None
}
/// The committed Google credential: the versioned pointer, else the legacy slot.
fn google_credential(dir: &std::path::Path, secrets: &dyn SecretStore) -> Option<String> {
    let state: AccountState =
        crate::store::read_json_opt(&account_dir(dir, IntegrationId::Google).join(STATE_FILE))
            .unwrap()
            .unwrap_or_default();
    let key = state
        .secret_key
        .unwrap_or_else(|| tokens_secret_key(IntegrationId::Google));
    secrets.get(&key).unwrap()
}
fn select_google_calendar(i: &Integrations) {
    let catalog = i.google_calendars().unwrap();
    assert_eq!(
        catalog
            .calendars
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        [GOOGLE_CALENDAR],
        "only calendars whose events may be read are offered"
    );
    i.configure_google_calendars(catalog.authority, &[GOOGLE_CALENDAR.into()])
        .unwrap();
}

/// The provider-wide copy a recording snapshot may take, admitted as the daemon admits it:
/// only events of `id` stamped with its live calendar authority (Google: the account and
/// selection pair; native calendars: the opt-in generation).
pub(crate) fn served_copy(
    service: &Integrations,
    dir: &std::path::Path,
    id: IntegrationId,
) -> Result<CalendarCache> {
    let read = || -> Result<CalendarCache> {
        Ok(
            crate::store::read_json_opt(&account_dir(dir, id).join(CALENDAR_FILE))?
                .unwrap_or_default(),
        )
    };
    let mut cache = if id == IntegrationId::Google {
        let Some(live) = service.google_authority()? else {
            return Ok(CalendarCache::default());
        };
        let mut cache = read()?;
        cache
            .events
            .retain(|event| event.integration == id && event.google == Some(live));
        cache
    } else {
        let Some(generation) = service.calendar_generation(id)? else {
            return Ok(CalendarCache::default());
        };
        let mut cache = read()?;
        cache.events.retain(|event| {
            event.integration == id && event.account_generation == Some(generation)
        });
        cache
    };
    cache.prune(Utc::now());
    Ok(cache)
}

/// The photo of one person the public batch lookup would let the app display.
pub(crate) fn cached_photo(dir: &std::path::Path, person_id: &str) -> Option<std::path::PathBuf> {
    cached_photos(dir, [((), person_id)], |path| Some(path.to_owned()))
        .pop()
        .map(|(_, path)| path)
}

fn microsoft_identity_fixture(request: &Request) -> Option<Response> {
    if request.target.starts_with("/api/me?") {
        return Some(reply(
            200,
            json!({"id":"synthetic-reader","mail":"reader@example.invalid"}),
        ));
    }
    if request.target.starts_with("/api/organization?") {
        return Some(reply(
            200,
            json!({"value":[{"id":"synthetic-tenant","displayName":"Example Organisation"}]}),
        ));
    }
    None
}

#[test]
fn google_login_checks_pkce_and_state_then_preserves_calendar_when_directory_denied() {
    let server = Server::new(|r| {
        if r.target.starts_with("/token") {
            return reply(200, google_token());
        }
        if r.target.starts_with("/directory") {
            return reply(403, json!({}));
        }
        if let Some(response) = google_identity_fixture(r) {
            return response;
        }
        if r.target.starts_with("/api") {
            if r.query().contains_key("pageToken") {
                return reply(200, json!({"items":[]}));
            }
            let start = Utc::now().to_rfc3339();
            let end = (Utc::now() + Delta::hours(1)).to_rfc3339();
            return reply(
                200,
                json!({"items":[{"id":"event-a","summary":"Invented planning","start":{"dateTime":start},"end":{"dateTime":end},"hangoutLink":"https://meet.google.com/fake-room","attendees":[{"displayName":"Mira Example","email":"mira@example.invalid"}]}],"nextPageToken":"second-page"}),
            );
        }
        reply(200, json!({}))
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let browser = Arc::new(CallbackBrowser::default());
    let config = IntegrationsConfig {
        integration: [(
            "google".into(),
            IntegrationSettings {
                workspace: true,
                ..settings()
            },
        )]
        .into_iter()
        .collect(),
    };
    let i = Integrations::new(
        tmp.path(),
        authoritative(tmp.path(), config),
        secrets.clone(),
        browser.clone(),
        Arc::new(|_| panic!("no notification expected")),
    )
    .unwrap()
    .with_endpoints(IntegrationId::Google, server.endpoints());
    i.set_client_secret(IntegrationId::Google, "synthetic-desktop-secret")
        .unwrap();
    i.login(IntegrationId::Google, &LoginAttemptId::generate())
        .unwrap();
    assert_eq!(browser.bad_state_status.load(Ordering::SeqCst), 400);
    let requests = server.requests.lock().unwrap().clone();
    let auth = requests
        .iter()
        .find(|r| r.target.starts_with("/authorize"))
        .unwrap()
        .query();
    let exchange = requests
        .iter()
        .find(|r| r.target == "/token")
        .unwrap()
        .form();
    assert_eq!(auth["code_challenge_method"], "S256");
    assert_eq!(auth["state"].len(), 22);
    assert_eq!(
        crate::pkce::challenge_for(&exchange["code_verifier"]),
        auth["code_challenge"]
    );
    assert_eq!(exchange["client_secret"], "synthetic-desktop-secret");
    assert_eq!(exchange["redirect_uri"], auth["redirect_uri"]);
    assert_eq!(exchange["grant_type"], "authorization_code");
    // The identity comes from the calendar scope alone; no calendar is selected for the user.
    let signed_in = i.status(IntegrationId::Google).unwrap().state;
    assert_eq!(
        signed_in.account.as_ref().and_then(|a| a.email.as_deref()),
        Some(GOOGLE_CALENDAR)
    );
    assert_eq!(signed_in.scopes, google_grant());
    assert!(signed_in.native_calendar_ids.is_empty());
    let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
    assert!(
        i.sync_calendar(IntegrationId::Google, window.0, window.1)
            .unwrap()
            .events
            .is_empty()
    );
    assert!(
        !server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.target.contains("/events?")),
        "nothing is read before an explicit selection"
    );
    select_google_calendar(&i);
    let authority = i.google_authority().unwrap();
    assert_eq!(
        authority.map(|a| a.generation),
        Some(i.account_state(IntegrationId::Google).unwrap().generation),
        "selecting calendars keeps the account generation"
    );
    let cache = i
        .sync_calendar(IntegrationId::Google, window.0, window.1)
        .unwrap();
    assert_eq!(cache.events.len(), 1);
    assert_eq!(cache.events[0].app.as_deref(), Some("meet"));
    assert_eq!(cache.events[0].google, authority);
    assert_eq!(cache.events[0].account_generation, None);
    let events = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.target.contains("/events?"))
        .unwrap()
        .clone();
    assert!(
        events
            .target
            .starts_with("/api/calendars/reader%40example.invalid/events?")
    );
    assert!(events.query()["fields"].starts_with("items(id,status,summary,start,end"));
    assert!(!events.query()["fields"].contains("description"));
    assert_eq!(
        i.directory(IntegrationId::Google, "Mira")
            .unwrap_err()
            .code(),
        "forbidden"
    );
    let status = i.status(IntegrationId::Google).unwrap();
    assert_eq!(status.state.capabilities.calendar, Capability::Available);
    assert_eq!(status.state.capabilities.directory, Capability::Denied);
    assert!(!status.admin_controlled_scopes.is_empty());
    assert_eq!(
        served_copy(&i, tmp.path(), IntegrationId::Google)
            .unwrap()
            .events
            .len(),
        1
    );
    assert_eq!(browser.opens.load(Ordering::SeqCst), 1);
    let cache_path = account_dir(tmp.path(), IntegrationId::Google).join("calendar.json");
    assert!(cache_path.exists());
    i.logout(IntegrationId::Google).unwrap();
    assert!(!cache_path.exists());
    assert!(
        secrets
            .get(&tokens_secret_key(IntegrationId::Google))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        secrets
            .get(&client_secret_key(IntegrationId::Google))
            .unwrap()
            .as_deref(),
        Some("synthetic-desktop-secret")
    );
    i.login(IntegrationId::Google, &LoginAttemptId::generate())
        .unwrap();
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|r| r.target == "/token")
            .unwrap()
            .form()["client_secret"],
        "synthetic-desktop-secret"
    );
}

#[test]
fn failed_login_preserves_working_account_until_successful_replacement() {
    let exchanges = Arc::new(AtomicUsize::new(0));
    let count = exchanges.clone();
    let server = Server::new(move |r| {
        if r.target == "/token" {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                return reply(400, json!({"error":"access_denied"}));
            }
            return reply(200, google_token());
        }
        if let Some(response) = google_identity_fixture(r) {
            return response;
        }
        reply(200, json!({"value":[]}))
    });
    let tmp = tempfile::tempdir().unwrap();
    let id = IntegrationId::Google;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| panic!("no reauth notification expected")),
    );
    let dir = account_dir(tmp.path(), id);
    let old = AccountState {
        generation: 1,
        status: AccountStatus::Active,
        account: Some(AccountInfo {
            name: Some("Mira Example".into()),
            email: None,
        }),
        capabilities: Capabilities {
            calendar: Capability::Available,
            directory: Capability::Denied,
        },
        last_sync: Some(Utc::now()),
        // A working account is bound to the configured client it signed in with.
        client_id: settings().client_id,
        scopes: google_grant(),
        native_calendar_ids: vec![GOOGLE_CALENDAR.into()],
        ..Default::default()
    };
    write_json_private(&dir.join(STATE_FILE), &old).unwrap();
    write_json_private(&dir.join("calendar.json"), &json!({"events":[]})).unwrap();
    crate::store::write_private(&dir.join("photos/p-123abc.img"), b"old-photo").unwrap();
    let credential = secrets.get(&tokens_secret_key(id)).unwrap();
    assert_eq!(
        i.login(id, &LoginAttemptId::generate()).unwrap_err().code(),
        "oauth"
    );
    let after = i.status(id).unwrap().state;
    let mut expected = old;
    expected.generation = after.generation;
    assert_eq!(after, expected);
    assert_eq!(secrets.get(&tokens_secret_key(id)).unwrap(), credential);
    assert!(dir.join("calendar.json").exists());
    assert!(dir.join("photos/p-123abc.img").exists());
    i.directory(id, "Mira").unwrap();
    let replaced = i.login(id, &LoginAttemptId::generate()).unwrap().state;
    assert_eq!(replaced.status, AccountStatus::Active);
    assert_eq!(
        replaced.account.and_then(|a| a.email).as_deref(),
        Some(GOOGLE_CALENDAR)
    );
    // Another verified account than before: its selection is not inherited.
    assert!(replaced.native_calendar_ids.is_empty());
    assert!(replaced.secret_key.is_some());
    assert!(replaced.pending_secrets.is_empty());
    assert_eq!(replaced.capabilities.calendar, Capability::Unknown);
    assert!(replaced.last_sync.is_none());
    assert!(!dir.join("calendar.json").exists());
    assert!(!dir.join("photos/p-123abc.img").exists());
    assert_ne!(secrets.get(&tokens_secret_key(id)).unwrap(), credential);
}

#[test]
fn removed_provider_settings_do_not_disable_supported_integrations() {
    let server = Server::new(|_| reply(200, json!({"value":[], "items":[]})));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    for id in [IntegrationId::Google, IntegrationId::MicrosoftGraph] {
        seed(
            tmp.path(),
            id,
            secrets.as_ref(),
            Utc::now().timestamp() + 3600,
        );
    }
    let config = IntegrationsConfig {
        integration: [
            ("google".into(), settings()),
            ("microsoft-graph".into(), settings()),
            (
                "teams-session".into(),
                IntegrationSettings {
                    tenant: Some("invalid/tenant".into()),
                    ..settings()
                },
            ),
        ]
        .into_iter()
        .collect(),
    };
    let i = Integrations::new(
        tmp.path(),
        authoritative(tmp.path(), config),
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
    .unwrap()
    .with_endpoints(IntegrationId::Google, server.endpoints())
    .with_endpoints(IntegrationId::MicrosoftGraph, server.endpoints());
    assert_eq!(i.statuses().unwrap().len(), IntegrationId::ALL.len());
    i.sync_calendar(
        IntegrationId::Google,
        Utc::now(),
        Utc::now() + Delta::days(1),
    )
    .unwrap();
    // Microsoft sync is account-qualified. A removed provider's malformed
    // settings must not replace that contract with a configuration error.
    assert_eq!(
        i.sync_calendar(
            IntegrationId::MicrosoftGraph,
            Utc::now(),
            Utc::now() + Delta::days(1),
        )
        .unwrap_err()
        .code(),
        "account_required"
    );
    assert!(IntegrationId::parse("teams-session").is_err());
    assert!(IntegrationId::parse("outlook-calendar").is_err());
}

#[test]
fn api_unauthorized_refreshes_and_retries_before_requesting_sign_in() {
    for operation in ["calendar", "directory", "photo"] {
        for outcome in ["success", "unauthorized", "invalid_grant", "transient"] {
            let server = Server::new(move |r| {
                if r.target == "/token" {
                    return match outcome {
                        "invalid_grant" => reply(400, json!({"error":"invalid_grant"})),
                        "transient" => reply(503, json!({})),
                        _ => reply(200, token()),
                    };
                }
                if r.authorization.as_deref() == Some("Bearer synthetic-old-access")
                    || outcome == "unauthorized"
                {
                    return reply(401, json!({}));
                }
                if operation == "photo" {
                    return Response {
                        status: 200,
                        body: b"new-photo".to_vec(),
                    };
                }
                reply(200, json!({"value":[]}))
            });
            let tmp = tempfile::tempdir().unwrap();
            let id = IntegrationId::Google;
            let secrets = Arc::new(FakeSecretStore::default());
            seed(
                tmp.path(),
                id,
                secrets.as_ref(),
                Utc::now().timestamp() + 3600,
            );
            let notifications = Arc::new(AtomicUsize::new(0));
            let count = notifications.clone();
            let i = facade(
                tmp.path(),
                id,
                &server,
                secrets,
                Arc::new(FakeBrowserLauncher::default()),
                Arc::new(move |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                }),
            );
            let remote = DirectoryPerson {
                remote_id: "remote".into(),
                name: None,
                email: None,
                photo_url: Some(format!("{}/directory/photo", server.base)),
                source: None,
            };
            let result = match operation {
                "calendar" => i
                    .sync_calendar(id, Utc::now(), Utc::now() + Delta::days(1))
                    .map(|_| ()),
                "directory" => i.directory(id, "Mira").map(|_| ()),
                _ => i
                    .cache_photo(id, "p-123abc", &remote, &|_| true)
                    .map(|photo| {
                        assert_eq!(std::fs::read(photo.unwrap()).unwrap(), b"new-photo");
                    }),
            };
            if outcome == "success" {
                result.unwrap();
            } else {
                assert_eq!(
                    result.unwrap_err().code(),
                    if outcome == "transient" {
                        "transient"
                    } else {
                        "reauth_required"
                    }
                );
            }
            let needs_sign_in = matches!(outcome, "unauthorized" | "invalid_grant");
            assert_eq!(
                i.status(id).unwrap().state.status,
                if needs_sign_in {
                    AccountStatus::ReauthRequired
                } else {
                    AccountStatus::Active
                }
            );
            assert_eq!(
                notifications.load(Ordering::SeqCst),
                usize::from(needs_sign_in)
            );
            let requests = server.requests.lock().unwrap();
            assert_eq!(
                requests[0].authorization.as_deref(),
                Some("Bearer synthetic-old-access")
            );
            assert_eq!(requests[1].target, "/token");
            assert_eq!(
                requests.len(),
                if outcome == "invalid_grant" { 2 } else { 3 }
            );
            if matches!(outcome, "success" | "unauthorized") {
                assert_eq!(
                    requests[2].authorization.as_deref(),
                    Some("Bearer synthetic-new-access")
                );
            }
        }
    }
}

#[test]
fn google_rate_limit_403_is_transient_for_calendar_directory_and_photo() {
    let server = Server::new(|_| {
        reply(
            403,
            json!({
                "error": {"code": 403, "errors": [{"reason": "rateLimitExceeded", "domain": "usageLimits"}]}
            }),
        )
    });
    let tmp = tempfile::tempdir().unwrap();
    let id = IntegrationId::Google;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let config = IntegrationsConfig {
        integration: [(
            id.to_string(),
            IntegrationSettings {
                workspace: true,
                ..settings()
            },
        )]
        .into_iter()
        .collect(),
    };
    let i = Integrations::new(
        tmp.path(),
        authoritative(tmp.path(), config),
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| panic!("rate limiting does not require sign-in")),
    )
    .unwrap()
    .with_endpoints(id, server.endpoints());
    assert_eq!(
        i.sync_calendar(id, Utc::now(), Utc::now() + Delta::days(1))
            .unwrap_err()
            .code(),
        "transient"
    );
    assert_eq!(i.directory(id, "Mira").unwrap_err().code(), "transient");
    let remote = DirectoryPerson {
        remote_id: "people/synthetic".into(),
        name: None,
        email: None,
        photo_url: Some(format!("{}/directory/photo", server.base)),
        source: None,
    };
    assert_eq!(
        i.cache_photo(id, "p-123abc", &remote, &|_| true)
            .unwrap_err()
            .code(),
        "transient"
    );
    let status = i.status(id).unwrap().state;
    assert_eq!(status.status, AccountStatus::Active);
    assert_eq!(status.capabilities.calendar, Capability::Error);
    assert_eq!(status.capabilities.directory, Capability::Error);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|r| r.target != "/token"));
}

#[test]
fn informational_reauth_callback_leaves_notification_for_daemon_delivery() {
    let server = Server::new(|_| reply(400, json!({"error":"invalid_grant"})));
    let tmp = tempfile::tempdir().unwrap();
    let id = IntegrationId::Google;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(tmp.path(), id, secrets.as_ref(), 0);
    let printed = Arc::new(AtomicUsize::new(0));
    let count = printed.clone();
    let cli = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .with_reauth_notification_delivery(false);
    assert_eq!(cli.keep_alive(id).unwrap_err().code(), "reauth_required");
    assert_eq!(printed.load(Ordering::SeqCst), 1);
    assert!(!cli.status(id).unwrap().state.reauth_notified);
    let delivered = Arc::new(AtomicUsize::new(0));
    let count = delivered.clone();
    let daemon = facade(
        tmp.path(),
        id,
        &server,
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }),
    );
    daemon.keep_alive(id).unwrap();
    daemon.keep_alive(id).unwrap();
    assert_eq!(delivered.load(Ordering::SeqCst), 1);
    assert!(daemon.status(id).unwrap().state.reauth_notified);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[test]
fn simultaneous_expired_requests_single_flight_rotation_across_facades() {
    let refreshes = Arc::new(AtomicUsize::new(0));
    let count = refreshes.clone();
    let server = Server::new(move |r| {
        if r.target == "/token" {
            count.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(50));
            return reply(200, token());
        }
        reply(200, json!({"value":[]}))
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let id = IntegrationId::Google;
    seed(tmp.path(), id, secrets.as_ref(), 0);
    let a = Arc::new(facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    ));
    let b = Arc::new(facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    ));
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|n| {
            let i = if n % 2 == 0 { a.clone() } else { b.clone() };
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                i.directory(id, "Mira").unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    let stored: Tokens =
        serde_json::from_str(&google_credential(tmp.path(), secrets.as_ref()).unwrap()).unwrap();
    // The rotation is a new committed credential version; the legacy slot is gone.
    assert!(secrets.get(&tokens_secret_key(id)).unwrap().is_none());
    assert_eq!(
        stored.refresh_token.as_deref(),
        Some("synthetic-rotated-refresh")
    );
    let reqs = server.requests.lock().unwrap();
    let refresh = reqs.iter().find(|r| r.target == "/token").unwrap();
    assert_eq!(refresh.method, "POST");
    assert_eq!(refresh.form()["refresh_token"], "synthetic-old-refresh");
    assert!(
        reqs.iter()
            .filter(|r| r.target.starts_with("/directory"))
            .all(|r| r.authorization.as_deref() == Some("Bearer synthetic-new-access"))
    );
}

#[test]
fn delayed_refresh_after_logout_never_restores_credentials() {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = Server::new(move |_| {
        entered_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        reply(200, token())
    });
    let tmp = tempfile::tempdir().unwrap();
    let id = IntegrationId::Google;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(tmp.path(), id, secrets.as_ref(), 0);
    let i = Arc::new(facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    ));
    let worker = {
        let i = i.clone();
        thread::spawn(move || i.keep_alive(id))
    };
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    i.logout(id).unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
    assert!(secrets.get(&tokens_secret_key(id)).unwrap().is_none());
    assert_eq!(i.status(id).unwrap().state.status, AccountStatus::SignedOut);
}

#[test]
fn interaction_required_notifies_once_without_browser_and_transient_refresh_retries_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let server = Server::new(move |_| {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            reply(503, json!({}))
        } else {
            reply(
                400,
                json!({"error":"interaction_required","error_description":"must not be surfaced"}),
            )
        }
    });
    let tmp = tempfile::tempdir().unwrap();
    let id = IntegrationId::Google;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(tmp.path(), id, secrets.as_ref(), 0);
    let notifications = Arc::new(AtomicUsize::new(0));
    let n = notifications.clone();
    let browser = Arc::new(FakeBrowserLauncher::default());
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets,
        browser.clone(),
        Arc::new(move |_| {
            n.fetch_add(1, Ordering::SeqCst);
        }),
    );
    assert_eq!(i.keep_alive(id).unwrap_err().code(), "reauth_required");
    i.keep_alive(id).unwrap();
    assert_eq!(
        i.directory(id, "Mira").unwrap_err().code(),
        "reauth_required"
    );
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(browser.opened.lock().unwrap().is_empty());
    assert_eq!(
        i.status(id).unwrap().state.status,
        AccountStatus::ReauthRequired
    );
}

#[test]
fn graph_pagination_and_directory_photos_only_for_existing_people() {
    let server = Server::new(|r| {
        if r.target == "/token" {
            return reply(200, token());
        }
        if r.target.starts_with("/api/me?") {
            return reply(200, json!({"id":"reader","mail":"reader@example.invalid"}));
        }
        if r.target.starts_with("/api/organization?") {
            return reply(200, json!({"value":[{"id":"tenant-a"}]}));
        }
        if r.target.starts_with("/api/me/calendars?") {
            return reply(
                200,
                json!({"value":[{"id":"work","owner":{"address":"reader@example.invalid"}}]}),
            );
        }
        if r.target.starts_with("/api/me/calendars/work?") {
            assert_eq!(r.query()["$select"], "id");
            return reply(200, json!({"id":"work"}));
        }
        if r.target.starts_with("/authorize") {
            return reply(200, json!({}));
        }
        if r.target.contains("photo/$value") {
            return Response {
                status: 200,
                body: b"synthetic-photo".to_vec(),
            };
        }
        if r.target.starts_with("/directory") {
            return reply(
                200,
                json!({"value":[{"id":"remote-mira","displayName":"Mira Example","mail":"mira@example.invalid"}]}),
            );
        }
        if r.target.starts_with("/api/second") {
            return reply(200, json!({"value":[]}));
        }
        let now = Utc::now();
        reply(
            200,
            json!({"value":[{"id":"graph-event","subject":"Synthetic planning","start":{"dateTime":now.format("%Y-%m-%dT%H:%M:%S").to_string(),"timeZone":"UTC"},"end":{"dateTime":(now+Delta::hours(1)).to_rfc3339()},"onlineMeeting":{"joinUrl":"https://teams.microsoft.com/l/meetup-join/synthetic"},"attendees":[{"emailAddress":{"name":"Mira Example","address":"mira@example.invalid"}}]}],"@odata.nextLink":format!("http://{}/api/second",r.host)}),
        )
    });
    let tmp = tempfile::tempdir().unwrap();
    let id = IntegrationId::MicrosoftGraph;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets,
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    );
    let account = i.add_account(id, &LoginAttemptId::generate()).unwrap();
    i.configure_calendars(
        &account.account,
        account.generation,
        account.selection_generation,
        &[CalendarTarget::Own {
            calendar_id: "work".into(),
        }],
    )
    .unwrap();
    i.sync_account(
        &account.account,
        Utc::now() - Delta::days(1),
        Utc::now() + Delta::days(1),
    )
    .unwrap();
    let calendar = i.cached_account_calendar(&account.account).unwrap();
    assert_eq!(
        calendar.events[0].attendees[0].name.as_deref(),
        Some("Mira Example")
    );
    assert_eq!(calendar.events[0].app.as_deref(), Some("teams"));
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.target == "/api/second")
    );
    let person = i
        .account_directory(&account.account, "Mira")
        .unwrap()
        .people
        .remove(0);
    assert!(
        i.cache_account_photo(
            &account.account,
            account.generation,
            "p-a1b2c3",
            &person,
            &|_| false
        )
        .unwrap()
        .is_none()
    );
    let photo = i
        .cache_account_photo(
            &account.account,
            account.generation,
            "p-a1b2c3",
            &person,
            &|_| true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(&photo).unwrap(), b"synthetic-photo");
    forget_person_photos(tmp.path(), "p-a1b2c3").unwrap();
    assert!(!photo.exists());
    let photo = i
        .cache_account_photo(
            &account.account,
            account.generation,
            "p-a1b2c3",
            &person,
            &|_| true,
        )
        .unwrap()
        .unwrap();
    i.remove_account(&account.account, account.generation)
        .unwrap();
    assert!(!photo.exists());
}

#[test]
fn google_workspace_directory_and_refresh_without_rotation() {
    let server = Server::new(|r| {
        if r.target == "/token" {
            return reply(
                200,
                json!({"access_token":"synthetic-fresh-access","expires_in":3600}),
            );
        }
        if r.target.starts_with("/directory") {
            return reply(
                200,
                json!({"people":[{"resourceName":"people/synthetic","names":[{"displayName":"Lio Example"}],"emailAddresses":[{"value":"lio@example.invalid"}],"photos":[{"url":"https://lh3.googleusercontent.com/synthetic"}]}]}),
            );
        }
        reply(404, json!({}))
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    seed(tmp.path(), IntegrationId::Google, secrets.as_ref(), 0);
    let config = IntegrationsConfig {
        integration: [(
            "google".into(),
            IntegrationSettings {
                workspace: true,
                ..settings()
            },
        )]
        .into_iter()
        .collect(),
    };
    let i = Integrations::new(
        tmp.path(),
        authoritative(tmp.path(), config),
        secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
    .unwrap()
    .with_endpoints(IntegrationId::Google, server.endpoints());
    let people = i.directory(IntegrationId::Google, "Lio").unwrap();
    assert_eq!(people[0].name.as_deref(), Some("Lio Example"));
    assert_eq!(people[0].email.as_deref(), Some("lio@example.invalid"));
    assert_eq!(
        i.status(IntegrationId::Google)
            .unwrap()
            .state
            .capabilities
            .directory,
        Capability::Available
    );
    let stored: Tokens =
        serde_json::from_str(&google_credential(tmp.path(), secrets.as_ref()).unwrap()).unwrap();
    assert_eq!(
        stored.refresh_token.as_deref(),
        Some("synthetic-old-refresh")
    );
    // The refresh asks for the stored grant, never more.
    let refresh = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.target == "/token")
        .unwrap()
        .form();
    assert_eq!(refresh["scope"], google_grant().join(" "));
    let requests = server.requests.lock().unwrap();
    let query = requests
        .iter()
        .find(|r| r.target.starts_with("/directory"))
        .unwrap()
        .query();
    assert_eq!(query["sources"], "DIRECTORY_SOURCE_TYPE_DOMAIN_PROFILE");
}

#[test]
fn pagination_cannot_disclose_bearer_to_another_origin() {
    let destination = Server::new(|_| reply(200, json!({"value":[]})));
    let url = format!("{}/steal", destination.base);
    let source = Server::new(move |r| {
        if let Some(identity) = microsoft_identity_fixture(r) {
            return identity;
        }
        if r.target == "/token" {
            return reply(200, token());
        }
        if r.target.starts_with("/authorize") {
            return reply(200, json!({}));
        }
        reply(200, json!({"value":[],"@odata.nextLink":url}))
    });
    let tmp = tempfile::tempdir().unwrap();
    let id = IntegrationId::MicrosoftGraph;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let i = facade(
        tmp.path(),
        id,
        &source,
        secrets,
        Arc::new(CallbackBrowser::default()),
        Arc::new(|_| {}),
    );
    let account = i.add_account(id, &LoginAttemptId::generate()).unwrap();
    assert_eq!(
        i.account_directory(&account.account, "Mira")
            .unwrap_err()
            .code(),
        "invalid_pagination"
    );
    assert!(destination.requests.lock().unwrap().is_empty());
}

#[test]
fn delayed_calendar_and_photo_writes_are_generation_and_person_fenced() {
    for photo in [false, true] {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let server = Server::new(move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            if photo {
                Response {
                    status: 200,
                    body: b"synthetic-photo".to_vec(),
                }
            } else {
                reply(200, json!({"value":[]}))
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let id = IntegrationId::Google;
        let secrets = Arc::new(FakeSecretStore::default());
        seed(
            tmp.path(),
            id,
            secrets.as_ref(),
            Utc::now().timestamp() + 3600,
        );
        let i = Arc::new(facade(
            tmp.path(),
            id,
            &server,
            secrets,
            Arc::new(FakeBrowserLauncher::default()),
            Arc::new(|_| {}),
        ));
        let remote = DirectoryPerson {
            remote_id: "remote".into(),
            name: None,
            email: None,
            photo_url: Some(format!(
                "{}/directory/users/remote/photo/$value",
                server.base
            )),
            source: None,
        };
        let worker = {
            let i = i.clone();
            thread::spawn(move || {
                if photo {
                    i.cache_photo(id, "p-123abc", &remote, &|_| true)
                        .map(|_| ())
                } else {
                    i.sync_calendar(id, Utc::now(), Utc::now() + Delta::days(1))
                        .map(|_| ())
                }
            })
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        i.logout(id).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
        assert!(
            !account_dir(tmp.path(), id)
                .join(if photo {
                    "photos/1/p-123abc.img"
                } else {
                    "calendar.json"
                })
                .exists()
        );
    }
    let exists = Arc::new(AtomicBool::new(true));
    let alive = exists.clone();
    let server = Server::new(move |_| {
        alive.store(false, Ordering::SeqCst);
        Response {
            status: 200,
            body: b"synthetic-photo".to_vec(),
        }
    });
    let tmp = tempfile::tempdir().unwrap();
    let id = IntegrationId::Google;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    );
    let remote = DirectoryPerson {
        remote_id: "remote".into(),
        name: None,
        email: None,
        photo_url: Some(format!("{}/directory/photo", server.base)),
        source: None,
    };
    assert!(
        i.cache_photo(id, "p-123abc", &remote, &|_| exists.load(Ordering::SeqCst))
            .unwrap()
            .is_none()
    );
}

#[test]
fn generation_commit_holds_off_sign_out_and_refuses_a_replaced_account() {
    let tmp = tempfile::tempdir().unwrap();
    let id = IntegrationId::Google;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(tmp.path(), id, secrets.as_ref(), 0);
    let open = || {
        Integrations::new(
            tmp.path(),
            IntegrationsConfig::default(),
            secrets.clone(),
            Arc::new(FakeBrowserLauncher::default()),
            Arc::new(|_| {}),
        )
        .unwrap()
    };
    let app = open();
    let signed_out = Arc::new(AtomicBool::new(false));
    let committed = app
        .commit_for_generation(id, 1, || {
            // A sign-out from another facade starts while the commit runs.
            let other = open();
            let done = signed_out.clone();
            let logout = thread::spawn(move || {
                other.logout(id).unwrap();
                done.store(true, Ordering::SeqCst);
            });
            thread::sleep(Duration::from_millis(200));
            assert!(
                !signed_out.load(Ordering::SeqCst),
                "sign-out waits for the commit"
            );
            Ok(logout)
        })
        .unwrap();
    committed.join().unwrap();
    assert!(signed_out.load(Ordering::SeqCst));
    // The sign-out bumped the generation: a commit for the old one never runs.
    let ran = AtomicBool::new(false);
    let refused = app.commit_for_generation(id, 1, || {
        ran.store(true, Ordering::SeqCst);
        Ok(())
    });
    assert_eq!(refused.unwrap_err().code(), "superseded");
    assert!(!ran.load(Ordering::SeqCst));
    // Signed out at the new generation is refused too.
    assert_eq!(
        app.commit_for_generation(id, 2, || Ok(()))
            .unwrap_err()
            .code(),
        "superseded"
    );
}

#[test]
fn setup_is_offline() {
    let server = Server::new(|_| panic!("setup must not reach HTTP"));
    let tmp = tempfile::tempdir().unwrap();
    let browser = Arc::new(FakeBrowserLauncher::default());
    let _integrations = facade(
        tmp.path(),
        IntegrationId::MicrosoftGraph,
        &server,
        Arc::new(FakeSecretStore::default()),
        browser.clone(),
        Arc::new(|_| {}),
    );
    assert!(
        setup(IntegrationId::MicrosoftGraph, &settings())
            .instructions
            .contains("http://localhost")
    );
    let google = setup(IntegrationId::Google, &settings()).instructions;
    assert!(google.contains("--client-secret-stdin"));
    assert!(google.contains("Settings"));
    assert!(!google.contains("set_client_secret"));
    assert!(browser.opened.lock().unwrap().is_empty());
    assert!(server.requests.lock().unwrap().is_empty());
}

fn graph_config(client_id: Option<&str>) -> IntegrationsConfig {
    IntegrationsConfig {
        integration: [(
            IntegrationId::MicrosoftGraph.to_string(),
            IntegrationSettings {
                client_id: client_id.map(Into::into),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
    }
}
fn graph_facade(
    dir: &std::path::Path,
    client_id: Option<&str>,
    server: &Server,
    secrets: Arc<dyn SecretStore>,
    browser: Arc<dyn BrowserLauncher>,
) -> Integrations {
    Integrations::new(
        dir,
        authoritative(dir, graph_config(client_id)),
        secrets,
        browser,
        Arc::new(|_| {}),
    )
    .unwrap()
    .with_endpoints(IntegrationId::MicrosoftGraph, server.endpoints())
}
fn graph_server() -> Server {
    Server::new(move |r| microsoft_identity_fixture(r).unwrap_or_else(|| reply(200, token())))
}
fn graph_login(i: &Integrations) -> Result<MicrosoftAccount> {
    i.add_account(IntegrationId::MicrosoftGraph, &LoginAttemptId::generate())
}

#[test]
fn oauth_connectors_require_a_registered_client() {
    let server = Server::new(|_| panic!("an unconfigured connector must not reach the network"));
    let tmp = tempfile::tempdir().unwrap();
    let browser = Arc::new(FakeBrowserLauncher::default());
    let mut i = Integrations::new(
        tmp.path(),
        IntegrationsConfig::parse(
            r#"
[integration.teams-session]
client_id = "retired-client"
[integration.outlook-calendar]
client_id = "retired-native"
"#,
        )
        .unwrap(),
        Arc::new(FakeSecretStore::default()),
        browser.clone(),
        Arc::new(|_| {}),
    )
    .unwrap();
    for id in [IntegrationId::MicrosoftGraph, IntegrationId::Google] {
        i = i.with_endpoints(id, server.endpoints());
        let status = i.status(id).unwrap();
        assert!(!status.configured);
        assert_eq!(
            i.login(id, &LoginAttemptId::generate()).unwrap_err().code(),
            "not_configured"
        );
        if id.is_graph() {
            assert_eq!(i.keep_alive(id).unwrap_err().code(), "account_required");
        } else {
            i.keep_alive(id).unwrap();
        }
    }
    assert!(browser.opened.lock().unwrap().is_empty());
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(!tmp.path().join("integrations/teams-session").exists());
    assert!(!tmp.path().join("integrations/outlook-calendar").exists());
}

#[test]
fn changing_the_configured_client_retires_the_sign_in_but_equal_edits_preserve_it() {
    let id = IntegrationId::MicrosoftGraph;
    let server = graph_server();
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let browser = Arc::new(CallbackBrowser::default());
    let facade = |client| {
        graph_facade(
            tmp.path(),
            client,
            &server,
            secrets.clone(),
            browser.clone(),
        )
    };
    let initial = facade(Some("synthetic-client"));
    let account = graph_login(&initial).unwrap();
    let equal = facade(Some(" synthetic-client "));
    let change = equal
        .retire_changed_client(id, &graph_config(Some("synthetic-client")).settings(id))
        .unwrap();
    assert!(!change.effective_changed && !change.sign_in_retired);
    assert_eq!(
        equal.list_accounts(Some(id)).unwrap()[0].generation,
        account.generation
    );
    let requests = server.requests.lock().unwrap().len();
    let overridden = facade(Some("synthetic-override"));
    assert_eq!(
        overridden
            .keep_account_alive(&account.account)
            .unwrap_err()
            .code(),
        "superseded"
    );
    assert_eq!(server.requests.lock().unwrap().len(), requests);
    let change = overridden
        .retire_changed_client(id, &graph_config(Some("synthetic-client")).settings(id))
        .unwrap();
    assert!(change.effective_changed && change.sign_in_retired);
    let retired = overridden.list_accounts(Some(id)).unwrap().remove(0);
    assert_eq!(retired.status, AccountStatus::ReauthRequired);
    assert!(retired.generation > account.generation);
    assert!(secrets.secrets.lock().unwrap().is_empty());
    assert_eq!(
        overridden
            .commit_for_account_generation(&account.account, account.generation, || Ok(()))
            .unwrap_err()
            .code(),
        "superseded"
    );
    let reconnected = overridden
        .reconnect_account(
            &account.account,
            retired.generation,
            &LoginAttemptId::generate(),
        )
        .unwrap();
    assert_eq!(reconnected.account, account.account);
    assert_eq!(reconnected.identity.client_id, "synthetic-override");
    assert!(
        !overridden
            .retire_changed_client(id, &graph_config(Some("synthetic-client")).settings(id))
            .unwrap()
            .sign_in_retired
    );
    let reset = facade(None);
    assert!(
        reset
            .retire_changed_client(id, &graph_config(Some("synthetic-override")).settings(id))
            .unwrap()
            .sign_in_retired
    );
    let retired = reset.list_accounts(Some(id)).unwrap().remove(0);
    reset
        .remove_account(&retired.account, retired.generation)
        .unwrap();
    assert!(reset.list_accounts(Some(id)).unwrap().is_empty());
}

#[test]
fn unbound_legacy_credentials_require_sign_in_without_network_or_cached_calendar_access() {
    for id in [IntegrationId::MicrosoftGraph, IntegrationId::Google] {
        let tmp = tempfile::tempdir().unwrap();
        let server = Server::new(|_| reply(500, json!({"error":"unexpected_request"})));
        let secrets = Arc::new(FakeSecretStore::default());
        let browser = Arc::new(FakeBrowserLauncher::default());
        seed(
            tmp.path(),
            id,
            secrets.as_ref(),
            Utc::now().timestamp() + 3600,
        );
        let service = facade(
            tmp.path(),
            id,
            &server,
            secrets.clone(),
            browser.clone(),
            Arc::new(|_| {}),
        );
        let mut state = service.account_state(id).unwrap();
        state.client_id = None;
        let dir = account_dir(tmp.path(), id);
        write_json_private(&dir.join(STATE_FILE), &state).unwrap();
        let now = Utc::now();
        write_json_private(
            &dir.join(CALENDAR_FILE),
            &CalendarCache {
                events: vec![CalendarEvent {
                    id: "synthetic-legacy-event".into(),
                    integration: id,
                    title: Some("Synthetic calendar event".into()),
                    start: now,
                    end: now + Delta::hours(1),
                    join_links: Vec::new(),
                    app: None,
                    attendees: Vec::new(),
                    account_generation: None,
                    remote: None,
                    google: None,
                    ical_uid: None,
                    occurrence_id: None,
                }],
            },
        )
        .unwrap();
        let credential = secrets.get(&tokens_secret_key(id)).unwrap();
        assert_eq!(
            service.status(id).unwrap().state.status,
            AccountStatus::ReauthRequired
        );
        if id == IntegrationId::Google {
            assert_eq!(
                service
                    .sync_calendar(id, now, now + Delta::days(1))
                    .unwrap_err()
                    .code(),
                "reauth_required"
            );
            assert!(
                served_copy(&service, tmp.path(), id)
                    .unwrap()
                    .events
                    .is_empty()
            );
        } else {
            assert_eq!(
                service.legacy_microsoft_status(id).unwrap(),
                LegacyMicrosoftStatus::Quarantined
            );
            assert!(service.list_accounts(Some(id)).unwrap().is_empty());
            assert_eq!(
                service
                    .sync_calendar(id, now, now + Delta::days(1))
                    .unwrap_err()
                    .code(),
                "account_required"
            );
        }
        assert!(server.requests.lock().unwrap().is_empty());
        assert!(browser.opened.lock().unwrap().is_empty());
        assert_eq!(secrets.get(&tokens_secret_key(id)).unwrap(), credential);
        // Read-only status neither destroys the old sign-in nor pretends it is bound.
        assert_eq!(service.account_state(id).unwrap(), state);
    }
}

#[test]
fn a_login_prepared_with_a_client_the_configuration_no_longer_names_is_superseded() {
    let id = IntegrationId::MicrosoftGraph;
    let server = graph_server();
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    let browser = Arc::new(FakeBrowserLauncher::default());
    // A facade loaded with client A; the configuration then changes to client B.
    let stale = graph_facade(
        tmp.path(),
        Some("synthetic-client-a"),
        &server,
        secrets.clone(),
        browser.clone(),
    );
    authoritative(tmp.path(), graph_config(Some("synthetic-client-b")));
    let before = stale.account_state(id).unwrap();
    assert_eq!(graph_login(&stale).unwrap_err().code(), "superseded");
    // Admission refused: no generation reserved, no browser, no request, no token.
    assert_eq!(stale.account_state(id).unwrap(), before);
    assert!(browser.opened.lock().unwrap().is_empty());
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(secrets.get(&tokens_secret_key(id)).unwrap().is_none());

    // Editing while loopback sign-in is open must also refuse publication.
    struct EditingBrowser(std::path::PathBuf);
    impl BrowserLauncher for EditingBrowser {
        fn open(&self, url: &str) -> broscribe_platform::Result<()> {
            authoritative(&self.0, graph_config(Some("synthetic-client-b")));
            CallbackBrowser::default().open(url)
        }
    }
    let signing_in = graph_facade(
        tmp.path(),
        Some("synthetic-client-a"),
        &server,
        secrets.clone(),
        Arc::new(EditingBrowser(tmp.path().to_owned())),
    );
    let error = graph_login(&signing_in).unwrap_err();
    assert_eq!(error.code(), "superseded");
    assert!(secrets.get(&tokens_secret_key(id)).unwrap().is_none());
    assert_eq!(
        stale.account_state(id).unwrap().status,
        AccountStatus::SignedOut
    );
}

#[test]
fn a_stale_facade_never_retires_a_sign_in_issued_to_the_current_client() {
    let id = IntegrationId::Google;
    let server = Server::new(|_| panic!("a stale facade must not reach the provider"));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    seed(tmp.path(), id, secrets.as_ref(), 0);
    // The account was signed in again with client B after this facade loaded client A.
    let dir = account_dir(tmp.path(), id);
    let mut state: AccountState = crate::store::read_json_opt(&dir.join(STATE_FILE))
        .unwrap()
        .unwrap();
    state.client_id = Some("synthetic-client-b".into());
    write_json_private(&dir.join(STATE_FILE), &state).unwrap();
    let notified = Arc::new(AtomicBool::new(false));
    let flag = notified.clone();
    let stale = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(move |_| flag.store(true, Ordering::SeqCst)),
    );
    // The status projection says the facade cannot use it; maintenance leaves it alone.
    assert_eq!(
        stale.status(id).unwrap().state.status,
        AccountStatus::ReauthRequired
    );
    stale.keep_alive(id).unwrap();
    assert_eq!(stale.account_state(id).unwrap(), state);
    assert!(!notified.load(Ordering::SeqCst));
    assert!(secrets.get(&tokens_secret_key(id)).unwrap().is_some());
    assert!(server.requests.lock().unwrap().is_empty());
}

#[path = "microsoft_tests.rs"]
mod microsoft_tests;

#[path = "login_tests.rs"]
mod login_tests;

#[path = "signout_tests.rs"]
mod signout_tests;

#[test]
fn google_single_slot_filters_private_all_day_declined_and_resource_metadata() {
    let server = Server::new(|_| {
        let now = Utc::now();
        let base = json!({"id":"keep","summary":"Synthetic event","start":{"dateTime":now.to_rfc3339()},"end":{"dateTime":(now+Delta::hours(1)).to_rfc3339()},"attendees":[
            {"displayName":"Guest Example","email":"guest@example.invalid","responseStatus":"accepted"},
            {"email":"declined@example.invalid","responseStatus":"declined"},
            {"email":"room@example.invalid","resource":true}
        ]});
        let mut items = vec![base.clone()];
        for (field, value) in [
            ("visibility", json!("private")),
            ("visibility", json!("confidential")),
            ("status", json!("cancelled")),
        ] {
            let mut event = base.clone();
            event[field] = value;
            items.push(event);
        }
        let mut all_day = base.clone();
        all_day["start"] = json!({"date":"2026-01-01"});
        items.push(all_day);
        let mut declined = base;
        declined["attendees"][0]["self"] = json!(true);
        declined["attendees"][0]["responseStatus"] = json!("declined");
        items.push(declined);
        reply(200, json!({"items":items}))
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        IntegrationId::Google,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let service = facade(
        tmp.path(),
        IntegrationId::Google,
        &server,
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    );
    let read = service
        .sync_calendar(
            IntegrationId::Google,
            Utc::now(),
            Utc::now() + Delta::days(1),
        )
        .unwrap();
    assert_eq!(read.events.len(), 1);
    assert_eq!(read.events[0].attendees.len(), 1);
    assert_eq!(
        read.events[0].attendees[0].email.as_deref(),
        Some("guest@example.invalid")
    );
    assert!(read.events[0].remote.is_none());
    // Stamped with the account and selection generations it was read under, for
    // recording fences; never with the native scalar stamp.
    assert_eq!(read.events[0].google, Some(google_stamp(1, 0)));
    assert_eq!(read.events[0].account_generation, None);
}

fn google_stamp(generation: u64, selection_generation: u64) -> GoogleCalendarStamp {
    GoogleCalendarStamp {
        generation,
        selection_generation,
    }
}

#[path = "legacy_discard_tests.rs"]
mod legacy_discard_tests;

#[path = "tenant_authority_tests.rs"]
mod tenant_authority_tests;

fn google_event_page() -> Value {
    let now = Utc::now();
    json!({"items":[{"id":"event-g","summary":"Synthetic review","start":{"dateTime":now.to_rfc3339()},"end":{"dateTime":(now+Delta::hours(1)).to_rfc3339()}}]})
}

#[test]
fn google_calendar_denial_is_fenced_before_the_copy_and_stale_reads_never_publish() {
    let id = IntegrationId::Google;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let reading = Server::new(move |r| {
        if r.target.contains("/events?") {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        reply(200, google_event_page())
    });
    let refusing = Server::new(|_| {
        reply(
            403,
            json!({"error":{"code":403,"errors":[{"reason":"forbidden"}]}}),
        )
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let open = |server: &Server| {
        Arc::new(facade(
            tmp.path(),
            id,
            server,
            secrets.clone(),
            Arc::new(FakeBrowserLauncher::default()),
            Arc::new(|_| {}),
        ))
    };
    let stale = open(&reading);
    let denying = open(&refusing);
    let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
    let state = || denying.account_state(id).unwrap();
    let read_in_background = || {
        let stale = stale.clone();
        thread::spawn(move || stale.sync_calendar(id, window.0, window.1).map(|_| ()))
    };

    // First-read denial: it advances the selection generation (fencing priors) and the
    // calendar fence, and an older successful read never publishes after it. The account
    // generation, which directory results and photos belong to, stays.
    let worker = read_in_background();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        denying
            .sync_calendar(id, window.0, window.1)
            .unwrap_err()
            .code(),
        "forbidden"
    );
    let denied = state();
    assert_eq!(denied.generation, 1);
    assert_eq!(denied.selection_generation, 1);
    assert_eq!(denied.calendar_fence, 1);
    assert_eq!(denied.capabilities.calendar, Capability::Denied);
    release_tx.send(()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
    let copy = account_dir(tmp.path(), id).join(CALENDAR_FILE);
    assert!(!copy.exists());
    assert_eq!(
        denying.google_authority().unwrap(),
        Some(google_stamp(1, 1))
    );

    // A repeated denial keeps the generations but still fences a read that began before it.
    let worker = read_in_background();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        denying
            .sync_calendar(id, window.0, window.1)
            .unwrap_err()
            .code(),
        "forbidden"
    );
    assert_eq!(state().selection_generation, 1);
    assert_eq!(state().generation, 1);
    assert_eq!(state().calendar_fence, 2);
    release_tx.send(()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
    assert!(!copy.exists());

    // A read that starts after the denial publishes, stamped with the live authority.
    release_tx.send(()).unwrap();
    let published = stale.sync_calendar(id, window.0, window.1).unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(published.events.len(), 1);
    assert_eq!(published.events[0].google, Some(google_stamp(1, 1)));
    assert_eq!(
        served_copy(&stale, tmp.path(), id).unwrap().events,
        published.events
    );

    // Revoking a published copy is saved before the copy is removed: when removal fails,
    // the state is already fenced and the old generation's priors no longer verify.
    std::fs::remove_file(&copy).unwrap();
    std::fs::create_dir_all(copy.join("blocker")).unwrap();
    assert!(denying.sync_calendar(id, window.0, window.1).is_err());
    let fenced = state();
    assert_eq!(fenced.generation, 1);
    assert_eq!(fenced.selection_generation, 2);
    assert_eq!(fenced.capabilities.calendar, Capability::Denied);
    assert_eq!(
        denying.google_authority().unwrap(),
        Some(google_stamp(1, 2))
    );
}

/// A secret store whose versioned Google writes can be made to fail.
#[derive(Default)]
struct FlakyVersions {
    inner: FakeSecretStore,
    fail: AtomicBool,
}
impl SecretStore for FlakyVersions {
    fn get(&self, key: &str) -> broscribe_platform::Result<Option<String>> {
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &str) -> broscribe_platform::Result<()> {
        if self.fail.load(Ordering::SeqCst) && key.starts_with("integration/google/tokens/") {
            return Err(broscribe_core::Error::Internal(
                "synthetic keychain failure".into(),
            ));
        }
        self.inner.set(key, value)
    }
    fn delete(&self, key: &str) -> broscribe_platform::Result<()> {
        self.inner.delete(key)
    }
}

#[test]
fn google_credential_replacement_is_staged_and_a_failure_keeps_the_working_credential() {
    let id = IntegrationId::Google;
    let server = Server::new(|r| {
        if r.target == "/token" {
            return reply(200, google_token());
        }
        reply(200, json!({"people":[]}))
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FlakyVersions::default());
    seed(tmp.path(), id, secrets.as_ref(), 0);
    let working = google_credential(tmp.path(), secrets.as_ref()).unwrap();
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| panic!("a storage failure is not a sign-in failure")),
    );
    secrets.fail.store(true, Ordering::SeqCst);
    assert_eq!(i.directory(id, "Mira").unwrap_err().code(), "internal");
    let dir = account_dir(tmp.path(), id);
    let staged: AccountState = crate::store::read_json_opt(&dir.join(STATE_FILE))
        .unwrap()
        .unwrap();
    // The intent was durable; the pointer still names the working credential.
    assert!(staged.secret_key.is_none());
    assert_eq!(staged.pending_secrets.len(), 1);
    assert_eq!(staged.status, AccountStatus::Active);
    assert_eq!(
        google_credential(tmp.path(), secrets.as_ref()).unwrap(),
        working
    );

    // A crash between staging and commit leaves a written but uncommitted version; the
    // next transaction deletes it and keeps the committed credential.
    let orphan = crate::store::google_secret_version();
    secrets
        .inner
        .set(&orphan, "synthetic uncommitted credential")
        .unwrap();
    let mut crashed = staged.clone();
    crashed.pending_secrets.push(orphan.clone());
    write_json_private(&dir.join(STATE_FILE), &crashed).unwrap();
    secrets.fail.store(false, Ordering::SeqCst);
    let recovered = i.account_state(id).unwrap();
    assert!(recovered.pending_secrets.is_empty());
    assert!(secrets.get(&orphan).unwrap().is_none());
    assert_eq!(
        google_credential(tmp.path(), secrets.as_ref()).unwrap(),
        working
    );

    // A successful replacement commits a new version and retires the old slot.
    i.directory(id, "Mira").unwrap();
    let committed = i.account_state(id).unwrap();
    assert!(
        committed
            .secret_key
            .as_deref()
            .is_some_and(|key| key.starts_with("integration/google/tokens/"))
    );
    assert!(committed.pending_secrets.is_empty());
    assert!(secrets.get(&tokens_secret_key(id)).unwrap().is_none());
    let stored: Tokens =
        serde_json::from_str(&google_credential(tmp.path(), secrets.as_ref()).unwrap()).unwrap();
    assert_eq!(
        stored.refresh_token.as_deref(),
        Some("synthetic-rotated-refresh")
    );

    // A recovery reference outside the Google namespace is refused, never deleted.
    let mut tampered = committed;
    tampered
        .pending_secrets
        .push("integration/microsoft-graph/tokens".into());
    write_json_private(&dir.join(STATE_FILE), &tampered).unwrap();
    secrets
        .inner
        .set("integration/microsoft-graph/tokens", "synthetic unrelated")
        .unwrap();
    assert_eq!(
        i.account_state(id).unwrap_err().code(),
        "invalid_secret_reference"
    );
    assert!(
        secrets
            .get("integration/microsoft-graph/tokens")
            .unwrap()
            .is_some()
    );
}

#[test]
fn google_refresh_and_directory_use_the_stored_grant_not_a_later_configuration() {
    let id = IntegrationId::Google;
    let server = Server::new(|r| {
        if r.target == "/token" {
            return reply(200, token());
        }
        reply(200, json!({"people":[]}))
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    seed(tmp.path(), id, secrets.as_ref(), 0);
    // Signed in for the calendar only; the configuration enabled Workspace afterwards.
    let dir = account_dir(tmp.path(), id);
    let mut state: AccountState = crate::store::read_json_opt(&dir.join(STATE_FILE))
        .unwrap()
        .unwrap();
    state.scopes = vec![crate::config::GOOGLE_CALENDAR_READONLY.into()];
    write_json_private(&dir.join(STATE_FILE), &state).unwrap();
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    );
    assert_eq!(
        i.directory(id, "Mira").unwrap_err().code(),
        "insufficient_grant"
    );
    let requests = server.requests.lock().unwrap().clone();
    let refresh = requests
        .iter()
        .find(|r| r.target == "/token")
        .unwrap()
        .form();
    assert_eq!(refresh["scope"], crate::config::GOOGLE_CALENDAR_READONLY);
    assert!(!requests.iter().any(|r| r.target.starts_with("/directory")));
    assert_eq!(
        i.status(id).unwrap().state.capabilities.directory,
        Capability::Denied
    );
}

#[test]
fn google_selection_is_explicit_validated_and_fences_the_previous_copy() {
    let id = IntegrationId::Google;
    let server = Server::new(|r| {
        if let Some(response) = google_identity_fixture(r) {
            return response;
        }
        reply(200, google_event_page())
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    );
    let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
    assert_eq!(
        i.sync_calendar(id, window.0, window.1)
            .unwrap()
            .events
            .len(),
        1
    );
    let copy = account_dir(tmp.path(), id).join(CALENDAR_FILE);
    assert!(copy.exists());
    let catalog = i.google_calendars().unwrap();
    assert_eq!(catalog.selected, [GOOGLE_CALENDAR]);
    // Calendars outside the readable catalog, and stale catalogs, are refused unchanged.
    assert_eq!(
        i.configure_google_calendars(catalog.authority, &["busy@example.invalid".into()])
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    for stale in [
        google_stamp(
            catalog.authority.generation + 1,
            catalog.authority.selection_generation,
        ),
        google_stamp(
            catalog.authority.generation,
            catalog.authority.selection_generation + 1,
        ),
    ] {
        assert_eq!(
            i.configure_google_calendars(stale, &[GOOGLE_CALENDAR.into()])
                .unwrap_err()
                .code(),
            "superseded"
        );
    }
    assert!(copy.exists());
    // Clearing the selection reads nothing more and fences the old copy and its priors;
    // only the selection generation advances.
    let status = i
        .configure_google_calendars(catalog.authority, &[])
        .unwrap();
    assert!(status.state.native_calendar_ids.is_empty());
    assert_eq!(status.state.generation, catalog.authority.generation);
    assert_eq!(
        status.state.selection_generation,
        catalog.authority.selection_generation + 1
    );
    assert!(!copy.exists());
    let requests = server.requests.lock().unwrap().len();
    assert!(
        i.sync_calendar(id, window.0, window.1)
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(server.requests.lock().unwrap().len(), requests);
    assert!(served_copy(&i, tmp.path(), id).unwrap().events.is_empty());
}

/// One synthetic event of the Google calendar `calendar`.
fn google_calendar_page(calendar: &str) -> Value {
    let now = Utc::now();
    json!({"items":[{"id":format!("event-{calendar}"),"summary":"Synthetic review","start":{"dateTime":now.to_rfc3339()},"end":{"dateTime":(now+Delta::hours(1)).to_rfc3339()}}]})
}

/// Seeds an active Google account selecting `calendars` and opens a facade on `server`.
fn google_selecting(tmp: &std::path::Path, server: &Server, calendars: &[&str]) -> Integrations {
    let id = IntegrationId::Google;
    let secrets = Arc::new(FakeSecretStore::default());
    seed(tmp, id, secrets.as_ref(), Utc::now().timestamp() + 3600);
    let path = account_dir(tmp, id).join(STATE_FILE);
    let mut state: AccountState = crate::store::read_json_opt(&path).unwrap().unwrap();
    state.native_calendar_ids = calendars.iter().map(|c| (*c).to_owned()).collect();
    write_json_private(&path, &state).unwrap();
    facade(
        tmp,
        id,
        server,
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
}

#[test]
fn a_vanished_google_calendar_leaves_the_selection_and_fences_its_copy_while_others_publish() {
    let id = IntegrationId::Google;
    let gone = Arc::new(AtomicBool::new(false));
    let vanished = gone.clone();
    let server = Server::new(move |r| {
        if r.target.contains("/calendars/gone") {
            return if vanished.load(Ordering::SeqCst) {
                reply(404, json!({"error":{"code":404,"message":"Not Found"}}))
            } else {
                reply(200, google_calendar_page("gone"))
            };
        }
        if r.target.contains("/calendars/") && r.target.contains("/events?") {
            return reply(200, google_calendar_page("work"));
        }
        panic!("unexpected request {}", r.target)
    });
    let tmp = tempfile::tempdir().unwrap();
    let i = google_selecting(
        tmp.path(),
        &server,
        &[GOOGLE_CALENDAR, "gone@example.invalid"],
    );
    let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
    assert_eq!(
        i.sync_calendar(id, window.0, window.1)
            .unwrap()
            .events
            .len(),
        2
    );
    assert_eq!(served_copy(&i, tmp.path(), id).unwrap().events.len(), 2);

    // The calendar is deleted: the healthy one still publishes, under a new selection
    // generation that no longer verifies the vanished calendar's copy or the priors taken
    // from it.
    gone.store(true, Ordering::SeqCst);
    let read = i.sync_calendar(id, window.0, window.1).unwrap();
    assert_eq!(read.events.len(), 1);
    assert_eq!(read.events[0].id, "event-work");
    assert_eq!(read.events[0].google, Some(google_stamp(1, 1)));
    let state = i.account_state(id).unwrap();
    assert_eq!(state.native_calendar_ids, [GOOGLE_CALENDAR]);
    assert_eq!(state.generation, 1);
    assert_eq!(state.selection_generation, 1);
    assert_eq!(state.calendar_fence, 0);
    assert_eq!(state.capabilities.calendar, Capability::Available);
    assert_eq!(i.google_authority().unwrap(), Some(google_stamp(1, 1)));
    assert_eq!(served_copy(&i, tmp.path(), id).unwrap().events, read.events);

    // It is never read again, and nothing takes its place.
    let before = server.requests.lock().unwrap().len();
    i.sync_calendar(id, window.0, window.1).unwrap();
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), before + 1);
    assert!(!requests[before].target.contains("/calendars/gone"));
}

#[test]
fn a_refused_google_calendar_is_lost_alone_unless_the_account_itself_is_refused() {
    let id = IntegrationId::Google;
    let catalog_refused = Arc::new(AtomicBool::new(false));
    let refused = catalog_refused.clone();
    let server = Server::new(move |r| {
        let forbidden = || {
            reply(
                403,
                json!({"error":{"code":403,"errors":[{"reason":"forbidden"}]}}),
            )
        };
        if r.target.starts_with("/api/users/me/calendarList?") {
            return if refused.load(Ordering::SeqCst) {
                forbidden()
            } else {
                google_identity_fixture(r).unwrap()
            };
        }
        if r.target.contains("/calendars/unshared") {
            return forbidden();
        }
        reply(200, google_calendar_page("work"))
    });
    let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));

    // Another calendar was read: the account's access stands without asking anything else.
    let tmp = tempfile::tempdir().unwrap();
    let i = google_selecting(
        tmp.path(),
        &server,
        &[GOOGLE_CALENDAR, "unshared@example.invalid"],
    );
    let read = i.sync_calendar(id, window.0, window.1).unwrap();
    assert_eq!(read.events.len(), 1);
    let state = i.account_state(id).unwrap();
    assert_eq!(state.native_calendar_ids, [GOOGLE_CALENDAR]);
    assert_eq!(state.capabilities.calendar, Capability::Available);
    assert_eq!(state.calendar_fence, 0);
    assert!(
        !server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.target.contains("calendarList"))
    );
    drop(i);

    // Only that calendar was selected: the account's calendar list still answers, so only
    // that calendar is lost. Nothing is read in its place.
    let tmp = tempfile::tempdir().unwrap();
    let i = google_selecting(tmp.path(), &server, &["unshared@example.invalid"]);
    assert!(
        i.sync_calendar(id, window.0, window.1)
            .unwrap()
            .events
            .is_empty()
    );
    let state = i.account_state(id).unwrap();
    assert!(state.native_calendar_ids.is_empty());
    assert_eq!(state.capabilities.calendar, Capability::NotRequested);
    assert_eq!(state.status, AccountStatus::Active);
    assert_eq!(state.calendar_fence, 0);
    drop(i);

    // The calendar list is refused too: the account's calendar access itself was withdrawn.
    catalog_refused.store(true, Ordering::SeqCst);
    let tmp = tempfile::tempdir().unwrap();
    let i = google_selecting(tmp.path(), &server, &["unshared@example.invalid"]);
    assert_eq!(
        i.sync_calendar(id, window.0, window.1).unwrap_err().code(),
        "forbidden"
    );
    let state = i.account_state(id).unwrap();
    assert_eq!(state.capabilities.calendar, Capability::Denied);
    assert_eq!(state.calendar_fence, 1);
    assert_eq!(state.native_calendar_ids, ["unshared@example.invalid"]);
}

fn google_refusal(status: u16) -> Response {
    if status == 403 {
        reply(
            403,
            json!({"error":{"code":403,"errors":[{"reason":"forbidden"}]}}),
        )
    } else {
        reply(
            status,
            json!({"error":{"code":status,"message":"Synthetic failure"}}),
        )
    }
}

#[test]
fn google_losses_seen_before_a_failing_sibling_are_withdrawn_and_supersede_older_reads() {
    const LOST: &str = "lost@example.invalid";
    const FLAKY: &str = "flaky@example.invalid";
    let id = IntegrationId::Google;
    for refusal in [404, 403] {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let park = Arc::new(AtomicBool::new(false));
        let parking = park.clone();
        // Every selected calendar is readable here; a parked read holds at its first one.
        let readable = Server::new(move |r| {
            if r.target.contains("/events?") && parking.swap(false, Ordering::SeqCst) {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            let calendar = ["lost", "flaky"]
                .into_iter()
                .find(|c| r.target.contains(&format!("/calendars/{c}")))
                .unwrap_or("work");
            reply(200, google_calendar_page(calendar))
        });
        // The first selected calendar is gone or refused, the next one reads, the last one
        // fails transiently. The calendar list is never needed: another calendar was read.
        let down = Arc::new(AtomicBool::new(true));
        let flaky_down = down.clone();
        let mixed = Server::new(move |r| {
            if r.target.contains("/calendars/lost") {
                return google_refusal(refusal);
            }
            if r.target.contains("/calendars/flaky") {
                return if flaky_down.load(Ordering::SeqCst) {
                    google_refusal(503)
                } else {
                    reply(200, google_calendar_page("flaky"))
                };
            }
            if r.target.contains("/events?") {
                return reply(200, google_calendar_page("work"));
            }
            panic!("unexpected request {}", r.target)
        });
        let tmp = tempfile::tempdir().unwrap();
        let secrets = Arc::new(FakeSecretStore::default());
        seed(
            tmp.path(),
            id,
            secrets.as_ref(),
            Utc::now().timestamp() + 3600,
        );
        let path = account_dir(tmp.path(), id).join(STATE_FILE);
        let mut state: AccountState = crate::store::read_json_opt(&path).unwrap().unwrap();
        state.native_calendar_ids = vec![LOST.into(), GOOGLE_CALENDAR.into(), FLAKY.into()];
        write_json_private(&path, &state).unwrap();
        let open = |server: &Server| {
            Arc::new(facade(
                tmp.path(),
                id,
                server,
                secrets.clone(),
                Arc::new(FakeBrowserLauncher::default()),
                Arc::new(|_| {}),
            ))
        };
        let older = open(&readable);
        let observing = open(&mixed);
        let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
        let copy = account_dir(tmp.path(), id).join(CALENDAR_FILE);

        // A published copy of all three calendars, and the priors a recording took from it.
        assert_eq!(
            older
                .sync_calendar(id, window.0, window.1)
                .unwrap()
                .events
                .len(),
            3
        );
        let priors = served_copy(&older, tmp.path(), id).unwrap().events;
        assert_eq!(priors.len(), 3);
        assert!(priors.iter().all(|e| e.google == Some(google_stamp(1, 0))));

        // An earlier read of the whole selection is parked before it can commit.
        park.store(true, Ordering::SeqCst);
        let worker = {
            let older = older.clone();
            thread::spawn(move || older.sync_calendar(id, window.0, window.1).map(|_| ()))
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // The loss is withdrawn durably although the read as a whole fails, and that
        // failure is still reported: nothing incomplete is published.
        assert_eq!(
            observing
                .sync_calendar(id, window.0, window.1)
                .unwrap_err()
                .code(),
            "transient"
        );
        let state = observing.account_state(id).unwrap();
        assert_eq!(state.native_calendar_ids, [GOOGLE_CALENDAR, FLAKY]);
        assert_eq!(state.generation, 1);
        assert_eq!(state.selection_generation, 1);
        assert_eq!(state.calendar_fence, 0);
        assert_eq!(state.status, AccountStatus::Active);
        assert_eq!(state.capabilities.calendar, Capability::Error);
        assert_eq!(state.last_sync, None);
        let live = observing.google_authority().unwrap();
        assert_eq!(live, Some(google_stamp(1, 1)));
        assert!(!copy.exists());
        // The old copy and every prior taken from it no longer verify.
        assert!(priors.iter().all(|e| e.google != live));
        assert!(
            served_copy(&observing, tmp.path(), id)
                .unwrap()
                .events
                .is_empty()
        );

        // The parked earlier success is superseded and publishes nothing.
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
        assert!(!copy.exists());
        assert_eq!(observing.google_authority().unwrap(), live);

        // Once the sibling recovers, the remaining selection publishes under the live
        // authority and the lost calendar is never read again.
        down.store(false, Ordering::SeqCst);
        let before = mixed.requests.lock().unwrap().len();
        let read = observing.sync_calendar(id, window.0, window.1).unwrap();
        assert_eq!(read.events.len(), 2);
        assert!(read.events.iter().all(|e| e.google == live));
        assert_eq!(
            served_copy(&observing, tmp.path(), id).unwrap().events,
            read.events
        );
        assert!(
            !mixed.requests.lock().unwrap()[before..]
                .iter()
                .any(|r| r.target.contains("/calendars/lost"))
        );
        assert_eq!(
            observing.account_state(id).unwrap().capabilities.calendar,
            Capability::Available
        );
    }
}

#[test]
fn a_lone_refused_google_calendar_is_withdrawn_even_when_the_calendar_list_cannot_be_read() {
    let id = IntegrationId::Google;
    let server = Server::new(|r| {
        if r.target.starts_with("/api/users/me/calendarList?") {
            return google_refusal(503);
        }
        if r.target.contains("/calendars/unshared") {
            return google_refusal(403);
        }
        panic!("unexpected request {}", r.target)
    });
    let tmp = tempfile::tempdir().unwrap();
    let i = google_selecting(tmp.path(), &server, &["unshared@example.invalid"]);
    // A copy published under the current authority, before the calendar was unshared.
    let now = Utc::now();
    write_json_private(
        &account_dir(tmp.path(), id).join(CALENDAR_FILE),
        &CalendarCache {
            events: vec![CalendarEvent {
                id: "synthetic-unshared-event".into(),
                integration: id,
                title: Some("Synthetic unshared meeting".into()),
                start: now,
                end: now + Delta::hours(1),
                join_links: Vec::new(),
                app: None,
                attendees: Vec::new(),
                account_generation: None,
                remote: None,
                google: Some(google_stamp(1, 0)),
                ical_uid: None,
                occurrence_id: None,
            }],
        },
    )
    .unwrap();
    assert_eq!(served_copy(&i, tmp.path(), id).unwrap().events.len(), 1);
    assert_eq!(
        i.sync_calendar(id, now - Delta::days(1), now + Delta::days(1))
            .unwrap_err()
            .code(),
        "transient"
    );
    let state = i.account_state(id).unwrap();
    assert!(state.native_calendar_ids.is_empty());
    assert_eq!(state.selection_generation, 1);
    assert_eq!(state.generation, 1);
    // Not an account-wide denial: that was never established.
    assert_eq!(state.calendar_fence, 0);
    assert_eq!(state.capabilities.calendar, Capability::Error);
    assert_eq!(state.status, AccountStatus::Active);
    assert!(!account_dir(tmp.path(), id).join(CALENDAR_FILE).exists());
    assert!(served_copy(&i, tmp.path(), id).unwrap().events.is_empty());
}

#[test]
fn google_losses_seen_before_a_rejected_token_survive_a_failed_refresh_and_supersede_older_reads() {
    const LOST: &str = "lost@example.invalid";
    const CHALLENGED: &str = "challenged@example.invalid";
    let id = IntegrationId::Google;
    for refusal in [404, 403] {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let park = Arc::new(AtomicBool::new(false));
        let parking = park.clone();
        // Both selected calendars are readable here; a parked read holds at its first one.
        let readable = Server::new(move |r| {
            if r.target.contains("/events?") && parking.swap(false, Ordering::SeqCst) {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            let calendar = if r.target.contains("/calendars/lost") {
                "lost"
            } else {
                "challenged"
            };
            reply(200, google_calendar_page(calendar))
        });
        // The first selected calendar is gone or refused; the next one rejects the old
        // access token, and the token endpoint fails transiently until `refresh_up`.
        let refresh_up = Arc::new(AtomicBool::new(false));
        let up = refresh_up.clone();
        let challenging = Server::new(move |r| {
            if r.target == "/token" {
                return if up.load(Ordering::SeqCst) {
                    reply(200, google_token())
                } else {
                    google_refusal(503)
                };
            }
            if r.target.starts_with("/api/users/me/calendarList?") {
                return reply(
                    200,
                    json!({"items":[
                        {"id":LOST,"summary":"Synthetic lost","accessRole":"reader"},
                        {"id":CHALLENGED,"summary":"Synthetic challenged","accessRole":"reader"}
                    ]}),
                );
            }
            if r.target.contains("/calendars/lost") {
                return google_refusal(refusal);
            }
            if r.target.contains("/calendars/challenged") {
                return if r.authorization.as_deref() == Some("Bearer synthetic-new-access") {
                    reply(200, google_calendar_page("challenged"))
                } else {
                    google_refusal(401)
                };
            }
            panic!("unexpected request {}", r.target)
        });
        let tmp = tempfile::tempdir().unwrap();
        let secrets = Arc::new(FakeSecretStore::default());
        seed(
            tmp.path(),
            id,
            secrets.as_ref(),
            Utc::now().timestamp() + 3600,
        );
        let path = account_dir(tmp.path(), id).join(STATE_FILE);
        let mut state: AccountState = crate::store::read_json_opt(&path).unwrap().unwrap();
        state.native_calendar_ids = vec![LOST.into(), CHALLENGED.into()];
        write_json_private(&path, &state).unwrap();
        let notifications = Arc::new(AtomicUsize::new(0));
        let open = |server: &Server| {
            let count = notifications.clone();
            Arc::new(facade(
                tmp.path(),
                id,
                server,
                secrets.clone(),
                Arc::new(FakeBrowserLauncher::default()),
                Arc::new(move |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                }),
            ))
        };
        let older = open(&readable);
        let observing = open(&challenging);
        let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
        let copy = account_dir(tmp.path(), id).join(CALENDAR_FILE);
        let targets = |from: usize| -> Vec<String> {
            challenging.requests.lock().unwrap()[from..]
                .iter()
                .map(|r| r.target.clone())
                .collect()
        };

        // A published copy of both calendars, and the priors a recording took from it.
        assert_eq!(
            older
                .sync_calendar(id, window.0, window.1)
                .unwrap()
                .events
                .len(),
            2
        );
        let priors = served_copy(&older, tmp.path(), id).unwrap().events;
        assert_eq!(priors.len(), 2);
        assert!(priors.iter().all(|e| e.google == Some(google_stamp(1, 0))));

        // An earlier read of the whole selection is parked before it can commit.
        park.store(true, Ordering::SeqCst);
        let worker = {
            let older = older.clone();
            thread::spawn(move || older.sync_calendar(id, window.0, window.1).map(|_| ()))
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // The loss was answered under an accepted token; the next calendar rejects that
        // token and both refresh attempts fail transiently. The sync reports that failure
        // without publishing anything, but only after the loss left the selection durably.
        // No calendar list is asked with a rejected token, and nothing needs sign-in.
        assert_eq!(
            observing
                .sync_calendar(id, window.0, window.1)
                .unwrap_err()
                .code(),
            "transient"
        );
        let failed = targets(0);
        assert_eq!(failed.len(), 4);
        assert!(failed[0].contains("/calendars/lost"));
        assert!(failed[1].contains("/calendars/challenged"));
        assert_eq!(failed[2..], ["/token", "/token"]);
        let state = observing.account_state(id).unwrap();
        assert_eq!(state.native_calendar_ids, [CHALLENGED]);
        assert_eq!(state.generation, 1);
        assert_eq!(state.selection_generation, 1);
        assert_eq!(state.calendar_fence, 0);
        assert_eq!(state.status, AccountStatus::Active);
        assert_eq!(state.capabilities.calendar, Capability::Error);
        assert_eq!(state.last_sync, None);
        assert!(
            google_credential(tmp.path(), secrets.as_ref())
                .unwrap()
                .contains("synthetic-old-access")
        );
        assert_eq!(notifications.load(Ordering::SeqCst), 0);
        let live = observing.google_authority().unwrap();
        assert_eq!(live, Some(google_stamp(1, 1)));
        assert!(!copy.exists());
        // The old copy and every prior taken from it no longer verify.
        assert!(priors.iter().all(|e| e.google != live));
        assert!(
            served_copy(&observing, tmp.path(), id)
                .unwrap()
                .events
                .is_empty()
        );

        // The parked earlier success is superseded and publishes nothing.
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
        assert!(!copy.exists());
        assert_eq!(observing.google_authority().unwrap(), live);

        // Once the token endpoint answers, the one legitimate retry refreshes and the
        // remaining selection publishes under the live authority. The lost calendar is
        // never requested again.
        refresh_up.store(true, Ordering::SeqCst);
        let before = challenging.requests.lock().unwrap().len();
        let read = observing.sync_calendar(id, window.0, window.1).unwrap();
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].id, "event-challenged");
        assert!(read.events.iter().all(|e| e.google == live));
        assert_eq!(
            served_copy(&observing, tmp.path(), id).unwrap().events,
            read.events
        );
        let recovered = targets(before);
        assert_eq!(recovered.len(), 3);
        assert!(recovered[0].contains("/calendars/challenged"));
        assert_eq!(recovered[1], "/token");
        assert!(recovered[2].contains("/calendars/challenged"));
        let state = observing.account_state(id).unwrap();
        assert_eq!(state.capabilities.calendar, Capability::Available);
        assert_eq!(state.selection_generation, 1);
        assert_eq!(state.status, AccountStatus::Active);
        assert_eq!(notifications.load(Ordering::SeqCst), 0);

        // Only an explicit reselection reads it again.
        let catalog = observing.google_calendars().unwrap();
        assert_eq!(Some(catalog.authority), live);
        observing
            .configure_google_calendars(catalog.authority, &[LOST.into(), CHALLENGED.into()])
            .unwrap();
        let before = challenging.requests.lock().unwrap().len();
        observing.sync_calendar(id, window.0, window.1).unwrap();
        assert!(
            targets(before)
                .iter()
                .any(|target| target.contains("/calendars/lost"))
        );
    }
}

#[test]
fn google_losses_seen_before_a_rejected_token_survive_a_rejected_refresh() {
    const LOST: &str = "lost@example.invalid";
    const CHALLENGED: &str = "challenged@example.invalid";
    let id = IntegrationId::Google;
    let server = Server::new(|r| {
        if r.target == "/token" {
            return reply(400, json!({"error":"invalid_grant"}));
        }
        if r.target.contains("/calendars/lost") {
            return google_refusal(404);
        }
        if r.target.contains("/calendars/challenged") {
            return google_refusal(401);
        }
        panic!("unexpected request {}", r.target)
    });
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let path = account_dir(tmp.path(), id).join(STATE_FILE);
    let mut state: AccountState = crate::store::read_json_opt(&path).unwrap().unwrap();
    state.native_calendar_ids = vec![LOST.into(), CHALLENGED.into()];
    write_json_private(&path, &state).unwrap();
    let notifications = Arc::new(AtomicUsize::new(0));
    let count = notifications.clone();
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        }),
    );
    assert_eq!(
        i.sync_calendar(id, Utc::now(), Utc::now() + Delta::days(1))
            .unwrap_err()
            .code(),
        "reauth_required"
    );
    // The provider refused the refresh: sign-in is required, and the loss observed under
    // the accepted token still left the selection. The account generation stays.
    let state = i.account_state(id).unwrap();
    assert_eq!(state.status, AccountStatus::ReauthRequired);
    assert_eq!(state.native_calendar_ids, [CHALLENGED]);
    assert_eq!(state.generation, 1);
    assert_eq!(state.selection_generation, 1);
    assert_eq!(state.calendar_fence, 0);
    assert_eq!(state.capabilities.calendar, Capability::Error);
    assert_eq!(i.google_authority().unwrap(), None);
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    assert_eq!(server.requests.lock().unwrap().len(), 3);
}

#[test]
fn a_stale_failing_google_read_never_replaces_the_status_or_selection_of_a_newer_one() {
    const LOST: &str = "lost@example.invalid";
    const WORK: &str = "work@example.invalid";
    let id = IntegrationId::Google;
    for reselected in [vec![WORK.to_owned()], Vec::new()] {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        // The stale read loses its first calendar, then its next one fails after the user
        // has saved another selection.
        let parked = Server::new(move |r| {
            if r.target.contains("/calendars/lost") {
                return google_refusal(404);
            }
            if r.target.contains("/events?") {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                return google_refusal(503);
            }
            panic!("unexpected request {}", r.target)
        });
        let live = Server::new(|r| {
            if r.target.starts_with("/api/users/me/calendarList?") {
                return reply(
                    200,
                    json!({"items":[
                        {"id":GOOGLE_CALENDAR,"summary":"Reader Example","primary":true,"accessRole":"owner"},
                        {"id":WORK,"summary":"Synthetic work","accessRole":"reader"}
                    ]}),
                );
            }
            if r.target.contains("/calendars/work") {
                return reply(200, google_calendar_page("work"));
            }
            panic!("unexpected request {}", r.target)
        });
        let tmp = tempfile::tempdir().unwrap();
        let secrets = Arc::new(FakeSecretStore::default());
        seed(
            tmp.path(),
            id,
            secrets.as_ref(),
            Utc::now().timestamp() + 3600,
        );
        let path = account_dir(tmp.path(), id).join(STATE_FILE);
        let mut state: AccountState = crate::store::read_json_opt(&path).unwrap().unwrap();
        state.native_calendar_ids = vec![LOST.into(), GOOGLE_CALENDAR.into()];
        write_json_private(&path, &state).unwrap();
        let open = |server: &Server| {
            facade(
                tmp.path(),
                id,
                server,
                secrets.clone(),
                Arc::new(FakeBrowserLauncher::default()),
                Arc::new(|_| {}),
            )
        };
        let stale = Arc::new(open(&parked));
        let current = open(&live);
        let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
        let worker = {
            let stale = stale.clone();
            thread::spawn(move || stale.sync_calendar(id, window.0, window.1).map(|_| ()))
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // The user saves a new selection (or clears it) while that read is in flight.
        let catalog = current.google_calendars().unwrap();
        assert_eq!(catalog.authority, google_stamp(1, 0));
        current
            .configure_google_calendars(catalog.authority, &reselected)
            .unwrap();
        let chosen = current.account_state(id).unwrap();
        assert_eq!(chosen.native_calendar_ids, reselected);
        assert_eq!(chosen.selection_generation, 1);
        assert_eq!(
            chosen.capabilities.calendar,
            if reselected.is_empty() {
                Capability::NotRequested
            } else {
                Capability::Unknown
            }
        );

        // The stale read reports its own failure, but neither withdraws its loss from the
        // newer selection nor marks that selection failed.
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().code(), "transient");
        assert_eq!(current.account_state(id).unwrap(), chosen);
        assert_eq!(
            current.google_authority().unwrap(),
            Some(google_stamp(1, 1))
        );

        // The newer selection syncs on its own terms.
        let read = current.sync_calendar(id, window.0, window.1).unwrap();
        assert_eq!(read.events.len(), reselected.len());
        assert_eq!(
            current.account_state(id).unwrap().capabilities.calendar,
            if reselected.is_empty() {
                Capability::NotRequested
            } else {
                Capability::Available
            }
        );
    }
}

/// A Google read parked under one selection is refused account-wide (its calendar and
/// then the calendar list answer 403) only after the user saved another selection, which
/// may already have synced. That late denial belongs to the old selection: it reports
/// `superseded` and leaves the newer selection's status, authority and copy alone, and an
/// explicitly emptied selection stays `NotRequested`.
#[test]
fn a_stale_google_account_refusal_never_denies_or_removes_a_newer_selection() {
    const WORK: &str = "work@example.invalid";
    let id = IntegrationId::Google;
    for reselected in [vec![WORK.to_owned()], Vec::new()] {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let parked = Server::new(move |r| {
            if r.target.contains("/events?") {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                return google_refusal(403);
            }
            if r.target.starts_with("/api/users/me/calendarList?") {
                return google_refusal(403);
            }
            panic!("unexpected request {}", r.target)
        });
        let live = Server::new(|r| {
            if r.target.starts_with("/api/users/me/calendarList?") {
                return reply(
                    200,
                    json!({"items":[
                        {"id":GOOGLE_CALENDAR,"summary":"Reader Example","primary":true,"accessRole":"owner"},
                        {"id":WORK,"summary":"Synthetic work","accessRole":"reader"}
                    ]}),
                );
            }
            if r.target.contains("/calendars/work") {
                return reply(200, google_calendar_page("work"));
            }
            panic!("unexpected request {}", r.target)
        });
        let tmp = tempfile::tempdir().unwrap();
        let secrets = Arc::new(FakeSecretStore::default());
        seed(
            tmp.path(),
            id,
            secrets.as_ref(),
            Utc::now().timestamp() + 3600,
        );
        let open = |server: &Server| {
            facade(
                tmp.path(),
                id,
                server,
                secrets.clone(),
                Arc::new(FakeBrowserLauncher::default()),
                Arc::new(|_| {}),
            )
        };
        let stale = Arc::new(open(&parked));
        let current = open(&live);
        let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
        let worker = {
            let stale = stale.clone();
            thread::spawn(move || stale.sync_calendar(id, window.0, window.1).map(|_| ()))
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        // While the old read is parked, the user saves another selection (or clears it),
        // and a nonempty one syncs successfully.
        let catalog = current.google_calendars().unwrap();
        assert_eq!(catalog.authority, google_stamp(1, 0));
        current
            .configure_google_calendars(catalog.authority, &reselected)
            .unwrap();
        let read = current.sync_calendar(id, window.0, window.1).unwrap();
        assert_eq!(read.events.len(), reselected.len());
        let chosen = current.account_state(id).unwrap();
        assert_eq!(chosen.native_calendar_ids, reselected);
        assert_eq!(chosen.selection_generation, 1);
        assert_eq!(chosen.calendar_fence, 0);
        assert_eq!(
            chosen.capabilities.calendar,
            if reselected.is_empty() {
                Capability::NotRequested
            } else {
                Capability::Available
            }
        );
        let authority = current.google_authority().unwrap();
        assert_eq!(authority, Some(google_stamp(1, 1)));
        let copy = served_copy(&current, tmp.path(), id).unwrap();
        assert_eq!(copy.events, read.events);

        // The old read is then refused for the whole account.
        release_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
        assert_eq!(current.account_state(id).unwrap(), chosen);
        assert_eq!(current.google_authority().unwrap(), authority);
        assert_eq!(served_copy(&current, tmp.path(), id).unwrap(), copy);
        assert!(
            parked
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.target.starts_with("/api/users/me/calendarList?")),
            "the old read was refused account-wide, not for one calendar"
        );
    }
}

#[test]
fn google_photos_and_directory_work_survive_calendar_changes_but_not_sign_in_retirement() {
    let id = IntegrationId::Google;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let gone = Arc::new(AtomicBool::new(false));
    let refuse_calendars = Arc::new(AtomicBool::new(false));
    let park_photo = Arc::new(AtomicBool::new(false));
    let (vanished, refused, parking) = (gone.clone(), refuse_calendars.clone(), park_photo.clone());
    let server = Server::new(move |r| {
        if r.target.starts_with("/directory/photo") {
            if parking.swap(false, Ordering::SeqCst) {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            return Response {
                status: 200,
                body: b"synthetic-photo".to_vec(),
            };
        }
        // Calendar-only refusal: the calendar list and every calendar, not the directory.
        if refused.load(Ordering::SeqCst) {
            return google_refusal(403);
        }
        if let Some(response) = google_identity_fixture(r) {
            return response;
        }
        if r.target.contains("/calendars/gone") && vanished.load(Ordering::SeqCst) {
            return google_refusal(404);
        }
        if r.target.contains("/events?") {
            return reply(200, google_calendar_page("work"));
        }
        panic!("unexpected request {}", r.target)
    });
    // The facade canonicalizes its root; keep every path assertion on
    // the same filesystem identity, including macOS's /var symlink.
    let tmp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let path = account_dir(tmp.path(), id).join(STATE_FILE);
    let mut state: AccountState = crate::store::read_json_opt(&path).unwrap().unwrap();
    state.native_calendar_ids = vec![GOOGLE_CALENDAR.into(), "gone@example.invalid".into()];
    write_json_private(&path, &state).unwrap();
    let i = Arc::new(facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    ));
    let remote = DirectoryPerson {
        remote_id: "people/synthetic".into(),
        name: None,
        email: None,
        photo_url: Some(format!("{}/directory/photo", server.base)),
        source: None,
    };
    let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));
    let account = account_generation(tmp.path(), id).unwrap();
    let photo = i
        .cache_photo(id, "p-123abc", &remote, &|_| true)
        .unwrap()
        .unwrap();
    let served = || cached_photo(tmp.path(), "p-123abc");
    assert_eq!(served(), Some(photo.clone()));

    // A background sync loses a selected calendar: only calendar authority moves.
    gone.store(true, Ordering::SeqCst);
    i.sync_calendar(id, window.0, window.1).unwrap();
    assert_eq!(
        i.google_authority().unwrap(),
        Some(google_stamp(account, 1))
    );
    assert_eq!(served(), Some(photo.clone()));

    // An explicit reselection: the old calendar authority is rejected, photos stay.
    let catalog = i.google_calendars().unwrap();
    assert_eq!(catalog.authority, google_stamp(account, 1));
    i.configure_google_calendars(catalog.authority, &[GOOGLE_CALENDAR.into()])
        .unwrap();
    assert_eq!(
        i.configure_google_calendars(catalog.authority, &[GOOGLE_CALENDAR.into()])
            .unwrap_err()
            .code(),
        "superseded"
    );
    assert_eq!(
        i.google_authority().unwrap(),
        Some(google_stamp(account, 2))
    );
    assert_eq!(served(), Some(photo.clone()));

    // A calendar-only denial fences the calendar copy, not the directory or its photos.
    refuse_calendars.store(true, Ordering::SeqCst);
    assert_eq!(
        i.sync_calendar(id, window.0, window.1).unwrap_err().code(),
        "forbidden"
    );
    refuse_calendars.store(false, Ordering::SeqCst);
    let state = i.account_state(id).unwrap();
    assert_eq!(state.capabilities.calendar, Capability::Denied);
    assert_ne!(state.capabilities.directory, Capability::Denied);
    assert_eq!(state.generation, account);
    assert_eq!(state.selection_generation, 3);
    assert_eq!(served(), Some(photo.clone()));

    // A photo download in flight across a selection change still commits and is served.
    park_photo.store(true, Ordering::SeqCst);
    let worker = {
        let (i, remote) = (i.clone(), remote.clone());
        thread::spawn(move || i.cache_photo(id, "p-abc456", &remote, &|_| true))
    };
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let live = i.google_authority().unwrap().unwrap();
    i.configure_google_calendars(live, &[]).unwrap();
    release_tx.send(()).unwrap();
    let later = worker.join().unwrap().unwrap().unwrap();
    assert_eq!(cached_photo(tmp.path(), "p-abc456"), Some(later));
    // A directory-derived library change for the account generation still commits.
    i.commit_for_generation(id, account, || Ok(())).unwrap();
    assert_eq!(served(), Some(photo));

    // Retiring the sign-in (its client changed) still fences photos and directory work.
    let replacement = Integrations::new(
        tmp.path(),
        authoritative(
            tmp.path(),
            IntegrationsConfig {
                integration: [(
                    id.to_string(),
                    IntegrationSettings {
                        client_id: Some("synthetic-replacement-client".into()),
                        ..settings()
                    },
                )]
                .into_iter()
                .collect(),
            },
        ),
        secrets,
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
    .unwrap();
    assert!(
        replacement
            .retire_changed_client(id, &settings())
            .unwrap()
            .sign_in_retired
    );
    assert_eq!(served(), None);
    assert_eq!(cached_photo(tmp.path(), "p-abc456"), None);
    assert_eq!(
        i.commit_for_generation(id, account, || Ok(()))
            .unwrap_err()
            .code(),
        "superseded"
    );
    assert_eq!(replacement.google_authority().unwrap(), None);
}

#[test]
fn a_google_photo_downloaded_while_workspace_is_turned_off_is_never_committed() {
    let id = IntegrationId::Google;
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let server = Server::new(move |_| {
        entered_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        Response {
            status: 200,
            body: b"synthetic-photo".to_vec(),
        }
    });
    let tmp = tempfile::tempdir().unwrap();
    let i = Arc::new(google_selecting(tmp.path(), &server, &[GOOGLE_CALENDAR]));
    let remote = DirectoryPerson {
        remote_id: "remote".into(),
        name: None,
        email: None,
        photo_url: Some(format!("{}/directory/photo", server.base)),
        source: None,
    };
    let worker = {
        let i = i.clone();
        thread::spawn(move || i.cache_photo(id, "p-123abc", &remote, &|_| true))
    };
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    authoritative(
        tmp.path(),
        IntegrationsConfig {
            integration: [(
                id.to_string(),
                IntegrationSettings {
                    workspace: false,
                    ..settings()
                },
            )]
            .into_iter()
            .collect(),
        },
    );
    release_tx.send(()).unwrap();
    assert_eq!(worker.join().unwrap().unwrap_err().code(), "superseded");
    assert!(
        !account_dir(tmp.path(), id)
            .join("photos/1/p-123abc.img")
            .exists()
    );
    assert_eq!(cached_photo(tmp.path(), "p-123abc"), None);
}

#[test]
fn an_unreadable_trust_store_stops_only_provider_traffic_never_native_calendars_or_sign_out() {
    use broscribe_platform::fakes::FakeTrustStore;
    use broscribe_platform::{
        CalendarAuthorization, CalendarProvider, FakeSystemCalendar, SystemCalendarEvent,
        SystemCalendarInfo,
    };
    let id = IntegrationId::Google;
    let server = Server::new(|_| reply(200, json!({"items":[], "people":[]})));
    let tmp = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FakeSecretStore::default());
    seed(
        tmp.path(),
        id,
        secrets.as_ref(),
        Utc::now().timestamp() + 3600,
    );
    let native = Arc::new(FakeSystemCalendar::default());
    *native.authorization.lock().unwrap() = CalendarAuthorization::Granted;
    *native.calendars.lock().unwrap() = vec![SystemCalendarInfo {
        id: "work".into(),
        title: "Synthetic work".into(),
        source: "Synthetic source".into(),
    }];
    let now = Utc::now().timestamp();
    *native.events.lock().unwrap() = vec![SystemCalendarEvent {
        id: "native-event".into(),
        calendar_id: "work".into(),
        ical_uid: None,
        original_start_s: now,
        title: Some("Synthetic local meeting".into()),
        start_s: now,
        end_s: now + 3600,
        links: Vec::new(),
        attendees: Vec::new(),
    }];
    let browser = Arc::new(FakeBrowserLauncher::default());
    let unreadable = FakeTrustStore::default();
    let i = facade(
        tmp.path(),
        id,
        &server,
        secrets.clone(),
        browser.clone(),
        Arc::new(|_| panic!("an unavailable transport is not a sign-in failure")),
    )
    .with_trust_store(&unreadable)
    .with_system_calendar(native);
    assert_eq!(unreadable.reads.load(Ordering::SeqCst), 1);
    let window = (Utc::now() - Delta::days(1), Utc::now() + Delta::days(1));

    // Every provider route fails closed with a stable code and sends nothing.
    for code in [
        i.sync_calendar(id, window.0, window.1).unwrap_err().code(),
        i.directory(id, "Mira").unwrap_err().code(),
        i.google_calendars().unwrap_err().code(),
        i.login(id, &LoginAttemptId::generate()).unwrap_err().code(),
    ] {
        assert_eq!(code, "trust_store_unavailable");
    }
    assert!(server.requests.lock().unwrap().is_empty());
    assert!(browser.opened.lock().unwrap().is_empty());
    // Not mistaken for a denial or a rejected sign-in: authority and credentials stand.
    let state = i.account_state(id).unwrap();
    assert_eq!(state.status, AccountStatus::Active);
    assert_eq!(state.generation, 1);
    assert_eq!(state.capabilities.calendar, Capability::Error);
    assert!(google_credential(tmp.path(), secrets.as_ref()).is_some());

    // Native calendars need no provider transport and keep working.
    let native_id = IntegrationId::SystemCalendar;
    i.connect_system_calendar(CalendarProvider::EventKit)
        .unwrap();
    i.configure_native_calendars(CalendarProvider::EventKit, &["work".into()])
        .unwrap();
    let local = i.sync_calendar(native_id, window.0, window.1).unwrap();
    assert_eq!(local.events.len(), 1);
    assert_eq!(
        served_copy(&i, tmp.path(), native_id).unwrap().events,
        local.events
    );

    // Sign-out is local: credentials and copies go without any provider request.
    i.logout(id).unwrap();
    assert!(google_credential(tmp.path(), secrets.as_ref()).is_none());
    i.logout(native_id).unwrap();
    assert!(server.requests.lock().unwrap().is_empty());

    // A readable store is read once and used as the only trust anchors.
    let corporate = FakeTrustStore {
        policy: Mutex::new(Some(broscribe_platform::TlsTrust::Certificates(vec![
            b"synthetic DER root".to_vec(),
        ]))),
        ..Default::default()
    };
    assert!(Http::from_trust_store(&corporate).ready().is_ok());
    assert_eq!(corporate.reads.load(Ordering::SeqCst), 1);
    let empty = FakeTrustStore {
        policy: Mutex::new(Some(broscribe_platform::TlsTrust::Certificates(Vec::new()))),
        ..Default::default()
    };
    assert_eq!(
        Http::from_trust_store(&empty).ready().err().unwrap().code(),
        "trust_store_unavailable"
    );
}

#[test]
fn plain_sign_out_keeps_unknown_photo_entries_and_removes_only_cached_photos() {
    use crate::store::{PHOTOS_DIR, provider_photo_dir, write_private};
    let temp = tempfile::tempdir().unwrap();
    let service = Integrations::new(
        temp.path(),
        IntegrationsConfig::default(),
        Arc::new(FakeSecretStore::default()),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
    .unwrap();
    for id in [IntegrationId::Google, IntegrationId::SystemCalendar] {
        let account = account_dir(temp.path(), id);
        let photos = account.join(PHOTOS_DIR);
        let generation = provider_photo_dir(&account, 5);
        let empty_after = provider_photo_dir(&account, 6);
        for path in [
            photos.join("p-987abc.img"),
            generation.join("p-123abc.img"),
            empty_after.join("p-123abc.img"),
        ] {
            write_private(&path, b"synthetic photo").unwrap();
        }
        let file = generation.join("personal.txt");
        let family = photos.join("p-family.img");
        std::fs::write(&family, b"synthetic user image").unwrap();
        let folder = photos.join("archive");
        std::fs::write(&file, b"synthetic user file").unwrap();
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("p-personal.img"), b"synthetic user file").unwrap();
        service.logout(id).unwrap();
        assert!(file.exists() && family.exists() && folder.join("p-personal.img").exists());
        assert!(!photos.join("p-987abc.img").exists() && !generation.join("p-123abc.img").exists());
        assert!(!empty_after.exists());
    }
}

#[cfg(unix)]
#[test]
fn local_sign_out_refuses_linked_integration_components_before_writing() {
    use std::os::unix::fs::symlink;
    for link_parent in [false, true] {
        let profile = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_provider = if link_parent {
            outside.path().join("google")
        } else {
            outside.path().to_owned()
        };
        std::fs::create_dir_all(outside_provider.join("photos")).unwrap();
        let state = outside_provider.join(STATE_FILE);
        write_json_private(&state, &crate::store::AccountState::default()).unwrap();
        let before = std::fs::read(&state).unwrap();
        let photo = outside_provider.join("photos/p-123abc.img");
        std::fs::write(&photo, b"synthetic outside photo").unwrap();
        let service = Integrations::new(
            profile.path(),
            IntegrationsConfig::default(),
            Arc::new(FakeSecretStore::default()),
            Arc::new(FakeBrowserLauncher::default()),
            Arc::new(|_| {}),
        )
        .unwrap();
        let integrations = profile.path().join("integrations");
        if link_parent {
            symlink(outside.path(), integrations).unwrap();
        } else {
            std::fs::create_dir(&integrations).unwrap();
            symlink(
                outside.path(),
                account_dir(profile.path(), IntegrationId::Google),
            )
            .unwrap();
        }
        let error = service.logout(IntegrationId::Google).unwrap_err();
        assert_eq!(error.code(), "unsafe_integration_path");
        let result = service.logout_and_revoke(IntegrationId::Google).unwrap();
        let local = result.local.unwrap_err();
        assert_eq!(local.code, "unsafe_integration_path");
        assert!(!local.message.contains(&*profile.path().to_string_lossy()));
        assert_eq!(
            result.remote_revocation,
            RevocationOutcome::Failed {
                code: "not_attempted".into()
            }
        );
        assert!(!outside_provider.join("state.lock").exists());
        assert_eq!(std::fs::read(&state).unwrap(), before);
        assert_eq!(std::fs::read(&photo).unwrap(), b"synthetic outside photo");
    }
}

#[cfg(unix)]
#[test]
fn linked_profile_root_uses_the_target_for_integration_state() {
    let top = tempfile::tempdir().unwrap();
    let actual = top.path().join("on-another-disk");
    let alias = top.path().join("profile");
    std::fs::create_dir(&actual).unwrap();
    std::os::unix::fs::symlink(&actual, &alias).unwrap();
    let service = Integrations::new(
        &alias,
        IntegrationsConfig::default(),
        Arc::new(FakeSecretStore::default()),
        Arc::new(FakeBrowserLauncher::default()),
        Arc::new(|_| {}),
    )
    .unwrap();
    service.logout(IntegrationId::Google).unwrap();
    assert!(
        account_dir(&actual, IntegrationId::Google)
            .join(STATE_FILE)
            .is_file()
    );
}

#[cfg(unix)]
#[test]
fn transaction_refuses_linked_state_and_lock_leaves() {
    use crate::store::STATE_LOCK;
    for leaf in [STATE_FILE, STATE_LOCK] {
        let profile = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join(leaf);
        let contents = b"{\"generation\":19}";
        std::fs::write(&target, contents).unwrap();
        let service = Integrations::new(
            profile.path(),
            IntegrationsConfig::default(),
            Arc::new(FakeSecretStore::default()),
            Arc::new(FakeBrowserLauncher::default()),
            Arc::new(|_| {}),
        )
        .unwrap();
        let directory = account_dir(profile.path(), IntegrationId::Google);
        std::fs::create_dir_all(&directory).unwrap();
        let link = directory.join(leaf);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            service.logout(IntegrationId::Google).is_err(),
            "opened {leaf}"
        );
        assert_eq!(std::fs::read_link(link).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), contents);

        // Even a dangling state link is refused, rather than read as absent.
        std::fs::remove_file(target).unwrap();
        assert!(
            service.logout(IntegrationId::Google).is_err(),
            "opened dangling {leaf}"
        );
    }
}
