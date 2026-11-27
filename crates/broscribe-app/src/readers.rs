//! GPUI-loop AX host. The daemon subscription forwards StartReader/StopReader
//! to `handle_event`; AX never runs in `broscribed` or a second UI loop.
//! Observers stay on GPUI's loop; whatever else may block runs on GPUI's
//! background executor: making the platform reader (on the first start), a
//! start's rule lookup, permission check, `reader.attach` and
//! `AXManualAccessibility` acquisition, a stopped reader's release of that
//! switch, snapshots (blocking tree reads) and event transport. Every
//! completed snapshot is sent, deduplicated events or not: its call
//! observation is the heartbeat the daemon's automatic stop needs (plan H),
//! at least every 2 s while a reader runs.
//!
//! Incarnations are daemon-issued: before a reader starts (first start,
//! after this host stopped it on a lag, a reconnect or a failed snapshot)
//! the host asks `reader.attach` for a fresh one, naming the generation the
//! daemon announced, and only then starts its sequences at 1. A
//! `start_reader` naming the incarnation this host already runs, the
//! announcement it attached from or an older one (a plain subscription
//! snapshot, a resync, an announcement still queued behind the snapshot)
//! changes nothing. A failed attach or start leaves no clock and no polling
//! behind.
//!
//! A start this host could not make is reported to the daemon
//! (`reader.refused`) as one typed, content-free stage: an observed
//! untrusted access, no rule for the app, an unreadable rule set before the
//! attach, an attach that issued nothing this host knows of, or a start
//! that could not begin observing after it. It names only a generation
//! this host actually knows (the one announced before the attach, the one
//! issued after it), never a guessed one; the daemon keeps it only while
//! that incarnation is current. It is never call evidence, never a batch
//! or a heartbeat.
//!
//! The announced recording time is anchored when the announcement arrives,
//! so the time a start spends being prepared is never lost from the
//! reader's timeline.
//!
//! `AXManualAccessibility` (Chromium's and Electron's switch) keeps the
//! host's rules: a start turns it on only when the app reports it off and
//! owns it only once it reads back on, and it goes off again only if
//! BroScribe turned it on and it is still on. These reads and writes run in
//! one queue, in the order they were asked for. A switch BroScribe turned
//! on is one lease per app process, claimed by every start being prepared
//! and every reader running that relies on it: a later start of that
//! process, of the same session or another, joins the lease instead of
//! reading the switch BroScribe turned on as the app's own, and the switch
//! goes back only with the lease's last claim, whichever start or reader
//! that is — a start abandoned after it turned the switch on gives it back
//! only when nothing else claims it. Quitting gives switches back at once.
use broscribe_core::maintenance::ModeLease;
use broscribe_core::{Library, Result, config::Config};
use broscribe_ipc::{Call, Event, IpcError};
use broscribe_platform::{
    AppUiReader, AxChange, AxChangeCallback, AxLimits, AxObservation, AxScope, AxScopePolicy,
    AxTree,
};
use broscribe_platform_native::NativePlatform;
use broscribe_platform_native::ipc::{ANSWER_TIMEOUT, CallError, CancelToken, Client, Endpoint};
use broscribe_readers::{
    Polled, ReaderBatch, ReaderHost, ReaderSession, ReaderStartRefusal, SessionPolicy, SnapshotJob,
    builtin_rules,
};
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use gpui::{App, AppContext as _, Context, Entity, Task};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

struct Clock {
    anchor: Instant,
    elapsed_s: f64,
    aliases_ready: bool,
}

/// Why `reader.attach` issued no incarnation this host can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachFailure {
    /// The daemon refused it as stale: the announced incarnation is no
    /// longer current or the session has no reader. Nothing to report.
    Stale,
    /// Nothing this host knows of was issued: not sent, another error, an
    /// answer without a generation, or an outcome that stayed unknown.
    Failed,
}

/// A start this host could not make (`reader.refused`): the generation it
/// actually knows and the typed stage that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartRefused {
    pub session_id: String,
    pub app_id: String,
    pub pid: u32,
    pub reader_generation: u64,
    pub refusal: ReaderStartRefusal,
}

/// The daemon side of a reader host: fresh incarnations (`reader.attach`)
/// and the starts it could not make (`reader.refused`).
pub trait ReaderLink: Send + Sync {
    /// The new generation for `session_id`, issued only while `told` (the
    /// generation `start_reader` announced) is still current. `cancel` is
    /// set once the start is no longer wanted.
    fn attach(
        &self,
        session_id: &str,
        told: u64,
        cancel: &CancelToken,
    ) -> std::result::Result<u64, AttachFailure>;
    /// Reports a start that could not be made; best effort, off GPUI's
    /// loop. The daemon keeps it only while its incarnation is current.
    fn refuse(&self, refused: &StartRefused);
}

/// One `reader.attach` exchange: connecting, sending and its answer.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a lost `reader.attach` answer is asked for by its request ID.
const ATTACH_RECONCILE: Duration = Duration::from_secs(30);

/// [`ReaderLink`] over the agent's local endpoint. A lost answer is asked
/// for by the same request ID (`request.status`), never by attaching again:
/// the generation the daemon issued is the one this host runs. When that
/// stays unknown the start fails and this host forgets the incarnation;
/// the daemon announces the current one again once nothing was heard from
/// it, and the host attaches from that.
pub struct AgentLink(Endpoint);
impl ReaderLink for AgentLink {
    fn attach(
        &self,
        session_id: &str,
        told: u64,
        cancel: &CancelToken,
    ) -> std::result::Result<u64, AttachFailure> {
        let value = Client::call_mutating_cancellable(
            &self.0,
            Call::ReaderAttach {
                session_id: session_id.to_owned(),
                reader_generation: told,
            },
            ATTACH_TIMEOUT,
            ATTACH_RECONCILE,
            cancel,
        )
        .map_err(|error| match error {
            // Typed codes, never message text.
            CallError::Remote(e)
                if e.code == IpcError::STALE_READER
                    || e.code == broscribe_core::lifecycle::STALE_SESSION =>
            {
                AttachFailure::Stale
            }
            CallError::Remote(_) | CallError::NotSent(_) | CallError::Uncertain { .. } => {
                AttachFailure::Failed
            }
        })?;
        value["reader_generation"]
            .as_u64()
            .ok_or(AttachFailure::Failed)
    }
    fn refuse(&self, refused: &StartRefused) {
        let call = Call::ReaderRefused {
            session_id: refused.session_id.clone(),
            app_id: refused.app_id.clone(),
            pid: refused.pid,
            reader_generation: refused.reader_generation,
            refusal: refused.refusal,
        };
        // Idempotent and fenced by generation: one try; a lost report
        // leaves the daemon's honest "silent", and the next announcement
        // reports again.
        let outcome = match Client::connect_daemon(&self.0, ATTACH_TIMEOUT) {
            Ok(Some(mut client)) => client.call(call).map(drop),
            Ok(None) | Err(_) => Ok(()),
        };
        if let Err(CallError::Remote(e)) = &outcome
            && e.code != IpcError::STALE_READER
        {
            tracing::warn!(code = %e.code, "reporting a reader start refusal");
        }
    }
}

/// The incarnation of one session's reader on this host.
struct Incarnation {
    /// The generation `start_reader` announced, which the attach named.
    told: u64,
    /// The generation the daemon issued; `None` while the start is prepared.
    issued: Option<u64>,
    /// Set when this incarnation stops: its pending attach gives up.
    cancel: CancelToken,
    /// What its start acquired of the app's switch: while it runs, its
    /// claim on the switch's lease. Forgetting the incarnation gives it
    /// back.
    _claim: Option<Acquired>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Makes the platform's accessibility reader; called once, off GPUI's loop.
type ReaderFactory = Box<dyn FnOnce() -> Arc<dyn AppUiReader> + Send>;

/// The platform reader, made by the first start that needs it.
type LazyReader = LazyLock<Arc<StagedReader>, Box<dyn FnOnce() -> Arc<StagedReader> + Send>>;

fn lazy_reader(make: ReaderFactory, queue: Arc<ManualQueue>) -> LazyReader {
    LazyLock::new(Box::new(move || {
        Arc::new(StagedReader {
            inner: make(),
            queue,
            staged: Mutex::default(),
        })
    }))
}

/// `AXManualAccessibility` reads and writes, in the order they were asked
/// for, and the switches BroScribe turned on.
struct ManualQueue {
    _admission: Arc<ModeLease>,
    jobs: Mutex<VecDeque<ManualJob>>,
    /// Held while jobs run: by the background worker, or by a host call that
    /// must see every earlier job done first.
    running: Mutex<()>,
    /// The app quits: a job runs where it is made, and nothing is acquired.
    closed: AtomicBool,
    /// Per app process, the switch BroScribe turned on (read off, turned on,
    /// read back on): one lease, made by one acquisition and given back once.
    leases: Mutex<HashMap<u32, Lease>>,
    wake: mpsc::UnboundedSender<()>,
}
/// One app's switch that BroScribe turned on.
struct Lease {
    /// The reader that turned it on, and turns it off.
    reader: Arc<dyn AppUiReader>,
    /// Starts being prepared and readers running that rely on it. At zero
    /// its release is queued; a start acquiring before that release runs
    /// claims it again, and the release then leaves it on.
    claims: usize,
}
enum ManualJob {
    /// A start's acquisition, made ahead for the host.
    Acquire {
        reader: Arc<dyn AppUiReader>,
        pid: u32,
        reply: oneshot::Sender<Acquired>,
    },
    /// `pid`'s lease goes back, unless it was claimed again meanwhile.
    Release { pid: u32 },
}
impl ManualQueue {
    /// The queue and its background worker.
    fn new(admission: Arc<ModeLease>) -> (Arc<Self>, impl Future<Output = ()> + Send + 'static) {
        let (wake, mut woken) = mpsc::unbounded();
        let queue = Arc::new(Self {
            _admission: admission,
            jobs: Mutex::default(),
            running: Mutex::new(()),
            closed: AtomicBool::new(false),
            leases: Mutex::default(),
            wake,
        });
        let worker = {
            let queue = Arc::clone(&queue);
            async move {
                while woken.next().await.is_some() {
                    queue.run_queued();
                }
            }
        };
        (queue, worker)
    }

    fn push(self: &Arc<Self>, job: ManualJob) {
        if self.closed.load(Ordering::Acquire) {
            self.run(job);
            return;
        }
        lock(&self.jobs).push_back(job);
        self.wake.unbounded_send(()).ok();
    }

    /// Runs every queued job, after the one running now.
    fn run_queued(self: &Arc<Self>) {
        let _running = lock(&self.running);
        loop {
            let job = lock(&self.jobs).pop_front();
            let Some(job) = job else {
                break;
            };
            self.run(job);
        }
    }

    fn run(self: &Arc<Self>, job: ManualJob) {
        match job {
            ManualJob::Acquire { reader, pid, reply } => {
                if self.closed.load(Ordering::Acquire) || reply.is_canceled() {
                    return;
                }
                let joined = if let Some(lease) = lock(&self.leases).get_mut(&pid) {
                    lease.claims += 1;
                    true
                } else {
                    false
                };
                let acquired = if joined {
                    Acquired::joined(pid, self)
                } else {
                    self.acquire_now(reader, pid)
                };
                // No longer awaited: dropping it gives its claim back.
                reply.send(acquired).ok();
            }
            ManualJob::Release { pid } => {
                let released = {
                    let mut leases = lock(&self.leases);
                    // Not when claimed again, or already given back.
                    if leases.get(&pid).is_some_and(|lease| lease.claims == 0) {
                        leases.remove(&pid)
                    } else {
                        None
                    }
                };
                if let Some(lease) = released {
                    restore(lease.reader.as_ref(), pid);
                }
            }
        }
    }

    /// The host's acquisition of a switch BroScribe holds no lease on: read
    /// it; when off, turn it on and read it back. Only a switch read back on
    /// becomes a lease, claimed by this acquisition.
    fn acquire_now(self: &Arc<Self>, reader: Arc<dyn AppUiReader>, pid: u32) -> Acquired {
        let first = reader.manual_accessibility(pid);
        let off = matches!(first, Ok(Some(false)));
        let mut acquired = Acquired {
            pid,
            reads: VecDeque::from([first]),
            turn_on: None,
            claim: None,
        };
        if off {
            let turned_on = reader.set_manual_accessibility(pid, true);
            let read_back = reader.manual_accessibility(pid);
            if turned_on.is_ok() && matches!(read_back, Ok(Some(true))) {
                lock(&self.leases).insert(pid, Lease { reader, claims: 1 });
                acquired.claim = Some(Arc::clone(self));
            }
            acquired.turn_on = Some(turned_on);
            acquired.reads.push_back(read_back);
        }
        acquired
    }

    /// Gives back one claim on `pid`'s lease; the last one queues its
    /// release.
    fn unclaim(self: &Arc<Self>, pid: u32) {
        let last = lock(&self.leases).get_mut(&pid).is_some_and(|lease| {
            lease.claims -= 1;
            lease.claims == 0
        });
        if last {
            self.push(ManualJob::Release { pid });
        }
    }

    /// One start's acquisition, after every release asked for before it.
    async fn acquire(self: Arc<Self>, reader: Arc<dyn AppUiReader>, pid: u32) -> Option<Acquired> {
        let (reply, acquired) = oneshot::channel();
        self.push(ManualJob::Acquire { reader, pid, reply });
        acquired.await.ok()
    }

    /// Quitting: what is queued runs now, and later jobs where they are
    /// made. Nothing starts any more, so every switch BroScribe still holds
    /// goes back now, whatever still claims it.
    fn close(self: &Arc<Self>) {
        self.closed.store(true, Ordering::Release);
        self.run_queued();
        let leases: Vec<_> = lock(&self.leases).drain().collect();
        for (pid, lease) in leases {
            restore(lease.reader.as_ref(), pid);
        }
    }
}

/// Turns the switch BroScribe turned on off again, if it is still on.
fn restore(reader: &dyn AppUiReader, pid: u32) {
    if matches!(reader.manual_accessibility(pid), Ok(Some(true))) {
        reader.set_manual_accessibility(pid, false).ok();
    }
}

/// A start's `AXManualAccessibility` acquisition, made ahead the way the
/// host makes it and replayed to it, with the start's claim on the lease
/// when BroScribe holds the switch. Dropped, it gives that claim back.
struct Acquired {
    pid: u32,
    reads: VecDeque<Result<Option<bool>>>,
    turn_on: Option<Result<()>>,
    claim: Option<Arc<ManualQueue>>,
}
impl Acquired {
    /// A claim on the lease of a switch BroScribe turned on. It replays the
    /// acquisition that made the lease — read off, turned on, read back on —
    /// never a read of the switch BroScribe set, so the host records it as
    /// BroScribe's to give back, as it does for a reader joining its own
    /// lease of the app.
    fn joined(pid: u32, queue: &Arc<ManualQueue>) -> Self {
        Self {
            pid,
            reads: VecDeque::from([Ok(Some(false)), Ok(Some(true))]),
            turn_on: Some(Ok(())),
            claim: Some(Arc::clone(queue)),
        }
    }
}
impl Drop for Acquired {
    fn drop(&mut self) {
        if let Some(queue) = self.claim.take() {
            queue.unclaim(self.pid);
        }
    }
}

/// The reader the main-thread [`ReaderHost`] uses: a start's permission
/// check and acquisition, made ahead on the background executor, are
/// replayed to it, and a stopping reader leaves the switch to its lease.
/// Every other call goes to the platform reader.
struct StagedReader {
    inner: Arc<dyn AppUiReader>,
    queue: Arc<ManualQueue>,
    staged: Mutex<Staging>,
}
/// Set for the length of one host call on GPUI's loop.
#[derive(Default)]
struct Staging {
    /// The start's permission, checked ahead.
    trusted: bool,
    acquired: Option<Acquired>,
    /// Readers stop: the switch goes back with its lease's last claim.
    releasing: bool,
}
impl StagedReader {
    /// Starts `session` with what was made ahead; returns what it left.
    fn start(
        &self,
        host: &mut ReaderHost,
        session: ReaderSession,
        acquired: Option<Acquired>,
    ) -> (
        std::result::Result<bool, ReaderStartRefusal>,
        Option<Acquired>,
    ) {
        *lock(&self.staged) = Staging {
            trusted: true,
            acquired,
            releasing: false,
        };
        let started = host.start(session);
        let left = std::mem::take(&mut *lock(&self.staged)).acquired;
        (started, left)
    }

    /// Runs a host call that may stop readers; the switch they leave goes
    /// back with its lease's last claim, in the queue.
    fn releasing<R>(&self, call: impl FnOnce() -> R) -> R {
        lock(&self.staged).releasing = true;
        let result = call();
        lock(&self.staged).releasing = false;
        result
    }
}
impl AppUiReader for StagedReader {
    fn is_trusted(&self) -> bool {
        let checked = std::mem::take(&mut lock(&self.staged).trusted);
        checked || self.inner.is_trusted()
    }
    fn request_trust(&self) -> bool {
        self.inner.request_trust()
    }
    fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree> {
        self.inner.snapshot(pid, limits)
    }
    fn scope(&self, pid: u32, policy: AxScopePolicy) -> AxScope {
        self.inner.scope(pid, policy)
    }
    fn snapshot_in(&self, scope: &AxScope, limits: AxLimits) -> Result<AxTree> {
        self.inner.snapshot_in(scope, limits)
    }
    fn destroyed(&self, scope: &AxScope, elements: &[String]) -> Result<Vec<String>> {
        self.inner.destroyed(scope, elements)
    }
    fn observe(
        &self,
        pid: u32,
        changes: &[AxChange],
        callback: AxChangeCallback,
    ) -> Result<Box<dyn AxObservation>> {
        self.inner.observe(pid, changes, callback)
    }
    fn manual_accessibility(&self, pid: u32) -> Result<Option<bool>> {
        {
            let mut staged = lock(&self.staged);
            if let Some(read) = staged
                .acquired
                .as_mut()
                .filter(|acquired| acquired.pid == pid)
                .and_then(|acquired| acquired.reads.pop_front())
            {
                return read;
            }
            // A stopping reader's restore is its lease's: nothing here for
            // the host to turn off, and no read of the switch on GPUI's loop.
            if staged.releasing {
                return Ok(None);
            }
        }
        // Anything else sees every queued change first.
        self.queue.run_queued();
        self.inner.manual_accessibility(pid)
    }
    fn set_manual_accessibility(&self, pid: u32, enabled: bool) -> Result<()> {
        {
            let mut staged = lock(&self.staged);
            if enabled
                && let Some(turned_on) = staged
                    .acquired
                    .as_mut()
                    .filter(|acquired| acquired.pid == pid)
                    .and_then(|acquired| acquired.turn_on.take())
            {
                return turned_on;
            }
        }
        self.queue.run_queued();
        self.inner.set_manual_accessibility(pid, enabled)
    }
}

/// What a start's preparation made.
struct Prepared {
    reader: Arc<StagedReader>,
    generation: u64,
    acquired: Option<Acquired>,
}

/// The `reader.attach` a start makes.
struct AttachRequest {
    session_id: String,
    /// The announced generation it names.
    told: u64,
    /// Set once the start is no longer wanted.
    cancel: CancelToken,
}

/// Why a start was not prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotPrepared {
    /// The announced incarnation could not be started; reported for it.
    Refused(ReaderStartRefusal),
    /// The daemon refused the attach as stale: nothing to report.
    Stale,
}

/// Off GPUI's loop: the platform reader, the app's rule, the permission, a
/// daemon-issued incarnation, then (when the rule uses it) the app's switch
/// in the queue's order: acquired, or a claim on the lease BroScribe holds
/// on it. Each failure is typed by its stage, never by its text.
async fn prepare(
    native: Arc<LazyReader>,
    link: Arc<dyn ReaderLink>,
    attach: AttachRequest,
    app: String,
    pid: u32,
) -> std::result::Result<Prepared, NotPrepared> {
    use NotPrepared::Refused;
    let reader = Arc::clone(LazyLock::force(&native));
    let manual = builtin_rules()
        .map_err(|_| Refused(ReaderStartRefusal::Prepare))?
        .into_iter()
        .find(|rule| rule.id == app)
        .ok_or(Refused(ReaderStartRefusal::NoRule))?
        .manual_accessibility;
    if !reader.inner.is_trusted() {
        return Err(Refused(ReaderStartRefusal::Untrusted));
    }
    let generation = link
        .attach(&attach.session_id, attach.told, &attach.cancel)
        .map_err(|failure| match failure {
            AttachFailure::Stale => NotPrepared::Stale,
            AttachFailure::Failed => Refused(ReaderStartRefusal::Attach),
        })?;
    let acquired = if manual {
        Arc::clone(&reader.queue)
            .acquire(Arc::clone(&reader.inner), pid)
            .await
    } else {
        None
    };
    Ok(Prepared {
        reader,
        generation,
        acquired,
    })
}

/// The host, once a start made the platform reader.
struct Hosted {
    host: ReaderHost,
    reader: Arc<StagedReader>,
}

/// Keep this entity alive for the app/accessory lifetime. Its drop detaches AX
/// observers, restores Chromium switches we own, and cancels its tasks.
pub struct AppReaders {
    hosted: Option<Hosted>,
    native: Arc<LazyReader>,
    manual: Arc<ManualQueue>,
    clocks: HashMap<String, Clock>,
    incarnations: HashMap<String, Incarnation>,
    link: Arc<dyn ReaderLink>,
    outgoing: mpsc::Sender<ReaderBatch>,
    data_dir: PathBuf,
    admission: Arc<ModeLease>,
    /// The 1 Hz snapshot loop; runs only while a reader session exists, so an
    /// idle app never wakes GPUI's loop.
    poll: Option<Task<()>>,
    _manual: Task<()>,
    _send: Task<()>,
    _dispatch: Task<()>,
}
impl AppReaders {
    /// Makes nothing platform-specific on GPUI's loop: the first start makes
    /// the platform reader on the background executor.
    pub fn attach(data_dir: PathBuf, admission: Arc<ModeLease>, cx: &mut App) -> Entity<Self> {
        let link = Arc::new(AgentLink(Endpoint::new(&data_dir)));
        Self::with_factory(
            Box::new(|| -> Arc<dyn AppUiReader> { Arc::from(NativePlatform::app_ui_reader()) }),
            link,
            data_dir,
            admission,
            cx,
        )
    }
    pub fn with_reader(
        reader: Arc<dyn AppUiReader>,
        link: Arc<dyn ReaderLink>,
        data_dir: PathBuf,
        admission: Arc<ModeLease>,
        cx: &mut App,
    ) -> Entity<Self> {
        Self::with_factory(
            Box::new(move || Arc::clone(&reader)),
            link,
            data_dir,
            admission,
            cx,
        )
    }
    fn with_factory(
        make: ReaderFactory,
        link: Arc<dyn ReaderLink>,
        data_dir: PathBuf,
        admission: Arc<ModeLease>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            // Bounded: a stalled daemon must never grow an unbounded text queue.
            let (outgoing, mut incoming) = mpsc::channel::<ReaderBatch>(16);
            let endpoint = Endpoint::new(&data_dir);
            let send_admission = admission.clone();
            let send = cx.background_spawn(async move {
                let _admission = send_admission;
                // One connection, reopened when it breaks.
                let mut client: Option<Client> = None;
                while let Some(batch) = incoming.next().await {
                    for _ in 0..2 {
                        if client.is_none() {
                            // The agent may have just (re)started.
                            client = Client::connect_daemon(&endpoint, ANSWER_TIMEOUT)
                                .ok()
                                .flatten();
                        }
                        let Some(open) = client.as_mut() else {
                            break;
                        };
                        // No content-bearing diagnostics. A lost best-effort
                        // reader batch is not retried beyond a reconnect and
                        // never interrupts audio capture; a refused one
                        // (stale session or reader generation) is dropped.
                        match open.call(Call::ReaderEvents {
                            session_id: batch.session_id.clone(),
                            batch: batch.clone(),
                        }) {
                            Err(CallError::NotSent(_)) => {
                                client = None;
                            }
                            Ok(_) | Err(_) => break,
                        }
                    }
                }
            });
            let mut this = Self::parts(make, link, data_dir, outgoing, admission, cx);
            this._send = send;
            this._dispatch = cx.spawn(async move |_, _| NativePlatform::dispatch_ui_events().await);
            this
        })
    }
    /// A host whose batches go to `outgoing` instead of the daemon socket.
    #[cfg(test)]
    fn with_outgoing(
        make: ReaderFactory,
        link: Arc<dyn ReaderLink>,
        data_dir: PathBuf,
        outgoing: mpsc::Sender<ReaderBatch>,
        cx: &mut App,
    ) -> Entity<Self> {
        let admission = crate::test_support::admission(&data_dir);
        cx.new(|cx| Self::parts(make, link, data_dir, outgoing, admission, cx))
    }
    fn parts(
        make: ReaderFactory,
        link: Arc<dyn ReaderLink>,
        data_dir: PathBuf,
        outgoing: mpsc::Sender<ReaderBatch>,
        admission: Arc<ModeLease>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (manual, worker) = ManualQueue::new(admission.clone());
        let native = Arc::new(lazy_reader(make, Arc::clone(&manual)));
        Self {
            hosted: None,
            native,
            manual,
            clocks: HashMap::new(),
            incarnations: HashMap::new(),
            link,
            outgoing,
            data_dir,
            admission,
            poll: None,
            _manual: cx.background_spawn(worker),
            _send: Task::ready(()),
            _dispatch: Task::ready(()),
        }
    }
    /// StartReader only comes from the daemon after auto/accepted-ask recording
    /// begins. Ignored/off detections must never generate this event.
    pub fn handle_event(&mut self, event: &Event, cx: &mut Context<Self>) -> Result<()> {
        match event {
            Event::StartReader {
                session_id,
                app_id,
                pid,
                elapsed_s,
                reader_generation,
            } => {
                if !elapsed_s.is_finite() || *elapsed_s < 0.0 {
                    return Err(broscribe_core::Error::InvalidInput(
                        "invalid reader timeline anchor".into(),
                    ));
                }
                let told = *reader_generation;
                // Generations only grow for a session: the incarnation this
                // host runs, the announcement it attached from (still
                // queued behind a subscription snapshot) or an older one
                // resets nothing, running or still being prepared.
                if let Some(held) = self.incarnations.get(session_id)
                    && told <= held.issued.unwrap_or(held.told)
                {
                    return Ok(());
                }
                // The announced time is now: preparing the start (rule,
                // permission, attach, switch) does not stop the recording.
                let anchor = cx.background_executor().now();
                // A new announcement: the old incarnation stops here.
                self.stop_session(session_id);
                let cancel = CancelToken::new();
                self.incarnations.insert(
                    session_id.clone(),
                    Incarnation {
                        told,
                        issued: None,
                        cancel: cancel.clone(),
                        _claim: None,
                    },
                );
                let start = ReaderSession {
                    session_id: session_id.clone(),
                    app: app_id.clone(),
                    pid: *pid,
                    reader_generation: told,
                    policy: SessionPolicy::Auto,
                    accepted: true,
                    paused: false,
                };
                let elapsed_s = *elapsed_s;
                let preparing = cx.background_spawn(prepare(
                    Arc::clone(&self.native),
                    Arc::clone(&self.link),
                    AttachRequest {
                        session_id: session_id.clone(),
                        told,
                        cancel,
                    },
                    app_id.clone(),
                    *pid,
                ));
                cx.spawn(async move |this, cx| {
                    let prepared = preparing.await;
                    this.update(cx, |this, cx| {
                        this.attached(start, (anchor, elapsed_s), prepared, cx)
                    })
                    .ok();
                })
                .detach();
            }
            Event::StopReader { session_id } => self.stop_session(session_id),
            // Events were missed: every reader stops; the daemon's resync
            // (`start_reader`) right after attaches fresh incarnations.
            Event::Lagged { .. } => self.stop_all(),
            Event::State(state) if state.recording.is_none() => self.stop_all(),
            _ => {}
        }
        self.sync_poll(cx);
        Ok(())
    }
    /// A prepared start: runs the issued incarnation while it is still the
    /// one wanted, which then holds the start's claim on the app's switch; a
    /// refusal or a failed start leaves nothing running (no clock, no
    /// polling) and is reported for the generation known at its stage, and
    /// an unwanted one gives its claim back. Its timeline is `announced`:
    /// the recording time the announcement named, at the executor instant
    /// it arrived.
    fn attached(
        &mut self,
        mut start: ReaderSession,
        announced: (Instant, f64),
        prepared: std::result::Result<Prepared, NotPrepared>,
        cx: &mut Context<Self>,
    ) {
        let session_id = start.session_id.clone();
        let wanted = self
            .incarnations
            .get(&session_id)
            .is_some_and(|held| held.issued.is_none() && held.told == start.reader_generation);
        if !wanted {
            // Its claim goes back with it: the switch stays on while a newer
            // start or a running reader still claims it.
            return;
        }
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(not) => {
                // Before or without an issued incarnation: only the
                // announced generation is known.
                if let NotPrepared::Refused(refusal) = not {
                    tracing::warn!(?refusal, "no meeting reader for this recording");
                    self.refuse(&start, refusal, cx);
                }
                self.stop_session(&session_id);
                self.sync_poll(cx);
                return;
            }
        };
        let generation = prepared.generation;
        start.reader_generation = generation;
        let refused = start.clone();
        let hosted = self.hosted.get_or_insert_with(|| Hosted {
            host: ReaderHost::new(prepared.reader.clone()),
            reader: prepared.reader.clone(),
        });
        // This session's earlier run stopped when this incarnation was
        // announced: the host has nothing of it to release here.
        let (started, claim) = hosted
            .reader
            .start(&mut hosted.host, start, prepared.acquired);
        // Held for as long as this incarnation is: a failed start below
        // gives it back once its reader stopped.
        if let Some(held) = self.incarnations.get_mut(&session_id) {
            held._claim = claim;
        }
        match started {
            Ok(true) => {}
            Ok(false) => {
                self.stop_session(&session_id);
                self.sync_poll(cx);
                return;
            }
            Err(refusal) => {
                // After the attach: the issued generation is known.
                tracing::warn!(?refusal, "starting the meeting reader");
                self.refuse(&refused, refusal, cx);
                self.stop_session(&session_id);
                self.sync_poll(cx);
                return;
            }
        }
        if let Some(held) = self.incarnations.get_mut(&session_id) {
            held.issued = Some(generation);
        }
        let (anchor, elapsed_s) = announced;
        self.clocks.insert(
            session_id.clone(),
            Clock {
                anchor,
                elapsed_s,
                aliases_ready: false,
            },
        );
        let data_dir = self.data_dir.clone();
        let admission = self.admission.clone();
        let load = cx.background_executor().spawn(async move {
            let _admission = admission;
            load_self_aliases(&data_dir)
        });
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, _| {
                // Ignore a read belonging to an ended/restarted reader.
                if this
                    .incarnations
                    .get(&session_id)
                    .is_none_or(|held| held.issued != Some(generation))
                {
                    return;
                }
                let aliases = match result {
                    Ok(aliases) => aliases,
                    Err(error) => {
                        tracing::warn!(%error, "loading reader self aliases");
                        Vec::new()
                    }
                };
                this.set_self_aliases(&session_id, aliases);
                if let Some(clock) = this.clocks.get_mut(&session_id) {
                    clock.aliases_ready = true;
                }
            })
            .ok();
        })
        .detach();
        self.sync_poll(cx);
    }
    /// Reports off GPUI's loop that `start` (its session, app, process and
    /// generation) could not be made.
    fn refuse(&self, start: &ReaderSession, refusal: ReaderStartRefusal, cx: &mut Context<Self>) {
        let link = Arc::clone(&self.link);
        let refused = StartRefused {
            session_id: start.session_id.clone(),
            app_id: start.app.clone(),
            pid: start.pid,
            reader_generation: start.reader_generation,
            refusal,
        };
        let admission = self.admission.clone();
        cx.background_spawn(async move {
            let _admission = admission;
            link.refuse(&refused)
        })
        .detach();
    }
    /// Starts the poll loop when a session exists and drops it (cancelling
    /// its timer) when none is left.
    fn sync_poll(&mut self, cx: &mut Context<Self>) {
        if self.clocks.is_empty() {
            self.poll = None;
        } else if self.poll.is_none() {
            self.poll = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                        break;
                    }
                }
            }));
        }
    }
    pub fn set_self_aliases(&mut self, session_id: &str, aliases: Vec<String>) {
        if let Some(hosted) = &mut self.hosted {
            hosted.host.set_self_aliases(session_id, aliases);
        }
    }
    pub fn disconnected(&mut self, cx: &mut Context<Self>) {
        self.stop_all();
        self.sync_poll(cx);
    }
    /// Stops one session's reader and forgets its incarnation and clock; a
    /// start of it still being prepared gives up its attach.
    fn stop_session(&mut self, session_id: &str) {
        if let Some(Hosted { host, reader }) = &mut self.hosted {
            reader.releasing(|| host.stop(session_id));
        }
        self.clocks.remove(session_id);
        if let Some(held) = self.incarnations.remove(session_id) {
            held.cancel.cancel();
        }
    }
    fn stop_all(&mut self) {
        if let Some(Hosted { host, reader }) = &mut self.hosted {
            reader.releasing(|| {
                for id in host.session_ids() {
                    host.stop(&id);
                }
            });
        }
        self.clocks.clear();
        for (_, held) in self.incarnations.drain() {
            held.cancel.cancel();
        }
    }
    /// Starts the due snapshots on the background executor; a slow or large
    /// accessibility tree never blocks GPUI's loop.
    fn tick(&mut self, cx: &mut Context<Self>) {
        let Some(hosted) = self.hosted.as_mut() else {
            return;
        };
        let mut ended = Vec::new();
        let mut jobs = Vec::new();
        let now = cx.background_executor().now();
        for (id, clock) in &self.clocks {
            if !clock.aliases_ready {
                continue;
            }
            let elapsed = now.saturating_duration_since(clock.anchor).as_secs_f64();
            match hosted.host.begin_poll(id, clock.elapsed_s + elapsed) {
                Ok(Some(job)) => jobs.push(job),
                Ok(None) => {}
                Err(_) => ended.push(id.clone()),
            }
        }
        for id in ended {
            self.stop_session(&id);
        }
        self.sync_poll(cx);
        for job in jobs {
            let snapshot = cx.background_executor().spawn(async move {
                let tree = job.run();
                (job, tree)
            });
            cx.spawn(async move |this, cx| {
                let (job, tree) = snapshot.await;
                this.update(cx, |this, cx| this.finish_snapshot(job, tree, cx))
                    .ok();
            })
            .detach();
        }
    }
    /// Applies a snapshot on GPUI's loop (stale ones are ignored by the host).
    fn finish_snapshot(&mut self, job: SnapshotJob, tree: Result<AxTree>, cx: &mut Context<Self>) {
        let id = job.session_id().to_owned();
        let polled = match &mut self.hosted {
            // A failed snapshot stops its reader inside the host.
            Some(Hosted { host, reader }) => reader.releasing(|| host.finish_poll(job, tree)),
            None => Polled::Stale,
        };
        match polled {
            Polled::Batch(batch) => {
                let _ = self.outgoing.try_send(batch);
            }
            Polled::Stale => {}
            // The reader stopped: its last (unknown) observation still goes,
            // so the daemon stops trusting earlier evidence at once. Its
            // incarnation is forgotten: the next announcement attaches anew.
            Polled::Failed { batch, .. } => {
                let _ = self.outgoing.try_send(batch);
                self.stop_session(&id);
                self.sync_poll(cx);
            }
        }
    }
}
impl Drop for AppReaders {
    fn drop(&mut self) {
        for held in self.incarnations.values() {
            held.cancel.cancel();
        }
        // Readers still running stop here, leaving their switches to the
        // leases; quitting then gives every switch BroScribe holds back at
        // once, and later claims given back find nothing left to release.
        if let Some(Hosted { host, reader }) = self.hosted.take() {
            reader.releasing(|| drop(host));
        }
        self.manual.close();
    }
}
fn load_self_aliases(data_dir: &std::path::Path) -> Result<Vec<String>> {
    let config = Config::load(data_dir)?;
    let paths = broscribe_platform::resolve_paths(NativePlatform::paths().as_ref())?;
    let library = Library::open_existing(paths.library(None, config.library.as_deref()))?;
    Ok(library
        .list_people()?
        .into_iter()
        .filter(|person| person.is_self)
        .flat_map(|person| std::iter::once(person.name).chain(person.aliases))
        .filter(|name| !name.trim().is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon side of `reader.attach`: issues `told + 1`; `strict`
    /// refuses a `told` other than the generation it issued last as stale.
    /// Records every request and whether its start was still wanted;
    /// `lose_next` carries out the next attach but loses its answer for
    /// good; `fail_next` sends nothing. Records every `reader.refused`.
    #[derive(Default)]
    struct FakeLink {
        strict: bool,
        current: std::sync::Mutex<Option<u64>>,
        asked: std::sync::Mutex<Vec<u64>>,
        cancelled: std::sync::Mutex<Vec<bool>>,
        lose_next: AtomicBool,
        fail_next: AtomicBool,
        refused: std::sync::Mutex<Vec<StartRefused>>,
    }
    impl ReaderLink for FakeLink {
        fn attach(
            &self,
            _: &str,
            told: u64,
            cancel: &CancelToken,
        ) -> std::result::Result<u64, AttachFailure> {
            self.asked.lock().unwrap().push(told);
            self.cancelled.lock().unwrap().push(cancel.is_cancelled());
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(AttachFailure::Failed);
            }
            let mut current = self.current.lock().unwrap();
            if self.strict && current.is_some_and(|c| c != told) {
                return Err(AttachFailure::Stale);
            }
            *current = Some(told + 1);
            if self.lose_next.swap(false, Ordering::SeqCst) {
                return Err(AttachFailure::Failed);
            }
            Ok(told + 1)
        }
        fn refuse(&self, refused: &StartRefused) {
            self.refused.lock().unwrap().push(refused.clone());
        }
    }
    fn link() -> Arc<dyn ReaderLink> {
        Arc::new(FakeLink::default())
    }
    /// A data directory with a synthetic library, so self aliases never read
    /// the owner's.
    fn synthetic_data() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library")).unwrap();
        let mut config = Config::default();
        config.library = Some(library.root().to_owned());
        config.save(dir.path()).unwrap();
        dir
    }
    fn made(reader: Arc<dyn AppUiReader>) -> ReaderFactory {
        Box::new(move || reader)
    }
    fn start_reader(app: &str, generation: u64) -> Event {
        Event::StartReader {
            session_id: "synthetic-session".into(),
            app_id: app.into(),
            pid: 4242,
            elapsed_s: 1.0,
            reader_generation: generation,
        }
    }
    fn stop_reader() -> Event {
        Event::StopReader {
            session_id: "synthetic-session".into(),
        }
    }

    /// Counts what a reader start or stop may do only off GPUI's loop.
    #[derive(Default)]
    struct Counting {
        inner: broscribe_platform::fakes::FakeAppUiReader,
        trust_checks: std::sync::atomic::AtomicUsize,
        manual_reads: std::sync::atomic::AtomicUsize,
    }
    impl AppUiReader for Counting {
        fn is_trusted(&self) -> bool {
            self.trust_checks.fetch_add(1, Ordering::SeqCst);
            self.inner.is_trusted()
        }
        fn request_trust(&self) -> bool {
            self.inner.request_trust()
        }
        fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree> {
            self.inner.snapshot(pid, limits)
        }
        fn observe(
            &self,
            pid: u32,
            changes: &[AxChange],
            callback: AxChangeCallback,
        ) -> Result<Box<dyn AxObservation>> {
            self.inner.observe(pid, changes, callback)
        }
        fn manual_accessibility(&self, pid: u32) -> Result<Option<bool>> {
            self.manual_reads.fetch_add(1, Ordering::SeqCst);
            self.inner.manual_accessibility(pid)
        }
        fn set_manual_accessibility(&self, pid: u32, enabled: bool) -> Result<()> {
            self.inner.set_manual_accessibility(pid, enabled)
        }
    }

    /// UI37: making the platform reader, a start's permission check and
    /// `AXManualAccessibility` acquisition, and a stopped reader's release
    /// all run on the background executor; GPUI's loop only attaches and
    /// detaches the observer and replays what was made ahead (the host's
    /// own checks are not made again). A start abandoned while it was
    /// prepared turns the switch it turned on off again.
    #[gpui::test]
    async fn reader_setup_and_release_leave_only_the_observer_on_gpuis_loop(
        cx: &mut gpui::TestAppContext,
    ) {
        let reader = Arc::new(Counting::default());
        reader.inner.set_tree(
            4242,
            AxTree {
                root: AxNode::new("AXApplication"),
                incomplete: Default::default(),
            },
        );
        reader.inner.manual.lock().unwrap().insert(4242, false);
        let makes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let factory: ReaderFactory = {
            let (reader, makes) = (reader.clone(), makes.clone());
            Box::new(move || -> Arc<dyn AppUiReader> {
                makes.fetch_add(1, Ordering::SeqCst);
                reader
            })
        };
        let (tx, _rx) = mpsc::channel(16);
        let data = synthetic_data();
        let readers =
            cx.update(|cx| AppReaders::with_outgoing(factory, link(), data.path().into(), tx, cx));
        cx.run_until_parked();
        assert_eq!(makes.load(Ordering::SeqCst), 0, "made before any start");
        let counts = || {
            (
                reader.trust_checks.load(Ordering::SeqCst),
                reader.manual_reads.load(Ordering::SeqCst),
                reader.inner.manual_calls.lock().unwrap().clone(),
            )
        };
        readers.update(cx, |r, cx| {
            r.handle_event(&start_reader("teams", 1), cx).unwrap()
        });
        assert_eq!(counts(), (0, 0, vec![]), "blocking calls on GPUI's loop");
        cx.run_until_parked();
        assert_eq!(makes.load(Ordering::SeqCst), 1);
        // One check and one acquisition (read, turn on, read back).
        assert_eq!(counts(), (1, 2, vec![(4242, true)]));
        assert_eq!(reader.inner.observer_count(), 1);
        assert!(readers.read_with(cx, |r, _| r.clocks.contains_key("synthetic-session")));

        readers.update(cx, |r, cx| r.handle_event(&stop_reader(), cx).unwrap());
        assert_eq!(reader.inner.observer_count(), 0);
        assert_eq!(
            counts(),
            (1, 2, vec![(4242, true)]),
            "released on GPUI's loop"
        );
        cx.run_until_parked();
        assert_eq!(counts(), (1, 3, vec![(4242, true), (4242, false)]));

        readers.update(cx, |r, cx| {
            r.handle_event(&start_reader("teams", 5), cx).unwrap();
            r.handle_event(&stop_reader(), cx).unwrap();
        });
        cx.run_until_parked();
        assert_eq!(makes.load(Ordering::SeqCst), 1);
        assert_eq!(
            counts().2,
            [(4242, true), (4242, false), (4242, true), (4242, false)]
        );
        assert_eq!(reader.inner.manual.lock().unwrap().get(&4242), Some(&false));
        assert_eq!(reader.inner.observer_count(), 0);
        assert!(readers.read_with(cx, |r, _| r.incarnations.is_empty() && r.poll.is_none()));
    }

    /// Notes the app's switch whenever its tree is read.
    #[derive(Default)]
    struct SwitchWatch {
        inner: broscribe_platform::fakes::FakeAppUiReader,
        seen: std::sync::Mutex<Vec<Option<bool>>>,
    }
    impl SwitchWatch {
        /// An app at 4242 whose switch reads `on`.
        fn app(on: bool) -> Arc<Self> {
            let reader = Arc::new(Self::default());
            reader.inner.set_tree(
                4242,
                AxTree {
                    root: AxNode::new("AXApplication"),
                    incomplete: Default::default(),
                },
            );
            reader.inner.manual.lock().unwrap().insert(4242, on);
            reader
        }
        fn calls(&self) -> Vec<(u32, bool)> {
            self.inner.manual_calls.lock().unwrap().clone()
        }
        fn switch(&self) -> Option<bool> {
            self.inner.manual.lock().unwrap().get(&4242).copied()
        }
    }
    impl AppUiReader for SwitchWatch {
        fn is_trusted(&self) -> bool {
            self.inner.is_trusted()
        }
        fn request_trust(&self) -> bool {
            self.inner.request_trust()
        }
        fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree> {
            let switch = self.inner.manual.lock().unwrap().get(&pid).copied();
            self.seen.lock().unwrap().push(switch);
            self.inner.snapshot(pid, limits)
        }
        fn observe(
            &self,
            pid: u32,
            changes: &[AxChange],
            callback: AxChangeCallback,
        ) -> Result<Box<dyn AxObservation>> {
            self.inner.observe(pid, changes, callback)
        }
        fn manual_accessibility(&self, pid: u32) -> Result<Option<bool>> {
            self.inner.manual_accessibility(pid)
        }
        fn set_manual_accessibility(&self, pid: u32, enabled: bool) -> Result<()> {
            self.inner.set_manual_accessibility(pid, enabled)
        }
    }

    /// Runs only the background executor until it has nothing left: starts
    /// are prepared, and their results wait for GPUI's loop.
    fn prepare_only(cx: &mut gpui::TestAppContext) {
        while cx.executor().dispatcher().as_test().unwrap().tick(true) {}
    }

    fn announce(session: &str, generation: u64) -> Event {
        Event::StartReader {
            session_id: session.into(),
            app_id: "teams".into(),
            pid: 4242,
            elapsed_s: 1.0,
            reader_generation: generation,
        }
    }
    fn end(session: &str) -> Event {
        Event::StopReader {
            session_id: session.into(),
        }
    }

    /// NA12 (R2 F1): a start's preparation turned the app's switch on and
    /// the connection to the daemon was lost before its result reached
    /// GPUI's loop. The resubscription announces the session's current
    /// generation (the one that start's attach issued) for the same
    /// process, and the replacement start is prepared while the switch is
    /// on. Whichever result is applied first, the replacement claims what
    /// the abandoned start turned on: the switch is never touched while the
    /// replacement's incarnation (the only one whose batches go out) reads,
    /// its health reports the switch as BroScribe's, and it goes back off
    /// exactly once, off GPUI's loop, when the replacement stops. A switch
    /// the app already had on is never turned off. GPUI's test scheduler
    /// applies results in announcement order, so the replacement-first
    /// order holds the abandoned start's own preparation and applies it
    /// after the replacement started.
    #[gpui::test]
    async fn an_abandoned_start_hands_the_switch_it_turned_on_to_its_replacement(
        cx: &mut gpui::TestAppContext,
    ) {
        use broscribe_readers::ManualAccessibility;
        for (replacement_first, already_on) in [(false, false), (true, false), (false, true)] {
            let reader = SwitchWatch::app(already_on);
            let fake = Arc::new(FakeLink {
                strict: true,
                ..FakeLink::default()
            });
            let (tx, mut rx) = mpsc::channel(64);
            let dir = synthetic_data();
            let readers = cx.update(|cx| {
                AppReaders::with_outgoing(
                    made(reader.clone()),
                    fake.clone(),
                    dir.path().into(),
                    tx,
                    cx,
                )
            });
            cx.run_until_parked();
            let turned_on: &[(u32, bool)] = if already_on { &[] } else { &[(4242, true)] };
            let issued = |cx: &mut gpui::TestAppContext| {
                readers.read_with(cx, |r, _| {
                    r.incarnations
                        .get("synthetic-session")
                        .and_then(|held| held.issued)
                })
            };

            // Generation 1: its preparation attaches (the daemon issues 2)
            // and turns the switch on; its result waits for GPUI's loop.
            let held = if replacement_first {
                let (native, link) =
                    readers.read_with(cx, |r, _| (Arc::clone(&r.native), Arc::clone(&r.link)));
                Some(cx.executor().spawn(prepare(
                    native,
                    link,
                    AttachRequest {
                        session_id: "synthetic-session".into(),
                        told: 1,
                        cancel: CancelToken::new(),
                    },
                    "teams".into(),
                    4242,
                )))
            } else {
                readers.update(cx, |r, cx| {
                    r.handle_event(&start_reader("teams", 1), cx).unwrap()
                });
                None
            };
            prepare_only(cx);
            assert_eq!(reader.calls(), turned_on);
            assert_eq!(reader.switch(), Some(true));
            assert_eq!(issued(cx), None, "nothing applied yet");

            // The connection is lost; the resubscription announces 2.
            readers.update(cx, |r, cx| {
                r.disconnected(cx);
                r.handle_event(&start_reader("teams", 2), cx).unwrap();
            });
            prepare_only(cx);
            assert_eq!(*fake.asked.lock().unwrap(), [1, 2]);
            assert_eq!(reader.calls(), turned_on, "the replacement set nothing");
            if let Some(held) = held {
                cx.run_until_parked();
                assert_eq!(issued(cx), Some(3), "the replacement runs first");
                let abandoned = held.await;
                readers.update(cx, |r, cx| {
                    let at = cx.background_executor().now();
                    let start = ReaderSession {
                        session_id: "synthetic-session".into(),
                        app: "teams".into(),
                        pid: 4242,
                        reader_generation: 1,
                        policy: SessionPolicy::Auto,
                        accepted: true,
                        paused: false,
                    };
                    r.attached(start, (at, 1.0), abandoned, cx);
                });
            }
            cx.run_until_parked();
            assert_eq!(issued(cx), Some(3));
            assert_eq!(reader.inner.observer_count(), 1);
            assert_eq!(
                reader.calls(),
                turned_on,
                "the abandoned start gave nothing back"
            );
            assert_eq!(reader.switch(), Some(true));

            let (mut now, mut batches) = (0, Vec::new());
            run_for(cx, &mut rx, &mut now, 3, &mut batches);
            let owner = if already_on {
                ManualAccessibility::AlreadyOn
            } else {
                ManualAccessibility::EnabledByBroscribe
            };
            assert!(!batches.is_empty());
            for (_, batch) in &batches {
                assert_eq!(batch.reader_generation, 3);
                assert_eq!(batch.health.manual_accessibility, owner);
            }
            let seen = reader.seen.lock().unwrap().clone();
            assert!(
                !seen.is_empty() && seen.iter().all(|on| *on == Some(true)),
                "{seen:?}"
            );
            assert_eq!(
                reader.calls(),
                turned_on,
                "untouched while the replacement reads"
            );

            // The announcements this host attached from change nothing.
            readers.update(cx, |r, cx| {
                r.handle_event(&start_reader("teams", 1), cx).unwrap();
                r.handle_event(&start_reader("teams", 2), cx).unwrap();
            });
            cx.run_until_parked();
            assert_eq!(*fake.asked.lock().unwrap(), [1, 2]);
            assert_eq!(issued(cx), Some(3));

            readers.update(cx, |r, cx| r.handle_event(&stop_reader(), cx).unwrap());
            assert_eq!(reader.calls(), turned_on, "given back off GPUI's loop");
            cx.run_until_parked();
            let restored: &[(u32, bool)] = if already_on {
                &[]
            } else {
                &[(4242, true), (4242, false)]
            };
            assert_eq!(reader.calls(), restored);
            assert_eq!(reader.switch(), Some(already_on));
            assert_eq!(reader.inner.observer_count(), 0);
            assert!(readers.read_with(cx, |r, _| r.incarnations.is_empty() && r.poll.is_none()));
        }
    }

    /// NA12: two recordings read one app process. The first is stopped
    /// before its result is applied, after the second was prepared on the
    /// switch it turned on: the second keeps it on, with nothing set again.
    /// A new start of the first joins the second's lease; stopping the
    /// second leaves it on for the first, and it goes back off once, off
    /// GPUI's loop, when the first's reader stops on a failed snapshot.
    #[gpui::test]
    async fn the_switch_goes_back_only_with_the_last_claim_on_its_process(
        cx: &mut gpui::TestAppContext,
    ) {
        use broscribe_readers::ManualAccessibility;
        let reader = SwitchWatch::app(false);
        let (tx, mut rx) = mpsc::channel(64);
        let dir = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_outgoing(made(reader.clone()), link(), dir.path().into(), tx, cx)
        });
        let running = |cx: &mut gpui::TestAppContext| {
            readers.read_with(cx, |r, _| {
                let mut ids: Vec<String> = r
                    .incarnations
                    .iter()
                    .filter(|(_, held)| held.issued.is_some())
                    .map(|(id, _)| id.clone())
                    .collect();
                ids.sort();
                ids
            })
        };
        let (mut now, mut batches) = (0, Vec::new());
        let mut read_by = |cx: &mut gpui::TestAppContext, session: &str| {
            reader.seen.lock().unwrap().clear();
            batches.clear();
            run_for(cx, &mut rx, &mut now, 3, &mut batches);
            assert!(!batches.is_empty());
            for (_, batch) in batches.iter() {
                assert_eq!(batch.session_id, session);
                assert_eq!(
                    batch.health.manual_accessibility,
                    ManualAccessibility::EnabledByBroscribe
                );
            }
            let seen = reader.seen.lock().unwrap().clone();
            assert!(
                !seen.is_empty() && seen.iter().all(|on| *on == Some(true)),
                "{seen:?}"
            );
        };

        readers.update(cx, |r, cx| {
            r.handle_event(&announce("synthetic-first", 1), cx).unwrap();
            r.handle_event(&announce("synthetic-second", 1), cx)
                .unwrap();
        });
        prepare_only(cx);
        assert_eq!(reader.calls(), [(4242, true)]);
        readers.update(cx, |r, cx| {
            r.handle_event(&end("synthetic-first"), cx).unwrap()
        });
        cx.run_until_parked();
        assert_eq!(running(cx), ["synthetic-second"]);
        assert_eq!(reader.calls(), [(4242, true)]);
        read_by(cx, "synthetic-second");

        readers.update(cx, |r, cx| {
            r.handle_event(&announce("synthetic-first", 3), cx).unwrap()
        });
        cx.run_until_parked();
        readers.update(cx, |r, cx| {
            r.handle_event(&end("synthetic-second"), cx).unwrap()
        });
        cx.run_until_parked();
        assert_eq!(running(cx), ["synthetic-first"]);
        assert_eq!(reader.calls(), [(4242, true)]);
        read_by(cx, "synthetic-first");

        // The app's windows go away: the reader stops on a failed snapshot.
        reader.inner.trees.lock().unwrap().clear();
        let mut last = Vec::new();
        run_for(cx, &mut rx, &mut now, 3, &mut last);
        assert_eq!(last.len(), 1, "one final batch");
        assert_eq!(reader.calls(), [(4242, true), (4242, false)]);
        assert_eq!(reader.switch(), Some(false));
        assert!(running(cx).is_empty());
        assert!(readers.read_with(cx, |r, _| r.poll.is_none()));
    }

    /// Quitting gives the switch back at once and once, while a reader runs
    /// and another start still holds a claim on it; that start's result,
    /// applied after the quit, gives nothing back again.
    #[gpui::test]
    async fn quitting_gives_a_claimed_switch_back_once(cx: &mut gpui::TestAppContext) {
        let reader = SwitchWatch::app(false);
        let (tx, _rx) = mpsc::channel(64);
        let dir = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_outgoing(made(reader.clone()), link(), dir.path().into(), tx, cx)
        });
        readers.update(cx, |r, cx| {
            r.handle_event(&announce("synthetic-first", 1), cx).unwrap()
        });
        cx.run_until_parked();
        readers.update(cx, |r, cx| {
            r.handle_event(&announce("synthetic-second", 1), cx)
                .unwrap()
        });
        prepare_only(cx);
        assert_eq!(reader.calls(), [(4242, true)]);
        assert_eq!(reader.inner.observer_count(), 1);

        cx.update(|_| drop(readers));
        assert_eq!(reader.calls(), [(4242, true), (4242, false)]);
        assert_eq!(reader.inner.observer_count(), 0);
        cx.run_until_parked();
        assert_eq!(reader.calls(), [(4242, true), (4242, false)]);
        assert_eq!(reader.switch(), Some(false));
    }

    /// USR01/RT01 at startup: two announcements before either attach
    /// answered (a resync, then a newer incarnation). The late answer for the
    /// older one starts nothing; only the newest incarnation reads, and every
    /// batch carries it.
    #[gpui::test]
    async fn only_the_newest_announcement_starts_and_its_batches_carry_it(
        cx: &mut gpui::TestAppContext,
    ) {
        let reader = Arc::new(broscribe_platform::fakes::FakeAppUiReader::default());
        reader.set_tree(4242, in_call());
        let fake = Arc::new(FakeLink::default());
        let (tx, mut rx) = mpsc::channel(64);
        let dir = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_outgoing(
                made(reader.clone()),
                fake.clone(),
                dir.path().into(),
                tx,
                cx,
            )
        });
        readers.update(cx, |r, cx| {
            r.handle_event(&start_reader("meet-chrome", 1), cx).unwrap();
            r.handle_event(&start_reader("meet-chrome", 5), cx).unwrap();
        });
        cx.run_until_parked();
        let mut asked = fake.asked.lock().unwrap().clone();
        asked.sort_unstable();
        assert_eq!(asked, [1, 5]);
        assert_eq!(
            readers.read_with(cx, |r, _| r
                .incarnations
                .get("synthetic-session")
                .and_then(|i| i.issued)),
            Some(6)
        );
        assert_eq!(reader.observer_count(), 1);
        let (mut now, mut batches) = (0, Vec::new());
        run_for(cx, &mut rx, &mut now, 4, &mut batches);
        assert!(!batches.is_empty());
        assert!(batches.iter().all(|(_, b)| b.reader_generation == 6));
    }

    /// RT01: a reader starts only with a daemon-issued incarnation; the
    /// announcement of the incarnation it runs (a plain subscription
    /// snapshot or resync) and the announcement it attached from (queued
    /// behind the snapshot) reset nothing; a lag stops it and the resync
    /// attaches a fresh one; a refused attach (NA11) leaves no clock and no
    /// polling behind.
    #[gpui::test]
    async fn readers_take_daemon_issued_incarnations_and_never_reset_on_a_plain_subscribe(
        cx: &mut gpui::TestAppContext,
    ) {
        let reader = Arc::new(broscribe_platform::fakes::FakeAppUiReader::default());
        reader.set_tree(
            4242,
            AxTree {
                root: AxNode::new("AXApplication"),
                incomplete: Default::default(),
            },
        );
        let fake = Arc::new(FakeLink {
            strict: true,
            ..FakeLink::default()
        });
        let data = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_reader(
                reader.clone(),
                fake.clone(),
                data.path().into(),
                crate::test_support::admission(data.path()),
                cx,
            )
        });
        let start = |generation| Event::StartReader {
            session_id: "synthetic-session".into(),
            app_id: "teams".into(),
            pid: 4242,
            elapsed_s: 1.0,
            reader_generation: generation,
        };
        let handle = |event: &Event, cx: &mut gpui::TestAppContext| {
            readers.update(cx, |r, cx| r.handle_event(event, cx).unwrap());
            cx.run_until_parked();
        };
        let running = |cx: &mut gpui::TestAppContext| {
            readers.read_with(cx, |r, _| {
                (
                    r.incarnations
                        .get("synthetic-session")
                        .and_then(|i| i.issued),
                    r.poll.is_some(),
                    r.clocks.contains_key("synthetic-session"),
                )
            })
        };
        handle(&start(1), cx);
        assert_eq!(running(cx), (Some(2), true, true));
        // The daemon now reports the issued incarnation: no reset, no attach.
        handle(&start(2), cx);
        assert_eq!(running(cx), (Some(2), true, true));
        assert_eq!(*fake.asked.lock().unwrap(), [1]);
        // The original announcement, delivered after the snapshot that
        // already named the issued incarnation: still nothing to reset, and
        // the strict daemon is never asked to attach from it again.
        handle(&start(1), cx);
        assert_eq!(running(cx), (Some(2), true, true));
        assert_eq!(*fake.asked.lock().unwrap(), [1]);
        assert_eq!(reader.observer_count(), 1);
        // Missed events: stopped, then the resync attaches anew.
        handle(&Event::Lagged { dropped: 4 }, cx);
        assert_eq!(running(cx), (None, false, false));
        handle(&start(2), cx);
        assert_eq!(running(cx), (Some(3), true, true));
        // An announcement the daemon no longer issues from: refused, and
        // nothing keeps a clock or polls.
        handle(&start(7), cx);
        assert_eq!(running(cx), (None, false, false));
        assert_eq!(*fake.asked.lock().unwrap(), [1, 2, 7]);
        assert!(
            fake.refused.lock().unwrap().is_empty(),
            "a stale attach is no refusal"
        );
    }

    /// NA33/F3: the daemon carried out an attach whose answer stayed lost
    /// (asked for by its request ID without an answer). The start fails and
    /// leaves no incarnation, clock, observer or polling behind, and the
    /// attach is not run again; the daemon's next announcement of the
    /// incarnation it issued starts the reader from there, and every batch
    /// carries the one issued then. A start stopped while it is prepared
    /// gives up its attach.
    #[gpui::test]
    async fn a_lost_attach_answer_leaves_nothing_stuck(cx: &mut gpui::TestAppContext) {
        let reader = Arc::new(broscribe_platform::fakes::FakeAppUiReader::default());
        reader.set_tree(4242, in_call());
        let fake = Arc::new(FakeLink {
            strict: true,
            ..FakeLink::default()
        });
        fake.lose_next.store(true, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::channel(64);
        let dir = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_outgoing(
                made(reader.clone()),
                fake.clone(),
                dir.path().into(),
                tx,
                cx,
            )
        });
        let handle = |event: &Event, cx: &mut gpui::TestAppContext| {
            readers.update(cx, |r, cx| r.handle_event(event, cx).unwrap());
            cx.run_until_parked();
        };
        let idle = |cx: &mut gpui::TestAppContext| {
            readers.read_with(cx, |r, _| {
                r.incarnations.is_empty() && r.clocks.is_empty() && r.poll.is_none()
            })
        };
        handle(&start_reader("meet-chrome", 1), cx);
        assert_eq!(
            *fake.current.lock().unwrap(),
            Some(2),
            "the daemon issued 2"
        );
        assert!(idle(cx));
        assert_eq!(reader.observer_count(), 0);
        assert_eq!(*fake.asked.lock().unwrap(), [1]);
        // Reported for the generation it knows (1), never the one the
        // daemon may have issued (2): the daemon ignores it as stale.
        assert_eq!(
            *fake.refused.lock().unwrap(),
            [StartRefused {
                session_id: "synthetic-session".into(),
                app_id: "meet-chrome".into(),
                pid: 4242,
                reader_generation: 1,
                refusal: ReaderStartRefusal::Attach,
            }]
        );

        // Nothing was heard of 2: the daemon announces it again.
        handle(&start_reader("meet-chrome", 2), cx);
        assert_eq!(*fake.asked.lock().unwrap(), [1, 2]);
        assert_eq!(
            readers.read_with(cx, |r, _| r
                .incarnations
                .get("synthetic-session")
                .and_then(|i| i.issued)),
            Some(3)
        );
        let (mut now, mut batches) = (0, Vec::new());
        run_for(cx, &mut rx, &mut now, 3, &mut batches);
        assert!(!batches.is_empty());
        assert!(batches.iter().all(|(_, b)| b.reader_generation == 3));

        // Paused and resumed: the start of 4 stops before its attach ran.
        readers.update(cx, |r, cx| {
            r.handle_event(&stop_reader(), cx).unwrap();
            r.handle_event(&start_reader("meet-chrome", 4), cx).unwrap();
            r.handle_event(&stop_reader(), cx).unwrap();
        });
        cx.run_until_parked();
        assert_eq!(*fake.cancelled.lock().unwrap(), [false, false, true]);
        assert!(idle(cx));
    }

    /// RT01/USR01 (REV02 runtime F1, native F2): a non-transient snapshot
    /// failure sends one last incomplete batch reporting `Failed` and
    /// stops the reader, giving back its claim on the app's switch. A
    /// re-announcement of the incarnation it ran, delivered while it still
    /// ran, changed nothing; delivered after the failure, it restarts the
    /// reader through `reader.attach` as a new incarnation whose sequences
    /// start at 1 and whose start claims the switch again.
    #[gpui::test]
    async fn a_failed_snapshot_restarts_its_reader_from_the_daemons_next_announcement(
        cx: &mut gpui::TestAppContext,
    ) {
        use broscribe_readers::{CallState, IncompleteReason};
        let reader = Arc::new(broscribe_platform::fakes::FakeAppUiReader::default());
        reader.set_tree(4242, in_call());
        reader.manual.lock().unwrap().insert(4242, false);
        let fake = Arc::new(FakeLink {
            strict: true,
            ..FakeLink::default()
        });
        let (tx, mut rx) = mpsc::channel(64);
        let dir = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_outgoing(
                made(reader.clone()),
                fake.clone(),
                dir.path().into(),
                tx,
                cx,
            )
        });
        let handle = |event: &Event, cx: &mut gpui::TestAppContext| {
            readers.update(cx, |r, cx| r.handle_event(event, cx).unwrap());
            cx.run_until_parked();
        };
        let issued = |cx: &mut gpui::TestAppContext| {
            readers.read_with(cx, |r, _| {
                r.incarnations
                    .get("synthetic-session")
                    .and_then(|i| i.issued)
            })
        };
        let (mut now, mut batches) = (0, Vec::new());
        handle(&start_reader("meet-chrome", 1), cx);
        assert_eq!(issued(cx), Some(2));
        run_for(cx, &mut rx, &mut now, 3, &mut batches);
        assert!(!batches.is_empty());
        assert!(batches.iter().all(|(_, b)| b.reader_generation == 2));
        // Announced again while it runs: ignored, nothing attached.
        handle(&start_reader("meet-chrome", 2), cx);
        assert_eq!(*fake.asked.lock().unwrap(), [1]);
        assert_eq!(issued(cx), Some(2));

        // The app's tree can no longer be read: not transient.
        reader.trees.lock().unwrap().clear();
        batches.clear();
        run_for(cx, &mut rx, &mut now, 3, &mut batches);
        assert_eq!(batches.len(), 1, "one final batch");
        let last = &batches[0].1;
        assert_eq!(last.reader_generation, 2);
        assert_eq!(last.call_observation.state, CallState::Unknown);
        assert!(!last.call_observation.complete);
        assert_eq!(last.health.incomplete, Some(IncompleteReason::Failed));
        assert!(readers.read_with(cx, |r, _| {
            r.incarnations.is_empty() && r.clocks.is_empty() && r.poll.is_none()
        }));
        assert_eq!(reader.observer_count(), 0);
        assert_eq!(
            *reader.manual_calls.lock().unwrap(),
            [(4242, true), (4242, false)]
        );

        // The daemon announces the incarnation it still holds current.
        reader.set_tree(4242, in_call());
        handle(&start_reader("meet-chrome", 2), cx);
        assert_eq!(*fake.asked.lock().unwrap(), [1, 2]);
        assert_eq!(issued(cx), Some(3));
        assert_eq!(reader.observer_count(), 1);
        assert_eq!(
            *reader.manual_calls.lock().unwrap(),
            [(4242, true), (4242, false), (4242, true)]
        );
        // A repeat of that announcement, still queued, resets nothing.
        handle(&start_reader("meet-chrome", 2), cx);
        assert_eq!(*fake.asked.lock().unwrap(), [1, 2]);
        batches.clear();
        run_for(cx, &mut rx, &mut now, 3, &mut batches);
        assert!(!batches.is_empty());
        assert!(batches.iter().all(|(_, b)| b.reader_generation == 3));
        assert_eq!(batches[0].1.call_observation.sequence, 1);
        assert_eq!(batches[0].1.call_observation.state, CallState::Active);
        assert!(batches.iter().all(|(_, b)| b.health.incomplete.is_none()));
    }

    /// A platform reader whose accessibility trust the test grants later.
    #[derive(Default)]
    struct Revocable {
        inner: broscribe_platform::fakes::FakeAppUiReader,
        untrusted: AtomicBool,
    }
    impl AppUiReader for Revocable {
        fn is_trusted(&self) -> bool {
            !self.untrusted.load(Ordering::SeqCst) && self.inner.is_trusted()
        }
        fn request_trust(&self) -> bool {
            self.inner.request_trust()
        }
        fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree> {
            self.inner.snapshot(pid, limits)
        }
        fn observe(
            &self,
            pid: u32,
            changes: &[AxChange],
            callback: AxChangeCallback,
        ) -> Result<Box<dyn AxObservation>> {
            self.inner.observe(pid, changes, callback)
        }
        fn manual_accessibility(&self, pid: u32) -> Result<Option<bool>> {
            self.inner.manual_accessibility(pid)
        }
        fn set_manual_accessibility(&self, pid: u32, enabled: bool) -> Result<()> {
            self.inner.set_manual_accessibility(pid, enabled)
        }
    }

    /// NA35: a start this host cannot make is reported as its typed stage
    /// (observed untrusted access, no rule, an attach that issued nothing
    /// known) for the announced generation, the only one it knows; nothing
    /// is attached for a refused permission or rule, and no batch, clock,
    /// observer or polling remains. The daemon's next announcement starts
    /// the reader once the cause is gone, with no further report.
    #[gpui::test]
    async fn refused_starts_report_their_typed_stage_and_nothing_else(
        cx: &mut gpui::TestAppContext,
    ) {
        let reader = Arc::new(Revocable::default());
        reader.inner.set_tree(4242, in_call());
        reader.untrusted.store(true, Ordering::SeqCst);
        let fake = Arc::new(FakeLink::default());
        let (tx, mut rx) = mpsc::channel(64);
        let dir = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_outgoing(
                made(reader.clone()),
                fake.clone(),
                dir.path().into(),
                tx,
                cx,
            )
        });
        let handle = |event: &Event, cx: &mut gpui::TestAppContext| {
            readers.update(cx, |r, cx| r.handle_event(event, cx).unwrap());
            cx.run_until_parked();
        };
        let idle = |cx: &mut gpui::TestAppContext| {
            readers.read_with(cx, |r, _| {
                r.incarnations.is_empty() && r.clocks.is_empty() && r.poll.is_none()
            })
        };
        let refused = |app: &str, generation, refusal| StartRefused {
            session_id: "synthetic-session".into(),
            app_id: app.into(),
            pid: 4242,
            reader_generation: generation,
            refusal,
        };
        let (mut now, mut batches) = (0, Vec::new());

        handle(&start_reader("meet-chrome", 1), cx);
        assert!(idle(cx));
        handle(&start_reader("synthetic-app-without-rule", 2), cx);
        assert!(idle(cx));
        assert!(fake.asked.lock().unwrap().is_empty(), "nothing attached");
        reader.untrusted.store(false, Ordering::SeqCst);
        fake.fail_next.store(true, Ordering::SeqCst);
        handle(&start_reader("meet-chrome", 3), cx);
        assert!(idle(cx));
        assert_eq!(*fake.asked.lock().unwrap(), [3]);
        run_for(cx, &mut rx, &mut now, 3, &mut batches);
        assert!(batches.is_empty(), "no batch or heartbeat for a refusal");
        assert_eq!(reader.inner.observer_count(), 0);
        assert_eq!(
            *fake.refused.lock().unwrap(),
            [
                refused("meet-chrome", 1, ReaderStartRefusal::Untrusted),
                refused("synthetic-app-without-rule", 2, ReaderStartRefusal::NoRule),
                refused("meet-chrome", 3, ReaderStartRefusal::Attach),
            ]
        );

        // Announced again: the reader starts on the issued incarnation.
        handle(&start_reader("meet-chrome", 3), cx);
        assert_eq!(*fake.asked.lock().unwrap(), [3, 3]);
        run_for(cx, &mut rx, &mut now, 3, &mut batches);
        assert!(!batches.is_empty());
        assert!(batches.iter().all(|(_, b)| b.reader_generation == 4));
        assert_eq!(fake.refused.lock().unwrap().len(), 3);
    }
    use broscribe_core::{PersonId, schema::Person};
    use broscribe_platform::{AxNode, AxTree};
    use broscribe_readers::{ReaderEvent, RuleEngine};

    #[test]
    fn each_alias_load_uses_current_me_name_and_aliases_for_caption_identity() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("library")).unwrap();
        let mut config = Config::default();
        config.library = Some(library.root().to_owned());
        config.save(dir.path()).unwrap();
        let mut me = Person::new(PersonId::parse("p-111111").unwrap(), "Synthetic Owner");
        me.is_self = true;
        me.aliases = vec!["Synthetic Alias".into()];
        let other = Person::new(PersonId::parse("p-222222").unwrap(), "Other Attendee");
        library
            .write(|tx| {
                tx.save_person(&me)?;
                tx.save_person(&other)
            })
            .unwrap();
        assert_eq!(
            load_self_aliases(dir.path()).unwrap(),
            vec!["Synthetic Owner", "Synthetic Alias"]
        );
        me.name = "Updated Owner".into();
        me.aliases = vec!["Updated Alias".into()];
        library.write(|tx| tx.save_person(&me)).unwrap();
        let aliases = load_self_aliases(dir.path()).unwrap();
        assert_eq!(aliases, vec!["Updated Owner", "Updated Alias"]);

        let tree = teams_caption("Updated Alias");
        let rule = broscribe_readers::builtin_rules()
            .unwrap()
            .into_iter()
            .find(|rule| rule.id == "teams")
            .unwrap();
        let mut engine = RuleEngine::new(rule);
        engine.set_self_aliases(aliases);
        assert!(
            engine
                .read(&tree, 1.0)
                .iter()
                .any(|event| { matches!(event, ReaderEvent::Caption(caption) if caption.is_self) })
        );
    }

    /// A Teams window showing one caption by `speaker`.
    fn teams_caption(speaker: &str) -> AxTree {
        let node = |role: &str, id: &str, value: &str, children: Vec<AxNode>| AxNode {
            identifier: Some(id.into()),
            value: Some(value.into()),
            children,
            ..AxNode::new(role)
        };
        AxTree {
            incomplete: Default::default(),
            root: node(
                "AXApplication",
                "",
                "",
                vec![node(
                    "AXGroup",
                    "captions",
                    "",
                    vec![node(
                        "AXGroup",
                        "caption-1",
                        "",
                        vec![
                            node("AXStaticText", "speaker", speaker, vec![]),
                            node("AXStaticText", "text", "synthetic meeting words", vec![]),
                        ],
                    )],
                )],
            ),
        }
    }

    /// A daemon whose `reader.attach` runs `during` before answering.
    struct SlowLink {
        inner: FakeLink,
        during: Box<dyn Fn() + Send + Sync>,
    }
    impl ReaderLink for SlowLink {
        fn attach(
            &self,
            session_id: &str,
            told: u64,
            cancel: &CancelToken,
        ) -> std::result::Result<u64, AttachFailure> {
            (self.during)();
            self.inner.attach(session_id, told, cancel)
        }
        fn refuse(&self, refused: &StartRefused) {
            self.inner.refuse(refused);
        }
    }

    /// A start whose preparation takes five seconds (its attach answers
    /// five seconds of the executor's clock after the announcement) keeps
    /// the announced timeline: an announcement at recording second 10 and
    /// the first poll a second after the start observe second 16, never 11.
    #[gpui::test]
    async fn a_slow_start_keeps_the_time_its_announcement_named(cx: &mut gpui::TestAppContext) {
        let reader = Arc::new(broscribe_platform::fakes::FakeAppUiReader::default());
        reader.set_tree(4242, teams_caption("Synthetic Speaker"));
        let clock = cx
            .executor()
            .dispatcher()
            .as_test()
            .unwrap()
            .scheduler()
            .clock();
        let link = Arc::new(SlowLink {
            inner: FakeLink::default(),
            during: Box::new(move || clock.advance(Duration::from_secs(5))),
        });
        let (tx, mut rx) = mpsc::channel(16);
        let dir = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_outgoing(made(reader.clone()), link, dir.path().into(), tx, cx)
        });
        let announced = cx.executor().now();
        readers.update(cx, |r, cx| {
            r.handle_event(
                &Event::StartReader {
                    session_id: "synthetic-session".into(),
                    app_id: "teams".into(),
                    pid: 4242,
                    elapsed_s: 10.0,
                    reader_generation: 1,
                },
                cx,
            )
            .unwrap()
        });
        cx.run_until_parked();
        assert_eq!(
            cx.executor().now().saturating_duration_since(announced),
            Duration::from_secs(5),
            "the start was prepared five seconds after its announcement"
        );
        assert!(readers.read_with(cx, |r, _| r.clocks.contains_key("synthetic-session")));
        let (mut now, mut batches) = (0, Vec::new());
        run_for(cx, &mut rx, &mut now, 1, &mut batches);
        let observed: Vec<f64> = batches
            .iter()
            .flat_map(|(_, b)| &b.events)
            .filter_map(|event| match event {
                ReaderEvent::Caption(caption) => Some(caption.observed_s),
                _ => None,
            })
            .collect();
        assert_eq!(observed, [16.0]);
    }

    /// Counts snapshots and takes its time, like a large AT-SPI tree.
    struct SlowReader {
        inner: broscribe_platform::fakes::FakeAppUiReader,
        snapshots: std::sync::atomic::AtomicUsize,
    }
    impl AppUiReader for SlowReader {
        fn is_trusted(&self) -> bool {
            self.inner.is_trusted()
        }
        fn request_trust(&self) -> bool {
            self.inner.request_trust()
        }
        fn snapshot(
            &self,
            pid: u32,
            limits: broscribe_platform::AxLimits,
        ) -> broscribe_platform::Result<AxTree> {
            std::thread::sleep(Duration::from_millis(200));
            self.snapshots
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.snapshot(pid, limits)
        }
        fn observe(
            &self,
            pid: u32,
            changes: &[broscribe_platform::AxChange],
            callback: broscribe_platform::AxChangeCallback,
        ) -> broscribe_platform::Result<Box<dyn broscribe_platform::AxObservation>> {
            self.inner.observe(pid, changes, callback)
        }
        fn manual_accessibility(&self, pid: u32) -> broscribe_platform::Result<Option<bool>> {
            self.inner.manual_accessibility(pid)
        }
        fn set_manual_accessibility(
            &self,
            pid: u32,
            enabled: bool,
        ) -> broscribe_platform::Result<()> {
            self.inner.set_manual_accessibility(pid, enabled)
        }
    }

    #[gpui::test]
    async fn tick_never_snapshots_on_the_foreground(cx: &mut gpui::TestAppContext) {
        let inner = broscribe_platform::fakes::FakeAppUiReader::default();
        inner.set_tree(
            4242,
            AxTree {
                root: AxNode::new("AXApplication"),
                incomplete: Default::default(),
            },
        );
        let reader = Arc::new(SlowReader {
            inner,
            snapshots: Default::default(),
        });
        let data = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_reader(
                reader.clone(),
                link(),
                data.path().into(),
                crate::test_support::admission(data.path()),
                cx,
            )
        });
        readers.update(cx, |readers, cx| {
            readers
                .handle_event(
                    &Event::StartReader {
                        session_id: "synthetic-session".into(),
                        app_id: "teams".into(),
                        pid: 4242,
                        elapsed_s: 1.0,
                        reader_generation: 1,
                    },
                    cx,
                )
                .unwrap();
        });
        cx.run_until_parked();
        let count = || reader.snapshots.load(std::sync::atomic::Ordering::SeqCst);
        let started = Instant::now();
        readers.update(cx, |readers, cx| readers.tick(cx));
        assert!(started.elapsed() < Duration::from_millis(150));
        assert_eq!(count(), 0, "tick read the tree on GPUI's loop");
        // The snapshot runs on the background executor and is applied later.
        readers.update(cx, |readers, cx| readers.tick(cx));
        cx.run_until_parked();
        assert_eq!(count(), 1);
    }

    #[gpui::test]
    async fn polls_only_while_a_reader_session_exists(cx: &mut gpui::TestAppContext) {
        let inner = broscribe_platform::fakes::FakeAppUiReader::default();
        inner.set_tree(
            4242,
            AxTree {
                root: AxNode::new("AXApplication"),
                incomplete: Default::default(),
            },
        );
        let reader = Arc::new(SlowReader {
            inner,
            snapshots: Default::default(),
        });
        let data = synthetic_data();
        let readers = cx.update(|cx| {
            AppReaders::with_reader(
                reader.clone(),
                link(),
                data.path().into(),
                crate::test_support::admission(data.path()),
                cx,
            )
        });
        let polling =
            |cx: &mut gpui::TestAppContext| readers.read_with(cx, |r, _| r.poll.is_some());
        let start = Event::StartReader {
            session_id: "synthetic-session".into(),
            app_id: "teams".into(),
            pid: 4242,
            elapsed_s: 1.0,
            reader_generation: 1,
        };
        let stop = Event::StopReader {
            session_id: "synthetic-session".into(),
        };
        let handle = |event: &Event, cx: &mut gpui::TestAppContext| {
            readers.update(cx, |r, cx| r.handle_event(event, cx).unwrap());
            cx.run_until_parked();
        };
        // Idle: no timer at all, so GPUI's loop is never woken.
        assert!(!polling(cx));
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();
        assert_eq!(
            reader.snapshots.load(std::sync::atomic::Ordering::SeqCst),
            0
        );

        handle(&start, cx);
        assert!(polling(cx));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(
            reader.snapshots.load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        handle(&stop, cx);
        assert!(!polling(cx));
        handle(&start, cx);
        assert!(polling(cx));
        handle(&Event::Lagged { dropped: 1 }, cx);
        assert!(!polling(cx));
        handle(&start, cx);
        readers.update(cx, |r, cx| r.disconnected(cx));
        assert!(!polling(cx));
    }

    /// The call's own browser tab: one document element at the meeting URL.
    fn web(children: Vec<AxNode>) -> AxTree {
        let with = |role: &str, children: Vec<AxNode>| AxNode {
            children,
            ..AxNode::new(role)
        };
        let area = AxNode {
            document: Some(broscribe_platform::DocumentContext::new(
                Some("https://meet.google.com/abc-defg-hij".into()),
                Some("synthetic-document".into()),
                false,
            )),
            ..with("AXWebArea", children)
        };
        AxTree {
            root: with("AXApplication", vec![with("AXWindow", vec![area])]),
            incomplete: Default::default(),
        }
    }

    fn in_call() -> AxTree {
        web(vec![AxNode {
            description: Some("Leave call".into()),
            ..AxNode::new("AXButton")
        }])
    }

    fn post_call() -> AxTree {
        web(vec![
            AxNode {
                title: Some("You left the meeting".into()),
                ..AxNode::new("AXHeading")
            },
            AxNode {
                title: Some("Rejoin".into()),
                ..AxNode::new("AXButton")
            },
        ])
    }

    fn chat_decoy() -> AxTree {
        web(vec![AxNode {
            children: ["You left the meeting", "Rejoin", "Leave call"]
                .into_iter()
                .map(|text| AxNode {
                    value: Some(text.into()),
                    ..AxNode::new("AXStaticText")
                })
                .collect(),
            ..AxNode::new("AXList")
        }])
    }

    /// Advances the executor clock one second at a time, collecting every
    /// batch the readers hand to the transport with the second it left.
    fn run_for(
        cx: &mut gpui::TestAppContext,
        rx: &mut mpsc::Receiver<ReaderBatch>,
        now: &mut u64,
        seconds: u64,
        into: &mut Vec<(u64, ReaderBatch)>,
    ) {
        for _ in 0..seconds {
            cx.executor().advance_clock(Duration::from_secs(1));
            cx.run_until_parked();
            *now += 1;
            while let Ok(batch) = rx.try_recv() {
                into.push((*now, batch));
            }
        }
    }

    #[gpui::test]
    async fn record_auto_stop_reader_heartbeat_reports_call_state_to_the_daemon(
        cx: &mut gpui::TestAppContext,
    ) {
        use broscribe_readers::CallState;
        // A synthetic library, so self aliases never read the owner's.
        let dir = synthetic_data();
        let reader = Arc::new(broscribe_platform::fakes::FakeAppUiReader::default());
        reader.set_tree(4242, in_call());
        let (tx, mut rx) = mpsc::channel(256);
        let readers = cx.update(|cx| {
            AppReaders::with_outgoing(made(reader.clone()), link(), dir.path().into(), tx, cx)
        });
        readers.update(cx, |r, cx| {
            r.handle_event(
                &Event::StartReader {
                    session_id: "synthetic-session".into(),
                    app_id: "meet-chrome".into(),
                    pid: 4242,
                    elapsed_s: 0.0,
                    reader_generation: 9,
                },
                cx,
            )
            .unwrap()
        });
        cx.run_until_parked();
        let mut now = 0;
        let mut batches = Vec::new();
        run_for(cx, &mut rx, &mut now, 6, &mut batches);
        let active = batches.len();
        assert!(active >= 3, "{active}");
        // The call ends; its post-call page then stays quiet (no AX
        // notifications) through the 20 s debounce and 10 s grace.
        reader.set_tree(4242, post_call());
        run_for(cx, &mut rx, &mut now, 32, &mut batches);
        let ended: Vec<u64> = batches[active..]
            .iter()
            .filter(|(_, b)| b.call_observation.state == CallState::Ended)
            .map(|(at, _)| *at)
            .collect();
        assert!(ended.len() >= 15, "{ended:?}");
        assert!(ended.windows(2).all(|w| w[1] - w[0] <= 2), "{ended:?}");
        assert!(
            batches[active..]
                .iter()
                .filter(|(_, b)| b.call_observation.state == CallState::Ended)
                .skip(1)
                .all(|(_, b)| b.events.is_empty()),
            "deduplicated events, heartbeat kept"
        );
        // Chat that only mentions leaving reads as unknown.
        reader.set_tree(4242, chat_decoy());
        let before = batches.len();
        run_for(cx, &mut rx, &mut now, 3, &mut batches);
        assert!(
            batches[before..]
                .iter()
                .all(|(_, b)| b.call_observation.state == CallState::Unknown)
        );
        // The app goes away: one last incomplete Unknown, then no polling.
        reader.trees.lock().unwrap().clear();
        let before = batches.len();
        run_for(cx, &mut rx, &mut now, 3, &mut batches);
        let last = &batches[before..];
        assert_eq!(last.len(), 1, "one final batch");
        assert_eq!(last[0].1.call_observation.state, CallState::Unknown);
        assert!(!last[0].1.call_observation.complete);
        assert!(readers.read_with(cx, |r, _| r.poll.is_none()));

        // Every batch: this reader generation, increasing sequences, wall
        // clock taken per snapshot.
        assert!(batches.iter().all(|(_, b)| {
            b.reader_generation == 10
                && b.session_id == "synthetic-session"
                && b.app == "meet-chrome"
        }));
        assert!(batches.windows(2).all(|w| {
            w[1].1.call_observation.sequence > w[0].1.call_observation.sequence
                && w[1].1.call_observation.snapshot_started_at_ms
                    >= w[0].1.call_observation.snapshot_started_at_ms
        }));
        assert_eq!(batches[0].1.call_observation.state, CallState::Active);
    }
}
