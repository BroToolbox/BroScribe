use super::*;
use broscribe_platform::{AxNode, AxTree, DocumentContext, fakes::FakeAppUiReader};
use std::sync::atomic::AtomicU64;

const PID: u32 = 12345;
const SESSION: &str = "synthetic-session";

fn session(app: &str) -> ReaderSession {
    ReaderSession {
        session_id: SESSION.into(),
        app: app.into(),
        pid: PID,
        reader_generation: 7,
        policy: SessionPolicy::Auto,
        accepted: true,
        paused: false,
    }
}

fn empty_tree() -> AxTree {
    AxTree {
        root: AxNode::new("AXApplication"),
        incomplete: Default::default(),
    }
}

fn batch(polled: Option<Polled>) -> ReaderBatch {
    match polled {
        Some(Polled::Batch(batch)) => batch,
        other => panic!("expected a batch, got {other:?}"),
    }
}

#[test]
fn ignored_detections_never_start_observers() {
    let reader = Arc::new(FakeAppUiReader::default());
    let mut host = ReaderHost::new(reader.clone());
    for policy in [SessionPolicy::Off, SessionPolicy::Ask] {
        let mut s = session("teams");
        s.policy = policy;
        s.accepted = false;
        assert!(!host.start(s).unwrap());
    }
    assert_eq!(reader.observer_count(), 0);
    assert!(host.poll(SESSION, 1.0).unwrap().is_none());
}

#[test]
fn chromium_is_restored_only_when_owned_and_pause_stops_observation() {
    for original in [false, true] {
        let reader = Arc::new(FakeAppUiReader::default());
        reader.manual.lock().unwrap().insert(PID, original);
        reader.set_tree(PID, empty_tree());
        let mut host = ReaderHost::new(reader.clone());
        assert!(host.start(session("meet-chrome")).unwrap());
        assert_eq!(reader.observer_count(), 1);
        assert!(host.poll(SESSION, 10.0).unwrap().is_some());
        assert!(host.poll(SESSION, 10.5).unwrap().is_none());
        host.pause(SESSION);
        assert_eq!(reader.observer_count(), 0);
        assert_eq!(reader.manual.lock().unwrap()[&PID], original);
        assert_eq!(
            reader.manual_calls.lock().unwrap().len(),
            if original { 0 } else { 2 }
        );
    }
}

#[test]
fn denial_and_app_restart_do_not_leave_a_reader_running() {
    let mut denied = FakeAppUiReader::default();
    denied.denied = true;
    let denied = Arc::new(denied);
    let mut refused = ReaderHost::new(denied.clone());
    assert_eq!(
        refused.start(session("teams")),
        Err(ReaderStartRefusal::Untrusted)
    );
    assert_eq!(
        refused.start(session("synthetic-app-without-rule")),
        Err(ReaderStartRefusal::NoRule)
    );
    assert_eq!(denied.observer_count(), 0);
    assert!(refused.health(SESSION).is_none());
    assert!(refused.poll(SESSION, 1.0).unwrap().is_none());
    let reader = Arc::new(FakeAppUiReader::default());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("teams")).unwrap();
    // The app vanished: one last incomplete Unknown, then the reader stops.
    match host.poll(SESSION, 3.0).unwrap() {
        Some(Polled::Failed { batch, .. }) => {
            assert_eq!(batch.call_observation.state, CallState::Unknown);
            assert!(!batch.call_observation.complete);
            assert_eq!(batch.reader_generation, 7);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(reader.observer_count(), 0);
    assert!(host.poll(SESSION, 5.0).unwrap().is_none());
}

#[test]
fn split_poll_skips_while_out_and_drops_stale_snapshots() {
    let reader = Arc::new(FakeAppUiReader::default());
    reader.set_tree(PID, empty_tree());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("teams")).unwrap();
    let job = host.begin_poll(SESSION, 10.0).unwrap().unwrap();
    // Still out: no second snapshot, even when one would be due.
    assert!(host.begin_poll(SESSION, 20.0).unwrap().is_none());
    let tree = job.run();
    assert!(matches!(host.finish_poll(job, tree), Polled::Batch(_)));
    // A job from before a restart is ignored, and the restart polls anew.
    let stale = host.begin_poll(SESSION, 20.0).unwrap().unwrap();
    host.start(session("teams")).unwrap();
    assert!(matches!(
        host.finish_poll(stale, Err(Error::Internal("gone".into()))),
        Polled::Stale
    ));
    assert_eq!(reader.observer_count(), 1);
    assert!(host.begin_poll(SESSION, 22.0).unwrap().is_some());
}

// ---- call state ----

fn node(role: &str) -> AxNode {
    AxNode::new(role)
}

fn with(mut parent: AxNode, children: Vec<AxNode>) -> AxNode {
    parent.children = children;
    parent
}

const CALL: &str = "https://meet.google.com/abc-defg-hij";

/// One browser tab: a web area of document element `element` at `url`.
fn tab(element: &str, url: &str, children: Vec<AxNode>) -> AxNode {
    AxNode {
        document: Some(DocumentContext::new(
            Some(url.into()),
            Some(element.into()),
            false,
        )),
        ..with(node("AXWebArea"), children)
    }
}

/// The tab's window is minimized.
fn minimized(mut tab: AxNode) -> AxNode {
    let document = tab.document.take().unwrap();
    tab.document = Some(DocumentContext::new(
        document.url().map(Into::into),
        document.element().map(Into::into),
        true,
    ));
    tab
}

/// Browser windows, one tab each.
fn tabs(tabs: Vec<AxNode>) -> AxTree {
    AxTree {
        root: with(
            node("AXApplication"),
            tabs.into_iter()
                .map(|t| with(node("AXWindow"), vec![t]))
                .collect(),
        ),
        incomplete: Default::default(),
    }
}

/// The call's own tab (document `doc-1`).
fn web(children: Vec<AxNode>) -> AxTree {
    tabs(vec![tab("doc-1", CALL, children)])
}

fn button(description: Option<&str>, title: Option<&str>) -> AxNode {
    AxNode {
        description: description.map(Into::into),
        title: title.map(Into::into),
        ..node("AXButton")
    }
}

fn text(value: &str) -> AxNode {
    AxNode {
        value: Some(value.into()),
        ..node("AXStaticText")
    }
}

fn in_call() -> AxTree {
    web(vec![
        with(node("AXGroup"), vec![button(Some("Leave call"), None)]),
        // A chat that talks about leaving is irrelevant.
        with(node("AXList"), vec![text("You left the meeting")]),
    ])
}

fn post_call() -> AxTree {
    web(vec![
        AxNode {
            title: Some("You left the meeting".into()),
            ..node("AXHeading")
        },
        button(None, Some("Rejoin")),
    ])
}

/// Chat text naming the post-call surface, without its structure.
fn decoy() -> AxTree {
    web(vec![with(
        node("AXList"),
        vec![
            text("You left the meeting"),
            text("Rejoin"),
            text("Leave call"),
        ],
    )])
}

/// Wall clock the test advances.
fn clock(start: u64) -> (Arc<AtomicU64>, WallClock) {
    let now = Arc::new(AtomicU64::new(start));
    let read = Arc::clone(&now);
    (now, Arc::new(move || read.load(Ordering::SeqCst)))
}

#[test]
fn record_auto_stop_call_states_need_structure_and_an_earlier_active_call() {
    let reader = Arc::new(FakeAppUiReader::default());
    let (now, wall) = clock(1_000_000);
    let mut host = ReaderHost::with_wall_clock(reader.clone(), wall);
    host.start(session("meet-chrome")).unwrap();
    let mut t = 0.0;
    let mut observe = |host: &mut ReaderHost, tree: AxTree| {
        reader.set_tree(PID, tree);
        t += 2.0;
        now.fetch_add(2_000, Ordering::SeqCst);
        batch(host.poll(SESSION, t).unwrap())
    };
    // A leftover post-call page before any call: no end.
    let b = observe(&mut host, post_call());
    assert_eq!(
        (b.call_observation.state, b.call_observation.complete),
        (CallState::Unknown, true)
    );
    let b = observe(&mut host, in_call());
    assert_eq!(b.call_observation.state, CallState::Active);
    // Chat decoys are never call state.
    let b = observe(&mut host, decoy());
    assert_eq!(
        (b.call_observation.state, b.call_observation.complete),
        (CallState::Unknown, true)
    );
    let b = observe(&mut host, post_call());
    assert_eq!(
        (b.call_observation.state, b.call_observation.complete),
        (CallState::Ended, true)
    );
    // Unchanged quiet surface: still a batch (no events), still Ended.
    let b = observe(&mut host, post_call());
    assert!(b.events.is_empty());
    assert_eq!(b.call_observation.state, CallState::Ended);
    // A truncated tree proves nothing.
    let mut cut = post_call();
    cut.incomplete.node_cap = true;
    let b = observe(&mut host, cut);
    assert_eq!(
        (b.call_observation.state, b.call_observation.complete),
        (CallState::Unknown, false)
    );
    // Missing controls (minimised/hidden content) are unknown, not ended.
    let b = observe(&mut host, web(Vec::new()));
    assert_eq!(b.call_observation.state, CallState::Unknown);
    assert_eq!(b.call_observation.sequence, 7);
    assert_eq!(b.reader_generation, 7);
    assert_eq!(b.app, "meet-chrome");

    // A new incarnation (restart, resume) must see the call again first.
    let mut restarted = session("meet-chrome");
    restarted.reader_generation = 8;
    host.start(restarted).unwrap();
    let b = observe(&mut host, post_call());
    assert_eq!(b.call_observation.state, CallState::Unknown);
    assert_eq!(b.call_observation.sequence, 1, "sequence per incarnation");
    assert_eq!(b.reader_generation, 8);
}

fn call_controls() -> Vec<AxNode> {
    vec![with(
        node("AXGroup"),
        vec![button(Some("Leave call"), None)],
    )]
}

fn left_page() -> Vec<AxNode> {
    vec![
        AxNode {
            title: Some("You left the meeting".into()),
            ..node("AXHeading")
        },
        button(None, Some("Rejoin")),
    ]
}

fn states(host: &mut ReaderHost, reader: &FakeAppUiReader, trees: Vec<AxTree>) -> Vec<CallState> {
    let mut out = Vec::new();
    for (i, tree) in trees.into_iter().enumerate() {
        reader.set_tree(PID, tree);
        let b = batch(host.poll(SESSION, 100.0 + 2.0 * i as f64).unwrap());
        out.push(b.call_observation.state);
    }
    out
}

#[test]
fn record_auto_stop_an_end_belongs_to_the_document_and_call_seen_active() {
    let reader = Arc::new(FakeAppUiReader::default());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("meet-chrome")).unwrap();
    let other_page = tab("doc-9", "https://example.com/docs", vec![text("Anything")]);
    let observed = states(
        &mut host,
        &reader,
        vec![
            // Call A runs in doc-1.
            tabs(vec![tab("doc-1", CALL, call_controls())]),
            // Another tab comes forward: an old post-call page of the same
            // meeting code (doc-2) or an unrelated page. Not this call.
            tabs(vec![tab("doc-2", CALL, left_page())]),
            tabs(vec![other_page.clone()]),
            // Call A's document reloaded as a new document: unknown.
            tabs(vec![tab("doc-3", CALL, left_page())]),
            // Another meeting code in call A's document: unknown.
            tabs(vec![tab(
                "doc-1",
                "https://meet.google.com/xyz-wxyz-xyz",
                left_page(),
            )]),
            // Call A's post-call page in a minimized window: not current.
            tabs(vec![minimized(tab("doc-1", CALL, left_page()))]),
            // Call A's own document shows its post-call page: an end.
            tabs(vec![tab("doc-1", CALL, left_page()), other_page]),
        ],
    );
    assert_eq!(
        observed,
        [
            CallState::Active,
            CallState::Unknown,
            CallState::Unknown,
            CallState::Unknown,
            CallState::Unknown,
            CallState::Unknown,
            CallState::Ended,
        ]
    );
}

#[test]
fn record_auto_stop_an_unidentifiable_or_second_call_never_arms_an_end() {
    // An active call whose document identity cannot be read (no URL).
    let reader = Arc::new(FakeAppUiReader::default());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("meet-chrome")).unwrap();
    let mut anonymous = tab("doc-1", CALL, call_controls());
    anonymous.document = Some(DocumentContext::new(None, Some("doc-1".into()), false));
    assert_eq!(
        states(
            &mut host,
            &reader,
            vec![tabs(vec![anonymous]), web(left_page())],
        ),
        [CallState::Active, CallState::Unknown]
    );
    // Two different calls seen active in one incarnation.
    let reader = Arc::new(FakeAppUiReader::default());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("meet-chrome")).unwrap();
    assert_eq!(
        states(
            &mut host,
            &reader,
            vec![
                web(call_controls()),
                tabs(vec![tab(
                    "doc-5",
                    "https://meet.google.com/xyz-wxyz-xyz",
                    call_controls(),
                )]),
                web(left_page()),
            ],
        ),
        [CallState::Active, CallState::Active, CallState::Unknown]
    );
    // While any call control is visible, no post-call page is an end.
    let reader = Arc::new(FakeAppUiReader::default());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("meet-chrome")).unwrap();
    assert_eq!(
        states(
            &mut host,
            &reader,
            vec![
                web(call_controls()),
                tabs(vec![
                    tab("doc-1", CALL, left_page()),
                    tab(
                        "doc-7",
                        "https://meet.google.com/xyz-wxyz-xyz",
                        call_controls()
                    ),
                ]),
            ],
        ),
        [CallState::Active, CallState::Active]
    );
}
#[test]
fn record_auto_stop_rules_without_call_selectors_report_incomplete_unknown() {
    let reader = Arc::new(FakeAppUiReader::default());
    reader.set_tree(PID, in_call());
    let mut host = ReaderHost::new(reader);
    host.start(session("slack")).unwrap();
    let b = batch(host.poll(SESSION, 1.0).unwrap());
    assert_eq!(
        (b.call_observation.state, b.call_observation.complete),
        (CallState::Unknown, false)
    );
}

#[test]
fn a_full_identity_registry_is_reported_and_never_ends_a_call() {
    let reader = Arc::new(FakeAppUiReader::default());
    reader.set_tree(PID, in_call());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("meet-chrome")).unwrap();
    let b = batch(host.poll(SESSION, 1.0).unwrap());
    assert_eq!(b.call_observation.state, CallState::Active);
    assert_eq!(b.health.incomplete, None);
    // The registry is full: the post-call page is not whole evidence.
    let mut capped = post_call();
    capped.incomplete.identity_cap = true;
    capped.incomplete.read_failed = true;
    reader.set_tree(PID, capped);
    let b = batch(host.poll(SESSION, 3.0).unwrap());
    assert_eq!(
        (b.call_observation.state, b.call_observation.complete),
        (CallState::Unknown, false)
    );
    assert_eq!(b.health.incomplete, Some(IncompleteReason::IdentityCap));
    assert_eq!(host.health(SESSION).unwrap(), b.health);
    // A new incarnation has a fresh registry: its whole snapshot clears it.
    host.start(session("meet-chrome")).unwrap();
    reader.set_tree(PID, in_call());
    let b = batch(host.poll(SESSION, 5.0).unwrap());
    assert_eq!(b.health.incomplete, None);
}

#[test]
fn record_auto_stop_sequence_and_wall_clock_are_taken_before_the_snapshot() {
    let reader = Arc::new(FakeAppUiReader::default());
    reader.set_tree(PID, in_call());
    let (now, wall) = clock(5_000);
    let mut host = ReaderHost::with_wall_clock(reader, wall);
    host.start(session("meet-chrome")).unwrap();
    let job = host.begin_poll(SESSION, 0.0).unwrap().unwrap();
    assert_eq!((job.sequence(), job.snapshot_started_at_ms()), (1, 5_000));
    // A slow snapshot: the clock moves on while it runs.
    now.store(9_000, Ordering::SeqCst);
    let tree = job.run();
    let b = match host.finish_poll(job, tree) {
        Polled::Batch(b) => b,
        other => panic!("{other:?}"),
    };
    assert_eq!(b.call_observation.snapshot_started_at_ms, 5_000);
    assert_eq!(b.call_observation.sequence, 1);
    let next = host.begin_poll(SESSION, 2.0).unwrap().unwrap();
    assert_eq!((next.sequence(), next.snapshot_started_at_ms()), (2, 9_000));
}

#[test]
fn record_auto_stop_quiet_heartbeat_is_at_most_two_seconds_with_one_outstanding() {
    let reader = Arc::new(FakeAppUiReader::default());
    reader.set_tree(PID, post_call());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("meet-chrome")).unwrap();
    assert_eq!(reader.observer_count(), 1, "an observer that stays quiet");
    // The app's 1 Hz tick, with jitter, for 30 s: quiet, yet re-read every
    // other tick (never the old 5 s idle throttle).
    let mut taken = Vec::new();
    for tick in 0..30 {
        let jitter = if tick % 3 == 0 { 0.03 } else { -0.02 };
        let at = (f64::from(tick) + jitter).max(0.0);
        if host.poll(SESSION, at).unwrap().is_some() {
            taken.push(at);
        }
    }
    assert!(taken.len() >= 15, "{taken:?}");
    assert!(taken.windows(2).all(|w| w[1] - w[0] <= 2.1), "{taken:?}");
    // A change notification makes the next tick due.
    let job = host.begin_poll(SESSION, 40.0).unwrap().unwrap();
    let tree = job.run();
    host.finish_poll(job, tree);
    reader.emit(PID, AxChange::ValueChanged);
    assert!(host.begin_poll(SESSION, 41.0).unwrap().is_some());
    // One outstanding: a slow snapshot holds the next back.
    assert!(host.begin_poll(SESSION, 44.0).unwrap().is_none());
}

// ---- incarnation scope, window transitions, transient failures ----

use crate::ReaderEvent;
use broscribe_platform::{AxChangeCallback, AxScopePolicy};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap()
}

type Registry = Mutex<Vec<u64>>;

/// The fake reader plus what real platform readers add: a per-incarnation
/// registry in the scope, verified window destruction, transient failures
/// and apps posting no notifications.
#[derive(Default)]
struct Scripted {
    inner: FakeAppUiReader,
    /// Element tokens the platform verified destroyed.
    destroyed: Mutex<Vec<String>>,
    /// Every destruction probe, with the tokens asked about.
    probes: Mutex<Vec<Vec<String>>>,
    probe_fails: AtomicBool,
    /// Failures of the next snapshots, in order.
    errors: Mutex<std::collections::VecDeque<Error>>,
    observe_fails: bool,
    /// Each scope's registry: the walks made within it.
    registries: Mutex<Vec<std::sync::Weak<Registry>>>,
    walks: std::sync::atomic::AtomicU64,
}

impl AppUiReader for Scripted {
    fn is_trusted(&self) -> bool {
        self.inner.is_trusted()
    }
    fn request_trust(&self) -> bool {
        self.inner.request_trust()
    }
    fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree> {
        self.inner.snapshot(pid, limits)
    }
    fn scope(&self, pid: u32, policy: AxScopePolicy) -> AxScope {
        let registry: Arc<Registry> = Arc::default();
        lock(&self.registries).push(Arc::downgrade(&registry));
        AxScope::with_state(pid, policy, registry)
    }
    fn snapshot_in(&self, scope: &AxScope, limits: AxLimits) -> Result<AxTree> {
        if let Some(error) = lock(&self.errors).pop_front() {
            return Err(error);
        }
        let walk = self.walks.fetch_add(1, Ordering::SeqCst);
        if let Some(registry) = scope.state::<Registry>() {
            lock(registry).push(walk);
        }
        self.inner.snapshot_in(scope, limits)
    }
    fn destroyed(&self, _scope: &AxScope, elements: &[String]) -> Result<Vec<String>> {
        lock(&self.probes).push(elements.to_vec());
        if self.probe_fails.load(Ordering::SeqCst) {
            return Err(Error::Internal("probe failed".into()));
        }
        let destroyed = lock(&self.destroyed);
        Ok(elements
            .iter()
            .filter(|e| destroyed.contains(e))
            .cloned()
            .collect())
    }
    fn observe(
        &self,
        pid: u32,
        changes: &[AxChange],
        callback: AxChangeCallback,
    ) -> Result<Box<dyn AxObservation>> {
        if self.observe_fails {
            return Err(Error::Unsupported("no notifications".into()));
        }
        self.inner.observe(pid, changes, callback)
    }
    fn manual_accessibility(&self, pid: u32) -> Result<Option<bool>> {
        self.inner.manual_accessibility(pid)
    }
    fn set_manual_accessibility(&self, pid: u32, enabled: bool) -> Result<()> {
        self.inner.set_manual_accessibility(pid, enabled)
    }
}

/// A Teams top-level window with element token `token`, showing the
/// observed hang-up control when `in_call`.
fn call_window(token: &str, in_call: bool) -> AxNode {
    let content = if in_call {
        vec![with(
            node("AXGroup"),
            vec![AxNode {
                identifier: Some("hangup-button".into()),
                description: Some("Leave".into()),
                ..node("AXButton")
            }],
        )]
    } else {
        vec![node("AXGroup")]
    };
    AxNode {
        document: Some(DocumentContext::new(None, Some(token.into()), false)),
        ..with(node("AXWindow"), content)
    }
}

fn main_window() -> AxNode {
    call_window("win-main", false)
}

fn teams(windows: Vec<AxNode>) -> AxTree {
    AxTree {
        root: with(node("AXApplication"), windows),
        incomplete: Default::default(),
    }
}

#[test]
fn record_auto_stop_teams_ends_only_when_every_call_window_is_verified_destroyed() {
    let reader = Arc::new(Scripted::default());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("teams")).unwrap();
    let mut at = 0.0;
    let mut step = |host: &mut ReaderHost, tree: AxTree| {
        reader.inner.set_tree(PID, tree);
        at += 2.0;
        let b = batch(host.poll(SESSION, at).unwrap());
        (
            b.call_observation.state,
            b.call_observation.complete,
            b.health,
        )
    };
    let probes = || lock(&reader.probes).len();
    let state = |r: (CallState, bool, ReaderHealth)| (r.0, r.1);

    let in_call = teams(vec![main_window(), call_window("win-call", true)]);
    assert_eq!(state(step(&mut host, in_call)), (CallState::Active, true));
    // Present without its control (minimized, another view): alive, so
    // not even probed.
    let quiet = teams(vec![main_window(), call_window("win-call", false)]);
    assert_eq!(state(step(&mut host, quiet)), (CallState::Unknown, true));
    assert_eq!(probes(), 0);
    // Missing from the snapshot (another Space, enumeration gap) but not
    // verified destroyed: unknown.
    assert_eq!(
        state(step(&mut host, teams(vec![main_window()]))),
        (CallState::Unknown, true)
    );
    assert_eq!(*lock(&reader.probes).last().unwrap(), ["win-call"]);
    lock(&reader.destroyed).push("win-call".into());
    // Destroyed, but an incomplete snapshot proves nothing (no probe).
    let mut cut = teams(vec![main_window()]);
    cut.incomplete.time_budget = true;
    let before = probes();
    let (s, complete, health) = step(&mut host, cut);
    assert_eq!((s, complete), (CallState::Unknown, false));
    assert_eq!(health.incomplete, Some(IncompleteReason::TimeBudget));
    assert_eq!(probes(), before);
    // A failed probe proves nothing either.
    reader.probe_fails.store(true, Ordering::SeqCst);
    assert_eq!(
        state(step(&mut host, teams(vec![main_window()]))),
        (CallState::Unknown, true)
    );
    reader.probe_fails.store(false, Ordering::SeqCst);
    // Verified destroyed, complete, nothing in call: ended, and it stays
    // ended while that holds.
    for _ in 0..2 {
        let (s, complete, health) = step(&mut host, teams(vec![main_window()]));
        assert_eq!((s, complete), (CallState::Ended, true));
        assert_eq!(health.incomplete, None);
    }
    // The call shows again in another window: active vetoes.
    let moved = teams(vec![main_window(), call_window("win-2", true)]);
    assert_eq!(state(step(&mut host, moved)), (CallState::Active, true));
    // Only one of the two call windows is destroyed: no end.
    assert_eq!(
        state(step(&mut host, teams(vec![main_window()]))),
        (CallState::Unknown, true)
    );
    lock(&reader.destroyed).push("win-2".into());
    assert_eq!(
        state(step(&mut host, teams(vec![main_window()]))),
        (CallState::Ended, true)
    );
    // A new incarnation must see the call active itself first.
    let mut restarted = session("teams");
    restarted.reader_generation = 8;
    host.start(restarted).unwrap();
    let before = probes();
    assert_eq!(
        state(step(&mut host, teams(vec![main_window()]))),
        (CallState::Unknown, true)
    );
    assert_eq!(probes(), before);
}

#[test]
fn record_auto_stop_untrackable_call_windows_never_end() {
    let reader = Arc::new(Scripted::default());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("teams")).unwrap();
    let mut anonymous = call_window("win-call", true);
    anonymous.document = None;
    lock(&reader.destroyed).push("win-call".into());
    let observed = [
        teams(vec![main_window(), call_window("win-call", true)]),
        teams(vec![main_window(), anonymous]),
        teams(vec![main_window()]),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, tree)| {
        reader.inner.set_tree(PID, tree);
        batch(host.poll(SESSION, 2.0 * (i as f64 + 1.0)).unwrap())
            .call_observation
            .state
    })
    .collect::<Vec<_>>();
    assert_eq!(
        observed,
        [CallState::Active, CallState::Active, CallState::Unknown]
    );
}

#[test]
fn record_auto_stop_a_detached_walk_of_an_earlier_incarnation_never_feeds_a_newer_one() {
    let reader = Arc::new(Scripted::default());
    reader
        .inner
        .set_tree(PID, teams(vec![call_window("win-call", true)]));
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("teams")).unwrap();
    let old = host.begin_poll(SESSION, 1.0).unwrap().unwrap();
    let mut restarted = session("teams");
    restarted.reader_generation = 8;
    host.start(restarted).unwrap();
    let registries = lock(&reader.registries).clone();
    assert_eq!(registries.len(), 2);
    // The old walk finishes after the restart, within its own scope only.
    let tree = old.run();
    assert_eq!(*lock(&registries[0].upgrade().unwrap()), [0]);
    assert!(lock(&registries[1].upgrade().unwrap()).is_empty());
    assert!(matches!(host.finish_poll(old, tree), Polled::Stale));
    assert!(
        registries[0].upgrade().is_none(),
        "an old scope dies with its last job"
    );
    // Its active call armed nothing in the new incarnation.
    reader.inner.set_tree(PID, teams(vec![main_window()]));
    lock(&reader.destroyed).push("win-call".into());
    let b = batch(host.poll(SESSION, 3.0).unwrap());
    assert_eq!(b.call_observation.state, CallState::Unknown);
    assert_eq!(b.reader_generation, 8);
    assert_eq!(*lock(&registries[1].upgrade().unwrap()), [1]);
    host.stop(SESSION);
    assert!(registries[1].upgrade().is_none());
}

#[test]
fn transient_failures_back_off_honestly_and_keep_the_reader() {
    let reader = Arc::new(Scripted::default());
    reader
        .inner
        .set_tree(PID, teams(vec![call_window("win-call", true)]));
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("teams")).unwrap();
    for _ in 0..3 {
        lock(&reader.errors).push_back(broscribe_platform::ax_transient("busy"));
    }
    let failed = |polled: Option<Polled>| {
        let b = batch(polled);
        assert_eq!(
            (b.call_observation.state, b.call_observation.complete),
            (CallState::Unknown, false)
        );
        assert_eq!(b.health.incomplete, Some(IncompleteReason::Transient));
        b.health
    };
    let health = failed(host.poll(SESSION, 10.0).unwrap());
    assert_eq!(health.consecutive_failures, 1);
    assert_eq!(health.retry_after_s, Some(10.0 + MIN_INTERVAL_S));
    let health = failed(host.poll(SESSION, 11.6).unwrap());
    assert_eq!(health.consecutive_failures, 2);
    assert_eq!(health.retry_after_s, Some(11.6 + 2.0 * MIN_INTERVAL_S));
    // Not before the backoff ends, even when a heartbeat is due.
    reader.inner.emit(PID, AxChange::ValueChanged);
    assert!(host.poll(SESSION, 13.0).unwrap().is_none());
    let health = failed(host.poll(SESSION, 13.5).unwrap());
    assert_eq!(health.consecutive_failures, 3);
    assert_eq!(host.health(SESSION).unwrap(), health);
    assert!(host.poll(SESSION, 17.0).unwrap().is_none());
    let b = batch(host.poll(SESSION, 17.2).unwrap());
    assert_eq!(b.call_observation.state, CallState::Active);
    assert_eq!(b.health.consecutive_failures, 0);
    assert_eq!((b.health.incomplete, b.health.retry_after_s), (None, None));
    // Backoff is capped.
    for _ in 0..20 {
        lock(&reader.errors).push_back(broscribe_platform::ax_transient("busy"));
    }
    let mut at = 20.0;
    let mut longest: f64 = 0.0;
    for _ in 0..20 {
        let job = loop {
            at += 0.5;
            if let Some(job) = host.begin_poll(SESSION, at).unwrap() {
                break job;
            }
        };
        let tree = job.run();
        let Polled::Batch(b) = host.finish_poll(job, tree) else {
            panic!("transient failures keep the reader");
        };
        longest = longest.max(b.health.retry_after_s.unwrap() - at);
    }
    assert!((longest - MAX_BACKOFF_S).abs() < 1e-9, "{longest}");
    // Any other failure stops the reader.
    lock(&reader.errors).push_back(Error::NotFound("gone".into()));
    let polled = loop {
        at += 0.5;
        if let Some(polled) = host.poll(SESSION, at).unwrap() {
            break polled;
        }
    };
    match polled {
        Polled::Failed { batch, .. } => {
            assert_eq!(batch.health.incomplete, Some(IncompleteReason::Failed));
            assert!(!batch.call_observation.complete);
        }
        other => panic!("{other:?}"),
    }
    assert!(host.health(SESSION).is_none());
}

#[test]
fn polling_only_readers_keep_their_heartbeat_and_scope() {
    let reader = Arc::new(Scripted {
        observe_fails: true,
        ..Scripted::default()
    });
    reader
        .inner
        .set_tree(PID, teams(vec![call_window("win-call", true)]));
    let mut host = ReaderHost::new(reader.clone());
    assert!(host.start(session("teams")).unwrap());
    assert_eq!(reader.inner.observer_count(), 0);
    // Without notifications every tick past the minimum spacing is due.
    assert!(host.poll(SESSION, 1.0).unwrap().is_some());
    assert!(host.poll(SESSION, 1.5).unwrap().is_none());
    assert!(host.poll(SESSION, 1.95).unwrap().is_some());
    let registry = lock(&reader.registries)[0].clone();
    assert_eq!(lock(&registry.upgrade().unwrap()).len(), 2);
    host.stop(SESSION);
    assert!(
        registry.upgrade().is_none(),
        "the scope ends with the reader, observer or not"
    );
}

#[test]
fn manual_accessibility_lease_covers_electron_and_never_blocks_reading() {
    let reader = Arc::new(FakeAppUiReader::default());
    reader.manual.lock().unwrap().insert(PID, false);
    reader.set_tree(PID, empty_tree());
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("slack")).unwrap();
    assert!(reader.manual.lock().unwrap()[&PID]);
    assert_eq!(
        host.health(SESSION).unwrap().manual_accessibility,
        ManualAccessibility::EnabledByBroscribe
    );
    // Two readers of one process share one lease.
    let mut other = session("slack");
    other.session_id = "other-session".into();
    host.start(other).unwrap();
    host.stop(SESSION);
    assert!(reader.manual.lock().unwrap()[&PID]);
    host.stop("other-session");
    assert!(!reader.manual.lock().unwrap()[&PID]);
    assert_eq!(
        *reader.manual_calls.lock().unwrap(),
        [(PID, true), (PID, false)]
    );
    // Turned off by someone else meanwhile: left alone.
    host.start(session("slack")).unwrap();
    reader.manual.lock().unwrap().insert(PID, false);
    host.stop(SESSION);
    assert_eq!(reader.manual_calls.lock().unwrap().len(), 3);
    // An app without the switch still gets read.
    let plain = Arc::new(FakeAppUiReader::default());
    plain.set_tree(PID, empty_tree());
    let mut host = ReaderHost::new(plain.clone());
    assert!(host.start(session("discord")).unwrap());
    assert_eq!(
        host.health(SESSION).unwrap().manual_accessibility,
        ManualAccessibility::Unsupported
    );
    assert!(host.poll(SESSION, 1.0).unwrap().is_some());
    // Native apps' rules never touch it.
    assert!(host.start(session("zoom")).unwrap());
    assert_eq!(
        host.health(SESSION).unwrap().manual_accessibility,
        ManualAccessibility::NotApplicable
    );
    assert!(plain.manual_calls.lock().unwrap().is_empty());
}

#[test]
fn browser_snapshots_skip_documents_of_other_origins() {
    let reader = Arc::new(FakeAppUiReader::default());
    // Another tab holds caption-shaped content: it is never read.
    let captions = with(
        AxNode {
            description: Some("Captions".into()),
            ..node("AXGroup")
        },
        vec![with(
            node("AXGroup"),
            vec![text("Private Person"), text("private words here")],
        )],
    );
    reader.set_tree(
        PID,
        tabs(vec![
            tab("doc-1", CALL, call_controls()),
            tab("doc-9", "https://mail.example.com/inbox", vec![captions]),
        ]),
    );
    let mut host = ReaderHost::new(reader.clone());
    host.start(session("meet-chrome")).unwrap();
    let b = batch(host.poll(SESSION, 1.0).unwrap());
    assert_eq!(b.call_observation.state, CallState::Active);
    assert!(
        !b.events
            .iter()
            .any(|e| matches!(e, ReaderEvent::Caption(_)))
    );
    assert_eq!(
        (b.health.scoped_documents, b.health.skipped_documents),
        (1, 1)
    );
}
