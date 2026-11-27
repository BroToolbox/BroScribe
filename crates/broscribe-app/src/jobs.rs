//! Processing through the `broscribe` CLI (plan §8 items 1, 10, 14, 18).
//!
//! The app never runs native models or heavy pipelines itself. Queue jobs are
//! executed by a `broscribe --library <lib> queue work --json-progress` child,
//! which claims jobs through the library queue (leases and lock, so it can run
//! next to a CLI started from a terminal), prints one JSON object per line on
//! stdout and exits when nothing is left. This module locates the CLI, runs
//! that child on a dedicated reader thread, parses its progress and batches it
//! for the UI at most [`PROGRESS_INTERVAL`] apart (≤4 Hz). The decisions
//! (when to start, restart or stop the child) live in [`crate::state`]. The
//! worker is durable work holding its own leases: the app never kills it.
//!
//! One-shot `--json` commands (for example `setup voice --from-file`) go
//! through [`CliRunner`] on GPUI's background executor. Their arguments are
//! flags and paths only: sensitive text goes to children on stdin or in
//! mode-0600 files, never in argv (AGENTS.md).
//!
//! Every child's output is bounded: one-shot stdout up to
//! [`MAX_CLI_OUTPUT`] (more is an `output_too_large` error, never a cut
//! reply), progress lines (worker stdout, model-pull stderr) up to
//! [`MAX_PROGRESS_LINE`] (longer lines are skipped whole), other stderr
//! discarded (never kept or logged), every pipe drained on its own thread
//! and readers given [`OUTPUT_DRAIN_GRACE`] after the child exits. Setup
//! and cancellable one-shots run as a contained process tree (the
//! platform's [`ProcessTreeControl`]): cancelling kills that tree only,
//! then reaps it, and only then does the call return `cancelled`.
//! Cooperative commands ([`CliRunner::run_blocking_cooperative`]) are
//! never killed: closing their stdin asks them to stop.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use broscribe_core::maintenance::ModeLease;
use broscribe_core::process::ProcessProbe;
use broscribe_core::queue::Queue;
use broscribe_core::{Error, Library, Result};
use broscribe_platform::{ProcessTreeControl, ResolvedPaths, TreeExit, wait_tree};
use broscribe_platform_native::NativePlatform;
use chrono::{DateTime, FixedOffset};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use gpui::{BackgroundExecutor, Context, Task};
use serde::Deserialize;

use crate::state::AppState;

/// Overrides the CLI binary the app spawns (development and tests).
pub const CLI_BIN_ENV: &str = "BROSCRIBE_CLI_BIN";
/// The CLI binary's file name; the bundle ships it next to the app binary
/// (`BroScribe.app/Contents/MacOS/broscribe`).
pub const CLI_BIN_NAME: &str = "broscribe";
/// Minimum time between two job-progress updates of the UI (≤4 Hz).
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// Crashes in a row after which the worker is no longer restarted.
pub const MAX_CONSECUTIVE_CRASHES: u32 = 5;
/// Pause before a crashed worker is started again.
pub const CRASH_RESTART_DELAY: Duration = Duration::from_secs(2);
/// Shortest wait before the worker is started for a waiting job that is
/// due ([`QueueCheck::wake_at`]): a due job a worker did not take is retried
/// at most this often, never in a tight loop.
pub const DEFERRED_WAKEUP_MIN: Duration = Duration::from_secs(1);
/// Failed jobs kept for display.
const RECENT_FAILURES: usize = 20;
/// Largest stdout a one-shot `--json` command may print; a larger reply is
/// the `output_too_large` error, never parsed cut.
pub const MAX_CLI_OUTPUT: usize = 64 << 20;
/// Longest progress line of a child (worker stdout, model-pull stderr);
/// longer lines are skipped whole.
pub const MAX_PROGRESS_LINE: usize = 64 << 10;
/// Time a child's output readers get after it exited (a stray descendant may
/// still hold the pipes).
pub const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(5);
/// Worker messages waiting for the UI before the reader thread waits for
/// the UI to take them (progress of one job merges, so only job starts,
/// ends, failures and exits count against it).
const MAX_PENDING_WORKER_MSGS: usize = 1024;

/// The CLI next to `current_exe`, unless `env_override` names one.
pub fn cli_path_from(current_exe: &Path, env_override: Option<OsString>) -> PathBuf {
    match env_override.filter(|v| !v.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            current_exe.with_file_name(format!("{CLI_BIN_NAME}{}", std::env::consts::EXE_SUFFIX))
        }
    }
}

/// The CLI to spawn: `$BROSCRIBE_CLI_BIN`, else `broscribe` next to this binary.
pub fn locate_cli() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(cli_path_from(&exe, std::env::var_os(CLI_BIN_ENV)))
}

/// Whether `cli` can be run: an existing regular file (symlinks followed)
/// that, on Unix, has an execute permission bit. `Err` says why not.
pub fn check_cli(cli: &Path) -> std::io::Result<()> {
    use std::io::{Error as IoError, ErrorKind};
    let meta = std::fs::metadata(cli)?;
    if !meta.is_file() {
        return Err(IoError::new(ErrorKind::InvalidInput, "not a file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(IoError::new(ErrorKind::PermissionDenied, "not executable"));
        }
    }
    Ok(())
}

// ---- progress protocol ----

/// `event` of a `queue work --json-progress` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventKind {
    JobStarted,
    Progress,
    JobDone,
    JobFailed,
    /// The queue is empty; the worker is about to exit.
    Idle,
    /// An event added by a newer CLI; ignored.
    #[serde(other)]
    Unknown,
}

/// One progress line:
/// `{"event":"job_started|progress|job_done|job_failed|idle","job_id","kind","fraction","message"}`.
/// Every field but `event` may be missing or `null`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ProgressEvent {
    pub event: ProgressEventKind,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub meeting_id: Option<broscribe_core::MeetingId>,
    /// 0.0–1.0 within the current job.
    #[serde(default)]
    pub fraction: Option<f64>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Parses one stdout line; `None` for blank lines and anything that is not a
/// progress object (the caller logs it).
pub fn parse_progress_line(line: &str) -> Option<ProgressEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// What the worker's reader thread reports to the app.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkerMsg {
    Progress(ProgressEvent),
    /// Exited successfully (the queue was empty).
    Finished,
    /// Exited with a failure status or was killed; the detail is the status.
    Crashed(String),
    /// Could not be started (CLI missing or not executable).
    SpawnFailed(WorkerSpawnFailure),
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkerSpawnFailure {
    executable: OsString,
    pub(crate) kind: std::io::ErrorKind,
}

impl WorkerSpawnFailure {
    fn new(executable: &Path, kind: std::io::ErrorKind) -> Self {
        Self {
            executable: executable
                .file_name()
                .unwrap_or("broscribe".as_ref())
                .to_owned(),
            kind,
        }
    }
}

impl std::fmt::Display for WorkerSpawnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.executable.to_string_lossy(), self.kind)
    }
}

/// Worker messages received between two UI updates. A `progress` event of a
/// job merges into that job's pending `progress` since the last other
/// message (newest fraction, message and kind win field by field, as
/// [`JobsStatus::apply`] would leave them), unknown events and repeated
/// `idle` are dropped (they change nothing), and job starts, ends, failures
/// and worker exits are always kept, in order.
#[derive(Debug, Default)]
pub struct WorkerBatch {
    msgs: Vec<WorkerMsg>,
}

impl WorkerBatch {
    pub fn push(&mut self, msg: WorkerMsg) {
        if let WorkerMsg::Progress(new) = &msg {
            match new.event {
                ProgressEventKind::Unknown => return,
                ProgressEventKind::Idle
                    if matches!(
                        self.msgs.last(),
                        Some(WorkerMsg::Progress(last)) if last.event == ProgressEventKind::Idle
                    ) =>
                {
                    return;
                }
                ProgressEventKind::Progress => {
                    let pending = self.msgs.iter_mut().rev().map_while(|m| match m {
                        WorkerMsg::Progress(p) if p.event == ProgressEventKind::Progress => Some(p),
                        _ => None,
                    });
                    for last in pending {
                        if last.job_id == new.job_id {
                            last.kind = new.kind.clone().or(last.kind.take());
                            last.fraction = new.fraction.or(last.fraction);
                            last.message = new.message.clone().or(last.message.take());
                            return;
                        }
                    }
                }
                _ => {}
            }
        }
        self.msgs.push(msg);
    }

    fn len(&self) -> usize {
        self.msgs.len()
    }

    fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }

    pub fn into_msgs(self) -> Vec<WorkerMsg> {
        self.msgs
    }
}

/// The pending batch shared by the worker's senders and the UI pump.
struct WorkerShared {
    pending: Mutex<PendingBatch>,
    /// Signalled when the pump took the batch or went away.
    room: Condvar,
}

struct PendingBatch {
    batch: WorkerBatch,
    receiver_alive: bool,
}

/// A bounded, coalescing channel for [`WorkerMsg`]s: the reader thread adds
/// to one pending [`WorkerBatch`] and the UI pump takes it whole. Wake-ups
/// are a bounded channel too, so a chatty worker never queues more than one
/// pending wake-up per sender.
pub fn worker_channel() -> (WorkerSender, WorkerReceiver) {
    let shared = Arc::new(WorkerShared {
        pending: Mutex::new(PendingBatch {
            batch: WorkerBatch::default(),
            receiver_alive: true,
        }),
        room: Condvar::new(),
    });
    let (wake_tx, wake_rx) = mpsc::channel(0);
    (
        WorkerSender {
            shared: Arc::clone(&shared),
            wake: Mutex::new(wake_tx),
        },
        WorkerReceiver {
            shared,
            wake: wake_rx,
        },
    )
}

/// The sending half of [`worker_channel`].
pub struct WorkerSender {
    shared: Arc<WorkerShared>,
    wake: Mutex<mpsc::Sender<()>>,
}

impl Clone for WorkerSender {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            wake: Mutex::new(lock(&self.wake).clone()),
        }
    }
}

impl WorkerSender {
    /// Adds `msg` to the pending batch without waiting (the foreground and
    /// tests). Dropped once the receiver is gone (the app quits).
    pub fn send(&self, msg: WorkerMsg) {
        self.push(msg, false);
    }

    /// [`Self::send`] from a reader thread: while
    /// [`MAX_PENDING_WORKER_MSGS`] messages wait for the UI it waits for the
    /// pump to take them, so a stalled UI slows the worker's output instead
    /// of growing memory or dropping a job's end.
    fn send_waiting(&self, msg: WorkerMsg) {
        self.push(msg, true);
    }

    fn push(&self, msg: WorkerMsg, wait_for_room: bool) {
        let mut pending = lock(&self.shared.pending);
        while wait_for_room
            && pending.receiver_alive
            && pending.batch.len() >= MAX_PENDING_WORKER_MSGS
        {
            pending = self
                .shared
                .room
                .wait(pending)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if !pending.receiver_alive {
            return;
        }
        pending.batch.push(msg);
        drop(pending);
        // Full: a wake-up is already pending and the pump takes this batch
        // after it. Disconnected: the pump is gone.
        let _ = lock(&self.wake).try_send(());
    }
}

/// The receiving half of [`worker_channel`]; consumed by [`spawn_worker_pump`].
pub struct WorkerReceiver {
    shared: Arc<WorkerShared>,
    wake: mpsc::Receiver<()>,
}

impl WorkerReceiver {
    fn take(&self) -> WorkerBatch {
        let batch = std::mem::take(&mut lock(&self.shared.pending).batch);
        self.shared.room.notify_all();
        batch
    }

    fn has_pending(&self) -> bool {
        !lock(&self.shared.pending).batch.is_empty()
    }
}

impl Drop for WorkerReceiver {
    fn drop(&mut self) {
        let mut pending = lock(&self.shared.pending);
        pending.receiver_alive = false;
        pending.batch = WorkerBatch::default();
        drop(pending);
        self.shared.room.notify_all();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- status shown by the UI ----

#[derive(Clone, Debug, Default, PartialEq)]
pub enum WorkerState {
    /// No worker running.
    #[default]
    Idle,
    Running,
    /// Crashed; starts again after [`CRASH_RESTART_DELAY`].
    Restarting {
        crashes: u32,
    },
    /// Not restarted until the next kick: the CLI could not be started, or it
    /// crashed [`MAX_CONSECUTIVE_CRASHES`] times in a row.
    Unavailable {
        detail: String,
        crashes: Option<u32>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveJob {
    pub job_id: String,
    pub kind: Option<String>,
    pub meeting_id: Option<broscribe_core::MeetingId>,
    pub fraction: Option<f64>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FailedJob {
    pub job_id: String,
    pub kind: Option<String>,
    pub meeting_id: Option<broscribe_core::MeetingId>,
    pub message: Option<String>,
}

/// Background processing as the UI shows it.
#[derive(Clone, Debug, Default)]
pub struct JobsStatus {
    pub worker: WorkerState,
    /// Jobs the worker reported as started and not finished yet.
    pub active: Vec<ActiveJob>,
    /// Jobs finished in this session.
    pub completed: u64,
    /// Newest last, at most 20.
    pub recent_failures: VecDeque<FailedJob>,
    /// Status of the last crashed worker.
    pub last_crash: Option<String>,
    /// UI updates applied so far (each one is a coalesced batch).
    pub ui_updates: u64,
}

impl JobsStatus {
    /// Applies one progress event and returns whether anything the UI shows
    /// changed (a repeated fraction or message does not), so callers notify
    /// only then. Events without a `job_id` (other than `idle`) are ignored.
    pub fn apply(&mut self, ev: ProgressEvent) -> bool {
        match ev.event {
            ProgressEventKind::Idle => {
                let changed = !self.active.is_empty();
                self.active.clear();
                changed
            }
            ProgressEventKind::Unknown => false,
            ProgressEventKind::JobStarted | ProgressEventKind::Progress => {
                let Some(job_id) = ev.job_id else {
                    return false;
                };
                let fraction = ev.fraction.map(|f| f.clamp(0.0, 1.0));
                match self.active.iter_mut().find(|j| j.job_id == job_id) {
                    Some(job) => {
                        let before = job.clone();
                        if ev.kind.is_some() {
                            job.kind = ev.kind;
                        }
                        if ev.meeting_id.is_some() || ev.event == ProgressEventKind::JobStarted {
                            job.meeting_id = ev.meeting_id;
                        }
                        if ev.event == ProgressEventKind::JobStarted || fraction.is_some() {
                            job.fraction = fraction;
                        }
                        if ev.message.is_some() || ev.event == ProgressEventKind::JobStarted {
                            job.message = ev.message;
                        }
                        *job != before
                    }
                    None => {
                        self.active.push(ActiveJob {
                            job_id,
                            kind: ev.kind,
                            meeting_id: ev.meeting_id,
                            fraction,
                            message: ev.message,
                        });
                        true
                    }
                }
            }
            ProgressEventKind::JobDone | ProgressEventKind::JobFailed => {
                let Some(job_id) = ev.job_id else {
                    return false;
                };
                let previous = self
                    .active
                    .iter()
                    .position(|j| j.job_id == job_id)
                    .map(|i| self.active.remove(i));
                if ev.event == ProgressEventKind::JobDone {
                    self.completed += 1;
                    return true;
                }
                self.record_failure(FailedJob {
                    job_id,
                    kind: ev
                        .kind
                        .or_else(|| previous.as_ref().and_then(|p| p.kind.clone())),
                    meeting_id: ev
                        .meeting_id
                        .or_else(|| previous.and_then(|p| p.meeting_id)),
                    message: ev.message,
                });
                true
            }
        }
    }

    /// Adds a failed job to [`Self::recent_failures`] (bounded).
    pub fn record_failure(&mut self, failed: FailedJob) {
        if self.recent_failures.len() == RECENT_FAILURES {
            self.recent_failures.pop_front();
        }
        self.recent_failures.push_back(failed);
    }

    /// The agent ended a run that left its job unfinished on purpose (a
    /// speaker recheck continuing in its next turn): no longer active,
    /// neither completed nor failed. Returns whether it was shown active.
    pub fn left_waiting(&mut self, job_id: &str) -> bool {
        let before = self.active.len();
        self.active.retain(|j| j.job_id != job_id);
        self.active.len() != before
    }
}

// ---- the worker child ----

/// What [`has_pending_jobs`] found.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct QueueCheck {
    /// Work a worker could pick up now.
    pub pending: bool,
    /// Crashed jobs with no attempts left, marked `failed` by this check;
    /// no worker ever reports them, so the caller shows them as failures.
    pub crashed: Vec<FailedJob>,
    /// When whoever runs jobs starts a worker again: the earliest time a
    /// job waiting for its `not_before` or its retry delay
    /// ([`retry_delay`]) becomes due. A worker exits while they wait. When
    /// such a job is already due (a worker that just exited tries each job
    /// once per run, so it may not have reached it), the time of the check.
    pub wake_at: Option<DateTime<FixedOffset>>,
}

/// First retry delay of a failed job, doubled per attempt spent; the same
/// policy as the agent's scheduler.
pub const RETRY_BACKOFF: Duration = Duration::from_secs(10);
/// Longest retry delay of a failed job.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(3600);

/// The delay before a job that spent `attempts` runs again: none for a
/// fresh job, then [`RETRY_BACKOFF`] doubled per further attempt, at most
/// [`MAX_RETRY_DELAY`].
pub fn retry_delay(attempts: u32) -> Duration {
    if attempts == 0 {
        return Duration::ZERO;
    }
    let factor = 1u32 << attempts.saturating_sub(1).min(16);
    RETRY_BACKOFF.saturating_mul(factor).min(MAX_RETRY_DELAY)
}

/// Whether the queue holds work a worker could pick up at `now` (the app's
/// clock), by the queue's own planner ([`QueueSnapshot::pending_work`]
/// with [`retry_delay`]): claimable jobs, `failed` ones with attempts left
/// included, whose lease no live process holds, once their `not_before`
/// and retry delay have passed; a job failed for good or an interrupted
/// interactive run never. Crashed jobs with no attempts left would never
/// be claimed again, so they are marked `failed` first (and returned) and
/// a retry can restart them. Waiting jobs give [`QueueCheck::wake_at`].
/// Reads `job.json` files, so call it off the main thread.
///
/// [`QueueSnapshot::pending_work`]: broscribe_core::queue::QueueSnapshot::pending_work
pub fn has_pending_jobs(
    library_root: &Path,
    probe: &dyn ProcessProbe,
    now: DateTime<FixedOffset>,
) -> Result<QueueCheck> {
    let queue = Queue::open(library_root)?;
    let snap = queue.snapshot()?;
    let work = snap.pending_work(probe, &retry_delay);
    // Due jobs that waited for a time; `runnable` is in the snapshot's order.
    let mut runnable = work.runnable.iter().peekable();
    let waited_due = snap.jobs.iter().any(|job| {
        if runnable.peek() != Some(&&job.id) {
            return false;
        }
        runnable.next();
        job.attempts > 0 || job.not_before.is_some()
    });
    let mut crashed = Vec::new();
    if !work.exhausted.is_empty() {
        // Bookkeeping only: a busy library must not hide pending work. An
        // exhausted job is never runnable, so the plan above stands.
        match Library::open(library_root)
            .and_then(|lib| lib.write(|tx| queue.fail_exhausted(tx, probe)))
        {
            Ok(failed) => {
                tracing::info!("queue: {} crashed job(s) out of attempts", failed.len());
                for id in failed {
                    let job = queue.get(&id).ok();
                    crashed.push(FailedJob {
                        job_id: id.to_string(),
                        kind: job.as_ref().map(|j| j.kind.clone()),
                        meeting_id: job
                            .as_ref()
                            .and_then(broscribe_core::queue::Job::meeting_id),
                        message: Some(
                            job.and_then(|j| j.error)
                                .unwrap_or_else(|| "worker crashed".into()),
                        ),
                    });
                }
            }
            Err(e) => tracing::warn!(code = e.code(), "queue: marking crashed jobs failed"),
        }
    }
    Ok(QueueCheck {
        // The snapshot was planned at its own read time; a waiting job the
        // app's clock has reached counts too.
        pending: !work.runnable.is_empty() || work.wake_at.is_some_and(|at| at <= now),
        crashed,
        wake_at: if waited_due {
            Some(snap.at())
        } else {
            work.wake_at
        },
    })
}

/// Starts the worker child: the CLI, its arguments (flags and paths only),
/// and the channel its progress and exit ([`WorkerMsg`]) go to. `Err` when
/// it could not be started at all.
#[cfg(test)]
pub(crate) type WorkerLauncher =
    Box<dyn Fn(&Path, &[OsString], &WorkerSender) -> std::io::Result<()>>;

/// The app's handle on the worker child: where the CLI is, whether a child
/// runs, and the channel its reader thread reports on.
pub struct WorkerControl {
    runner: CliRunner,
    tx: WorkerSender,
    #[cfg(test)]
    launch: Option<WorkerLauncher>,
    running: bool,
    pub(crate) checking: bool,
    /// The queue check in flight starts the worker when it finds work (a
    /// check that only schedules the next wake-up leaves it unset).
    pub(crate) start_after_check: bool,
    rerun: bool,
    pub(crate) crashes: u32,
}

impl WorkerControl {
    pub fn new(runner: CliRunner, tx: WorkerSender) -> Self {
        Self {
            runner,
            tx,
            #[cfg(test)]
            launch: None,
            running: false,
            checking: false,
            start_after_check: false,
            rerun: false,
            crashes: 0,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Asks for one more run after the current child exits (work was
    /// enqueued after it may have looked at the queue).
    pub(crate) fn request_rerun(&mut self) {
        self.rerun = true;
    }

    pub(crate) fn take_rerun(&mut self) -> bool {
        std::mem::take(&mut self.rerun)
    }

    pub(crate) fn exited(&mut self) {
        self.running = false;
    }

    #[cfg(test)]
    pub(crate) fn sender(&self) -> WorkerSender {
        self.tx.clone()
    }

    /// Replaces how the child is started (tests: a launcher on the test
    /// thread that reports through the same channel).
    #[cfg(test)]
    pub(crate) fn set_launcher(
        &mut self,
        launch: impl Fn(&Path, &[OsString], &WorkerSender) -> std::io::Result<()> + 'static,
    ) {
        self.launch = Some(Box::new(launch));
    }

    /// Starts `broscribe --library <lib> queue work --json-progress` (by
    /// default on a reader thread). The child is never killed by the app:
    /// it is durable work holding its own leases, and quitting lets it
    /// finish its current job (it ignores the broken progress pipe).
    pub(crate) fn spawn(&mut self) {
        let args = worker_args(&self.runner.library);
        #[cfg(test)]
        let launched = if let Some(launch) = &self.launch {
            launch(&self.runner.cli, &args, &self.tx)
        } else {
            launch_reader_thread(&self.runner, &args, &self.tx)
        };
        #[cfg(not(test))]
        let launched = launch_reader_thread(&self.runner, &args, &self.tx);
        match launched {
            Ok(()) => self.running = true,
            Err(e) => {
                self.tx.send(WorkerMsg::SpawnFailed(WorkerSpawnFailure::new(
                    &self.runner.cli,
                    e.kind(),
                )));
            }
        }
    }
}

/// The worker child's arguments: flags and the library path only.
fn worker_args(library: &Path) -> Vec<OsString> {
    vec![
        "--library".into(),
        library.as_os_str().to_owned(),
        "queue".into(),
        "work".into(),
        "--json-progress".into(),
    ]
}

/// The default [`WorkerLauncher`]: runs the child on a reader thread.
fn launch_reader_thread(
    runner: &CliRunner,
    args: &[OsString],
    tx: &WorkerSender,
) -> std::io::Result<()> {
    let runner = runner.clone();
    let args = args.to_vec();
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("queue-worker-reader".into())
        .spawn(move || run_worker_child(&runner, &args, &tx))
        .map(drop)
}

/// Runs the worker child to its end. Its progress lines are forwarded
/// through `tx` as they come (bounded per line, never logged raw); its
/// stderr is discarded, so nothing it writes there is kept or logged and a
/// quitting app cannot break it. After the child exits its stdout gets
/// [`OUTPUT_DRAIN_GRACE`] to close (a stray descendant may hold it), then
/// the exit is reported and later lines of this run are dropped.
fn run_worker_child(runner: &CliRunner, args: &[OsString], tx: &WorkerSender) {
    let spawned = runner
        .profile_command()
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            tx.send(WorkerMsg::SpawnFailed(WorkerSpawnFailure::new(
                &runner.cli,
                e.kind(),
            )));
            return;
        }
    };
    tracing::info!(pid = child.id(), "queue worker started");
    // Open while this run may still report progress; held while forwarding,
    // so no line lands after the exit message.
    let gate = Arc::new(Mutex::new(true));
    let (eof_tx, eof_rx) = std_mpsc::channel::<()>();
    if let Some(stdout) = child.stdout.take() {
        let tx = tx.clone();
        let gate = Arc::clone(&gate);
        let admission = runner.admission.clone();
        let reader = std::thread::Builder::new()
            .name("queue-worker-progress".into())
            .spawn(move || {
                let _admission = admission;
                let read = for_each_line(stdout, MAX_PROGRESS_LINE, |line| {
                    forward_progress_line(line, &gate, &tx);
                });
                if let Err(e) = read {
                    tracing::warn!(kind = ?e.kind(), "queue worker: reading progress failed");
                }
                let _ = eof_tx.send(());
            });
        if let Err(e) = reader {
            // Its progress is lost, never its exit.
            tracing::warn!(kind = ?e.kind(), "queue worker: no progress reader");
        }
    }
    let status = child.wait();
    if let Err(std_mpsc::RecvTimeoutError::Timeout) = eof_rx.recv_timeout(OUTPUT_DRAIN_GRACE) {
        tracing::warn!("queue worker: its output stayed open after it exited");
    }
    *lock(&gate) = false;
    let msg = match status {
        Ok(status) if status.success() => WorkerMsg::Finished,
        Ok(status) => WorkerMsg::Crashed(status.to_string()),
        Err(e) => WorkerMsg::Crashed(format!("waiting for the worker: {}", e.kind())),
    };
    tracing::info!("queue worker exited: {msg:?}");
    tx.send_waiting(msg);
}

/// Forwards one worker stdout line while `gate` is open. Lines that are not
/// progress objects are logged by size only: they may hold meeting text.
fn forward_progress_line(line: Line<'_>, gate: &Mutex<bool>, tx: &WorkerSender) {
    match line {
        Line::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            match parse_progress_line(&text) {
                Some(ev) => {
                    let open = lock(gate);
                    if *open {
                        tx.send_waiting(WorkerMsg::Progress(ev));
                    }
                }
                None if text.trim().is_empty() => {}
                None => tracing::warn!(
                    bytes = bytes.len(),
                    "queue worker: skipped an output line that is not progress"
                ),
            }
        }
        Line::TooLong(bytes) => tracing::warn!(
            bytes,
            limit = MAX_PROGRESS_LINE,
            "queue worker: skipped an over-long output line"
        ),
    }
}

/// One line of a child's output: its bytes without the newline, or only its
/// length when it exceeded the limit (never cut and parsed).
enum Line<'a> {
    Text(&'a [u8]),
    TooLong(usize),
}

/// Calls `f` for every newline-terminated line of `from` (and a last
/// unterminated one), holding at most `max` bytes of a line at a time.
fn for_each_line(
    mut from: impl Read,
    max: usize,
    mut f: impl FnMut(Line<'_>),
) -> std::io::Result<()> {
    let mut line = Vec::new();
    let mut skipped: Option<usize> = None;
    let mut chunk = [0u8; 8 * 1024];
    let mut emit = |line: &mut Vec<u8>, skipped: &mut Option<usize>| {
        match skipped.take() {
            Some(len) => f(Line::TooLong(len)),
            None => f(Line::Text(&line[..])),
        }
        line.clear();
    };
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let mut rest = &chunk[..n];
        while !rest.is_empty() {
            let newline = rest.iter().position(|&b| b == b'\n');
            let part = &rest[..newline.unwrap_or(rest.len())];
            if let Some(len) = skipped.as_mut() {
                *len += part.len();
            } else if line.len() + part.len() > max {
                skipped = Some(line.len() + part.len());
                line.clear();
            } else {
                line.extend_from_slice(part);
            }
            match newline {
                Some(i) => {
                    emit(&mut line, &mut skipped);
                    rest = &rest[i + 1..];
                }
                None => rest = &[],
            }
        }
    }
    if !line.is_empty() || skipped.is_some() {
        emit(&mut line, &mut skipped);
    }
    Ok(())
}

/// Receives worker messages on GPUI's foreground executor and applies them to
/// `AppState` in batches at most [`PROGRESS_INTERVAL`] apart. The first
/// message after a quiet period applies at once; later ones wait for the
/// interval, then everything pending meanwhile applies in one update. It
/// sleeps between wake-ups (never polls) and skips wake-ups whose messages
/// an earlier update already took.
pub(crate) fn spawn_worker_pump(rx: WorkerReceiver, cx: &mut Context<AppState>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = rx;
        let executor = cx.background_executor().clone();
        let mut last_update: Option<Instant> = None;
        loop {
            let open = rx.wake.next().await.is_some();
            if !rx.has_pending() {
                if open {
                    continue;
                }
                break;
            }
            if open && let Some(last) = last_update {
                let due = last + PROGRESS_INTERVAL;
                let now = executor.now();
                if due > now {
                    executor.timer(due - now).await;
                }
            }
            let batch = rx.take();
            last_update = Some(executor.now());
            if this
                .update(cx, |state, cx| state.apply_worker_batch(batch, cx))
                .is_err()
                || !open
            {
                break;
            }
        }
    })
}

// ---- one-shot CLI commands ----

/// How a one-shot child is treated when the app quits or a flow is cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildKind {
    /// Setup work bound to the UI flow (enrollment, Try-it): a contained
    /// process tree, cancelled by its own flag and by
    /// [`CliChildren::cancel_setup_children`] (setup cancel, quit).
    Setup,
    /// Anything else: left to finish, also when the app quits; it may
    /// already have committed. Only a caller's own cancel flag (the
    /// `*_cancellable*` calls, for read-only work) stops one.
    Command,
}

/// A failed one-shot command: the CLI's `{"error":{"code","message"}}`
/// ([`CliError::from_cli`] set), or one BroScribe made around the child:
/// `spawn`, `io`, `cancelled`, `bad_output`, `output_too_large`, `exit`.
///
/// Views show [`CliError::user_text`], never the code: `Display` ("{message}
/// ({code})") is for Copy details and Diagnostics only, and the log gets
/// the code alone (design §4.2 Plain failures).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError {
    pub code: String,
    pub message: String,
    /// The CLI answered with this error (its `{"error":…}`). `false` for a
    /// failure of the child itself, which the app made: it couldn't start,
    /// stopped, printed nothing readable or was cancelled.
    pub from_cli: bool,
}

impl CliError {
    /// A failure the app made around the child.
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            from_cli: false,
        }
    }

    /// The CLI's own `{"error":{"code","message"}}`.
    pub(crate) fn answered(code: &str, message: impl Into<String>) -> Self {
        Self {
            from_cli: true,
            ..Self::new(code, message)
        }
    }

    /// Why the command failed, as one plain sentence without the code: the
    /// {Cause} of "Couldn't <do X>. <Cause>. <Next step>." (WRT-04).
    ///
    /// - The child itself failing (an error the app made) says so: "couldn't
    ///   start", "stopped unexpectedly".
    /// - The CLI's own `io` is a file in words (its message names the path);
    ///   its `schema`, `decode`, `internal`, `lease_lost` and `inference`
    ///   get fixed sentences too ([`answered_cause`]), as [`core_user_text`]
    ///   gives them.
    /// - Any other refusal keeps the CLI's words ([`refusal_text`]):
    ///   capitalised, without the "invalid input: " prefix and without any
    ///   absolute path.
    ///
    /// Empty when nothing is known: the caller leaves the cause out.
    pub fn user_text(&self) -> String {
        match (self.code.as_str(), self.from_cli) {
            ("spawn", false) => "BroScribe's command-line tool couldn't start.".to_owned(),
            ("io" | "exit" | "bad_output" | "output_too_large", false) => {
                "BroScribe's command-line tool stopped unexpectedly.".to_owned()
            }
            ("cancelled", _) => "It was cancelled.".to_owned(),
            ("not_found", _) => "It no longer exists.".to_owned(),
            ("busy" | "job_running" | "library_busy", _) => {
                "BroScribe is still working on it.".to_owned()
            }
            ("internal", _) if self.message.trim().is_empty() => String::new(),
            (code, _) => {
                answered_cause(code).map_or_else(|| refusal_text(&self.message), str::to_owned)
            }
        }
    }

    /// A library error met around a command (reading what it answered),
    /// told like the CLI's own answer: its code, and [`core_user_text`] as
    /// the message, so no path reaches [`Self::user_text`] or `Display`.
    pub(crate) fn from_core(error: &Error) -> Self {
        Self::answered(error.code(), core_user_text(error))
    }

    /// [`Self::user_text`] for a model download (`models pull`): the CLI's
    /// `io` there is the download (an HTTP error, a checksum mismatch, a
    /// full disk), so it says "The download didn't finish.".
    pub fn download_text(&self) -> String {
        if self.from_cli && self.code == "io" {
            "The download didn't finish.".to_owned()
        } else {
            self.user_text()
        }
    }
}

/// The CLI's or the library's own file failure, in words (never the path).
const FILE_FAILED: &str = "BroScribe couldn't read or write a file.";
/// Damaged or unreadable library data (`schema`, `decode`), never its path.
const DATA_UNREADABLE: &str = "BroScribe couldn't read its library data.";
/// Another BroScribe process took over the work (`lease_lost`).
const LEASE_LOST: &str = "Another BroScribe process took over this work.";
/// A model run failed (`inference`).
const INFERENCE_FAILED: &str = "A model on this computer couldn't finish.";
/// Something went wrong inside BroScribe (`internal`).
const INTERNAL: &str = "Something went wrong inside BroScribe.";
/// `config.toml` can't be read: its parser's words quote the file's lines,
/// and its path names the user's data folder.
pub(crate) const SETTINGS_FILE_UNREADABLE: &str =
    "BroScribe's settings file can't be read. Fix or remove it, then try again.";

/// The fixed sentence of an answer whose message can name a path or hold
/// the system's words (`io`, `schema`, `decode`, `lease_lost`, `inference`,
/// `internal`), whoever answered it: the CLI ([`CliError::user_text`]) or
/// the recording agent ([`crate::daemon::agent_error_text`]). `None` for a
/// code whose message is a refusal in words ([`refusal_text`]).
pub fn answered_cause(code: &str) -> Option<&'static str> {
    Some(match code {
        "io" => FILE_FAILED,
        "schema" | "decode" => DATA_UNREADABLE,
        "lease_lost" => LEASE_LOST,
        "inference" => INFERENCE_FAILED,
        "internal" => INTERNAL,
        _ => return None,
    })
}

/// A refusal's own words as the {Cause} (an `invalid_input`, a service
/// error, any code without a fixed sentence): a sentence without "invalid
/// input: " and without an absolute path, which would name the user's data
/// or library folder (design §4.2 Plain failures). The advice stays, the
/// values go:
/// - `config.toml` that can't be read (its message starts with the file's
///   path, then the parser's words, which quote its lines) is
///   [`SETTINGS_FILE_UNREADABLE`];
/// - any other message about one file ("<path>: <why>", a template file the
///   user may fix by hand) names it by its file name only, its case kept;
/// - any other absolute path in the words is shortened to its file name.
///   Limit: a path with a space in the middle of a message keeps the words
///   after its first space.
pub fn refusal_text(message: &str) -> String {
    let message = message.trim();
    let message = message
        .strip_prefix("invalid input: ")
        .unwrap_or(message)
        .trim();
    match leading_path(message) {
        Some((file, _)) if file == broscribe_core::config::CONFIG_FILE => {
            SETTINGS_FILE_UNREADABLE.to_owned()
        }
        Some((file, why)) => {
            let why = why.strip_prefix("invalid input: ").unwrap_or(why);
            closed(format!("{file}: {}", without_paths(why)))
        }
        // A file name keeps its case.
        None if message
            .split(' ')
            .next()
            .is_some_and(|word| is_absolute_path(word.trim_start_matches(OPENING))) =>
        {
            closed(without_paths(message))
        }
        None => sentence(&without_paths(message)),
    }
}

/// `text` ending in a full stop (or the `.`, `!` or `?` it ends in).
fn closed(mut text: String) -> String {
    if !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }
    text
}

/// Punctuation that may open or close a path quoted in an error's words.
const OPENING: [char; 7] = ['(', '[', '"', '\'', '`', '“', '‘'];
const CLOSING: [char; 13] = [
    ')', ']', '"', '\'', '`', '”', '’', ',', ';', ':', '.', '!', '?',
];

/// "<absolute path>: <words>" as the path's file name and the words.
fn leading_path(message: &str) -> Option<(&str, &str)> {
    let (head, rest) = message.split_once(": ")?;
    is_absolute_path(head).then(|| (file_name(head), rest.trim()))
}

/// A path from the root of a disk or a home folder (`/…`, `~/…`, `C:\…`,
/// `\\server\…`), as it may appear in an error's words.
fn is_absolute_path(text: &str) -> bool {
    let mut chars = text.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some('/'), Some(c), _) => !c.is_whitespace() && c != '/',
        (Some('~'), Some('/' | '\\'), _) => true,
        (Some('\\'), Some('\\'), Some(c)) => !c.is_whitespace(),
        (Some(drive), Some(':'), Some('\\' | '/')) => drive.is_ascii_alphabetic(),
        _ => false,
    }
}

/// The last part of `path` (its file or folder name), whatever its
/// separators.
fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("a folder")
}

/// `text` with every absolute path word shortened to its file name; the
/// punctuation around it stays.
fn without_paths(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            let core = word.trim_start_matches(OPENING);
            let core = core.trim_end_matches(CLOSING);
            if core.is_empty() || !is_absolute_path(core) {
                return word.to_owned();
            }
            let start = word.len() - word.trim_start_matches(OPENING).len();
            let end = start + core.len();
            format!("{}{}{}", &word[..start], file_name(core), &word[end..])
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The library error a job's stored error holds: the queue keeps
/// `e.to_string()`, `Error`'s `Display`, whose prefix names its kind; the
/// words after it are the error's message. A system error ("… (os error
/// N)") is `io` of that error. `None` when the text is none of these (the
/// queue's own words, a service error's message): the caller says it its
/// own way, never raw.
pub fn stored_error(stored: &str) -> Option<Error> {
    let stored = stored.trim();
    type Kind = fn(String) -> Error;
    let kinds: [(&str, Kind); 17] = [
        ("daemon unavailable: ", Error::DaemonUnavailable),
        ("not configured: ", Error::NotConfigured),
        ("consent required: ", Error::ConsentRequired),
        ("sign-in required: ", Error::ReauthRequired),
        ("MCP is turned off in BroScribe: ", Error::McpDisabled),
        ("library busy: ", Error::LibraryBusy),
        ("not found: ", Error::NotFound),
        ("already exists: ", Error::AlreadyExists),
        ("invalid input: ", Error::InvalidInput),
        ("invalid manifest: ", Error::InvalidManifest),
        ("unsupported: ", Error::Unsupported),
        ("model missing: ", Error::ModelMissing),
        ("schema error: ", Error::Schema),
        ("decode error: ", Error::Decode),
        ("inference error: ", Error::Inference),
        ("lease lost: ", Error::LeaseLost),
        ("internal error: ", Error::Internal),
    ];
    if let Some(error) = kinds.iter().find_map(|(prefix, kind)| {
        stored
            .strip_prefix(prefix)
            .map(|message| kind(message.to_owned()))
    }) {
        return Some(error);
    }
    let (_, number) = stored.trim_end_matches(')').rsplit_once("(os error ")?;
    let number = number.trim().parse().ok()?;
    Some(Error::io(
        String::new(),
        std::io::Error::from_raw_os_error(number),
    ))
}

/// A playback-cache job's stored error, which can contain the recording path.
pub(crate) fn playback_cache_failure(stored: Option<&str>) -> String {
    if matches!(stored, Some("worker crashed")) {
        return "The background worker stopped unexpectedly.".into();
    }
    stored.and_then(stored_error).map_or_else(
        || "The preparation job failed.".into(),
        |e| core_user_text(&e),
    )
}

impl std::fmt::Display for CliError {
    /// "{message} ({code})": for tracing, Copy details and Diagnostics, never
    /// for visible text ([`CliError::user_text`]).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// Why something BroScribe did itself failed (reading the library, the audio
/// player, a file), as one plain sentence without the code, like
/// [`CliError::user_text`]. Errors whose message can hold a path (a damaged
/// meeting's folder is named after its title) get fixed sentences; only
/// [`Error::InvalidInput`] and [`Error::Service`] keep their words, through
/// [`refusal_text`]: the advice stays, an absolute path never does (an
/// unreadable `config.toml` is a fixed sentence).
pub fn core_user_text(error: &Error) -> String {
    match error {
        Error::NotFound(_) => "It no longer exists.".to_owned(),
        Error::LibraryBusy(_) => "BroScribe is still working on it.".to_owned(),
        Error::InvalidInput(message) => refusal_text(message),
        // The library names a meeting folder that is already there by its
        // path: said in words.
        Error::AlreadyExists(_) => "It already exists.".to_owned(),
        Error::Service { message, .. } => refusal_text(message),
        Error::Io { source, .. } => io_user_text(source),
        Error::Schema(_) | Error::Decode(_) => DATA_UNREADABLE.to_owned(),
        Error::LeaseLost(_) => LEASE_LOST.to_owned(),
        Error::Inference(_) => INFERENCE_FAILED.to_owned(),
        Error::Internal(_) => INTERNAL.to_owned(),
        Error::ModelMissing(_) => "A model it needs isn't installed.".to_owned(),
        Error::DaemonUnavailable(_) => "BroScribe Agent isn't running.".to_owned(),
        Error::ConsentRequired(_) => "The provider isn't turned on.".to_owned(),
        Error::ReauthRequired(_) => "You need to sign in again.".to_owned(),
        Error::NotConfigured(_) => "It isn't set up yet.".to_owned(),
        Error::McpDisabled(_) => "MCP is turned off in BroScribe.".to_owned(),
        Error::Unsupported(_) => "This isn't supported here.".to_owned(),
        Error::InvalidManifest(_) => "The import manifest couldn't be read.".to_owned(),
        Error::AccountExists { .. } => "That account is already connected.".to_owned(),
    }
}

/// [`core_user_text`] for a meeting's own files: damaged data reads "this
/// meeting's data", never the path of its folder.
pub fn meeting_user_text(error: &Error) -> String {
    match error {
        Error::Schema(_) | Error::Decode(_) => {
            "BroScribe couldn't read this meeting's data.".to_owned()
        }
        _ => core_user_text(error),
    }
}

/// Why a file operation of BroScribe's own failed, in words: never the path
/// or the system's error number (they stay in the log).
pub fn io_user_text(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "It no longer exists.".to_owned(),
        std::io::ErrorKind::PermissionDenied => "BroScribe isn't allowed to open it.".to_owned(),
        _ => "BroScribe couldn't read or write a file.".to_owned(),
    }
}

/// `text` as a sentence of a message: trimmed, without a leading "invalid
/// input: ", the first letter in upper case and ending in a full stop (or
/// the `.`, `!` or `?` it already ends in). Empty text stays empty.
pub fn sentence(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix("invalid input: ").unwrap_or(text).trim();
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out: String = first.to_uppercase().chain(chars).collect();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// One failure line in the pattern "Couldn't <do X>. <Cause>. <Next step>."
/// (design §4.2 Plain failures): `what` ("Couldn't load people."), then the
/// cause ([`CliError::user_text`], [`core_user_text`]), then what to do next.
/// Empty parts are left out, so an unknown cause is never "Unknown error".
pub fn failure_text(what: &str, cause: &str, next: &str) -> String {
    [what, cause, next]
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

impl std::error::Error for CliError {}

/// One registered run: spawning, running or being reaped.
struct RunEntry {
    id: u64,
    kind: ChildKind,
    cancel: Arc<AtomicBool>,
    /// Told once the run is over (its tree killed and reaped, or never
    /// started).
    stopped: Vec<oneshot::Sender<()>>,
}

struct ChildTable {
    next_id: u64,
    runs: Vec<RunEntry>,
}

/// One-shot runs not finished yet, so setup ones can be cancelled on a
/// setup cancel or quit.
#[derive(Clone)]
pub struct CliChildren(Arc<Mutex<ChildTable>>);

impl Default for CliChildren {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(ChildTable {
            next_id: 0,
            runs: Vec::new(),
        })))
    }
}

impl CliChildren {
    fn lock(&self) -> MutexGuard<'_, ChildTable> {
        lock(&self.0)
    }

    /// Registers a run before its child is spawned, so a cancel from then
    /// on reaches it; the registration ends when the run is over.
    fn register(&self, kind: ChildKind, cancel: Arc<AtomicBool>) -> Registration {
        let mut table = self.lock();
        table.next_id += 1;
        let id = table.next_id;
        table.runs.push(RunEntry {
            id,
            kind,
            cancel,
            stopped: Vec::new(),
        });
        Registration {
            children: self.clone(),
            id,
        }
    }

    /// One-shot runs not over yet (spawning, running or being reaped).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.lock().runs.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Cancels every [`ChildKind::Setup`] run without waiting: each run's
    /// own thread then kills its process tree and reaps it, and its call
    /// ends with `cancelled` only after that. [`ChildKind::Command`] runs
    /// are never touched. Await [`ChildrenStopping::finished`] (bounded by
    /// the caller) when cleanup must follow the reaping.
    pub fn cancel_setup_children(&self) -> ChildrenStopping {
        let mut table = self.lock();
        let stopped = table
            .runs
            .iter_mut()
            .filter(|run| run.kind == ChildKind::Setup)
            .map(|run| {
                run.cancel.store(true, Ordering::SeqCst);
                let (tx, rx) = oneshot::channel();
                run.stopped.push(tx);
                rx
            })
            .collect();
        ChildrenStopping { stopped }
    }
}

/// The setup runs [`CliChildren::cancel_setup_children`] cancelled.
#[must_use = "await `finished` when cleanup must wait for the reaping"]
pub struct ChildrenStopping {
    stopped: Vec<oneshot::Receiver<()>>,
}

impl ChildrenStopping {
    /// How many runs were cancelled.
    #[cfg(all(test, unix))]
    pub(crate) fn count(&self) -> usize {
        self.stopped.len()
    }

    /// Resolves once every cancelled run's tree is killed and reaped. Never
    /// blocks a thread; bound it with a timer where quitting must not hang.
    pub async fn finished(self) {
        for stopped in self.stopped {
            // Either outcome means the run is over.
            let _ = stopped.await;
        }
    }
}

/// Removes its run from the table when the run is over, then tells whoever
/// waits for it.
struct Registration {
    children: CliChildren,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let entry = {
            let mut table = self.children.lock();
            let i = table.runs.iter().position(|run| run.id == self.id);
            i.map(|i| table.runs.swap_remove(i))
        };
        for stopped in entry.into_iter().flat_map(|run| run.stopped) {
            let _ = stopped.send(());
        }
    }
}

/// A child's stdout up to a limit; past it the rest is drained unread, so the
/// child never blocks on a full pipe, and the reply counts as too large.
struct Captured {
    bytes: Vec<u8>,
    overflow: bool,
}

fn read_capped(mut from: impl Read, max: usize) -> std::io::Result<Captured> {
    let mut bytes = Vec::new();
    let mut overflow = false;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match from.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) if overflow || bytes.len() + n > max => {
                overflow = true;
                bytes = Vec::new();
            }
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Captured { bytes, overflow })
}

/// Logs a failed one-shot command (but a cancel) by its operation name
/// ([`operation_name`]) and code only: never its argv, the CLI's words or
/// the app's own message, any of which may hold a value the user typed or
/// a path. Views show [`CliError::user_text`].
fn log_failure(args: &[OsString], result: &std::result::Result<serde_json::Value, CliError>) {
    let Err(error) = result else {
        return;
    };
    if error.code == "cancelled" {
        return;
    }
    let operation = operation_name(args);
    let operation = operation.as_str();
    let code = error.code.as_str();
    if error.from_cli {
        tracing::warn!(operation, code, "CLI command refused");
    } else {
        tracing::warn!(operation, code, "CLI command failed");
    }
}

fn cancelled_error() -> CliError {
    CliError::new("cancelled", "the command was cancelled")
}

fn configure_windows_maintenance_command(
    command: &mut Command,
    args: &[OsString],
    current_dir: impl FnOnce() -> Option<PathBuf>,
) {
    let replaces_installation = match args.first().and_then(|arg| arg.to_str()) {
        Some("uninstall") => true,
        Some("update") => matches!(
            args.get(1).and_then(|arg| arg.to_str()),
            Some("install" | "rollback" | "recover")
        ),
        _ => false,
    };
    if !replaces_installation {
        return;
    }
    let base_var = broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE;
    let cli = broscribe_platform::stable_path(Path::new(command.get_program()));
    if let Some(cwd) = current_dir()
        && let Some(root) = broscribe_platform::about::install_folder(&cli)
        && let Some(parent) = root.parent()
        && broscribe_platform::uninstall::path_within(&cwd, &root, true)
    {
        // The app will exit before the replacement. The CLI keeps the
        // original base for configured paths it needs to protect.
        command
            .current_dir(broscribe_platform::plain_verbatim(parent))
            .env(base_var, cwd);
    }
}

/// Runs one-shot `broscribe --library <lib> --json …` commands.
#[derive(Clone)]
pub struct CliRunner {
    cli: PathBuf,
    library: PathBuf,
    data_dir_override: Option<PathBuf>,
    admission: Arc<ModeLease>,
    children: CliChildren,
    tree: Arc<dyn ProcessTreeControl>,
    max_output: usize,
}

impl CliRunner {
    /// Setup and cancellable runs are contained with this platform's
    /// process-tree control (a process group on Unix, a Job Object on
    /// Windows).
    pub fn new(
        cli: PathBuf,
        library: PathBuf,
        paths: &ResolvedPaths,
        admission: Arc<ModeLease>,
    ) -> Result<Self> {
        admission.require_normal()?;
        if admission.data_dir() != paths.data_dir {
            return Err(Error::InvalidInput(
                "CLI profile does not match startup admission".into(),
            ));
        }
        Ok(Self {
            cli: std::path::absolute(cli).map_err(|e| Error::io("resolving CLI path", e))?,
            library: std::path::absolute(library)
                .map_err(|e| Error::io("resolving library path", e))?,
            data_dir_override: paths.data_dir_overridden.then(|| paths.data_dir.clone()),
            admission,
            children: CliChildren::default(),
            tree: NativePlatform::process_tree_control(),
            max_output: MAX_CLI_OUTPUT,
        })
    }

    fn profile_command(&self) -> Command {
        let mut command = Command::new(&self.cli);
        command.env_remove(broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE);
        if let Some(data_dir) = &self.data_dir_override {
            command.env(broscribe_platform::paths::ENV_DATA_DIR, data_dir);
        } else {
            command.env_remove(broscribe_platform::paths::ENV_DATA_DIR);
        }
        command
    }

    fn command(&self, args: &[OsString], json: bool) -> Command {
        let mut command = self.profile_command();
        command.arg("--library").arg(&self.library);
        if json {
            command.arg("--json");
        }
        command.args(args);
        if NativePlatform::host_platform() == broscribe_platform::HostPlatform::Windows {
            configure_windows_maintenance_command(&mut command, args, || {
                std::env::current_dir().ok()
            });
        }
        command
    }

    pub fn admission(&self) -> Arc<ModeLease> {
        self.admission.clone()
    }

    /// Background work keeps admission until it actually completes, including
    /// work whose foreground view or task has already gone away.
    pub(crate) fn spawn<T: Send + 'static>(
        &self,
        executor: &BackgroundExecutor,
        work: impl Future<Output = T> + Send + 'static,
    ) -> Task<T> {
        let admission = self.admission.clone();
        executor.spawn(async move {
            let _admission = admission;
            work.await
        })
    }
    /// A smaller stdout limit, so tests overflow it without large output.
    #[cfg(test)]
    pub(crate) fn set_output_limit(&mut self, bytes: usize) {
        self.max_output = bytes;
    }

    pub fn cli(&self) -> &Path {
        &self.cli
    }

    pub fn children(&self) -> &CliChildren {
        &self.children
    }

    /// Runs `broscribe --library <lib> --json <args…>` on the background
    /// executor and returns its JSON output. `args` must be flags and paths
    /// only, never sensitive text.
    pub fn run(
        &self,
        args: Vec<OsString>,
        kind: ChildKind,
        executor: &BackgroundExecutor,
    ) -> Task<std::result::Result<serde_json::Value, CliError>> {
        let this = self.clone();
        executor.spawn(async move { this.run_blocking(&args, kind) })
    }

    /// Sends sensitive input through a pipe, never through argv or logs.
    pub fn run_with_stdin(
        &self,
        args: Vec<OsString>,
        kind: ChildKind,
        stdin: Vec<u8>,
        executor: &BackgroundExecutor,
    ) -> Task<std::result::Result<serde_json::Value, CliError>> {
        let this = self.clone();
        executor.spawn(async move { this.run_blocking_stdin(&args, kind, stdin) })
    }

    /// [`Self::run_blocking`] as a contained process tree that setting
    /// `cancelled` (from any thread, without waiting) stops: the run kills
    /// this tree only, reaps it and only then returns `cancelled`. A flag
    /// set before the spawn starts nothing. A child that exited before the
    /// cancel was seen returns its real result: callers decide by their own
    /// flag. Use it for setup work and read-only commands, never for
    /// ordinary writes that may already have committed.
    pub fn run_blocking_cancellable(
        &self,
        args: &[OsString],
        kind: ChildKind,
        cancelled: Arc<AtomicBool>,
    ) -> std::result::Result<serde_json::Value, CliError> {
        self.run_blocking_guarded(args, kind, None, Some(cancelled), None::<fn(&[u8])>)
    }

    /// A cancellable contained run with bounded stderr lines delivered on
    /// their own reader thread, concurrently with capped stdout.
    pub(crate) fn run_blocking_cancellable_progress(
        &self,
        args: &[OsString],
        kind: ChildKind,
        cancelled: Arc<AtomicBool>,
        on_stderr: impl FnMut(&[u8]) + Send + 'static,
    ) -> std::result::Result<serde_json::Value, CliError> {
        self.run_blocking_guarded(args, kind, None, Some(cancelled), Some(on_stderr))
    }

    /// [`Self::run_blocking_cancellable`] with sensitive input on stdin (a
    /// spec or query, never in argv or logs). A child that stops reading it
    /// still ends on cancel.
    pub fn run_blocking_cancellable_stdin(
        &self,
        args: &[OsString],
        kind: ChildKind,
        stdin: Vec<u8>,
        cancelled: Arc<AtomicBool>,
    ) -> std::result::Result<serde_json::Value, CliError> {
        self.run_blocking_guarded(args, kind, Some(stdin), Some(cancelled), None::<fn(&[u8])>)
    }

    /// [`Self::run`] on the calling thread (blocks until the child exits).
    pub fn run_blocking(
        &self,
        args: &[OsString],
        kind: ChildKind,
    ) -> std::result::Result<serde_json::Value, CliError> {
        self.run_blocking_guarded(args, kind, None, None, None::<fn(&[u8])>)
    }

    /// [`Self::run_with_stdin`] on the calling thread.
    pub fn run_blocking_stdin(
        &self,
        args: &[OsString],
        kind: ChildKind,
        stdin: Vec<u8>,
    ) -> std::result::Result<serde_json::Value, CliError> {
        self.run_blocking_guarded(args, kind, Some(stdin), None, None::<fn(&[u8])>)
    }

    /// [`Self::run_blocking_unlogged`], whose failure (but a cancel) goes to
    /// the log ([`log_failure`]): views show only [`CliError::user_text`].
    fn run_blocking_guarded(
        &self,
        args: &[OsString],
        kind: ChildKind,
        input: Option<Vec<u8>>,
        cancelled: Option<Arc<AtomicBool>>,
        on_stderr: Option<impl FnMut(&[u8]) + Send + 'static>,
    ) -> std::result::Result<serde_json::Value, CliError> {
        let result = self.run_blocking_unlogged(args, kind, input, cancelled, on_stderr);
        log_failure(args, &result);
        result
    }

    /// Setup runs and runs with a cancel flag are contained process trees;
    /// ordinary commands are not (a Job Object would end them with the app,
    /// and they are left to finish). Stdout is read up to the limit on its
    /// own thread and stdin written on another, so neither pipe can block
    /// the wait. Requested stderr progress uses the same bounded reader.
    fn run_blocking_unlogged(
        &self,
        args: &[OsString],
        kind: ChildKind,
        input: Option<Vec<u8>>,
        cancelled: Option<Arc<AtomicBool>>,
        on_stderr: Option<impl FnMut(&[u8]) + Send + 'static>,
    ) -> std::result::Result<serde_json::Value, CliError> {
        let contained = kind == ChildKind::Setup || cancelled.is_some();
        let cancel = cancelled.unwrap_or_default();
        let _registration = self.children.register(kind, Arc::clone(&cancel));
        if cancel.load(Ordering::SeqCst) {
            return Err(cancelled_error());
        }
        let mut command = self.command(args, true);
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(if on_stderr.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
        let contain_error = |e: Error| CliError::new("spawn", format!("containing the CLI: {e}"));
        if contained {
            self.tree.prepare(&mut command).map_err(contain_error)?;
        }
        let mut child = command
            .spawn()
            .map_err(|e| CliError::new("spawn", format!("{}: {e}", self.cli.display())))?;
        let mut tree = if contained {
            match self.tree.attach(&child) {
                Ok(tree) => Some(tree),
                Err(e) => {
                    // Not contained: stop it before it runs (still suspended
                    // on Windows) and reap it.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(contain_error(e));
                }
            }
        } else {
            None
        };

        let out_rx =
            spawn_output_reader(child.stdout.take(), self.max_output, self.admission.clone());
        let err_rx = on_stderr.map(|on_stderr| {
            spawn_stderr_reader(child.stderr.take(), on_stderr, self.admission.clone())
        });
        let delivered = match (input, child.stdin.take()) {
            (Some(input), Some(mut pipe)) => {
                let (done_tx, done_rx) = std_mpsc::channel();
                let admission = self.admission.clone();
                let writer =
                    std::thread::Builder::new()
                        .name("cli-input".into())
                        .spawn(move || {
                            let _admission = admission;
                            let ok = pipe.write_all(&input).and_then(|()| pipe.flush()).is_ok();
                            // Closed before reporting, so the child sees its end.
                            drop(pipe);
                            let _ = done_tx.send(ok);
                        });
                if let Err(e) = writer {
                    tracing::warn!("no CLI input writer: {e}");
                }
                Some(done_rx)
            }
            _ => None,
        };

        let exit = match tree.as_mut() {
            Some(tree) => wait_tree(tree.as_mut(), &mut child, None, Some(&*cancel))
                .map_err(|e| CliError::new("io", format!("waiting for the CLI: {e}")))?,
            None => TreeExit::Exited(
                child
                    .wait()
                    .map_err(|e| CliError::new("io", format!("waiting for the CLI: {e}")))?,
            ),
        };
        let drain_until = Instant::now() + OUTPUT_DRAIN_GRACE;
        let status = match exit {
            TreeExit::Exited(status) => status,
            TreeExit::Cancelled => {
                let _ = receive_output(&out_rx);
                if let Some(done) = &err_rx {
                    let _ =
                        done.recv_timeout(drain_until.saturating_duration_since(Instant::now()));
                }
                return Err(cancelled_error());
            }
            TreeExit::TimedOut => return Err(CliError::new("io", "the CLI timed out")),
        };
        let captured = receive_output(&out_rx)?;
        if let Some(done) = &err_rx {
            let _ = done.recv_timeout(drain_until.saturating_duration_since(Instant::now()));
        }
        let input_read = delivered.is_none_or(|done| {
            done.recv_timeout(drain_until.saturating_duration_since(Instant::now()))
                .unwrap_or(false)
        });
        if captured.overflow {
            return Err(self.output_too_large());
        }
        let result = cli_result(status, &captured.bytes);
        if result.is_ok() && !input_read {
            return Err(CliError::new("io", "the CLI did not read all of its input"));
        }
        result
    }

    fn output_too_large(&self) -> CliError {
        CliError::new(
            "output_too_large",
            format!("the CLI printed more than {} bytes", self.max_output),
        )
    }

    /// Runs `broscribe --library <lib> --json <args…>`, a command its
    /// caller ends cooperatively through stdin: the open pipe goes to
    /// `stdin` (closing it is the command's own cancellation, as for
    /// `models pull --cancel-on-stdin-eof`). Each stderr line up to
    /// [`MAX_PROGRESS_LINE`] goes to `on_stderr` on its own thread (longer
    /// ones are skipped whole) while stdout is read up to the output limit
    /// on another, so neither pipe can fill and block the child. The child
    /// is never killed: the result is its own answer once it exited, its
    /// readers given [`OUTPUT_DRAIN_GRACE`].
    ///
    /// A failure (but a cancel) is logged as for every one-shot command
    /// ([`log_failure`]).
    pub(crate) fn run_blocking_cooperative(
        &self,
        args: &[OsString],
        stdin: impl FnOnce(ChildStdin),
        on_stderr: impl FnMut(&[u8]) + Send + 'static,
    ) -> std::result::Result<serde_json::Value, CliError> {
        let result = self.run_blocking_cooperative_unlogged(args, stdin, on_stderr);
        log_failure(args, &result);
        result
    }

    fn run_blocking_cooperative_unlogged(
        &self,
        args: &[OsString],
        stdin: impl FnOnce(ChildStdin),
        on_stderr: impl FnMut(&[u8]) + Send + 'static,
    ) -> std::result::Result<serde_json::Value, CliError> {
        let mut child = self
            .command(args, true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CliError::new("spawn", format!("{}: {e}", self.cli.display())))?;
        let out_rx =
            spawn_output_reader(child.stdout.take(), self.max_output, self.admission.clone());
        let err_rx = spawn_stderr_reader(child.stderr.take(), on_stderr, self.admission.clone());
        if let Some(pipe) = child.stdin.take() {
            stdin(pipe);
        }
        let status = child
            .wait()
            .map_err(|e| CliError::new("io", format!("waiting for the CLI: {e}")))?;
        let drain_until = Instant::now() + OUTPUT_DRAIN_GRACE;
        let captured = receive_output(&out_rx)?;
        // Its last progress lines land before the answer (bounded wait).
        let _ = err_rx.recv_timeout(drain_until.saturating_duration_since(Instant::now()));
        if captured.overflow {
            return Err(self.output_too_large());
        }
        cli_result(status, &captured.bytes)
    }
}

/// The shared bounded stderr reader for cooperative and contained runs.
fn spawn_stderr_reader(
    stderr: Option<std::process::ChildStderr>,
    mut on_stderr: impl FnMut(&[u8]) + Send + 'static,
    admission: Arc<ModeLease>,
) -> std_mpsc::Receiver<()> {
    let (done, received) = std_mpsc::channel();
    if let Some(stderr) = stderr {
        let reader = std::thread::Builder::new()
            .name("cli-stderr".into())
            .spawn(move || {
                let _admission = admission;
                let read = for_each_line(stderr, MAX_PROGRESS_LINE, |line| match line {
                    Line::Text(bytes) => on_stderr(bytes),
                    Line::TooLong(bytes) => tracing::debug!(
                        bytes,
                        limit = MAX_PROGRESS_LINE,
                        "skipped an over-long CLI stderr line"
                    ),
                });
                if let Err(e) = read {
                    tracing::warn!(kind = ?e.kind(), "reading CLI stderr failed");
                }
                let _ = done.send(());
            });
        if let Err(e) = reader {
            tracing::warn!("no CLI stderr reader: {e}");
        }
    }
    received
}

/// Reads `stdout` up to `max` bytes on its own thread ([`read_capped`]);
/// the receiver gets what it read once the pipe closed.
fn spawn_output_reader(
    stdout: Option<ChildStdout>,
    max: usize,
    admission: Arc<ModeLease>,
) -> std_mpsc::Receiver<std::io::Result<Captured>> {
    let (out_tx, out_rx) = std_mpsc::channel();
    match stdout {
        Some(stdout) => {
            let reader = std::thread::Builder::new()
                .name("cli-output".into())
                .spawn(move || {
                    let _admission = admission;
                    let _ = out_tx.send(read_capped(stdout, max));
                });
            // Without a reader its pipe is closed (dropped with the
            // closure): the child sees a broken pipe, never a full one.
            if let Err(e) = reader {
                tracing::warn!("no CLI output reader: {e}");
            }
        }
        None => {
            let _ = out_tx.send(Ok(Captured {
                bytes: Vec::new(),
                overflow: false,
            }));
        }
    }
    out_rx
}

/// What [`spawn_output_reader`] read, waiting at most
/// [`OUTPUT_DRAIN_GRACE`] after the child exited.
fn receive_output(
    out_rx: &std_mpsc::Receiver<std::io::Result<Captured>>,
) -> std::result::Result<Captured, CliError> {
    match out_rx.recv_timeout(OUTPUT_DRAIN_GRACE) {
        Ok(Ok(captured)) => Ok(captured),
        Ok(Err(e)) => Err(CliError::new("io", format!("reading CLI output: {e}"))),
        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
            Err(CliError::new("io", "the CLI output could not be read"))
        }
        Err(std_mpsc::RecvTimeoutError::Timeout) => Err(CliError::new(
            "io",
            "the CLI exited but its output stayed open",
        )),
    }
}

/// Every `broscribe` command and its subcommands, as the CLI names them
/// (`broscribe-cli` `cli.rs`): the only words of argv the log may name.
const OPERATIONS: &[(&str, &[&str])] = &[
    ("actions", &["set-status"]),
    ("agents", &["status", "install", "uninstall"]),
    (
        "app",
        &[
            "open-at-login",
            "about",
            "reading-size",
            "live-identity",
            "uncalibrated-me-suggestions",
        ],
    ),
    ("audio", &["devices", "pin"]),
    ("audio-check", &[]),
    ("bench", &[]),
    (
        "daemon",
        &["status", "start", "stop", "install", "uninstall"],
    ),
    ("decisions", &[]),
    ("delete", &[]),
    ("detect", &["status", "policy"]),
    ("diagnostics", &[]),
    ("doctor", &[]),
    ("enrich", &["instructions"]),
    ("export", &[]),
    ("identity", &["calibrate", "re-resolve", "status"]),
    ("import", &[]),
    ("index", &["rebuild"]),
    (
        "integrations",
        &[
            "accounts",
            "add-account",
            "reconnect-account",
            "remove-account",
            "discard-legacy",
            "calendars",
            "select-calendars",
            "remove-calendar",
            "sync-account",
            "grant-account",
            "setup",
            "login",
            "cancel-login",
            "login-attempt",
            "status",
            "logout",
            "sync",
        ],
    ),
    ("library", &["check", "repair"]),
    ("maintenance", &["status", "enable", "disable"]),
    ("mcp", &["enable", "disable", "status"]),
    ("meetings", &[]),
    ("models", &["list", "pull", "use", "doctor"]),
    (
        "people",
        &[
            "list",
            "show",
            "lookup",
            "photo",
            "forget",
            "forget-voice",
            "edit",
            "undo-edit",
        ],
    ),
    (
        "providers",
        &[
            "list",
            "enable",
            "disable",
            "test",
            "set-key",
            "delete-key",
            "models",
        ],
    ),
    ("queue", &["work", "status", "discard-setup", "prune"]),
    ("readers", &[]),
    (
        "record",
        &[
            "start",
            "stop",
            "status",
            "retry",
            "pause",
            "resume",
            "lang",
            "bookmark",
            "note",
            "tag",
            "participant",
            "title",
            "discard",
            "process-now",
            "auto-stop",
            "live",
            "live-attribute",
        ],
    ),
    ("reprocess", &[]),
    (
        "retention",
        &["apply", "policy", "keep", "delete-now", "explain"],
    ),
    ("search", &[]),
    ("setup", &["voice", "try"]),
    ("show", &[]),
    (
        "speakers",
        &[
            "review",
            "samples",
            "dismiss",
            "restore",
            "name",
            "reject",
            "matches",
            "merge",
            "unmerge",
            "not-same",
            "duplicates",
            "live-review",
            "live-dismiss",
        ],
    ),
    ("tag", &[]),
    (
        "templates",
        &[
            "list", "show", "new", "edit", "delete", "reset", "auto", "run", "result",
        ],
    ),
    ("title", &[]),
    ("transcript", &["attribute"]),
    (
        "update",
        &[
            "status",
            "check",
            "download",
            "discard",
            "install",
            "confirm",
            "rollback",
            "admission",
            "recover",
            "settings",
            "release-key",
            "sign-manifest",
        ],
    ),
    ("uninstall", &[]),
    ("version", &[]),
];

/// The operation `args` run, for the log ("people edit", "templates new"):
/// a known command and subcommand ([`OPERATIONS`]), never another word of
/// argv, so no id, title, path or value; "command" when the command is not
/// known.
fn operation_name(args: &[OsString]) -> String {
    let mut words = args.iter().map(|arg| arg.to_str().unwrap_or_default());
    let Some(&(command, subcommands)) = words
        .next()
        .and_then(|first| OPERATIONS.iter().find(|(name, _)| *name == first))
    else {
        return "command".to_owned();
    };
    match words
        .next()
        .and_then(|second| subcommands.iter().find(|name| **name == second))
    {
        Some(subcommand) => format!("{command} {subcommand}"),
        None => command.to_owned(),
    }
}

/// Interprets a `--json` command's exit status and stdout.
fn cli_result(status: ExitStatus, out: &[u8]) -> std::result::Result<serde_json::Value, CliError> {
    let value: Option<serde_json::Value> = serde_json::from_slice(out).ok();
    if let Some(error) = value.as_ref().and_then(|v| v.get("error")) {
        let code = error["code"].as_str().unwrap_or("internal");
        let message = error["message"].as_str().unwrap_or("the command failed");
        return Err(CliError::answered(code, message));
    }
    if !status.success()
        && !value
            .as_ref()
            .is_some_and(|report| report["uninstall_report"] == true)
    {
        return Err(CliError::new(
            "exit",
            format!("the CLI exited with {status}"),
        ));
    }
    value.ok_or_else(|| CliError::new("bad_output", "the CLI printed no JSON"))
}

/// Why `path` cannot go to a child in a `--spec-stdin` JSON spec (its name
/// is not valid UTF-8): it `can't be <verb>` ("used", "imported"), with the
/// way out. Shown to the user only; the path itself never goes in argv.
pub(crate) fn unsendable_path(path: &Path, verb: &str) -> String {
    format!(
        "{} can't be {verb}: its name isn't valid UTF-8. Rename it, then try again.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::queue::JobState;
    use broscribe_platform::fakes::FakeProcessProbe;

    #[test]
    fn settings_reads_the_report_from_a_blocked_uninstall_preview() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt as _;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt as _;
        #[cfg(unix)]
        let status = ExitStatus::from_raw(2 << 8);
        #[cfg(windows)]
        let status = ExitStatus::from_raw(2);
        let report = br#"{"uninstall_report":true,"dry_run":true,"ready":false,"steps":[]}"#;
        assert_eq!(cli_result(status, report).unwrap()["ready"], false);
    }

    /// WRT-04: every kind of failure has words of its own; none shows its
    /// code, the CLI's path or a raw exit status. `Display` keeps the code
    /// for the log, Copy details and Diagnostics.
    #[test]
    fn user_text_says_why_in_words_for_every_kind() {
        let tool_stopped = "BroScribe's command-line tool stopped unexpectedly.";
        for (code, message, words) in [
            (
                "spawn",
                "/Applications/BroScribe.app/Contents/MacOS/broscribe: No such file or directory (os error 2)",
                "BroScribe's command-line tool couldn't start.",
            ),
            (
                "spawn",
                "containing the CLI: denied",
                "BroScribe's command-line tool couldn't start.",
            ),
            ("io", "the CLI timed out", tool_stopped),
            ("io", "waiting for the CLI: interrupted", tool_stopped),
            ("io", "the CLI output could not be read", tool_stopped),
            ("exit", "the CLI exited with exit status: 1", tool_stopped),
            ("bad_output", "the CLI printed no JSON", tool_stopped),
            (
                "output_too_large",
                "the CLI printed more than 64 MiB",
                tool_stopped,
            ),
            (
                "cancelled",
                "the command was cancelled",
                "It was cancelled.",
            ),
            (
                "not_found",
                "not found: meeting m-00000a",
                "It no longer exists.",
            ),
            (
                "busy",
                "capture device busy",
                "BroScribe is still working on it.",
            ),
            (
                "job_running",
                "a job is working on m-00000a",
                "BroScribe is still working on it.",
            ),
            (
                "library_busy",
                "library busy: another process is writing to /synthetic/library",
                "BroScribe is still working on it.",
            ),
            // A refusal keeps the CLI's own words: capitalised, without the
            // "invalid input: " prefix, ending in a full stop.
            (
                "invalid_input",
                "invalid input: several people are called that",
                "Several people are called that.",
            ),
            (
                "stale_person",
                "the person changed since it was read.",
                "The person changed since it was read.",
            ),
            (
                "consent_required",
                "Is the provider enabled?",
                "Is the provider enabled?",
            ),
            // Nothing known: the cause is left out, never "Unknown error".
            ("internal", "  ", ""),
        ] {
            let error = CliError::new(code, message);
            let text = error.user_text();
            assert_eq!(text, words, "{code}: {message}");
            assert!(!text.contains(&format!("({code})")), "{text}");
            assert!(
                !text.contains("CLI") && !text.contains("os error"),
                "{text}"
            );
            assert_eq!(error.to_string(), format!("{message} ({code})"));
        }
    }

    #[test]
    fn failure_lines_follow_one_pattern() {
        assert_eq!(
            failure_text(
                "Couldn't load people.",
                &CliError::new("exit", "the CLI exited with exit status: 1").user_text(),
                "Try again.",
            ),
            "Couldn't load people. BroScribe's command-line tool stopped unexpectedly. Try again."
        );
        // An unknown cause and a missing next step are left out.
        assert_eq!(
            failure_text(
                "Couldn't search.",
                &CliError::new("internal", "").user_text(),
                ""
            ),
            "Couldn't search."
        );
        assert_eq!(sentence("  invalid input: bad id "), "Bad id.");
        assert_eq!(sentence("Already one?"), "Already one?");
        assert_eq!(sentence(""), "");
        assert_eq!(sentence("\u{e9}t\u{e9} done"), "\u{c9}t\u{e9} done.");
    }

    /// In-process failures read like the CLI's: words, never a path or code.
    #[test]
    fn local_failures_say_why_in_words() {
        assert_eq!(
            core_user_text(&Error::NotFound("meeting m-00000a".into())),
            "It no longer exists."
        );
        assert_eq!(
            core_user_text(&Error::LibraryBusy("/synthetic/library".into())),
            "BroScribe is still working on it."
        );
        assert_eq!(
            core_user_text(&Error::InvalidInput("the name is too long".into())),
            "The name is too long."
        );
        let denied = Error::io(
            "/synthetic/library/meetings",
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert_eq!(
            core_user_text(&denied),
            "BroScribe isn't allowed to open it."
        );
        let other = Error::io("/synthetic/file", std::io::Error::other("disk on fire"));
        assert_eq!(
            core_user_text(&other),
            "BroScribe couldn't read or write a file."
        );
        assert_eq!(
            io_user_text(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            "It no longer exists."
        );
    }

    /// WRT-04: errors whose message can name a path (a meeting folder is
    /// named after its title) get fixed sentences; only invalid input and a
    /// service failure keep their words.
    #[test]
    fn core_errors_that_can_hold_a_path_get_fixed_sentences() {
        let path = "/synthetic/library/2026/Private title m-00000a/meeting.json";
        for (error, words) in [
            (
                Error::Schema(format!("{path}: missing field")),
                "BroScribe couldn't read its library data.",
            ),
            (
                Error::Decode(format!("{path}: bad JSON")),
                "BroScribe couldn't read its library data.",
            ),
            (
                Error::Internal(format!("{path}: unexpected")),
                "Something went wrong inside BroScribe.",
            ),
            (
                Error::LeaseLost(path.to_owned()),
                "Another BroScribe process took over this work.",
            ),
            (
                Error::Inference(path.to_owned()),
                "A model on this computer couldn't finish.",
            ),
            (
                Error::ModelMissing(path.to_owned()),
                "A model it needs isn't installed.",
            ),
            (
                Error::Unsupported(path.to_owned()),
                "This isn't supported here.",
            ),
            (
                Error::InvalidManifest(path.to_owned()),
                "The import manifest couldn't be read.",
            ),
            (Error::NotFound(path.to_owned()), "It no longer exists."),
            (
                Error::LibraryBusy(path.to_owned()),
                "BroScribe is still working on it.",
            ),
            (
                Error::io(path, std::io::Error::other(path)),
                "BroScribe couldn't read or write a file.",
            ),
            (
                Error::DaemonUnavailable(path.to_owned()),
                "BroScribe Agent isn't running.",
            ),
            (
                Error::ConsentRequired(path.to_owned()),
                "The provider isn't turned on.",
            ),
            (
                Error::ReauthRequired(path.to_owned()),
                "You need to sign in again.",
            ),
            (
                Error::NotConfigured(path.to_owned()),
                "It isn't set up yet.",
            ),
            (
                Error::McpDisabled(path.to_owned()),
                "MCP is turned off in BroScribe.",
            ),
            (Error::AlreadyExists(path.to_owned()), "It already exists."),
            (
                Error::AccountExists {
                    account_id: broscribe_core::calendar::AccountId::parse(
                        "0123456789abcdef0123456789abcdef",
                    )
                    .unwrap(),
                },
                "That account is already connected.",
            ),
        ] {
            let text = core_user_text(&error);
            assert_eq!(text, words, "{error:?}");
            assert!(!text.contains("/synthetic") && !text.contains(error.code()));
        }
        // A meeting's own data says so.
        assert_eq!(
            meeting_user_text(&Error::Schema(path.to_owned())),
            "BroScribe couldn't read this meeting's data."
        );
        assert_eq!(
            meeting_user_text(&Error::Decode(path.to_owned())),
            "BroScribe couldn't read this meeting's data."
        );
        assert_eq!(
            meeting_user_text(&Error::NotFound(path.to_owned())),
            "It no longer exists."
        );
        // Words written for the user stay.
        assert_eq!(
            core_user_text(&Error::InvalidInput("the title is empty".into())),
            "The title is empty."
        );
        assert_eq!(
            core_user_text(&Error::Service {
                code: "secret_store_timeout",
                message: "the keychain didn't answer".into(),
            }),
            "The keychain didn't answer."
        );
    }

    /// WRT-04: `InvalidInput` and refusals keep their advice, never a value
    /// naming the user's folders or quoting a file. The real library's
    /// errors for a malformed `config.toml` (its parser's excerpt) and an
    /// invalid one start with the file's absolute path: a fixed sentence,
    /// whoever reports them (the app, the CLI, the agent).
    #[test]
    fn refusals_keep_their_advice_without_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("synthetic-private-zq6");
        std::fs::create_dir(&data).unwrap();
        let config = broscribe_core::config::Config::path(&data);
        std::fs::write(&config, "[user]\nemails = [\"zq6@example.invalid\"\n").unwrap();
        let malformed = broscribe_core::config::Config::load(&data).unwrap_err();
        assert!(
            matches!(&malformed, Error::InvalidInput(m) if m.contains("zq6@example.invalid")),
            "{malformed:?}"
        );
        let private = data.to_string_lossy().into_owned();
        let file = |path: &str, why: &str| format!("{private}/{path}: {why}");
        for (text, words) in [
            (core_user_text(&malformed), SETTINGS_FILE_UNREADABLE),
            (
                CliError::answered("invalid_input", malformed.to_string()).user_text(),
                SETTINGS_FILE_UNREADABLE,
            ),
            (
                refusal_text(&file(
                    "config.toml",
                    "[identity.\"zq6\"] threshold out of range",
                )),
                SETTINGS_FILE_UNREADABLE,
            ),
            // A template file is named by its file name only, its case kept.
            (
                refusal_text(&format!(
                    "invalid input: {}",
                    file("templates/BANT-zq6.md", "the body is empty")
                )),
                "BANT-zq6.md: the body is empty.",
            ),
            // Any other absolute path in the words is its file name.
            (
                refusal_text(&format!(
                    "invalid input: the folder ({private}/inbox) is not a library"
                )),
                "The folder (inbox) is not a library.",
            ),
            (
                refusal_text("invalid input: C:\\Users\\zq6\\Recordings\\a.wav is not audio"),
                "a.wav is not audio.",
            ),
            (
                refusal_text("~/synthetic-private-zq6/b.wav is not audio"),
                "b.wav is not audio.",
            ),
            // A ratio or a date is not a path.
            (
                refusal_text("invalid input: use 1/2 or 2026/11/22"),
                "Use 1/2 or 2026/11/22.",
            ),
        ] {
            assert_eq!(text, words);
            for raw in [
                private.as_str(),
                "synthetic-private",
                "zq6@",
                "TOML",
                "line 2",
                "invalid input",
                "\\Users",
            ] {
                assert!(!text.contains(raw), "{raw} shown: {text}");
            }
        }
    }

    /// The log names a known operation, never another word of argv: no
    /// value, id or path.
    #[test]
    fn the_failure_log_names_only_a_known_operation() {
        let args = |words: &[&str]| words.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            operation_name(&args(&["people", "edit", "--spec-stdin", "p-1"])),
            "people edit"
        );
        // A lowercase template id is a value, not a subcommand.
        assert_eq!(
            operation_name(&args(&["templates", "new", "private-project"])),
            "templates new"
        );
        assert_eq!(
            operation_name(&args(&["templates", "private-project"])),
            "templates"
        );
        assert_eq!(
            operation_name(&args(&[
                "models",
                "pull",
                "synthetic-model",
                "--cancel-on-stdin-eof"
            ])),
            "models pull"
        );
        assert_eq!(operation_name(&args(&["show", "m-00000a"])), "show");
        assert_eq!(
            operation_name(&args(&["retention", "policy", "days:30"])),
            "retention policy"
        );
        assert_eq!(
            operation_name(&args(&["/synthetic/file.wav", "import"])),
            "command"
        );
        assert_eq!(operation_name(&args(&["private-project"])), "command");
        assert_eq!(operation_name(&[]), "command");
    }

    /// WRT-04: an `io` the CLI answered with is a file (or, for a model
    /// pull, the download) in words, never \"the tool stopped\"; the CLI's
    /// damaged-data, internal and inference errors get fixed sentences
    /// without their path.
    #[test]
    fn a_cli_answer_is_told_apart_from_the_childs_own_failure() {
        let tool_stopped = "BroScribe's command-line tool stopped unexpectedly.";
        let disk_full = CliError::answered(
            "io",
            "/synthetic/private-library/meetings/Private title/audio.wav: No space left on device (os error 28)",
        );
        assert!(disk_full.from_cli);
        assert_eq!(
            disk_full.user_text(),
            "BroScribe couldn't read or write a file."
        );
        let checksum = CliError::answered("io", "checksum mismatch for /synthetic/models/x.onnx");
        assert_eq!(checksum.download_text(), "The download didn't finish.");
        assert_ne!(checksum.user_text(), tool_stopped);
        // The app's own failure around the child keeps its words.
        let timed_out = CliError::new("io", "the CLI timed out");
        assert!(!timed_out.from_cli);
        assert_eq!(timed_out.user_text(), tool_stopped);
        assert_eq!(timed_out.download_text(), tool_stopped);
        for (code, message, words) in [
            (
                "schema",
                "schema error: /synthetic/Private title/meeting.json: missing field",
                "BroScribe couldn't read its library data.",
            ),
            (
                "decode",
                "decode error: /synthetic/Private title/transcript.json",
                "BroScribe couldn't read its library data.",
            ),
            (
                "internal",
                "internal error: /synthetic/private",
                "Something went wrong inside BroScribe.",
            ),
            (
                "lease_lost",
                "lease lost: /synthetic/private",
                "Another BroScribe process took over this work.",
            ),
            (
                "inference",
                "inference error: /synthetic/private",
                "A model on this computer couldn't finish.",
            ),
            ("internal", "", ""),
            (
                "invalid_input",
                "invalid input: the title is empty",
                "The title is empty.",
            ),
        ] {
            let text = CliError::answered(code, message).user_text();
            assert_eq!(text, words, "{code}");
            assert!(
                !text.contains("/synthetic") && !text.contains(code),
                "{text}"
            );
        }
    }

    /// The real answer of a child: its JSON error is the CLI's, whatever
    /// the exit status; no JSON at all is the app's `exit`.
    #[test]
    fn cli_result_marks_what_the_cli_answered() {
        let dir = tempfile::tempdir().unwrap();
        let answer = r#"{"error":{"code":"io","message":"/synthetic/private: denied"}}"#;
        let cli = crate::test_support::write_cli(
            &dir.path().join("cli"),
            &format!(
                "case \"$*\" in *answer*) printf '%s' '{answer}'; exit 1;; *) exit 3;; esac\n"
            ),
            &format!(
                "if($all -clike '*answer*'){{fx_print {};exit 1}};exit 3",
                crate::test_support::ps_str(answer)
            ),
        );
        let runner = crate::test_support::runner(cli, dir.path().join("lib"));
        let answered = runner
            .run_blocking(&["show".into(), "answer".into()], ChildKind::Command)
            .unwrap_err();
        assert!(answered.from_cli);
        assert_eq!(
            answered.user_text(),
            "BroScribe couldn't read or write a file."
        );
        let crashed = runner
            .run_blocking(&["show".into(), "crash".into()], ChildKind::Command)
            .unwrap_err();
        assert_eq!(crashed.code, "exit");
        assert!(!crashed.from_cli);
        assert_eq!(
            crashed.user_text(),
            "BroScribe's command-line tool stopped unexpectedly."
        );
    }

    /// Every failed one-shot command reaches the log by operation and code
    /// only: a lowercase template id, the CLI's words and a private path in
    /// them never do. A cancel is not logged.
    #[test]
    fn failed_commands_are_logged_by_operation_and_code_only() {
        let dir = tempfile::tempdir().unwrap();
        let answer =
            r#"{"error":{"code":"io","message":"/synthetic/private-folder/notes.md: denied"}}"#;
        let cli = crate::test_support::write_cli(
            &dir.path().join("cli"),
            &format!(
                "case \"$*\" in *private-project*) printf '%s' '{answer}'; exit 1;; *) exit 4;; esac\n"
            ),
            &format!(
                "if($all -clike '*private-project*'){{fx_print {};exit 1}};exit 4",
                crate::test_support::ps_str(answer)
            ),
        );
        let runner = crate::test_support::runner(cli, dir.path().join("lib"));
        let (log, _guard) = crate::test_support::LogCapture::start();
        let refused = runner.run_blocking(
            &["templates".into(), "new".into(), "private-project".into()],
            ChildKind::Command,
        );
        assert_eq!(refused.unwrap_err().code, "io");
        let failed = runner.run_blocking(
            &["templates".into(), "edit".into(), "other-private".into()],
            ChildKind::Command,
        );
        assert_eq!(failed.unwrap_err().code, "exit");
        let cancelled = runner.run_blocking_cancellable(
            &["templates".into(), "show".into(), "third-private".into()],
            ChildKind::Command,
            Arc::new(AtomicBool::new(true)),
        );
        assert_eq!(cancelled.unwrap_err().code, "cancelled");
        let text = log.text();
        assert!(
            text.contains("templates new") && text.contains("code=\"io\""),
            "{text}"
        );
        assert!(text.contains("CLI command refused"), "{text}");
        assert!(
            text.contains("templates edit") && text.contains("code=\"exit\""),
            "{text}"
        );
        assert!(text.contains("CLI command failed"), "{text}");
        for private in [
            "private-project",
            "other-private",
            "third-private",
            "/synthetic",
            "notes.md",
            "denied",
            "templates show",
        ] {
            assert!(!text.contains(private), "{private} reached the log: {text}");
        }
    }

    /// A model pull (the cooperative route) is logged like every command.
    #[test]
    fn a_failed_model_pull_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let answer =
            r#"{"error":{"code":"io","message":"checksum mismatch: /synthetic/private-models/x"}}"#;
        let cli = crate::test_support::write_cli(
            &dir.path().join("cli"),
            &format!("cat > /dev/null\nprintf '%s' '{answer}'\nexit 1\n"),
            &format!(
                "fx_drain_stdin;fx_print {};exit 1",
                crate::test_support::ps_str(answer)
            ),
        );
        let runner = crate::test_support::runner(cli, dir.path().join("lib"));
        let (log, _guard) = crate::test_support::LogCapture::start();
        let error = runner
            .run_blocking_cooperative(
                &[
                    "models".into(),
                    "pull".into(),
                    "synthetic-model".into(),
                    "--cancel-on-stdin-eof".into(),
                ],
                drop,
                |_| {},
            )
            .unwrap_err();
        assert!(error.from_cli);
        assert_eq!(error.download_text(), "The download didn't finish.");
        let text = log.text();
        assert!(
            text.contains("models pull") && text.contains("code=\"io\""),
            "{text}"
        );
        assert!(
            !text.contains("synthetic-model") && !text.contains("/synthetic"),
            "{text}"
        );
    }

    #[test]
    fn every_child_route_uses_the_admitted_profile_and_cwd_stable_library() {
        use broscribe_platform::paths::{
            ENV_DATA_DIR, ENV_LIBRARY, ENV_MAINTENANCE_CONFIG_BASE, resolve_paths_from,
        };
        const CHILD: &str = "BROSCRIBE_GUI_PROFILE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let inherited = dir.path().join("inherited-a");
            let other = dir.path().join("child-cwd");
            std::fs::create_dir_all(&inherited).unwrap();
            std::fs::create_dir_all(&other).unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "jobs::tests::every_child_route_uses_the_admitted_profile_and_cwd_stable_library", "--nocapture"])
                .current_dir(dir.path())
                .env(CHILD, "1")
                .env(ENV_DATA_DIR, &inherited)
                .env(ENV_MAINTENANCE_CONFIG_BASE, &inherited)
                .env("BROSCRIBE_GUI_OTHER_CWD", &other)
                .status().unwrap();
            assert!(status.success());
            for name in ["ordinary", "contained", "cooperative", "worker"] {
                assert_eq!(
                    std::fs::read(dir.path().join("profile-b").join(name)).unwrap(),
                    b"written"
                );
                assert_eq!(
                    std::fs::read(dir.path().join("library-b").join(name)).unwrap(),
                    b"written"
                );
                assert!(!inherited.join(name).exists());
                assert!(!other.join("library-b").join(name).exists());
            }
            assert_eq!(
                std::fs::read(dir.path().join("library-b/default")).unwrap(),
                b"written"
            );
            return;
        }
        let cwd = std::env::current_dir().unwrap();
        let native = broscribe_platform::fakes::FakePaths::new(cwd.join("native"));
        let paths = resolve_paths_from(&native, |key| match key {
            ENV_DATA_DIR => Some("profile-b".into()),
            ENV_LIBRARY => Some("library-b".into()),
            _ => None,
        })
        .unwrap();
        let admission = crate::test_support::admission(&paths.data_dir);
        std::fs::create_dir_all("library-b").unwrap();
        let executable = crate::test_support::write_cli(
            &cwd.join("synthetic-cli"),
            "[ -z \"${BROSCRIBE_MAINTENANCE_CONFIG_BASE+x}\" ] || exit 6\ncd \"$BROSCRIBE_GUI_OTHER_CWD\" || exit 2\nname=\"$4\"\n[ \"$3\" = queue ] && name=worker\nif [ \"$name\" = default ]; then [ -z \"${BROSCRIBE_DATA_DIR+x}\" ] || exit 3; else printf written > \"$BROSCRIBE_DATA_DIR/$name\" || exit 4; fi\nprintf written > \"$2/$name\" || exit 5\nprintf '{}'\n",
            "if(Test-Path Env:BROSCRIBE_MAINTENANCE_CONFIG_BASE){exit 6};Set-Location -LiteralPath $env:BROSCRIBE_GUI_OTHER_CWD;$name=fx_arg 4;if((fx_arg 3) -ceq 'queue'){$name='worker'};if($name -ceq 'default'){if(Test-Path Env:BROSCRIBE_DATA_DIR){exit 3}}else{[IO.File]::WriteAllText((Join-Path $env:BROSCRIBE_DATA_DIR $name),'written')};[IO.File]::WriteAllText((Join-Path (fx_arg 2) $name),'written');fx_print '{}'",
        );
        let cli = CliRunner::new(
            executable.clone(),
            paths.library(None, None),
            &paths,
            admission,
        )
        .unwrap();
        cli.run_blocking(&["ordinary".into()], ChildKind::Command)
            .unwrap();
        cli.run_blocking(&["contained".into()], ChildKind::Setup)
            .unwrap();
        cli.run_blocking_cooperative(&["cooperative".into()], drop, |_| {})
            .unwrap();
        let (tx, mut rx) = worker_channel();
        let mut worker = WorkerControl::new(cli.clone(), tx);
        worker.spawn();
        block_on_ready(async {
            'completion: loop {
                rx.wake.next().await.unwrap();
                for message in rx.take().into_msgs() {
                    match message {
                        WorkerMsg::Finished => break 'completion,
                        WorkerMsg::SpawnFailed(error) => panic!("profile worker failed: {error}"),
                        WorkerMsg::Crashed(error) => {
                            panic!("profile worker failed: {error}");
                        }
                        _ => {}
                    }
                }
            }
        });
        let defaults = resolve_paths_from(&native, |_| None).unwrap();
        let native_admission = crate::test_support::admission(&defaults.data_dir);
        CliRunner::new(executable, "library-b".into(), &defaults, native_admission)
            .unwrap()
            .run_blocking(&["default".into()], ChildKind::Command)
            .unwrap();
    }
    #[test]
    fn windows_settings_maintenance_command_preserves_config_base_outside_installation() {
        let dirs = tempfile::tempdir().unwrap();
        let outer = broscribe_platform::stable_path(dirs.path());
        let install = outer.join("install");
        let cwd = install.join("nested");
        std::fs::create_dir_all(&cwd).unwrap();
        let bin = install.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let cli = bin.join("broscribe.exe");
        std::fs::write(&cli, b"synthetic").unwrap();
        std::fs::write(
            install.join(broscribe_platform::update::BUILD_INFO_FILE),
            b"synthetic",
        )
        .unwrap();
        let base_var = broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE;
        for args in [
            &["uninstall", "--yes"][..],
            &["update", "install"],
            &["update", "rollback"],
            &["update", "recover"],
        ] {
            let args = args.iter().map(OsString::from).collect::<Vec<_>>();
            let mut command = Command::new(&cli);
            configure_windows_maintenance_command(&mut command, &args, || Some(cwd.clone()));
            assert_eq!(
                command.get_current_dir(),
                Some(broscribe_platform::plain_verbatim(&outer).as_path())
            );
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| { name == base_var && value == Some(cwd.as_os_str()) })
            );
        }

        let mut command = Command::new(&cli);
        configure_windows_maintenance_command(&mut command, &["uninstall".into()], || Some(outer));
        assert_eq!(command.get_current_dir(), None);
    }

    #[test]
    fn cli_locator_prefers_env_then_sibling() {
        let exe = Path::new("/Apps/BroScribe.app/Contents/MacOS/broscribe-app");
        let sibling = PathBuf::from(format!(
            "/Apps/BroScribe.app/Contents/MacOS/broscribe{}",
            std::env::consts::EXE_SUFFIX
        ));
        assert_eq!(cli_path_from(exe, None), sibling);
        assert_eq!(cli_path_from(exe, Some(OsString::new())), sibling);
        assert_eq!(
            cli_path_from(exe, Some("/opt/bin/broscribe".into())),
            PathBuf::from("/opt/bin/broscribe")
        );
    }

    #[test]
    fn parses_progress_lines() {
        let ev = parse_progress_line(
            r#"{"event":"progress","job_id":"j-1a2b3c","kind":"import","fraction":0.5,"message":"transcribing"}"#,
        )
        .unwrap();
        assert_eq!(ev.event, ProgressEventKind::Progress);
        assert_eq!(ev.job_id.as_deref(), Some("j-1a2b3c"));
        assert_eq!(ev.fraction, Some(0.5));
        let idle = parse_progress_line(r#"{"event":"idle","job_id":null}"#).unwrap();
        assert_eq!(idle.event, ProgressEventKind::Idle);
        let newer = parse_progress_line(r#"{"event":"something_new"}"#).unwrap();
        assert_eq!(newer.event, ProgressEventKind::Unknown);
        assert_eq!(parse_progress_line("  "), None);
        assert_eq!(parse_progress_line("warning: not json"), None);
    }

    fn progress(job: &str, fraction: f64) -> WorkerMsg {
        WorkerMsg::Progress(ProgressEvent {
            event: ProgressEventKind::Progress,
            job_id: Some(job.into()),
            kind: None,
            meeting_id: None,
            fraction: Some(fraction),
            message: None,
        })
    }

    #[test]
    fn batch_merges_progress_and_keeps_every_job_end() {
        let ev = |json: &str| WorkerMsg::Progress(parse_progress_line(json).unwrap());
        let mut sent = vec![
            ev(r#"{"event":"job_started","job_id":"j-1","kind":"import"}"#),
            ev(r#"{"event":"progress","job_id":"j-1","fraction":0.1,"message":"decoding"}"#),
        ];
        sent.extend((0..100).map(|i| progress("j-1", f64::from(i) / 100.0)));
        sent.extend([
            progress("j-2", 0.1),
            // Merges across the other job's progress.
            progress("j-1", 1.0),
            ev(r#"{"event":"something_new"}"#),
            ev(r#"{"event":"job_failed","job_id":"j-1","message":"boom"}"#),
            // After that end: kept apart.
            progress("j-1", 0.5),
            ev(r#"{"event":"idle"}"#),
            ev(r#"{"event":"idle"}"#),
            WorkerMsg::Finished,
        ]);
        let mut batch = WorkerBatch::default();
        for msg in sent.clone() {
            batch.push(msg);
        }
        let msgs = batch.into_msgs();
        assert_eq!(msgs.len(), 7, "{msgs:#?}");
        assert_eq!(
            msgs[1],
            WorkerMsg::Progress(ProgressEvent {
                event: ProgressEventKind::Progress,
                job_id: Some("j-1".into()),
                kind: None,
                meeting_id: None,
                fraction: Some(1.0),
                message: Some("decoding".into()),
            })
        );
        assert!(
            matches!(&msgs[3], WorkerMsg::Progress(p) if p.event == ProgressEventKind::JobFailed)
        );
        assert_eq!(msgs[6], WorkerMsg::Finished);

        // The merged batch leaves the same status as every line one by one.
        let status = |msgs: Vec<WorkerMsg>| {
            let mut s = JobsStatus::default();
            for msg in msgs {
                if let WorkerMsg::Progress(ev) = msg {
                    s.apply(ev);
                }
            }
            (s.active, s.completed, s.recent_failures)
        };
        assert_eq!(status(msgs), status(sent));
    }

    #[test]
    fn apply_reports_only_visible_changes() {
        let mut s = JobsStatus::default();
        let ev = |json: &str| parse_progress_line(json).unwrap();
        assert!(s.apply(ev(r#"{"event":"job_started","job_id":"j-1"}"#)));
        assert!(s.apply(ev(r#"{"event":"progress","job_id":"j-1","fraction":0.5}"#)));
        assert!(!s.apply(ev(r#"{"event":"progress","job_id":"j-1","fraction":0.5}"#)));
        assert!(!s.apply(ev(r#"{"event":"progress","job_id":"j-1"}"#)));
        assert!(!s.apply(ev(r#"{"event":"something_new"}"#)));
        assert!(s.apply(ev(r#"{"event":"idle"}"#)));
        assert!(!s.apply(ev(r#"{"event":"idle"}"#)));
    }

    #[test]
    fn worker_channel_waits_for_room_and_drops_nothing() {
        let (tx, rx) = worker_channel();
        let done = |n: usize| {
            WorkerMsg::Progress(ProgressEvent {
                event: ProgressEventKind::JobDone,
                job_id: Some(format!("j-{n}")),
                kind: None,
                meeting_id: None,
                fraction: None,
                message: None,
            })
        };
        let extra = 10;
        let sender = std::thread::spawn(move || {
            for n in 0..MAX_PENDING_WORKER_MSGS + extra {
                tx.send_waiting(done(n));
            }
            tx.send_waiting(WorkerMsg::Finished);
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while lock(&rx.shared.pending).batch.len() < MAX_PENDING_WORKER_MSGS {
            assert!(
                Instant::now() < deadline,
                "the sender never filled the batch"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        // Full: the sender waits instead of growing or dropping.
        assert!(!sender.is_finished());
        assert_eq!(
            lock(&rx.shared.pending).batch.len(),
            MAX_PENDING_WORKER_MSGS
        );
        let mut received = rx.take().into_msgs();
        sender.join().unwrap();
        received.extend(rx.take().into_msgs());
        assert_eq!(received.len(), MAX_PENDING_WORKER_MSGS + extra + 1);
        assert_eq!(
            received[MAX_PENDING_WORKER_MSGS + 3],
            done(MAX_PENDING_WORKER_MSGS + 3)
        );
        assert_eq!(received.last(), Some(&WorkerMsg::Finished));

        // Once the app's side is gone a sender never waits.
        let (tx, rx) = worker_channel();
        drop(rx);
        for n in 0..MAX_PENDING_WORKER_MSGS + extra {
            tx.send_waiting(done(n));
        }
    }

    #[cfg(unix)]
    fn synthetic_cli(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("synthetic-cli");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    fn wait_for(what: &str, ready: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn worker_spawn_failure_has_only_the_executable_name_and_error_kind() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("synthetic-private").join("missing-cli");
        let runner = crate::test_support::runner(executable, dir.path().into());
        let (tx, rx) = worker_channel();
        run_worker_child(&runner, &[], &tx);
        let messages = rx.take().into_msgs();
        let [WorkerMsg::SpawnFailed(failure)] = messages.as_slice() else {
            panic!("unexpected worker messages: {messages:?}");
        };
        assert_eq!(
            failure.to_string(),
            format!("missing-cli: {}", std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn worker_output_is_bounded_and_its_end_always_reported() {
        use crate::test_support::ps_str;
        let dir = tempfile::tempdir().unwrap();
        let started = r#"{"event":"job_started","job_id":"j-1","kind":"import"}"#;
        let progress = r#"{"event":"progress","job_id":"j-1","fraction":0.5}"#;
        let failed = r#"{"event":"job_failed","job_id":"j-1","message":"boom"}"#;
        let cli = crate::test_support::write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                r#"printf '%s\n' '{started}'
head -c {} /dev/zero | tr '\000' x; printf '\n'
printf 'not progress synthetic-secret\n'
head -c 1048576 /dev/zero >&2
printf '%s\n' '{progress}'
printf '%s' '{failed}'
exit 3
"#,
                MAX_PROGRESS_LINE + 1
            ),
            &format!(
                "fx_echo {};fx_print ('x'*{});fx_print ([string][char]10);\
                 fx_echo 'not progress synthetic-secret';\
                 fx_ebytes (New-Object byte[] 1048576);\
                 fx_echo {};fx_print {};exit 3",
                ps_str(started),
                MAX_PROGRESS_LINE + 1,
                ps_str(progress),
                ps_str(failed),
            ),
        );
        let (tx, rx) = worker_channel();
        let runner = crate::test_support::runner(cli, dir.path().into());
        run_worker_child(&runner, &[], &tx);
        let msgs = rx.take().into_msgs();
        let events: Vec<_> = msgs
            .iter()
            .map(|msg| match msg {
                WorkerMsg::Progress(p) => format!("{:?}", p.event),
                other => format!("{other:?}"),
            })
            .collect();
        // How the platform's `ExitStatus` reads exit code 3.
        let exit_3 = if cfg!(windows) {
            "exit code: 3"
        } else {
            "exit status: 3"
        };
        assert_eq!(
            events,
            [
                "JobStarted".to_owned(),
                "Progress".to_owned(),
                "JobFailed".to_owned(),
                format!("Crashed({exit_3:?})")
            ]
        );
    }

    #[test]
    fn one_shot_output_is_bounded_and_status_interpreted() {
        use crate::test_support::ps_str;
        let dir = tempfile::tempdir().unwrap();
        let run = |body: &str, ps: &str, limit: usize| {
            let cli = crate::test_support::write_cli(
                &dir.path().join("synthetic-cli"),
                &format!("{body}\n"),
                ps,
            );
            let mut cli = crate::test_support::runner(cli, dir.path().into());
            cli.set_output_limit(limit);
            let result = cli.run_blocking(&[], ChildKind::Command);
            assert!(cli.children().is_empty());
            result
        };
        // A flooded stderr is discarded; the reply still arrives.
        let ok = run(
            r#"head -c 1048576 /dev/zero >&2; printf '{"ok":true}'"#,
            &format!(
                "fx_ebytes (New-Object byte[] 1048576);fx_print {}",
                ps_str(r#"{"ok":true}"#)
            ),
            1024,
        )
        .unwrap();
        assert_eq!(ok["ok"], true);
        // Too large is an error, never a cut reply, and the child is drained.
        let large = run(
            r#"printf '{"text":"'; head -c 65536 /dev/zero | tr '\000' x; printf '"}'"#,
            &format!(
                "fx_print {};fx_print ('x'*65536);fx_print {}",
                ps_str(r#"{"text":""#),
                ps_str(r#""}"#)
            ),
            1024,
        )
        .unwrap_err();
        assert_eq!(large.code, "output_too_large");
        let refusal = run(
            r#"printf '{"error":{"code":"library_busy","message":"busy"}}'; exit 2"#,
            &format!(
                "fx_print {};exit 2",
                ps_str(r#"{"error":{"code":"library_busy","message":"busy"}}"#)
            ),
            1024,
        )
        .unwrap_err();
        assert_eq!(refusal.code, "library_busy");
        assert_eq!(run("exit 4", "exit 4", 1024).unwrap_err().code, "exit");
        assert_eq!(
            run("printf 'not json'", "fx_print 'not json'", 1024)
                .unwrap_err()
                .code,
            "bad_output"
        );
    }

    /// Each run records its leader and descendant after the fork completes.
    #[cfg(unix)]
    fn sleeping_cli(dir: &Path) -> CliRunner {
        let started = dir.join("started-");
        let cli = synthetic_cli(
            dir,
            &format!(
                "sleep 30 &\nprintf '%s %s\\n' \"$$\" \"$!\" > '{}'\"$4\"\nwait",
                started.display(),
            ),
        );
        crate::test_support::runner(cli, dir.into())
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_one_run_stops_and_reaps_only_its_tree() {
        let dir = tempfile::tempdir().unwrap();
        let started = |name| {
            let record =
                std::fs::read_to_string(dir.path().join(format!("started-{name}"))).ok()?;
            let (leader, descendant) = record.strip_suffix('\n')?.split_once(' ')?;
            Some((leader.to_owned(), descendant.to_owned()))
        };
        let live = |pid: &str, group: &str| {
            let info = Command::new("/bin/ps")
                .args(["-o", "pgid=", "-o", "stat=", "-p", pid])
                .output()
                .unwrap();
            assert!(info.stderr.is_empty(), "{:?}", info.stderr);
            let text = std::str::from_utf8(&info.stdout).unwrap();
            let mut fields = text.split_whitespace();
            // A zombie has exited. Only a member of this fixture's group
            // counts, even if an unrelated process has reused the PID.
            fields.next() == Some(group)
                && fields.next().is_some_and(|state| !state.starts_with('Z'))
        };
        let cli = sleeping_cli(dir.path());
        let start = |name: &str, kind: ChildKind| {
            let cli = cli.clone();
            let flag = Arc::new(AtomicBool::new(false));
            let cancel = Arc::clone(&flag);
            let args = vec![OsString::from(name)];
            let run = std::thread::spawn(move || cli.run_blocking_cancellable(&args, kind, cancel));
            (flag, run)
        };
        let (a_flag, a) = start("a", ChildKind::Command);
        let (b_flag, b) = start("b", ChildKind::Setup);
        wait_for("both descendant PIDs", || {
            started("a").is_some() && started("b").is_some()
        });
        let (a_leader, a_descendant) = started("a").unwrap();
        let (b_leader, b_descendant) = started("b").unwrap();
        assert!(live(&a_descendant, &a_leader));
        assert!(live(&b_descendant, &b_leader));

        let cancelled_at = Instant::now();
        a_flag.store(true, Ordering::SeqCst);
        assert_eq!(a.join().unwrap().unwrap_err().code, "cancelled");
        assert!(cancelled_at.elapsed() < Duration::from_secs(10));
        // Returned only after its tree was reaped: B alone is still running.
        assert_eq!(cli.children().len(), 1);
        assert!(!b.is_finished());
        wait_for("A's descendant to exit", || !live(&a_descendant, &a_leader));
        assert!(live(&b_descendant, &b_leader));

        b_flag.store(true, Ordering::SeqCst);
        assert_eq!(b.join().unwrap().unwrap_err().code, "cancelled");
        assert!(cli.children().is_empty());
        wait_for("B's descendant to exit", || !live(&b_descendant, &b_leader));
    }

    /// Polls `future` on this thread until it is ready.
    fn block_on_ready<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let std::task::Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the future"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn setup_cancel_spares_ordinary_commands() {
        let dir = tempfile::tempdir().unwrap();
        let cli = sleeping_cli(dir.path());
        let setup = {
            let cli = cli.clone();
            std::thread::spawn(move || cli.run_blocking(&["setup".into()], ChildKind::Setup))
        };
        let mut quick = crate::test_support::runner(
            synthetic_cli(
                &dir.path().join("quick"),
                "sleep 3; printf '{\"saved\":true}'",
            ),
            dir.path().into(),
        );
        quick.children = cli.children().clone();
        let command = std::thread::spawn(move || quick.run_blocking(&[], ChildKind::Command));
        wait_for("the setup child", || {
            dir.path().join("started-setup").exists()
        });
        wait_for("both runs", || cli.children().len() == 2);
        let stopping = cli.children().cancel_setup_children();
        assert_eq!(stopping.count(), 1);
        block_on_ready(stopping.finished());
        assert_eq!(setup.join().unwrap().unwrap_err().code, "cancelled");
        assert_eq!(command.join().unwrap().unwrap()["saved"], true);
        assert!(cli.children().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn cancellable_stdin_runs_deliver_input_or_stop_when_hung() {
        let dir = tempfile::tempdir().unwrap();
        let echo = crate::test_support::runner(synthetic_cli(dir.path(), "cat"), dir.path().into());
        let flag = Arc::new(AtomicBool::new(false));
        let reply = echo
            .run_blocking_cancellable_stdin(
                &["search".into(), "--query-stdin".into()],
                ChildKind::Command,
                br#"{"query":"synthetic-secret"}"#.to_vec(),
                flag,
            )
            .unwrap();
        assert_eq!(reply["query"], "synthetic-secret");

        // A child that never reads its input (larger than a pipe holds).
        let hung = sleeping_cli(&dir.path().join("hung"));
        let flag = Arc::new(AtomicBool::new(false));
        let run = {
            let hung = hung.clone();
            let cancel = Arc::clone(&flag);
            std::thread::spawn(move || {
                hung.run_blocking_cancellable_stdin(
                    &["hung".into()],
                    ChildKind::Setup,
                    vec![b'x'; 1 << 20],
                    cancel,
                )
            })
        };
        wait_for("the hung child", || {
            dir.path().join("hung/started-hung").exists()
        });
        flag.store(true, Ordering::SeqCst);
        assert_eq!(run.join().unwrap().unwrap_err().code, "cancelled");
        assert!(hung.children().is_empty());
    }

    #[test]
    fn status_tracks_jobs() {
        let mut s = JobsStatus::default();
        let ev = |json: &str| parse_progress_line(json).unwrap();
        s.apply(ev(
            r#"{"event":"job_started","job_id":"j-1","kind":"import"}"#,
        ));
        s.apply(ev(
            r#"{"event":"progress","job_id":"j-1","fraction":1.7,"message":"m"}"#,
        ));
        assert_eq!(s.active.len(), 1);
        assert_eq!(s.active[0].fraction, Some(1.0));
        assert_eq!(s.active[0].kind.as_deref(), Some("import"));
        s.apply(ev(
            r#"{"event":"job_started","job_id":"j-2","kind":"enrich"}"#,
        ));
        s.apply(ev(r#"{"event":"job_done","job_id":"j-1"}"#));
        s.apply(ev(
            r#"{"event":"job_failed","job_id":"j-2","message":"boom"}"#,
        ));
        assert!(s.active.is_empty());
        assert_eq!(s.completed, 1);
        assert_eq!(s.recent_failures.len(), 1);
        assert_eq!(s.recent_failures[0].kind.as_deref(), Some("enrich"));
        s.apply(ev(r#"{"event":"job_started","job_id":"j-3"}"#));
        s.apply(ev(r#"{"event":"idle"}"#));
        assert!(s.active.is_empty());
    }

    #[test]
    fn pending_jobs_follow_queue_state() {
        use broscribe_core::queue::Next;
        use broscribe_core::schema::now;

        let dir = tempfile::tempdir().unwrap();
        let lib = broscribe_core::Library::open(dir.path()).unwrap();
        let probe = FakeProcessProbe::new(1);
        assert!(!has_pending_jobs(lib.root(), &probe, now()).unwrap().pending);
        let queue = Queue::open(lib.root()).unwrap();
        let id = lib
            .write(|tx| queue.enqueue(tx, "import", "k1", serde_json::json!({})))
            .unwrap();
        assert!(has_pending_jobs(lib.root(), &probe, now()).unwrap().pending);
        lib.write(|tx| queue.set_state(tx, &id, JobState::Done, None))
            .unwrap();
        let idle = has_pending_jobs(lib.root(), &probe, now()).unwrap();
        assert!(!idle.pending);
        assert_eq!(idle.wake_at, None);

        // A job its run put back to wait: no work before its time, which is
        // when the worker must be started again; work once it has come.
        let waiting = lib
            .write(|tx| queue.enqueue(tx, "reidentify", "k2", serde_json::json!({})))
            .unwrap();
        let lease = queue.claim(&lib, &waiting, &probe).unwrap().unwrap();
        let due = now() + chrono::TimeDelta::seconds(60);
        lib.write(|tx| {
            lease.update_in(
                tx,
                Some(Next::Requeue {
                    not_before: Some(due),
                }),
                |_| Ok(()),
            )
        })
        .unwrap();
        lease.release(&lib).unwrap();
        let early = has_pending_jobs(lib.root(), &probe, now()).unwrap();
        assert!(!early.pending);
        assert_eq!(early.wake_at, Some(due));
        let late = has_pending_jobs(lib.root(), &probe, due).unwrap();
        assert!(late.pending);
        assert_eq!(late.wake_at, Some(due));
    }

    /// The app's fallback planner is the queue's own: a `failed` job with
    /// attempts left waits for its retry delay and its `not_before`, a
    /// held one is left to its holder, a due retry is work and wakes a
    /// worker at once, and a job failed for good never counts.
    #[test]
    fn failed_jobs_with_attempts_left_are_retried_after_their_delay() {
        use broscribe_core::fsutil::write_json_atomic;
        use broscribe_core::queue::JOB_JSON;
        use broscribe_core::schema::now;
        use chrono::TimeDelta;

        let dir = tempfile::tempdir().unwrap();
        let lib = broscribe_core::Library::open(dir.path()).unwrap();
        let queue = Queue::open(lib.root()).unwrap();
        let app = FakeProcessProbe::new(900);
        let worker = FakeProcessProbe::new(4242);
        let id = lib
            .write(|tx| queue.enqueue(tx, "import", "transient", serde_json::json!({})))
            .unwrap();
        let lease = queue.claim(&lib, &id, &worker).unwrap().unwrap();
        lease.fail(&lib, "synthetic transient failure").unwrap();
        // Its live worker still holds it: left to that worker.
        app.set_alive(4242, true);
        let held = has_pending_jobs(lib.root(), &app, now()).unwrap();
        assert!(!held.pending);
        assert_eq!(held.wake_at, None);
        lease.release(&lib).unwrap();

        let failed = queue.get(&id).unwrap();
        assert_eq!((failed.state, failed.attempts), (JobState::Failed, 1));
        let retry_at = failed.updated_at + TimeDelta::from_std(retry_delay(1)).unwrap();
        let early = has_pending_jobs(lib.root(), &app, now()).unwrap();
        assert!(!early.pending);
        assert_eq!(early.wake_at, Some(retry_at));
        assert!(
            has_pending_jobs(lib.root(), &app, retry_at)
                .unwrap()
                .pending
        );

        // A later `not_before` still bounds it.
        let job_json = queue.job_dir(&id).join(JOB_JSON);
        let mut stored = failed;
        let later = retry_at + TimeDelta::seconds(120);
        stored.not_before = Some(later);
        write_json_atomic(&job_json, &stored).unwrap();
        let bounded = has_pending_jobs(lib.root(), &app, retry_at).unwrap();
        assert!(!bounded.pending);
        assert_eq!(bounded.wake_at, Some(later));

        // Its delay passed while the worker that failed it ran on: work now,
        // and a wake-up at once for a check made as that worker exits.
        stored.not_before = None;
        stored.updated_at = now() - TimeDelta::seconds(60);
        write_json_atomic(&job_json, &stored).unwrap();
        let before = now();
        let due = has_pending_jobs(lib.root(), &app, before).unwrap();
        assert!(due.pending);
        assert!(due.wake_at.is_some_and(|at| before <= at && at <= now()));

        // An interrupted interactive run is failed for good.
        lib.write(|tx| queue.set_state(tx, &id, JobState::Done, None))
            .unwrap();
        let interactive = lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    "enrich",
                    "interactive",
                    serde_json::json!({ "interactive": true }),
                )
            })
            .unwrap();
        let lease = queue.claim(&lib, &interactive, &worker).unwrap().unwrap();
        lease.fail(&lib, "synthetic failure").unwrap();
        lease.release(&lib).unwrap();
        let never = has_pending_jobs(lib.root(), &app, now() + TimeDelta::days(1)).unwrap();
        assert!(!never.pending);
        assert_eq!(never.wake_at, None);
    }

    #[test]
    fn stranded_crashed_job_is_failed_and_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let lib = broscribe_core::Library::open(dir.path()).unwrap();
        let queue = Queue::open(lib.root()).unwrap();
        let input = serde_json::json!({ "path": "synthetic.wav" });
        let id = lib
            .write(|tx| queue.enqueue(tx, "import", "stranded", input.clone()))
            .unwrap();
        // Each worker dies holding the lease; the next one takes it over.
        // The last crash uses up the last attempt.
        for pid in 1..=broscribe_core::queue::MAX_ATTEMPTS {
            let worker = FakeProcessProbe::new(pid);
            drop(queue.claim(&lib, &id, &worker).unwrap().unwrap());
        }
        assert_eq!(queue.get(&id).unwrap().state, JobState::Running);

        // The app sees no runnable work, but no longer leaves the job stranded:
        // it is failed and reported, since no worker ever will.
        let app = FakeProcessProbe::new(900);
        let check = has_pending_jobs(lib.root(), &app, broscribe_core::schema::now()).unwrap();
        assert!(!check.pending);
        assert_eq!(
            check.crashed,
            [FailedJob {
                job_id: id.to_string(),
                kind: Some("import".into()),
                meeting_id: None,
                message: Some("worker crashed".into()),
            }]
        );
        let job = queue.get(&id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.error.as_deref(), Some("worker crashed"));
        let mut status = JobsStatus::default();
        status.record_failure(check.crashed[0].clone());
        assert_eq!(status.recent_failures[0].job_id, id.to_string());

        // A retry (re-enqueue) restarts it.
        lib.write(|tx| queue.enqueue(tx, "import", "stranded", input))
            .unwrap();
        assert_eq!(queue.get(&id).unwrap().state, JobState::Queued);
        assert!(
            has_pending_jobs(lib.root(), &app, broscribe_core::schema::now())
                .unwrap()
                .pending
        );
    }

    /// Unix only: it denies the write through POSIX directory mode bits
    /// (`0o555`); a Windows read-only attribute on a folder does not stop
    /// files being created in it, so there is no equivalent denial to stage.
    #[cfg(unix)]
    #[test]
    fn failing_to_mark_crashed_jobs_still_reports_pending_work() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let lib = broscribe_core::Library::open(dir.path()).unwrap();
        let queue = Queue::open(lib.root()).unwrap();
        let stranded = lib
            .write(|tx| queue.enqueue(tx, "import", "stranded", serde_json::json!({})))
            .unwrap();
        for pid in 1..=broscribe_core::queue::MAX_ATTEMPTS {
            drop(
                queue
                    .claim(&lib, &stranded, &FakeProcessProbe::new(pid))
                    .unwrap()
                    .unwrap(),
            );
        }
        lib.write(|tx| queue.enqueue(tx, "import", "waiting", serde_json::json!({})))
            .unwrap();
        // Marking the crashed job fails (its record cannot be rewritten).
        let job_dir = queue.job_dir(&stranded);
        std::fs::set_permissions(&job_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores directory permissions, so the failure can't be staged.
        let probe = job_dir.join(".write-probe");
        if std::fs::write(&probe, b"").is_ok() {
            std::fs::remove_file(&probe).unwrap();
            std::fs::set_permissions(&job_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let pending = has_pending_jobs(
            lib.root(),
            &FakeProcessProbe::new(900),
            broscribe_core::schema::now(),
        );
        std::fs::set_permissions(&job_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let pending = pending.unwrap();
        assert!(pending.pending);
        assert!(pending.crashed.is_empty());
        assert_eq!(queue.get(&stranded).unwrap().state, JobState::Running);
    }

    #[test]
    fn cancelled_setup_never_spawns_a_child() {
        let dir = tempfile::tempdir().unwrap();
        let cli =
            crate::test_support::runner(dir.path().join("missing"), dir.path().join("library"));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let error = cli
            .run_blocking_cancellable(&[], ChildKind::Setup, cancelled)
            .unwrap_err();
        assert_eq!(error.code, "cancelled");
        assert!(cli.children().is_empty());
    }

    #[gpui::test]
    async fn sensitive_stdin_reaches_child_without_argv(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let executable = crate::test_support::write_cli(
            &dir.path().join("synthetic-cli"),
            "for arg in \"$@\"; do case \"$arg\" in *synthetic-secret*) exit 9;; esac; done\ncat\n",
            "foreach($a in $argv){if($a -clike '*synthetic-secret*'){exit 9}};fx_copy_stdin",
        );
        let cli = crate::test_support::runner(executable, dir.path().into());
        let result = cx
            .update(|cx| {
                cli.run_with_stdin(
                    vec![
                        "integrations".into(),
                        "setup".into(),
                        "google".into(),
                        "--client-secret-stdin".into(),
                    ],
                    ChildKind::Command,
                    br#"{"received":"synthetic-secret"}"#.to_vec(),
                    cx.background_executor(),
                )
            })
            .await
            .unwrap();
        assert_eq!(result["received"], "synthetic-secret");
    }
}
