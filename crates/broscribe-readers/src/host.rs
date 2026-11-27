//! Session-scoped observations; construct/use/drop on GPUI's main thread.
//!
//! Every completed snapshot yields a [`ReaderBatch`] carrying a
//! [`CallObservation`], even when its roster/caption events were all
//! deduplicated away: the observation is the call-state heartbeat the
//! daemon's automatic stop consumes (plan H). While a reader runs, a
//! snapshot is due at most every [`MIN_INTERVAL_S`] and at least every
//! [`HEARTBEAT_S`] (quiet included); one snapshot is out at a time. A
//! transient accessibility failure keeps the reader running and retries
//! with a capped backoff, reporting an incomplete `Unknown` meanwhile.
//!
//! Each start is one reader incarnation: it owns its platform
//! [`AxScope`] (element registry), rule history and call binding. A
//! snapshot job keeps its own clone of the scope, so a detached walk of a
//! stopped incarnation never touches a newer one, and its result is
//! dropped as stale.
use crate::rules::{CallReading, Extracted, ReaderRule};
use crate::{
    CallObservation, CallState, IncompleteReason, ManualAccessibility, ReaderBatch, ReaderHealth,
    ReaderStartRefusal, RuleEngine, builtin_rules,
};
use broscribe_core::{Error, Result};
use broscribe_platform::{
    AppUiReader, AxChange, AxIncomplete, AxLimits, AxObservation, AxScope, AxTree, is_ax_transient,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Shortest spacing of two snapshots of one reader (recording seconds). A
/// little under the app's 1 Hz tick, so timer jitter never skips a tick.
pub const MIN_INTERVAL_S: f64 = 0.9;
/// A snapshot is due after this long even when the app posted no change.
/// With the 1 Hz tick that is every second tick: a quiet call surface is
/// re-read at least every 2 s, well inside the daemon's 5 s evidence expiry.
pub const HEARTBEAT_S: f64 = 1.5;
/// Longest wait before retrying after transient failures in a row.
pub const MAX_BACKOFF_S: f64 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPolicy {
    Auto,
    Ask,
    Off,
}
#[derive(Clone, Debug)]
pub struct ReaderSession {
    pub session_id: String,
    /// Embedded rule id (e.g. `teams`, `meet-chrome`), not a window title.
    pub app: String,
    pub pid: u32,
    /// The daemon-issued generation of this reader incarnation
    /// (StartReader); echoed in every batch.
    pub reader_generation: u64,
    pub policy: SessionPolicy,
    pub accepted: bool,
    pub paused: bool,
}
struct Running {
    session: ReaderSession,
    engine: RuleEngine,
    /// `None`: the app posts no notifications (polling-only reader).
    observation: Option<Box<dyn AxObservation>>,
    scope: AxScope,
    dirty: Arc<AtomicBool>,
    last_s: Option<f64>,
    /// This incarnation holds a manual-accessibility lease on its pid.
    manual: bool,
    /// Identifies this start of the session, so a snapshot taken for an
    /// earlier start is never applied to a restarted one.
    generation: u64,
    /// A snapshot job is out; no second one starts until it finishes.
    in_flight: bool,
    /// Call-observation sequence of the last snapshot begun.
    sequence: u64,
    /// The one call this incarnation saw active: only a post-call surface of
    /// that very call (same element, same call code) is an end.
    binding: Binding,
    /// `end_on_window_closed` rules: the windows that showed the call.
    windows: WindowBinding,
    /// Transient failures in a row.
    failures: u32,
    /// No snapshot before this recording second (transient backoff).
    retry_at_s: Option<f64>,
    health: ReaderHealth,
}
/// Which call a reader incarnation is tied to; in memory only, dropped
/// with the incarnation.
enum Binding {
    /// No active call seen yet.
    Unset,
    /// Hashed identity of the one identifiable call seen active.
    Bound(String),
    /// An active call without a readable identity, or two different ones:
    /// no explicit end is ever reported by this incarnation.
    Unbindable,
}
/// The top-level windows an incarnation saw showing the call.
enum WindowBinding {
    Unset,
    /// Every window token that showed an in-call control.
    Bound(BTreeSet<String>),
    /// A control was seen outside any trackable window: no window end.
    Unbindable,
}
struct ManualLease {
    users: usize,
    owned: bool,
    state: ManualAccessibility,
}

/// What a snapshot's tree showed, computed where the snapshot ran.
struct Prepared {
    extracted: Extracted,
    call: CallReading,
    /// Bound windows absent from the tree that the platform verified
    /// destroyed.
    destroyed: Vec<String>,
    scoped_documents: u32,
    skipped_documents: u32,
}

/// A due accessibility snapshot. [`SnapshotJob::run`] may run on any thread
/// (a slow tree must not block GPUI's loop); hand its result back to
/// [`ReaderHost::finish_poll`] on the loop. Its call-observation sequence
/// and wall-clock start were taken when it was begun, before any
/// asynchronous work. `run` also evaluates the rule on the snapshot, so the
/// loop only applies the result to the reader's history.
pub struct SnapshotJob {
    session_id: String,
    generation: u64,
    elapsed_s: f64,
    sequence: u64,
    snapshot_started_at_ms: u64,
    reader: Arc<dyn AppUiReader>,
    scope: AxScope,
    rule: Arc<ReaderRule>,
    /// The incarnation's bound windows when the job began.
    bound_windows: Vec<String>,
    prepared: Mutex<Option<Prepared>>,
}
impl SnapshotJob {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn snapshot_started_at_ms(&self) -> u64 {
        self.snapshot_started_at_ms
    }
    /// Reads the app's tree within the incarnation's scope and evaluates
    /// the rule on it; blocking, off GPUI's loop. Pass the result to
    /// [`ReaderHost::finish_poll`] unchanged.
    pub fn run(&self) -> Result<AxTree> {
        let tree = self.reader.snapshot_in(&self.scope, AxLimits::default())?;
        let prepared = self.prepare(&tree);
        *self
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(prepared);
        Ok(tree)
    }
    fn prepare(&self, tree: &AxTree) -> Prepared {
        let call = self.rule.read_call(tree);
        let missing: Vec<String> = self
            .bound_windows
            .iter()
            .filter(|token| !call.windows.contains(token))
            .cloned()
            .collect();
        // Only a complete snapshot with nothing in call can end a call:
        // the probe is not even made otherwise.
        let destroyed = if call.complete && call.active.is_empty() && !missing.is_empty() {
            // A failed probe proves nothing.
            self.reader
                .destroyed(&self.scope, &missing)
                .unwrap_or_default()
                .into_iter()
                .filter(|token| missing.contains(token))
                .collect()
        } else {
            Vec::new()
        };
        let (mut scoped, mut skipped) = (0, 0);
        count_documents(&self.scope, &tree.root, &mut scoped, &mut skipped);
        Prepared {
            extracted: self.rule.extract(tree),
            call,
            destroyed,
            scoped_documents: scoped,
            skipped_documents: skipped,
        }
    }
}

fn count_documents(
    scope: &AxScope,
    node: &broscribe_platform::AxNode,
    scoped: &mut u32,
    skipped: &mut u32,
) {
    if node.role == "AXWebArea" {
        if scope.skips(node) {
            *skipped += 1;
        } else {
            *scoped += 1;
        }
    }
    for child in &node.children {
        count_documents(scope, child, scoped, skipped);
    }
}

fn incomplete_reason(incomplete: AxIncomplete) -> Option<IncompleteReason> {
    // The registry stays full for the rest of the incarnation: the cause
    // that keeps every later window or document unidentified comes first.
    if incomplete.identity_cap {
        Some(IncompleteReason::IdentityCap)
    } else if incomplete.read_failed {
        Some(IncompleteReason::ReadFailed)
    } else if incomplete.time_budget {
        Some(IncompleteReason::TimeBudget)
    } else if incomplete.node_cap {
        Some(IncompleteReason::NodeCap)
    } else if incomplete.depth_cap {
        Some(IncompleteReason::DepthCap)
    } else if incomplete.clipped {
        Some(IncompleteReason::Clipped)
    } else {
        None
    }
}

/// What [`ReaderHost::finish_poll`] made of a snapshot.
#[derive(Debug)]
pub enum Polled {
    /// The snapshot belonged to a stopped or restarted reader: nothing to send.
    Stale,
    /// Send it (its events may be empty; its call observation is the point).
    /// A transient failure is a batch too: incomplete, `Unknown`, and the
    /// reader keeps running.
    Batch(ReaderBatch),
    /// The snapshot failed and the reader stopped: send this last batch (an
    /// incomplete `Unknown` observation), then report `error`.
    Failed { batch: ReaderBatch, error: Error },
}

impl Polled {
    /// The batch to send, if any.
    pub fn batch(&self) -> Option<&ReaderBatch> {
        match self {
            Polled::Stale => None,
            Polled::Batch(batch) | Polled::Failed { batch, .. } => Some(batch),
        }
    }
}

/// Host wall clock, milliseconds since the Unix epoch.
pub type WallClock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn system_wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub struct ReaderHost {
    reader: Arc<dyn AppUiReader>,
    sessions: HashMap<String, Running>,
    manual: HashMap<u32, ManualLease>,
    generation: u64,
    wall_ms: WallClock,
}
impl ReaderHost {
    pub fn new(reader: Arc<dyn AppUiReader>) -> Self {
        Self::with_wall_clock(reader, Arc::new(system_wall_ms))
    }
    /// [`Self::new`] with an injected wall clock (tests).
    pub fn with_wall_clock(reader: Arc<dyn AppUiReader>, wall_ms: WallClock) -> Self {
        Self {
            reader,
            sessions: HashMap::new(),
            manual: HashMap::new(),
            generation: 0,
            wall_ms,
        }
    }
    /// Returns false for ignored/off/unaccepted/paused detections, touching no
    /// AX API. A refusal (no rule, no permission, an unreadable rule set) is
    /// typed and non-fatal for the caller: nothing of the start remains.
    pub fn start(
        &mut self,
        session: ReaderSession,
    ) -> std::result::Result<bool, ReaderStartRefusal> {
        self.stop(&session.session_id);
        if session.paused
            || session.policy == SessionPolicy::Off
            || (session.policy == SessionPolicy::Ask && !session.accepted)
        {
            return Ok(false);
        }
        let rule = builtin_rules()
            .map_err(|_| ReaderStartRefusal::Observe)?
            .into_iter()
            .find(|r| r.id == session.app)
            .ok_or(ReaderStartRefusal::NoRule)?;
        if !self.reader.is_trusted() {
            return Err(ReaderStartRefusal::Untrusted);
        }
        let manual = rule.manual_accessibility;
        let manual_state = if manual {
            self.acquire_manual(session.pid)
        } else {
            ManualAccessibility::NotApplicable
        };
        let scope = self.reader.scope(session.pid, rule.scope_policy());
        let dirty = Arc::new(AtomicBool::new(true));
        let notify = Arc::clone(&dirty);
        // Notifications unsupported by an app are not fatal: snapshots run
        // on the heartbeat anyway. No second run loop or polling thread.
        let observation = self
            .reader
            .observe(
                session.pid,
                &AxChange::ALL,
                Box::new(move |_| {
                    notify.store(true, Ordering::Relaxed);
                }),
            )
            .ok();
        self.generation += 1;
        self.sessions.insert(
            session.session_id.clone(),
            Running {
                session,
                engine: RuleEngine::shared(Arc::new(rule)),
                observation,
                scope,
                dirty,
                last_s: None,
                manual,
                generation: self.generation,
                in_flight: false,
                sequence: 0,
                binding: Binding::Unset,
                windows: WindowBinding::Unset,
                failures: 0,
                retry_at_s: None,
                health: ReaderHealth {
                    manual_accessibility: manual_state,
                    ..ReaderHealth::default()
                },
            },
        );
        Ok(true)
    }
    /// Takes one user of `pid`'s manual-accessibility lease. The first user
    /// turns it on only when the app reports it off, and owns it only once
    /// the app reads back on; any failure leaves the reader running without
    /// it.
    fn acquire_manual(&mut self, pid: u32) -> ManualAccessibility {
        if let Some(lease) = self.manual.get_mut(&pid) {
            lease.users += 1;
            return lease.state;
        }
        let (owned, state) = match self.reader.manual_accessibility(pid) {
            Ok(None) => (false, ManualAccessibility::Unsupported),
            Ok(Some(true)) => (false, ManualAccessibility::AlreadyOn),
            Err(_) => (false, ManualAccessibility::Failed),
            Ok(Some(false)) => {
                let set = self.reader.set_manual_accessibility(pid, true);
                match (set, self.reader.manual_accessibility(pid)) {
                    (Ok(()), Ok(Some(true))) => (true, ManualAccessibility::EnabledByBroscribe),
                    _ => (false, ManualAccessibility::Failed),
                }
            }
        };
        self.manual.insert(
            pid,
            ManualLease {
                users: 1,
                owned,
                state,
            },
        );
        state
    }
    /// Gives back one user of `pid`'s lease; the last one turns the switch
    /// off again only if BroScribe turned it on and it is still on.
    fn release_manual(&mut self, pid: u32) {
        let Some(lease) = self.manual.get_mut(&pid) else {
            return;
        };
        lease.users -= 1;
        if lease.users > 0 {
            return;
        }
        let owned = lease.owned;
        self.manual.remove(&pid);
        if owned && matches!(self.reader.manual_accessibility(pid), Ok(Some(true))) {
            let _ = self.reader.set_manual_accessibility(pid, false);
        }
    }
    pub fn set_self_aliases(&mut self, session_id: &str, aliases: Vec<String>) {
        if let Some(run) = self.sessions.get_mut(session_id) {
            run.engine.set_self_aliases(aliases);
        }
    }
    pub fn pause(&mut self, session_id: &str) {
        self.stop(session_id);
    }
    pub fn stop(&mut self, session_id: &str) {
        if let Some(run) = self.sessions.remove(session_id) {
            let pid = run.session.pid;
            let manual = run.manual;
            // Detach the observation and release the scope before
            // restoring the app's switch.
            drop(run);
            if manual {
                self.release_manual(pid);
            }
        }
    }
    /// The latest diagnostics of `session_id`'s reader.
    pub fn health(&self, session_id: &str) -> Option<ReaderHealth> {
        self.sessions.get(session_id).map(|run| run.health.clone())
    }
    /// Begins and finishes a due snapshot on the calling thread (GPUI uses
    /// [`Self::begin_poll`]); `None` when none is due.
    pub fn poll(&mut self, session_id: &str, elapsed_s: f64) -> Result<Option<Polled>> {
        let Some(job) = self.begin_poll(session_id, elapsed_s)? else {
            return Ok(None);
        };
        let tree = job.run();
        Ok(Some(self.finish_poll(job, tree)))
    }
    /// The snapshot due for `session_id` at `elapsed_s` (recording seconds,
    /// pauses excluded), if any: never within [`MIN_INTERVAL_S`] of the last
    /// one or before a transient-failure backoff ends, then whenever the
    /// app posted a change, has no observer, or [`HEARTBEAT_S`] passed; none
    /// while an earlier one is still out. Its sequence and wall-clock start
    /// are assigned here.
    pub fn begin_poll(&mut self, session_id: &str, elapsed_s: f64) -> Result<Option<SnapshotJob>> {
        if !elapsed_s.is_finite() || elapsed_s < 0.0 {
            return Err(Error::InvalidInput("invalid recording timeline".into()));
        }
        let Some(run) = self.sessions.get_mut(session_id) else {
            return Ok(None);
        };
        if run.in_flight || run.retry_at_s.is_some_and(|at| elapsed_s < at) {
            return Ok(None);
        }
        if let Some(last) = run.last_s {
            let since = elapsed_s - last;
            if since < MIN_INTERVAL_S {
                return Ok(None);
            }
            if since < HEARTBEAT_S
                && run.observation.is_some()
                && !run.dirty.load(Ordering::Relaxed)
            {
                return Ok(None);
            }
        }
        run.last_s = Some(elapsed_s);
        run.dirty.swap(false, Ordering::Relaxed);
        run.in_flight = true;
        run.sequence += 1;
        let bound_windows = match &run.windows {
            WindowBinding::Bound(windows) => windows.iter().cloned().collect(),
            WindowBinding::Unset | WindowBinding::Unbindable => Vec::new(),
        };
        Ok(Some(SnapshotJob {
            session_id: session_id.into(),
            generation: run.generation,
            elapsed_s,
            sequence: run.sequence,
            snapshot_started_at_ms: (self.wall_ms)(),
            reader: Arc::clone(&self.reader),
            scope: run.scope.clone(),
            rule: Arc::clone(&run.engine.rule),
            bound_windows,
            prepared: Mutex::new(None),
        }))
    }
    /// Applies a finished [`SnapshotJob`]; stale when its reader stopped or
    /// restarted meanwhile. A transient failure keeps the reader and backs
    /// off; any other failure stops the reader. Both yield an incomplete
    /// `Unknown` observation.
    pub fn finish_poll(&mut self, job: SnapshotJob, tree: Result<AxTree>) -> Polled {
        let Some(run) = self
            .sessions
            .get_mut(&job.session_id)
            .filter(|run| run.generation == job.generation)
        else {
            return Polled::Stale;
        };
        run.in_flight = false;
        let base = |run: &Running, state: CallState, complete: bool| ReaderBatch {
            session_id: job.session_id.clone(),
            app: run.session.app.clone(),
            pid: run.session.pid,
            reader_generation: run.session.reader_generation,
            call_observation: CallObservation {
                sequence: job.sequence,
                snapshot_started_at_ms: job.snapshot_started_at_ms,
                state,
                complete,
            },
            events: Vec::new(),
            health: run.health.clone(),
        };
        let tree = match tree {
            Ok(tree) => tree,
            Err(error) if is_ax_transient(&error) => {
                run.failures = run.failures.saturating_add(1);
                let exponent = i32::try_from(run.failures - 1).unwrap_or(i32::MAX).min(16);
                let delay = (MIN_INTERVAL_S * 2f64.powi(exponent)).min(MAX_BACKOFF_S);
                run.retry_at_s = Some(job.elapsed_s + delay);
                run.health.incomplete = Some(IncompleteReason::Transient);
                run.health.consecutive_failures = run.failures;
                run.health.retry_after_s = run.retry_at_s;
                return Polled::Batch(base(run, CallState::Unknown, false));
            }
            Err(error) => {
                run.health.incomplete = Some(IncompleteReason::Failed);
                run.health.consecutive_failures = run.failures.saturating_add(1);
                run.health.retry_after_s = None;
                let batch = base(run, CallState::Unknown, false);
                self.stop(&job.session_id);
                return Polled::Failed { batch, error };
            }
        };
        let prepared = job
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // `run` evaluated the tree it returned; a tree produced otherwise
        // is evaluated here.
        let prepared = prepared.unwrap_or_else(|| job.prepare(&tree));
        let reading = &prepared.call;
        let state = if !reading.complete {
            CallState::Unknown
        } else if !reading.active.is_empty() {
            run.binding = match (&run.binding, reading.active.as_slice()) {
                (Binding::Unset, [Some(id)]) => Binding::Bound(id.clone()),
                (Binding::Bound(bound), [Some(id)]) if bound == id => Binding::Bound(id.clone()),
                _ => Binding::Unbindable,
            };
            for window in &reading.active_windows {
                run.windows = match (
                    std::mem::replace(&mut run.windows, WindowBinding::Unset),
                    window,
                ) {
                    (WindowBinding::Unbindable, _) | (_, None) => WindowBinding::Unbindable,
                    (WindowBinding::Unset, Some(token)) => {
                        WindowBinding::Bound(BTreeSet::from([token.clone()]))
                    }
                    (WindowBinding::Bound(mut windows), Some(token)) => {
                        windows.insert(token.clone());
                        WindowBinding::Bound(windows)
                    }
                };
            }
            CallState::Active
        } else if matches!(&run.binding, Binding::Bound(bound) if reading.ended.contains(bound)) {
            CallState::Ended
        } else if matches!(&run.windows, WindowBinding::Bound(windows)
            if windows.iter().all(|w| prepared.destroyed.contains(w)))
        {
            // Every window that showed this call was verified destroyed,
            // and nothing anywhere is in call.
            CallState::Ended
        } else {
            // Nothing, another call's or document's post-call page, windows
            // that merely went missing, or an end before any identifiable
            // active call: proves nothing.
            CallState::Unknown
        };
        run.failures = 0;
        run.retry_at_s = None;
        run.health.consecutive_failures = 0;
        run.health.retry_after_s = None;
        run.health.incomplete = if run.engine.rule.call.is_none() && !tree.truncated() {
            Some(IncompleteReason::NoCallRule)
        } else {
            incomplete_reason(tree.incomplete)
        };
        run.health.scoped_documents = prepared.scoped_documents;
        run.health.skipped_documents = prepared.skipped_documents;
        let complete = reading.complete;
        let events = run.engine.apply(prepared.extracted, job.elapsed_s);
        let mut batch = base(run, state, complete);
        batch.events = events;
        Polled::Batch(batch)
    }
    pub fn session_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }
}
impl Drop for ReaderHost {
    fn drop(&mut self) {
        for id in self.session_ids() {
            self.stop(&id);
        }
    }
}

#[cfg(test)]
mod tests;
