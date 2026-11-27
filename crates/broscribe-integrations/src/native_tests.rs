//! System-calendar service behavior through the platform fake. Synthetic data only: no
//! EventKit, permission prompt, secret store, browser or HTTP is ever reached.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use broscribe_platform::fakes::FakeBrowserLauncher;
use broscribe_platform::{
    BrowserLauncher, CalendarAuthorization, CalendarProvider, CalendarQuery, FakeSystemCalendar,
    SecretStore, SystemCalendar, SystemCalendarAttendee, SystemCalendarEvent, SystemCalendarInfo,
    UnsupportedSystemCalendar,
};
use chrono::{Duration as Delta, Utc};

use crate::store::{CALENDAR_FILE, STATE_FILE, read_json_opt, write_json_private};
use crate::*;

const ID: IntegrationId = IntegrationId::SystemCalendar;

/// System calendars never store credentials: any secret-store access fails the test,
/// except Google's status reading whether its client secret is stored.
struct NoSecrets;
impl SecretStore for NoSecrets {
    fn get(&self, key: &str) -> broscribe_platform::Result<Option<String>> {
        if key == client_secret_key(IntegrationId::Google) {
            return Ok(None);
        }
        panic!("system calendar read secret {key}")
    }
    fn set(&self, key: &str, _: &str) -> broscribe_platform::Result<()> {
        panic!("system calendar wrote secret {key}")
    }
    fn delete(&self, key: &str) -> broscribe_platform::Result<()> {
        panic!("system calendar deleted secret {key}")
    }
}

/// The fake, plus an optional gate that parks the next permission request or event query
/// until the test releases it (the OS answering slowly).
#[derive(Default)]
struct Gated {
    fake: FakeSystemCalendar,
    gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    authorization_gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
}
impl Gated {
    /// Returns (entered, release).
    fn hold(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *self.gate.lock().unwrap() = Some((entered_tx, release_rx));
        (entered_rx, release_tx)
    }
    fn hold_authorization(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *self.authorization_gate.lock().unwrap() = Some((entered_tx, release_rx));
        (entered_rx, release_tx)
    }
    fn pass(&self) {
        let gate = self.gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
    }
    fn set_authorization(&self, value: CalendarAuthorization) {
        *self.fake.authorization.lock().unwrap() = value;
    }
}
impl SystemCalendar for Gated {
    fn authorization(&self) -> broscribe_platform::Result<CalendarAuthorization> {
        let observed = self.fake.authorization();
        let gate = self.authorization_gate.lock().unwrap().take();
        if let Some((entered, release)) = gate {
            entered.send(()).unwrap();
            release.recv().unwrap();
        }
        observed
    }
    fn request_access(&self) -> broscribe_platform::Result<CalendarAuthorization> {
        self.pass();
        self.fake.request_access()
    }
    fn calendars(&self) -> broscribe_platform::Result<Vec<SystemCalendarInfo>> {
        self.fake.calendars()
    }
    fn events(
        &self,
        from_s: i64,
        to_s: i64,
        calendar_ids: &[String],
    ) -> broscribe_platform::Result<Vec<SystemCalendarEvent>> {
        self.pass();
        self.fake.events(from_s, to_s, calendar_ids)
    }
}

struct Harness {
    dir: tempfile::TempDir,
    service: Integrations,
    browser: Arc<FakeBrowserLauncher>,
    notifications: Arc<AtomicUsize>,
}
impl Harness {
    fn new(calendar: Option<Arc<dyn SystemCalendar>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let browser = Arc::new(FakeBrowserLauncher::default());
        let notifications = Arc::new(AtomicUsize::new(0));
        let counter = notifications.clone();
        let service = Integrations::new(
            dir.path(),
            IntegrationsConfig::default(),
            Arc::new(NoSecrets),
            browser.clone() as Arc<dyn BrowserLauncher>,
            Arc::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
        let service = match calendar {
            Some(calendar) => service.with_system_calendar(calendar),
            None => service,
        };
        Self {
            dir,
            service,
            browser,
            notifications,
        }
    }
    fn account(&self) -> std::path::PathBuf {
        account_dir(&self.dir.path().canonicalize().unwrap(), ID)
    }
    fn cache_file_exists(&self) -> bool {
        self.account().join(CALENDAR_FILE).exists()
    }
    fn state(&self) -> AccountState {
        self.service.status(ID).unwrap().state
    }
    /// The copy a recording snapshot may take under the live generation.
    fn served(&self) -> Result<CalendarCache> {
        crate::tests::served_copy(&self.service, self.dir.path(), ID)
    }
    fn sync(&self) -> Result<CalendarCache> {
        let from = Utc::now() - Delta::days(1);
        self.service.sync_calendar(ID, from, from + Delta::days(31))
    }
    fn connect(&self) -> Result<IntegrationStatus> {
        self.service
            .connect_system_calendar(CalendarProvider::EventKit)
    }
    fn with(calendar: Arc<dyn SystemCalendar>) -> Self {
        Self::new(Some(calendar))
    }
    /// Nothing reached a browser, a sign-in notification or (via `NoSecrets`) a credential.
    fn assert_no_oauth_side_effects(&self) {
        assert!(self.browser.opened.lock().unwrap().is_empty());
        assert_eq!(self.notifications.load(Ordering::SeqCst), 0);
    }
}

fn info(id: &str) -> SystemCalendarInfo {
    SystemCalendarInfo {
        id: id.into(),
        title: format!("Synthetic {id}"),
        source: "Synthetic source".into(),
    }
}

fn event(calendar_id: &str, id: &str) -> SystemCalendarEvent {
    let start = Utc::now().timestamp() + 600;
    SystemCalendarEvent {
        id: id.into(),
        calendar_id: calendar_id.into(),
        ical_uid: None,
        original_start_s: start,
        title: Some("Synthetic sync".into()),
        start_s: start,
        end_s: start + 3600,
        links: vec!["https://meet.google.com/synthetic".into()],
        attendees: vec![SystemCalendarAttendee {
            name: Some("Casey Example".into()),
            email: Some("casey@example.invalid".into()),
            is_self: false,
        }],
    }
}

fn granted_with_two_calendars() -> Arc<Gated> {
    let calendar = Arc::new(Gated::default());
    calendar.set_authorization(CalendarAuthorization::Granted);
    *calendar.fake.calendars.lock().unwrap() = vec![info("work"), info("personal")];
    *calendar.fake.events.lock().unwrap() =
        vec![event("work", "work-1"), event("personal", "personal-1")];
    calendar
}

#[test]
fn preexisting_grant_needs_explicit_connect_and_selection_before_any_event_query() {
    let calendar = granted_with_two_calendars();
    let h = Harness::with(calendar.clone());

    // A grant alone neither connects nor lists calendars nor reads events.
    let status = h.service.status(ID).unwrap();
    assert!(status.configured);
    assert_eq!(status.state.status, AccountStatus::SignedOut);
    let native = status.system_calendar.unwrap();
    assert_eq!(native.authorization, Some(CalendarAuthorization::Granted));
    assert!(!native.helper_required);
    assert!(native.calendars.is_empty());
    assert_eq!(calendar.fake.calendar_reads.load(Ordering::SeqCst), 0);
    assert_eq!(h.sync().unwrap_err().code(), "signed_out");
    assert_eq!(
        h.service
            .configure_native_calendars(CalendarProvider::EventKit, &["work".into()])
            .unwrap_err()
            .code(),
        "signed_out"
    );
    assert_eq!(
        h.service
            .native_generation(CalendarProvider::EventKit)
            .unwrap(),
        None
    );

    // Explicit connect: no prompt for an existing grant, and no calendar selected.
    let status = h.connect().unwrap();
    assert_eq!(status.state.status, AccountStatus::Active);
    assert!(status.state.native_calendar_ids.is_empty());
    assert_eq!(calendar.fake.access_requests.load(Ordering::SeqCst), 0);
    let labels = status.system_calendar.unwrap().calendars;
    assert_eq!(labels, [info("work"), info("personal")]);
    assert!(h.sync().unwrap().events.is_empty());
    assert!(calendar.fake.event_queries.lock().unwrap().is_empty());

    // Unknown IDs are refused without changing the selection or generation.
    let generation = status.state.generation;
    assert_eq!(
        h.service
            .configure_native_calendars(
                CalendarProvider::EventKit,
                &["work".into(), "missing".into()]
            )
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    assert_eq!(h.state().generation, generation);
    assert!(h.state().native_calendar_ids.is_empty());

    let status = h
        .service
        .configure_native_calendars(CalendarProvider::EventKit, &["work".into(), "work".into()])
        .unwrap();
    assert_eq!(status.state.native_calendar_ids, ["work"]);
    assert_eq!(status.state.generation, generation + 1);
    let from = Utc::now() - Delta::days(1);
    let to = from + Delta::days(31);
    let cache = h.service.sync_calendar(ID, from, to).unwrap();
    assert_eq!(cache.events.len(), 1);
    let synced = &cache.events[0];
    assert_eq!(synced.id, "work-1");
    assert_eq!(synced.integration, ID);
    assert_eq!(synced.app.as_deref(), Some("meet"));
    assert_eq!(synced.account_generation, Some(generation + 1));
    assert_eq!(
        *calendar.fake.event_queries.lock().unwrap(),
        [CalendarQuery {
            from_s: from.timestamp(),
            to_s: to.timestamp(),
            calendar_ids: vec!["work".into()],
        }]
    );
    assert_eq!(h.served().unwrap(), cache);
    assert_eq!(
        h.service
            .native_generation(CalendarProvider::EventKit)
            .unwrap(),
        Some(generation + 1)
    );

    // A selected calendar that disappeared resolves to nothing: never "all calendars".
    *calendar.fake.calendars.lock().unwrap() = vec![info("personal")];
    assert!(h.sync().unwrap().events.is_empty());
    // An explicitly empty selection reads nothing either.
    *calendar.fake.calendars.lock().unwrap() = vec![info("work"), info("personal")];
    h.service
        .configure_native_calendars(CalendarProvider::EventKit, &[])
        .unwrap();
    assert!(h.sync().unwrap().events.is_empty());
    assert_eq!(calendar.fake.event_queries.lock().unwrap().len(), 1);

    // Keep-alive has no tokens to refresh and never notifies.
    h.service.keep_alive(ID).unwrap();
    h.assert_no_oauth_side_effects();
}

#[test]
fn refused_or_unanswered_permission_never_opts_in_even_after_a_late_grant() {
    for (answer, code) in [
        (CalendarAuthorization::Denied, "calendar_denied"),
        (CalendarAuthorization::Restricted, "calendar_restricted"),
        (CalendarAuthorization::WriteOnly, "calendar_write_only"),
        (CalendarAuthorization::NotDetermined, "calendar_pending"),
    ] {
        let calendar = Arc::new(Gated::default());
        *calendar.fake.request_result.lock().unwrap() = answer;
        *calendar.fake.calendars.lock().unwrap() = vec![info("work")];
        let h = Harness::with(calendar.clone());
        assert_eq!(h.connect().unwrap_err().code(), code);
        assert_eq!(calendar.fake.access_requests.load(Ordering::SeqCst), 1);
        assert_eq!(h.state().status, AccountStatus::SignedOut);

        // The user allows access later in System Settings: still no opt-in or reads.
        calendar.set_authorization(CalendarAuthorization::Granted);
        let status = h.service.status(ID).unwrap();
        assert_eq!(status.state.status, AccountStatus::SignedOut);
        assert!(status.system_calendar.unwrap().calendars.is_empty());
        assert_eq!(
            h.service
                .native_generation(CalendarProvider::EventKit)
                .unwrap(),
            None
        );
        assert_eq!(h.sync().unwrap_err().code(), "signed_out");
        assert!(calendar.fake.event_queries.lock().unwrap().is_empty());
        h.assert_no_oauth_side_effects();
    }

    let h = Harness::with(Arc::new(UnsupportedSystemCalendar));
    assert_eq!(h.connect().unwrap_err().code(), "unsupported");
    assert_eq!(
        h.service
            .status(ID)
            .unwrap()
            .system_calendar
            .unwrap()
            .authorization,
        Some(CalendarAuthorization::Unsupported)
    );
}

#[test]
fn without_helper_authority_status_is_unknown_and_disconnect_still_works_offline() {
    let h = Harness::new(None);
    let account = h.account();
    write_json_private(
        &account.join(STATE_FILE),
        &AccountState {
            generation: 4,
            status: AccountStatus::Active,
            native_calendar_ids: vec!["work".into()],
            ..Default::default()
        },
    )
    .unwrap();
    write_json_private(&account.join(CALENDAR_FILE), &CalendarCache::default()).unwrap();

    let status = h.service.status(ID).unwrap();
    assert!(status.configured);
    assert!(status.admin_controlled_scopes.is_empty());
    let native = status.system_calendar.clone().unwrap();
    assert_eq!(native.authorization, None);
    assert!(native.helper_required);
    assert!(native.calendars.is_empty());
    // Unknown permission is explicit on the wire and survives a round trip.
    let wire = serde_json::to_value(&status).unwrap();
    assert!(wire["system_calendar"]["authorization"].is_null());
    assert_eq!(wire["state"]["native_calendar_ids"][0], "work");
    let back: IntegrationStatus = serde_json::from_value(wire).unwrap();
    assert_eq!(back.system_calendar, Some(native));
    // Other integrations keep omitting the native field.
    let google = serde_json::to_value(h.service.status(IntegrationId::Google).unwrap()).unwrap();
    assert!(google.get("system_calendar").is_none());
    assert_eq!(
        h.service.statuses().unwrap().len(),
        IntegrationId::ALL.len()
    );

    assert_eq!(h.connect().unwrap_err().code(), "helper_required");
    assert_eq!(
        h.service
            .configure_native_calendars(CalendarProvider::EventKit, &["work".into()])
            .unwrap_err()
            .code(),
        "helper_required"
    );
    assert_eq!(h.sync().unwrap_err().code(), "helper_required");
    assert_eq!(h.served().unwrap_err().code(), "helper_required");
    assert_eq!(
        h.service
            .native_generation(CalendarProvider::EventKit)
            .unwrap_err()
            .code(),
        "helper_required"
    );

    // No directory or photos for system calendars, and no credential is attempted.
    assert!(h.service.directory(ID, "Casey").unwrap().is_empty());
    let person = DirectoryPerson {
        remote_id: "synthetic".into(),
        name: Some("Casey Example".into()),
        email: None,
        photo_url: Some("https://example.invalid/photo".into()),
        source: None,
    };
    assert_eq!(
        h.service
            .cache_photo(ID, "p-synthetic", &person, &|_| true)
            .unwrap(),
        None
    );

    h.service.logout(ID).unwrap();
    let state = h.state();
    assert_eq!(state.generation, 5);
    assert_eq!(state.status, AccountStatus::SignedOut);
    assert!(state.native_calendar_ids.is_empty());
    assert!(!h.cache_file_exists());
    // Disconnecting again still fences: the generation always moves.
    h.service.logout(ID).unwrap();
    assert_eq!(h.state().generation, 6);
    h.service.keep_alive(ID).unwrap();
    h.assert_no_oauth_side_effects();
}

#[test]
fn disconnect_fences_a_pending_connect_and_a_second_connect_is_busy() {
    let calendar = Arc::new(Gated::default());
    *calendar.fake.request_result.lock().unwrap() = CalendarAuthorization::Granted;
    let h = Harness::with(calendar.clone());
    let (entered, release) = calendar.hold();
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| h.connect());
        entered.recv().unwrap();
        // The OS prompt is open: another request cannot open a second one.
        assert_eq!(h.connect().unwrap_err().code(), "busy");
        // Disconnect needs no OS answer and holds no lock across it.
        h.service.logout(ID).unwrap();
        release.send(()).unwrap();
        assert_eq!(pending.join().unwrap().unwrap_err().code(), "superseded");
    });
    assert_eq!(calendar.fake.access_requests.load(Ordering::SeqCst), 1);
    let state = h.state();
    assert_eq!(state.status, AccountStatus::SignedOut);
    assert_eq!(
        h.service
            .native_generation(CalendarProvider::EventKit)
            .unwrap(),
        None
    );

    // The grant arrived, but only a new explicit connect opts in (without another prompt).
    assert_eq!(h.connect().unwrap().state.status, AccountStatus::Active);
    assert_eq!(calendar.fake.access_requests.load(Ordering::SeqCst), 1);
    h.assert_no_oauth_side_effects();
}

#[test]
fn disconnect_or_reconfigure_during_a_query_discards_its_result() {
    let calendar = granted_with_two_calendars();
    let h = Harness::with(calendar.clone());
    h.connect().unwrap();
    h.service
        .configure_native_calendars(CalendarProvider::EventKit, &["work".into()])
        .unwrap();

    let (entered, release) = calendar.hold();
    std::thread::scope(|scope| {
        let sync = scope.spawn(|| h.sync());
        entered.recv().unwrap();
        h.service.logout(ID).unwrap();
        release.send(()).unwrap();
        assert_eq!(sync.join().unwrap().unwrap_err().code(), "superseded");
    });
    assert!(!h.cache_file_exists());
    assert_eq!(h.state().status, AccountStatus::SignedOut);

    h.connect().unwrap();
    h.service
        .configure_native_calendars(CalendarProvider::EventKit, &["work".into()])
        .unwrap();
    let (entered, release) = calendar.hold();
    std::thread::scope(|scope| {
        let sync = scope.spawn(|| h.sync());
        entered.recv().unwrap();
        h.service
            .configure_native_calendars(CalendarProvider::EventKit, &["personal".into()])
            .unwrap();
        release.send(()).unwrap();
        assert_eq!(sync.join().unwrap().unwrap_err().code(), "superseded");
    });
    assert!(!h.cache_file_exists());
    let cache = h.sync().unwrap();
    assert_eq!(cache.events.len(), 1);
    assert_eq!(cache.events[0].id, "personal-1");
    h.assert_no_oauth_side_effects();
}

#[test]
fn revoked_permission_hides_and_drops_the_native_cache() {
    let calendar = granted_with_two_calendars();
    let h = Harness::with(calendar.clone());
    h.connect().unwrap();
    h.service
        .configure_native_calendars(CalendarProvider::EventKit, &["work".into()])
        .unwrap();
    assert_eq!(h.sync().unwrap().events.len(), 1);
    let generation = h.state().generation;

    calendar.set_authorization(CalendarAuthorization::Denied);
    // Checked live before the copy is used, even before the next sync.
    assert!(h.served().unwrap().events.is_empty());
    assert_eq!(
        h.service
            .native_generation(CalendarProvider::EventKit)
            .unwrap(),
        None
    );
    let status = h.service.status(ID).unwrap();
    // The opt-in stays; the OS state is reported separately and no labels are listed.
    assert_eq!(status.state.status, AccountStatus::Active);
    let native = status.system_calendar.unwrap();
    assert_eq!(native.authorization, Some(CalendarAuthorization::Denied));
    assert!(native.calendars.is_empty());
    assert_eq!(h.sync().unwrap_err().code(), "calendar_denied");
    assert!(!h.cache_file_exists());
    let after_revocation = h.state().generation;
    assert!(after_revocation > generation);
    assert_eq!(calendar.fake.event_queries.lock().unwrap().len(), 1);

    // Granted again: only events stamped with the live generation are ever served.
    calendar.set_authorization(CalendarAuthorization::Granted);
    assert_eq!(
        h.service
            .native_generation(CalendarProvider::EventKit)
            .unwrap(),
        Some(after_revocation)
    );
    let mut stale = CalendarCache::default();
    for stamp in [None, Some(generation)] {
        let mut old = CalendarEvent::from_native(ID, event("work", "stale"), 0).unwrap();
        old.account_generation = stamp;
        stale.events.push(old);
    }
    write_json_private(&h.account().join(CALENDAR_FILE), &stale).unwrap();
    assert!(h.served().unwrap().events.is_empty());
    let on_disk: CalendarCache = read_json_opt(&h.account().join(CALENDAR_FILE))
        .unwrap()
        .unwrap();
    assert_eq!(on_disk.events.len(), 2, "reading never rewrites the copy");
    h.assert_no_oauth_side_effects();
}

#[test]
fn revoked_permission_fences_an_already_running_query_even_after_regrant() {
    let calendar = granted_with_two_calendars();
    let h = Harness::with(calendar.clone());
    h.connect().unwrap();
    h.service
        .configure_native_calendars(CalendarProvider::EventKit, &["work".into()])
        .unwrap();
    // The second epoch starts with no cache and a Denied capability. A new
    // authorized query must still be fenced by the next observed revocation.
    for _ in 0..2 {
        let old_generation = h.state().generation;
        let (entered, release) = calendar.hold();
        std::thread::scope(|scope| {
            let old_query = scope.spawn(|| h.sync());
            entered.recv().unwrap();
            calendar.set_authorization(CalendarAuthorization::Denied);
            assert_eq!(h.sync().unwrap_err().code(), "calendar_denied");
            let current = h.state().generation;
            assert_eq!(current, old_generation + 1);
            assert!(!h.cache_file_exists());
            assert_eq!(h.sync().unwrap_err().code(), "calendar_denied");
            assert_eq!(
                h.state().generation,
                current,
                "denied polls preserve the backoff key"
            );
            assert_eq!(h.state().status, AccountStatus::Active);
            assert_eq!(h.state().native_calendar_ids, ["work"]);
            // Only the generation fence, not a platform authorization error,
            // can reject the result after the OS grant has returned.
            calendar.set_authorization(CalendarAuthorization::Granted);
            assert_eq!(
                h.service
                    .native_generation(CalendarProvider::EventKit)
                    .unwrap(),
                Some(current)
            );
            release.send(()).unwrap();
            assert_eq!(old_query.join().unwrap().unwrap_err().code(), "superseded");
            assert!(!h.cache_file_exists());
        });
    }
    let current = h.state().generation;
    let fresh = h.sync().unwrap();
    assert_eq!(fresh.events.len(), 1);
    assert_eq!(fresh.events[0].account_generation, Some(current));
    h.assert_no_oauth_side_effects();
}

#[test]
fn stale_granted_observation_cannot_restart_a_query_after_another_denied_poll() {
    let calendar = granted_with_two_calendars();
    let h = Harness::with(calendar.clone());
    h.connect().unwrap();
    h.service
        .configure_native_calendars(CalendarProvider::EventKit, &["work".into()])
        .unwrap();
    calendar.set_authorization(CalendarAuthorization::Denied);
    assert_eq!(h.sync().unwrap_err().code(), "calendar_denied");
    let generation = h.state().generation;
    calendar.set_authorization(CalendarAuthorization::Granted);
    let (entered, release) = calendar.hold_authorization();
    std::thread::scope(|scope| {
        let sync = scope.spawn(|| h.sync());
        entered.recv().unwrap();
        calendar.set_authorization(CalendarAuthorization::Denied);
        assert_eq!(h.sync().unwrap_err().code(), "calendar_denied");
        assert_eq!(h.state().generation, generation);
        release.send(()).unwrap();
        assert_eq!(sync.join().unwrap().unwrap_err().code(), "calendar_denied");
    });
    assert!(calendar.fake.event_queries.lock().unwrap().is_empty());
    assert!(h.state().generation > generation);
    assert!(!h.cache_file_exists());
    h.assert_no_oauth_side_effects();
}
