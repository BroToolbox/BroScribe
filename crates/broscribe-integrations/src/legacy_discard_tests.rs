//! Synthetic public API and durable-protocol recovery tests; no production data.
use crate::store::PHOTOS_DIR;

use super::*;
use std::path::{Path, PathBuf};

const PROVIDER: IntegrationId = IntegrationId::MicrosoftGraph;

struct LegacyStore {
    inner: FakeSecretStore,
    deletes: Mutex<Vec<String>>,
    failure: AtomicUsize,
    root: PathBuf,
}

impl SecretStore for LegacyStore {
    fn get(&self, key: &str) -> broscribe_platform::Result<Option<String>> {
        self.inner.get(key)
    }

    fn set(&self, key: &str, value: &str) -> broscribe_platform::Result<()> {
        self.inner.set(key, value)
    }

    fn delete(&self, key: &str) -> broscribe_platform::Result<()> {
        self.deletes.lock().unwrap().push(key.into());
        if key == tokens_secret_key(PROVIDER) {
            let registry: Value =
                serde_json::from_slice(&std::fs::read(self.root.join("accounts.json")).unwrap())
                    .unwrap();
            let state: AccountState =
                serde_json::from_slice(&std::fs::read(self.root.join(STATE_FILE)).unwrap())
                    .unwrap();
            assert!(registry["legacy_discard"].is_object());
            assert!(registry["legacy_quarantined"].as_bool().unwrap());
            assert_eq!(state.status, AccountStatus::SignedOut);
            assert_eq!(
                registry["legacy_discard"]["fenced_generation"],
                json!(state.generation)
            );
            let failure = if self.failure.load(Ordering::SeqCst) == 3 {
                3
            } else {
                self.failure.swap(0, Ordering::SeqCst)
            };
            if failure == 2 {
                self.inner.delete(key)?;
            }
            if failure != 0 {
                return Err(broscribe_platform::Error::Service {
                    code: "synthetic_cleanup_failure",
                    message: "synthetic legacy credential deletion interruption".into(),
                });
            }
        }
        self.inner.delete(key)
    }
}

struct LegacyFixture {
    tmp: tempfile::TempDir,
    store: Arc<LegacyStore>,
    service: Integrations,
    tenant: Arc<AtomicUsize>,
    _server: Server,
}

impl LegacyFixture {
    fn new(working_legacy: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = account_dir(tmp.path(), PROVIDER);
        let store = Arc::new(LegacyStore {
            inner: FakeSecretStore::default(),
            deletes: Mutex::new(Vec::new()),
            failure: AtomicUsize::new(0),
            root: root.clone(),
        });
        write_json_private(
            &root.join(STATE_FILE),
            &AccountState {
                generation: 7,
                status: if working_legacy {
                    AccountStatus::Active
                } else {
                    AccountStatus::SignedOut
                },
                ..Default::default()
            },
        )
        .unwrap();
        if working_legacy {
            store
                .set(
                    &tokens_secret_key(PROVIDER),
                    "synthetic-unverified-legacy-credential",
                )
                .unwrap();
            write_json_private(&root.join(CALENDAR_FILE), &CalendarCache::default()).unwrap();
            crate::store::write_private(&root.join("photos/p-abc789.img"), b"legacy-photo")
                .unwrap();
        }
        let tenant = Arc::new(AtomicUsize::new(1));
        let current = tenant.clone();
        let server = Server::new(move |request| {
            if request.target.starts_with("/authorize") {
                return reply(200, json!({}));
            }
            if request.target == "/token" {
                let tenant = current.load(Ordering::SeqCst);
                return reply(
                    200,
                    json!({
                        "access_token":format!("legacy-test-access-{tenant}"),
                        "refresh_token":format!("legacy-test-refresh-{tenant}"),
                        "expires_in":3600,
                        "scope":"Calendars.Read User.Read User.ReadBasic.All"
                    }),
                );
            }
            let tenant = request
                .authorization
                .as_deref()
                .unwrap_or("")
                .trim_start_matches("Bearer legacy-test-access-");
            if request.target.starts_with("/api/me?") {
                return reply(
                    200,
                    json!({"id":"same-reader","mail":format!("reader-{tenant}@example.invalid")}),
                );
            }
            if request.target.starts_with("/api/organization?") {
                return reply(200, json!({"value":[{"id":format!("tenant-{tenant}")}]}));
            }
            if request.target.starts_with("/api/me/calendars?") {
                return reply(
                    200,
                    json!({"value":[{"id":"calendar","owner":{"address":format!("reader-{tenant}@example.invalid")}}]}),
                );
            }
            if request.target.contains("calendarView") {
                let now = Utc::now();
                return reply(
                    200,
                    json!({"value":[{"id":"same-event","subject":"Synthetic meeting","start":{"dateTime":now.to_rfc3339()},"end":{"dateTime":(now+Delta::hours(1)).to_rfc3339()}}]}),
                );
            }
            reply(200, json!({"id":"calendar"}))
        });
        let service = facade(
            tmp.path(),
            PROVIDER,
            &server,
            store.clone(),
            Arc::new(CallbackBrowser::default()),
            Arc::new(|_| {}),
        );
        Self {
            tmp,
            store,
            service,
            tenant,
            _server: server,
        }
    }

    fn root(&self) -> PathBuf {
        account_dir(self.tmp.path(), PROVIDER)
    }

    fn quarantine_generation(&self) -> u64 {
        assert_eq!(
            self.service.legacy_microsoft_status(PROVIDER).unwrap(),
            LegacyMicrosoftStatus::Quarantined
        );
        self.service.account_state(PROVIDER).unwrap().generation
    }

    fn add_selected(&self, tenant: usize) -> MicrosoftAccount {
        self.tenant.store(tenant, Ordering::SeqCst);
        let account = self
            .service
            .add_account(PROVIDER, &LoginAttemptId::generate())
            .unwrap();
        let account = self
            .service
            .configure_calendars(
                &account.account,
                account.generation,
                account.selection_generation,
                &[CalendarTarget::Own {
                    calendar_id: "calendar".into(),
                }],
            )
            .unwrap();
        self.service
            .sync_account(&account.account, Utc::now(), Utc::now() + Delta::days(1))
            .unwrap();
        self.service
            .list_accounts(Some(PROVIDER))
            .unwrap()
            .into_iter()
            .find(|found| found.account == account.account)
            .unwrap()
    }

    fn registry(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.root().join("accounts.json")).unwrap()).unwrap()
    }

    fn write_registry(&self, registry: &Value) {
        write_json_private(&self.root().join("accounts.json"), registry).unwrap();
    }
}

fn state_bytes(root: &Path) -> Vec<u8> {
    std::fs::read(root.join(STATE_FILE)).unwrap()
}

#[test]
fn explicit_legacy_discard_preserves_two_registered_tenants_and_google_at_every_crash_stage() {
    // Durable snapshots: before intent, intent persisted, SignedOut fence, token removed,
    // calendar removed, photos removed, quarantine cleared, intent removed.
    for stage in 0..8 {
        let h = LegacyFixture::new(true);
        let generation = h.quarantine_generation();
        let a = h.add_selected(1);
        let b = h.add_selected(2);
        let accounts = h.service.list_accounts(Some(PROVIDER)).unwrap();
        let cache_a = h.service.cached_account_calendar(&a.account).unwrap();
        let cache_b = h.service.cached_account_calendar(&b.account).unwrap();
        seed(
            h.tmp.path(),
            IntegrationId::Google,
            h.store.as_ref(),
            Utc::now().timestamp() + 3600,
        );
        let google_root = account_dir(h.tmp.path(), IntegrationId::Google);
        let google_state = state_bytes(&google_root);
        let google_secret = h
            .store
            .get(&tokens_secret_key(IntegrationId::Google))
            .unwrap();
        let mut registry = h.registry();
        if (1..7).contains(&stage) {
            registry["legacy_discard"] =
                json!({"from_generation":generation,"fenced_generation":generation+1});
        }
        if stage >= 2 {
            write_json_private(
                &h.root().join(STATE_FILE),
                &AccountState {
                    generation: generation + 1,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        if stage >= 3 {
            h.store.inner.delete(&tokens_secret_key(PROVIDER)).unwrap();
        }
        if stage >= 4 {
            std::fs::remove_file(h.root().join(CALENDAR_FILE)).unwrap();
        }
        if stage >= 5 {
            std::fs::remove_dir_all(h.root().join(PHOTOS_DIR)).unwrap();
        }
        if stage >= 6 {
            registry["legacy_quarantined"] = json!(false);
        }
        h.write_registry(&registry);
        h.store.deletes.lock().unwrap().clear();
        let registered_secrets: BTreeMap<_, _> = h
            .store
            .inner
            .secrets
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.contains("/accounts/"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        assert_eq!(
            h.service
                .discard_legacy_microsoft(PROVIDER, generation)
                .unwrap(),
            LegacyMicrosoftStatus::None
        );
        assert_eq!(
            h.service.legacy_microsoft_status(PROVIDER).unwrap(),
            LegacyMicrosoftStatus::None
        );
        let state = h.service.account_state(PROVIDER).unwrap();
        assert_eq!(state.status, AccountStatus::SignedOut);
        assert_eq!(state.generation, generation + 1);
        assert!(h.store.get(&tokens_secret_key(PROVIDER)).unwrap().is_none());
        assert!(!h.root().join(CALENDAR_FILE).exists());
        assert!(!h.root().join(PHOTOS_DIR).exists());
        assert!(h.registry().get("legacy_discard").is_none());
        assert_eq!(h.service.list_accounts(Some(PROVIDER)).unwrap(), accounts);
        assert_eq!(
            h.service.cached_account_calendar(&a.account).unwrap(),
            cache_a
        );
        assert_eq!(
            h.service.cached_account_calendar(&b.account).unwrap(),
            cache_b
        );
        let after: BTreeMap<_, _> = h
            .store
            .inner
            .secrets
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.contains("/accounts/"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        assert_eq!(after, registered_secrets);
        assert_eq!(state_bytes(&google_root), google_state);
        assert_eq!(
            h.store
                .get(&tokens_secret_key(IntegrationId::Google))
                .unwrap(),
            google_secret
        );
        let deletes = h.store.deletes.lock().unwrap().clone();
        assert!(
            deletes
                .iter()
                .all(|key| key == &tokens_secret_key(PROVIDER))
        );
        assert_eq!(
            h.service
                .discard_legacy_microsoft(PROVIDER, generation)
                .unwrap(),
            LegacyMicrosoftStatus::None
        );
        assert_eq!(*h.store.deletes.lock().unwrap(), deletes);
    }
}

#[test]
fn interrupted_secret_cleanup_recovers_before_the_signed_out_no_slot_branch() {
    for failure in [1, 2] {
        let h = LegacyFixture::new(true);
        let generation = h.quarantine_generation();
        h.store.failure.store(failure, Ordering::SeqCst);
        assert_eq!(
            h.service
                .discard_legacy_microsoft(PROVIDER, generation)
                .unwrap_err()
                .code(),
            "synthetic_cleanup_failure"
        );
        let pending = h.registry();
        assert_eq!(pending["legacy_quarantined"], json!(true));
        assert!(pending["legacy_discard"].is_object());
        let state: AccountState = serde_json::from_slice(&state_bytes(&h.root())).unwrap();
        assert_eq!(state.status, AccountStatus::SignedOut);
        assert_eq!(state.generation, generation + 1);
        assert_eq!(
            h.store.get(&tokens_secret_key(PROVIDER)).unwrap().is_some(),
            failure == 1
        );
        // The stale original fence identifies the already durable operation; recovery must
        // complete it before deciding there is no longer an active provider-wide slot.
        assert_eq!(
            h.service
                .discard_legacy_microsoft(PROVIDER, generation)
                .unwrap(),
            LegacyMicrosoftStatus::None
        );
        assert!(h.registry().get("legacy_discard").is_none());
        assert!(h.store.get(&tokens_secret_key(PROVIDER)).unwrap().is_none());
    }
}

#[test]
fn filesystem_cleanup_failure_retains_intent_and_quarantine_until_recovery() {
    for calendar_failure in [false, true] {
        let h = LegacyFixture::new(true);
        let generation = h.quarantine_generation();
        if calendar_failure {
            std::fs::remove_file(h.root().join(CALENDAR_FILE)).unwrap();
            std::fs::create_dir(h.root().join(CALENDAR_FILE)).unwrap();
        } else {
            std::fs::remove_dir_all(h.root().join(PHOTOS_DIR)).unwrap();
            std::fs::write(h.root().join(PHOTOS_DIR), b"synthetic wrong file type").unwrap();
        }
        assert!(
            h.service
                .discard_legacy_microsoft(PROVIDER, generation)
                .is_err()
        );
        assert!(h.registry()["legacy_discard"].is_object());
        assert_eq!(h.registry()["legacy_quarantined"], json!(true));
        if calendar_failure {
            std::fs::remove_dir(h.root().join(CALENDAR_FILE)).unwrap();
        } else {
            std::fs::remove_file(h.root().join(PHOTOS_DIR)).unwrap();
        }
        assert_eq!(
            h.service.legacy_microsoft_status(PROVIDER).unwrap(),
            LegacyMicrosoftStatus::None
        );
        assert!(h.registry().get("legacy_discard").is_none());
    }
}

#[test]
fn stale_generation_foreign_provider_and_failed_only_slots_never_discard_credentials() {
    let h = LegacyFixture::new(true);
    let generation = h.quarantine_generation();
    let before = state_bytes(&h.root());
    let credentials = h.store.inner.secrets.lock().unwrap().clone();
    assert_eq!(
        h.service
            .discard_legacy_microsoft(PROVIDER, generation - 1)
            .unwrap_err()
            .code(),
        "superseded"
    );
    for provider in [IntegrationId::Google, IntegrationId::SystemCalendar] {
        assert_eq!(
            h.service
                .discard_legacy_microsoft(provider, generation)
                .unwrap_err()
                .code(),
            "account_required"
        );
    }
    assert_eq!(state_bytes(&h.root()), before);
    assert_eq!(*h.store.inner.secrets.lock().unwrap(), credentials);
    assert!(h.store.deletes.lock().unwrap().is_empty());
    assert!(h.root().join(CALENDAR_FILE).exists());
    assert!(h.root().join("photos/p-abc789.img").exists());

    let h = LegacyFixture::new(false);
    let before = state_bytes(&h.root());
    assert_eq!(
        h.service.discard_legacy_microsoft(PROVIDER, 999).unwrap(),
        LegacyMicrosoftStatus::None
    );
    assert_eq!(state_bytes(&h.root()), before);
    assert!(h.store.inner.secrets.lock().unwrap().is_empty());
    assert!(h.store.deletes.lock().unwrap().is_empty());
}

#[test]
fn unexpected_legacy_generations_and_malformed_intents_fail_closed_before_deletion() {
    for invalid_fence in [false, true] {
        let h = LegacyFixture::new(true);
        let generation = h.quarantine_generation();
        let a = h.add_selected(1);
        let mut registry = h.registry();
        let accounts = registry["accounts"].clone();
        registry["legacy_discard"] = json!({"from_generation":generation,"fenced_generation":generation+if invalid_fence {2}else{1}});
        if !invalid_fence {
            write_json_private(
                &h.root().join(STATE_FILE),
                &AccountState {
                    generation: generation + 2,
                    status: AccountStatus::Active,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        h.write_registry(&registry);
        let state = state_bytes(&h.root());
        let credentials = h.store.inner.secrets.lock().unwrap().clone();
        assert_eq!(
            h.service
                .discard_legacy_microsoft(PROVIDER, generation)
                .unwrap_err()
                .code(),
            if invalid_fence {
                "invalid_legacy_discard"
            } else {
                "superseded"
            }
        );
        assert_eq!(state_bytes(&h.root()), state);
        assert_eq!(*h.store.inner.secrets.lock().unwrap(), credentials);
        assert!(h.store.deletes.lock().unwrap().is_empty());
        assert_eq!(h.registry()["accounts"], accounts);
        assert!(
            h.root()
                .join("accounts")
                .join(a.account.account_id.as_str())
                .join(CALENDAR_FILE)
                .exists()
        );
    }
    let h = LegacyFixture::new(true);
    let generation = h.quarantine_generation();
    h.store
        .set(
            &tokens_secret_key(IntegrationId::Google),
            "synthetic unrelated Google credential",
        )
        .unwrap();
    let mut registry = h.registry();
    registry["legacy_discard"] = json!({"from_generation":generation,"fenced_generation":generation+1,"key":"integration/google/tokens","path":"accounts"});
    h.write_registry(&registry);
    let credentials = h.store.inner.secrets.lock().unwrap().clone();
    assert!(h.service.legacy_microsoft_status(PROVIDER).is_err());
    assert_eq!(*h.store.inner.secrets.lock().unwrap(), credentials);
    assert!(h.store.deletes.lock().unwrap().is_empty());
}

#[test]
fn legacy_only_cleanup_failure_never_disables_registered_account_public_reads() {
    let h = LegacyFixture::new(true);
    let generation = h.quarantine_generation();
    let a = h.add_selected(1);
    let b = h.add_selected(2);
    let accounts = h.service.list_accounts(Some(PROVIDER)).unwrap();
    let authority_a = h.service.account_authority(&a.account).unwrap();
    let authority_b = h.service.account_authority(&b.account).unwrap();
    let cache_a = h.service.cached_account_calendar(&a.account).unwrap();
    let cache_b = h.service.cached_account_calendar(&b.account).unwrap();
    let rows = h.registry()["accounts"].clone();
    let credentials = h.store.inner.secrets.lock().unwrap().clone();
    h.store.failure.store(3, Ordering::SeqCst);
    assert_eq!(
        h.service
            .discard_legacy_microsoft(PROVIDER, generation)
            .unwrap_err()
            .code(),
        "synthetic_cleanup_failure"
    );
    assert_eq!(
        h.service.legacy_microsoft_status(PROVIDER).unwrap(),
        LegacyMicrosoftStatus::Quarantined
    );
    assert_eq!(h.service.list_accounts(Some(PROVIDER)).unwrap(), accounts);
    assert_eq!(
        h.service.account_authority(&a.account).unwrap(),
        authority_a
    );
    assert_eq!(
        h.service.account_authority(&b.account).unwrap(),
        authority_b
    );
    assert_eq!(
        h.service.cached_account_calendar(&a.account).unwrap(),
        cache_a
    );
    assert_eq!(
        h.service.cached_account_calendar(&b.account).unwrap(),
        cache_b
    );
    h.service.keep_account_alive(&a.account).unwrap();
    h.service.keep_account_alive(&b.account).unwrap();
    assert_eq!(h.registry()["accounts"], rows);
    assert_eq!(*h.store.inner.secrets.lock().unwrap(), credentials);
    assert!(h.registry()["legacy_discard"].is_object());
    h.store.failure.store(0, Ordering::SeqCst);
    assert_eq!(
        h.service
            .discard_legacy_microsoft(PROVIDER, generation)
            .unwrap(),
        LegacyMicrosoftStatus::None
    );
}
