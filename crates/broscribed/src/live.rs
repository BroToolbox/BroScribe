//! Live preview supervision (plan §12.1, §13.1, §14.1).
//!
//! The capture engine pushes 16 kHz frames, after journaling, into a
//! [`LiveQueue`]: bounded per track (about 10 s), never blocking, dropping
//! the oldest frames when full and remembering the dropped range. A
//! dedicated sender thread feeds `broscribe-worker live <session>` on its
//! stdin from that queue; capture never touches the pipe. A reader thread
//! relays the worker's events. Until the worker reports `ready`, only its
//! `loading` lines are signs of life, and it must be ready within
//! [`LiveSettings::load_deadline`] of its start whatever it reports; after
//! that any valid line is. The supervisor kills a worker that has shown no
//! sign of life for [`LiveSettings::silence_kill`] (stopped, stuck) or
//! missed the load deadline, and restarts it with backoff while the session
//! lasts; after [`LiveSettings::load_attempts`] consecutive load timeouts it
//! gives up for the session, and a worker that reports itself terminally
//! unavailable ([`LiveEvent::Unavailable`], e.g. its live model is not
//! installed) is never restarted for the session. Every step is published
//! as a [`PreviewState`]; the panel shows a `preview_gap` for dropped audio.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use broscribe_ipc::live::{Frame, LIVE_RATE, LiveIdentitySettings, write_frame};
use broscribe_ipc::{Event, LiveEvent, LiveFrame, LiveSink, PreviewState, Track, WorkerProblem};
use broscribe_platform_native::ipc::NonBlockingStdin;

use crate::bus::EventBus;
use crate::shutdown::Shutdown;
use crate::worker_cmd::WorkerCommand;

#[derive(Clone, Debug)]
pub struct LiveSettings {
    /// Kill a live worker that showed no sign of life for this long.
    pub silence_kill: Duration,
    /// Keepalive frame interval when no audio flows.
    pub keepalive: Duration,
    /// First restart delay; doubles per consecutive failure.
    pub restart_backoff: Duration,
    pub restart_backoff_max: Duration,
    /// Queue length per track, seconds of audio.
    pub queue_seconds: u32,
    /// A worker that ran this long resets the restart backoff.
    pub healthy_after: Duration,
    /// A worker not ready this long after it started is killed
    /// ([`WorkerProblem::LoadTimeout`]), however many `loading` lines it
    /// wrote.
    pub load_deadline: Duration,
    /// Consecutive load timeouts after which the worker is not restarted
    /// for the rest of the session; a worker that got ready resets them.
    pub load_attempts: u32,
    /// The companion live-speakers worker (plan D): window, hop and bounds.
    pub identity: LiveIdentitySettings,
}

impl Default for LiveSettings {
    fn default() -> Self {
        Self {
            silence_kill: Duration::from_secs(30),
            keepalive: Duration::from_secs(5),
            restart_backoff: Duration::from_secs(1),
            restart_backoff_max: Duration::from_secs(30),
            queue_seconds: 10,
            healthy_after: Duration::from_secs(60),
            load_deadline: Duration::from_secs(180),
            load_attempts: 3,
            identity: LiveIdentitySettings::default(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---- queue ----

/// A dropped range of one track, in samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    pub track: Track,
    pub from_sample: u64,
    pub to_sample: u64,
}

impl Gap {
    /// The `preview_gap` event of recording `session_id`, the one whose
    /// queue dropped the range.
    pub fn event(self, session_id: &str) -> Event {
        let rate = f64::from(LIVE_RATE);
        Event::PreviewGap {
            session_id: session_id.to_owned(),
            track: self.track,
            from_s: self.from_sample as f64 / rate,
            to_s: self.to_sample as f64 / rate,
        }
    }
}

/// What [`LiveQueue::pop`] returned.
#[derive(Debug, PartialEq)]
pub enum Popped {
    /// The next frame, with the range dropped before it on its track.
    Frame(LiveFrame, Option<Gap>),
    Timeout,
    Closed,
}

#[derive(Default)]
struct QueueInner {
    frames: [VecDeque<LiveFrame>; 2],
    samples: [usize; 2],
    gaps: [Option<(u64, u64)>; 2],
    /// Session language spans (start sample, tag), in order; every worker
    /// receives all of them.
    languages: Vec<(u64, String)>,
    closed: bool,
}

/// Bounded drop-oldest frame queue per track.
pub struct LiveQueue {
    cap_samples: usize,
    inner: Mutex<QueueInner>,
    ready: Condvar,
}

impl LiveQueue {
    pub fn new(cap_samples: usize) -> Self {
        Self {
            cap_samples: cap_samples.max(1),
            inner: Mutex::new(QueueInner::default()),
            ready: Condvar::new(),
        }
    }

    /// Queue holding `seconds` of 16 kHz audio per track.
    pub fn with_seconds(seconds: u32) -> Self {
        Self::new(seconds as usize * LIVE_RATE as usize)
    }

    /// The oldest frame across tracks, waiting up to `timeout`.
    pub fn pop(&self, timeout: Duration) -> Popped {
        self.pop_with_stop(timeout, None)
    }

    fn pop_with_stop(&self, timeout: Duration, stopped: Option<&AtomicBool>) -> Popped {
        let deadline = Instant::now() + timeout;
        let mut q = lock(&self.inner);
        loop {
            if stopped.is_some_and(|stop| stop.load(Ordering::Acquire)) {
                return Popped::Closed;
            }
            let next = Track::ALL
                .iter()
                .filter_map(|t| q.frames[t.index()].front().map(|f| (f.start_sample, *t)))
                .min();
            if let Some((_, track)) = next {
                let t = track.index();
                if let Some(frame) = q.frames[t].pop_front() {
                    q.samples[t] -= frame.samples.len();
                    let gap = q.gaps[t].take().map(|(from, to)| Gap {
                        track,
                        from_sample: from,
                        to_sample: to,
                    });
                    return Popped::Frame(frame, gap);
                }
            }
            if q.closed {
                return Popped::Closed;
            }
            let now = Instant::now();
            if now >= deadline {
                return Popped::Timeout;
            }
            q = self
                .ready
                .wait_timeout(q, deadline - now)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }

    /// Wakes only this worker's sender; queued audio stays for its successor.
    fn stop_sender(&self, stopped: &AtomicBool) {
        let _queue = lock(&self.inner);
        stopped.store(true, Ordering::Release);
        self.ready.notify_all();
    }

    /// Records a frame that was popped but never reached a worker.
    pub fn note_lost(&self, frame: &LiveFrame) {
        let mut q = lock(&self.inner);
        let t = frame.track.index();
        q.gaps[t] = Some(match q.gaps[t] {
            Some((from, to)) => (from.min(frame.start_sample), to.max(frame.end_sample())),
            None => (frame.start_sample, frame.end_sample()),
        });
    }

    /// Ends the queue: pushes are ignored, pops return [`Popped::Closed`]
    /// once drained.
    pub fn close(&self) {
        lock(&self.inner).closed = true;
        self.ready.notify_all();
    }

    /// Samples queued for `track` (bounded by the capacity).
    pub fn queued_samples(&self, track: Track) -> usize {
        lock(&self.inner).samples[track.index()]
    }

    /// Records a session language span starting at `start_sample`.
    pub fn set_language(&self, start_sample: u64, language: &str) {
        lock(&self.inner)
            .languages
            .push((start_sample, language.to_owned()));
    }

    /// Language spans from the `sent`-th on.
    pub(crate) fn languages_after(&self, sent: usize) -> Vec<(u64, String)> {
        lock(&self.inner)
            .languages
            .get(sent..)
            .map(<[_]>::to_vec)
            .unwrap_or_default()
    }
}

/// The live sample at recording time `seconds`.
pub fn sample_at(seconds: f64) -> u64 {
    (seconds.max(0.0) * f64::from(LIVE_RATE)).round() as u64
}

impl LiveSink for LiveQueue {
    fn push(&self, frame: LiveFrame) {
        let mut q = lock(&self.inner);
        if q.closed || frame.samples.is_empty() {
            return;
        }
        let t = frame.track.index();
        q.samples[t] += frame.samples.len();
        q.frames[t].push_back(frame);
        while q.samples[t] > self.cap_samples {
            let Some(old) = q.frames[t].pop_front() else {
                break;
            };
            q.samples[t] -= old.samples.len();
            q.gaps[t] = Some(match q.gaps[t] {
                Some((from, _)) => (from, old.end_sample()),
                None => (old.start_sample, old.end_sample()),
            });
        }
        drop(q);
        self.ready.notify_one();
    }
}

// ---- supervisor ----

/// Receives a preview state together with its immutable recording origin.
pub type PreviewCallback = dyn Fn(&str, PreviewState) + Send + Sync;

/// Everything one live session's supervisor needs.
pub struct LiveRun {
    pub worker: WorkerCommand,
    pub library: PathBuf,
    pub admission: Arc<broscribe_core::maintenance::ModeLease>,
    /// The recording this supervisor serves: every event and preview state
    /// it produces names it, however late it arrives.
    pub session_id: String,
    pub settings: LiveSettings,
    pub queue: Arc<LiveQueue>,
    pub bus: EventBus,
    /// Set when the session ends.
    pub stop: Arc<Shutdown>,
    /// Receives availability changes with the recording they belong to
    /// (`session_id`; the daemon mirrors them in its state only while that
    /// recording owns the preview).
    pub on_preview: Arc<PreviewCallback>,
    /// Turns every recognised segment into a live turn (speaker preview).
    pub preview: Option<Arc<crate::live_identity::LivePreview>>,
}

/// Runs live workers until `run.stop` is triggered, restarting them with
/// backoff, and publishes every [`PreviewState`] on the way; `Off` once the
/// session ends.
pub async fn supervise(run: LiveRun) {
    if let Err(error) = run.admission.require_normal() {
        tracing::warn!(code = error.code(), "live preview admission failed");
        (run.on_preview)(
            &run.session_id,
            PreviewState::Unavailable {
                reason: WorkerProblem::StartFailed,
            },
        );
        return;
    }
    let mut failures: u32 = 0;
    let mut load_timeouts: u32 = 0;
    while !run.stop.is_triggered() {
        let started = Instant::now();
        // Published before the reader thread exists, so it never overwrites
        // what the worker reports.
        (run.on_preview)(
            &run.session_id,
            PreviewState::Loading {
                model: None,
                elapsed_ms: 0,
            },
        );
        let ended = match spawn_worker(&run) {
            Ok(worker) => watch(&run, worker).await,
            Err(e) => {
                tracing::warn!("live preview worker did not start: {e}");
                Ended {
                    problem: Some(WorkerProblem::StartFailed),
                    ready: false,
                    terminal: false,
                }
            }
        };
        let Some(reason) = ended.problem else {
            break;
        };
        if run.stop.is_triggered() {
            break;
        }
        if ended.terminal {
            tracing::warn!(
                ?reason,
                "live preview worker cannot run; not restarting it for this session"
            );
            (run.on_preview)(&run.session_id, PreviewState::Unavailable { reason });
            run.stop.wait().await;
            break;
        }
        if ended.ready {
            load_timeouts = 0;
        }
        if reason == WorkerProblem::LoadTimeout {
            load_timeouts = load_timeouts.saturating_add(1);
            if load_timeouts >= run.settings.load_attempts.max(1) {
                tracing::warn!(
                    attempts = load_timeouts,
                    "live preview models not ready within the load deadline; giving up for this session"
                );
                (run.on_preview)(&run.session_id, PreviewState::Unavailable { reason });
                run.stop.wait().await;
                break;
            }
        }
        if started.elapsed() >= run.settings.healthy_after {
            failures = 0;
        }
        failures = failures.saturating_add(1);
        let delay = backoff(
            run.settings.restart_backoff,
            run.settings.restart_backoff_max,
            failures,
        );
        (run.on_preview)(
            &run.session_id,
            PreviewState::Restarting {
                reason,
                retry_in_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            },
        );
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = run.stop.wait() => break,
        }
    }
    (run.on_preview)(&run.session_id, PreviewState::Off);
}

/// `base · 2^(n-1)`, at most `max`.
pub fn backoff(base: Duration, max: Duration, n: u32) -> Duration {
    let factor = 1u32 << n.saturating_sub(1).min(16);
    base.saturating_mul(factor).min(max)
}

/// Signs of life of one worker, shared with its reader thread.
struct Life {
    /// When the worker was started.
    started: Instant,
    /// Milliseconds since `started` of the last sign of life.
    last_heard: AtomicU64,
    /// The worker reported `ready`.
    ready: AtomicBool,
    /// The worker reported itself terminally unavailable, and why (the
    /// first report wins).
    unavailable: OnceLock<WorkerProblem>,
}

impl Life {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_heard: AtomicU64::new(0),
            ready: AtomicBool::new(false),
            unavailable: OnceLock::new(),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn heard(&self) {
        self.last_heard.store(self.elapsed_ms(), Ordering::SeqCst);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// How long no sign of life came.
    fn silent_for(&self) -> Duration {
        Duration::from_millis(
            self.elapsed_ms()
                .saturating_sub(self.last_heard.load(Ordering::SeqCst)),
        )
    }
}

struct Worker {
    child: Child,
    sender: JoinHandle<()>,
    reader: JoinHandle<()>,
    life: Arc<Life>,
    sender_stop: Arc<AtomicBool>,
}

/// How one worker ended.
struct Ended {
    /// `None`: the session ended.
    problem: Option<WorkerProblem>,
    /// It had reported `ready`.
    ready: bool,
    /// It reported itself unavailable: never restarted for the session.
    terminal: bool,
}

fn spawn_worker(run: &LiveRun) -> std::io::Result<Worker> {
    let mut cmd = run.worker.std_command(
        &run.library,
        &[OsStr::new("live"), OsStr::new(&run.session_id)],
    );
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn()?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other("live worker pipes missing"));
    };
    let stdin = NonBlockingStdin::new(stdin)?;
    let life = Arc::new(Life::new());
    let sender_stop = Arc::new(AtomicBool::new(false));

    let sender = {
        let admission = Arc::clone(&run.admission);
        let queue = Arc::clone(&run.queue);
        let bus = run.bus.clone();
        let session_id = run.session_id.clone();
        let keepalive = run.settings.keepalive;
        let stopped = Arc::clone(&sender_stop);
        std::thread::Builder::new()
            .name("live-sender".into())
            .spawn(move || {
                let _admission = admission;
                send_frames(stdin, &session_id, &queue, &bus, keepalive, &stopped)
            })?
    };
    let reader = {
        let admission = Arc::clone(&run.admission);
        let bus = run.bus.clone();
        let session_id = run.session_id.clone();
        let life = Arc::clone(&life);
        let on_preview = Arc::clone(&run.on_preview);
        let preview = run.preview.clone();
        std::thread::Builder::new()
            .name("live-reader".into())
            .spawn(move || {
                let _admission = admission;
                read_events(
                    stdout,
                    &session_id,
                    &bus,
                    &life,
                    on_preview.as_ref(),
                    preview.as_deref(),
                )
            })?
    };
    Ok(Worker {
        child,
        sender,
        reader,
        life,
        sender_stop,
    })
}

/// Sender thread: queue → worker stdin, keepalives when idle. Ends on this
/// worker's stop, queue closure or a broken pipe; restart keeps the queue.
struct PipeWriter<'a>(NonBlockingStdin, &'a LiveQueue, &'a AtomicBool);
impl Write for PipeWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        loop {
            if self.2.load(Ordering::Acquire) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "live worker stopped",
                ));
            }
            match self.0.write(bytes) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if lock(&self.1.inner).closed {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "live queue closed",
                        ));
                    }
                    let _ = self.0.wait_writable(Duration::from_secs(1))?;
                }
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
fn send_frames(
    stdin: NonBlockingStdin,
    session_id: &str,
    queue: &LiveQueue,
    bus: &EventBus,
    keepalive: Duration,
    stopped: &AtomicBool,
) {
    let mut stdin = PipeWriter(stdin, queue, stopped);
    // Spans this worker has; sent before any later audio frame.
    let mut languages_sent = 0;
    loop {
        let popped = queue.pop_with_stop(keepalive, Some(stopped));
        for (start_sample, language) in queue.languages_after(languages_sent) {
            let frame = Frame::Language {
                start_sample,
                language,
            };
            if write_frame(&mut stdin, &frame).is_err() {
                if let Popped::Frame(f, _) = &popped {
                    queue.note_lost(f);
                }
                return;
            }
            languages_sent += 1;
        }
        match popped {
            Popped::Frame(frame, gap) => {
                if let Some(g) = gap {
                    bus.publish(g.event(session_id));
                }
                let frame = Frame::Audio(frame);
                if write_frame(&mut stdin, &frame)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    if let Frame::Audio(f) = &frame {
                        queue.note_lost(f);
                        bus.publish(
                            Gap {
                                track: f.track,
                                from_sample: f.start_sample,
                                to_sample: f.end_sample(),
                            }
                            .event(session_id),
                        );
                    }
                    return;
                }
            }
            Popped::Timeout => {
                if write_frame(&mut stdin, &Frame::Keepalive)
                    .and_then(|()| stdin.flush())
                    .is_err()
                {
                    return;
                }
            }
            Popped::Closed => return,
        }
    }
}

/// Reader thread: worker stdout lines → events and preview states, each
/// naming `session_id`, the recording this worker serves. Before `ready`,
/// only `loading` lines are signs of life (a warning or garbage is not);
/// after it, every valid line is. A terminal `unavailable` line ends
/// reading: the worker exits after it.
fn read_events(
    stdout: std::process::ChildStdout,
    session_id: &str,
    bus: &EventBus,
    life: &Life,
    on_preview: &PreviewCallback,
    preview: Option<&crate::live_identity::LivePreview>,
) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else {
            return;
        };
        let event = match LiveEvent::parse(&line) {
            Ok(event) => event,
            Err(e) => {
                tracing::warn!("live preview: {e}");
                continue;
            }
        };
        let ready = life.is_ready();
        if ready || matches!(event, LiveEvent::Loading { .. } | LiveEvent::Ready { .. }) {
            life.heard();
        }
        match event {
            LiveEvent::Loading { model, .. } => {
                if !ready {
                    on_preview(
                        session_id,
                        PreviewState::Loading {
                            model: Some(model),
                            elapsed_ms: life.elapsed_ms(),
                        },
                    );
                }
            }
            LiveEvent::Ready { model } => {
                life.ready.store(true, Ordering::SeqCst);
                on_preview(session_id, PreviewState::Ready { model });
            }
            LiveEvent::Ack { .. } => {}
            LiveEvent::Segment(seg) => {
                if let Some(preview) = preview {
                    preview.on_segment(&seg);
                }
                bus.publish(Event::LiveTranscript {
                    session_id: session_id.to_owned(),
                    segment: seg,
                });
            }
            LiveEvent::Warning { message } => {
                tracing::warn!("live preview: {message}");
            }
            LiveEvent::Unavailable { reason } => {
                let _ = life.unavailable.set(reason);
                return;
            }
        }
    }
}

/// Waits for the worker to exit, killing it when it shows no sign of life,
/// misses the load deadline, reports itself unavailable or the session
/// ends; then joins its threads.
async fn watch(run: &LiveRun, mut worker: Worker) -> Ended {
    let settings = &run.settings;
    let tick = Duration::from_secs(1)
        .min(settings.silence_kill)
        .min(settings.load_deadline);
    let problem = loop {
        match worker.child.try_wait() {
            Ok(Some(status)) => {
                tracing::warn!("live preview worker exited ({status})");
                break Some(WorkerProblem::Exited);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("live preview worker: {e}");
                let _ = worker.child.kill();
                break Some(WorkerProblem::Exited);
            }
        }
        if run.stop.is_triggered() {
            let _ = worker.child.kill();
            break None;
        }
        let life = &worker.life;
        if let Some(&reason) = life.unavailable.get() {
            let _ = worker.child.kill();
            break Some(reason);
        }
        if !life.is_ready() && life.started.elapsed() > settings.load_deadline {
            tracing::warn!(
                "live preview worker not ready within {:?}; restarting it",
                settings.load_deadline
            );
            let _ = worker.child.kill();
            break Some(WorkerProblem::LoadTimeout);
        }
        if life.silent_for() > settings.silence_kill {
            tracing::warn!(
                "live preview worker silent for {:?}; restarting it",
                settings.silence_kill
            );
            let _ = worker.child.kill();
            break Some(WorkerProblem::Silent);
        }
        tokio::select! {
            () = tokio::time::sleep(tick) => {}
            () = run.stop.wait() => {}
        }
    };
    run.queue.stop_sender(&worker.sender_stop);
    // Reap and join off the async workers. The sender's queue wait was
    // interrupted without closing the recording's shared audio queue.
    let life = Arc::clone(&worker.life);
    let _ = tokio::task::spawn_blocking(move || {
        let _ = worker.child.wait();
        let _ = worker.sender.join();
        let _ = worker.reader.join();
    })
    .await;
    // A worker that said it cannot run is terminal, whichever way its end
    // was noticed first (exit, kill, or the line read after the exit).
    let (problem, terminal) = match (problem, life.unavailable.get()) {
        (Some(_), Some(&reason)) => (Some(reason), true),
        (problem, _) => (problem, false),
    };
    Ended {
        problem,
        ready: life.is_ready(),
        terminal,
    }
}

#[cfg(test)]
#[path = "../tests/fixtures/compile_live_timing_worker.rs"]
mod timing_fixture;

/// Synthetic children shared by the live protocol tests. Timing-sensitive
/// children are native Rust; script fixtures use inline PowerShell on
/// Windows, never a policy override, with byte-exact raw streams.
#[cfg(test)]
pub(crate) mod test_support {
    use super::WorkerCommand;

    /// Compile the std-only timing child and launch it once before starting
    /// any supervisor clocks. Every test owns its executable; no shared
    /// cache or shell.
    pub(crate) struct TimingWorker {
        _dir: tempfile::TempDir,
        program: std::path::PathBuf,
    }

    impl TimingWorker {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let program = dir.path().join(format!(
                "live-timing-worker{}",
                std::env::consts::EXE_SUFFIX
            ));
            super::timing_fixture::compile_live_timing_worker(&program);
            // The first launch of a freshly built executable is far slower
            // than later ones (one-time OS checks of the new file); under
            // load it alone outlasted the tests' silence limits.
            let warm = std::process::Command::new(&program)
                .arg("warm-up")
                .status()
                .expect("launch live timing fixture");
            assert!(warm.success(), "warm up live timing fixture: {warm}");
            Self { _dir: dir, program }
        }

        pub(crate) fn command(&self, args: &[&str]) -> WorkerCommand {
            WorkerCommand {
                program: self.program.clone(),
                prefix: args.iter().map(|arg| (*arg).into()).collect(),
                data_dir_override: None,
            }
        }
    }

    pub(crate) fn worker(sh: &str, ps: &str) -> WorkerCommand {
        #[cfg(unix)]
        {
            let _ = ps;
            WorkerCommand {
                program: "/bin/sh".into(),
                prefix: vec!["-c".into(), sh.into(), "worker".into()],
                data_dir_override: None,
            }
        }
        #[cfg(windows)]
        {
            let _ = sh;
            let prelude = concat!(
                "$ErrorActionPreference='Stop';",
                "$fx_utf8=New-Object System.Text.UTF8Encoding $false;",
                "$fx_in=[Console]::OpenStandardInput();",
                "$fx_out=[Console]::OpenStandardOutput();",
                "function fx_echo([string]$t){",
                "$b=$fx_utf8.GetBytes($t+[char]10);",
                "$fx_out.Write($b,0,$b.Length);$fx_out.Flush()};",
                "function fx_drain_stdin{$fx_in.CopyTo([IO.Stream]::Null)};",
                "function fx_save_stdin([string]$p){",
                "$f=[IO.File]::Create($p);",
                "try{$fx_in.CopyTo($f)}finally{$f.Close()}};",
                "function fx_append([string]$p,[string]$t){",
                "[IO.File]::AppendAllText($p,$t,$fx_utf8)};",
            );
            WorkerCommand {
                program: "powershell.exe".into(),
                prefix: vec![
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    // WorkerCommand appends the real worker argv. These
                    // fixtures do not consume it; a comment keeps arbitrary
                    // library paths out of PowerShell's command grammar.
                    format!("& {{ {prelude} {ps} }}; exit 0; #").into(),
                ],
                data_dir_override: None,
            }
        }
    }

    /// ASCII PowerShell expression with no raw double quotes, as in the
    /// app's native process fixtures. Handles Unicode paths and JSON.
    pub(crate) fn ps_str(text: &str) -> String {
        let mut parts = Vec::new();
        let mut run = String::new();
        for unit in text.encode_utf16() {
            match char::from_u32(u32::from(unit)) {
                Some(c) if (' '..='~').contains(&c) && c != '"' => {
                    if c == '\'' {
                        run.push('\'');
                    }
                    run.push(c);
                }
                _ => {
                    if !run.is_empty() {
                        parts.push(format!("'{}'", std::mem::take(&mut run)));
                    }
                    parts.push(format!("[string][char]{unit}"));
                }
            }
        }
        if !run.is_empty() || parts.is_empty() {
            parts.push(format!("'{run}'"));
        }
        format!("({})", parts.join("+"))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{TimingWorker, ps_str, worker};
    use super::*;

    fn frame(track: Track, start: u64, n: usize) -> LiveFrame {
        LiveFrame {
            track,
            start_sample: start,
            samples: vec![0.1; n],
        }
    }

    #[test]
    fn full_queue_drops_oldest_and_reports_the_gap() {
        let q = LiveQueue::new(300);
        for i in 0..5 {
            q.push(frame(Track::Mic, i * 100, 100));
        }
        assert_eq!(q.queued_samples(Track::Mic), 300);
        match q.pop(Duration::ZERO) {
            Popped::Frame(f, gap) => {
                assert_eq!(f.start_sample, 200);
                assert_eq!(
                    gap,
                    Some(Gap {
                        track: Track::Mic,
                        from_sample: 0,
                        to_sample: 200
                    })
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(q.pop(Duration::ZERO), Popped::Frame(_, None)));
    }

    #[test]
    fn tracks_interleave_by_time_and_close_drains() {
        let q = LiveQueue::new(1000);
        q.push(frame(Track::System, 50, 10));
        q.push(frame(Track::Mic, 0, 10));
        q.push(frame(Track::Mic, 100, 10));
        let order: Vec<(Track, u64)> = (0..3)
            .map(|_| match q.pop(Duration::ZERO) {
                Popped::Frame(f, _) => (f.track, f.start_sample),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            order,
            [(Track::Mic, 0), (Track::System, 50), (Track::Mic, 100)]
        );
        assert_eq!(q.pop(Duration::from_millis(10)), Popped::Timeout);
        q.close();
        q.push(frame(Track::Mic, 200, 10));
        assert_eq!(q.pop(Duration::from_secs(5)), Popped::Closed);
    }

    #[test]
    fn push_never_blocks_without_a_consumer() {
        let q = LiveQueue::with_seconds(1);
        let start = Instant::now();
        for i in 0..10_000u64 {
            q.push(frame(Track::System, i * 1600, 1600));
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(q.queued_samples(Track::System) <= LIVE_RATE as usize);
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let b = |n| backoff(Duration::from_secs(1), Duration::from_secs(30), n);
        assert_eq!(b(1), Duration::from_secs(1));
        assert_eq!(b(3), Duration::from_secs(4));
        assert_eq!(b(10), Duration::from_secs(30));
    }

    #[test]
    fn record_pipe_writer_waits_beyond_one_second_for_consumer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("received");
        let worker = worker(
            &format!("sleep 1.3; cat > '{}'", path.display()),
            &format!(
                "Start-Sleep -Milliseconds 1300; fx_save_stdin {}",
                ps_str(path.to_str().unwrap())
            ),
        );
        let mut child = worker
            .std_command(tmp.path(), &[])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = NonBlockingStdin::new(child.stdin.take().unwrap()).unwrap();
        let queue = LiveQueue::new(1);
        let stopped = AtomicBool::new(false);
        let mut pipe = PipeWriter(stdin, &queue, &stopped);
        pipe.write_all(&vec![42; 1_000_000]).unwrap();
        drop(pipe);
        assert!(child.wait().unwrap().success());
        assert_eq!(std::fs::metadata(path).unwrap().len(), 1_000_000);
    }

    /// Every worker the sender feeds, a restarted one included, gets every
    /// language span before the audio popped after it.
    #[test]
    fn record_live_sender_forwards_language_spans_to_each_worker() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = LiveQueue::new(LIVE_RATE as usize);
        queue.set_language(0, "en");
        queue.push(frame(Track::Mic, 0, 160));
        queue.set_language(sample_at(0.02), "pt");
        queue.push(frame(Track::Mic, 320, 160));
        queue.close();
        let received = |name: &str| {
            let path = tmp.path().join(name);
            let worker = worker(
                &format!("cat > '{}'", path.display()),
                &format!("fx_save_stdin {}", ps_str(path.to_str().unwrap())),
            );
            let mut child = worker
                .std_command(tmp.path(), &[])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let stdin = NonBlockingStdin::new(child.stdin.take().unwrap()).unwrap();
            send_frames(
                stdin,
                "synthetic-live",
                &queue,
                &EventBus::new(8),
                Duration::from_secs(5),
                &AtomicBool::new(false),
            );
            assert!(child.wait().unwrap().success());
            let bytes = std::fs::read(path).unwrap();
            let mut r = bytes.as_slice();
            std::iter::from_fn(|| broscribe_ipc::live::read_frame(&mut r).unwrap())
                .collect::<Vec<_>>()
        };
        let language = |start_sample, language: &str| Frame::Language {
            start_sample,
            language: language.into(),
        };
        assert_eq!(
            received("first"),
            [
                language(0, "en"),
                language(320, "pt"),
                Frame::Audio(frame(Track::Mic, 0, 160)),
                Frame::Audio(frame(Track::Mic, 320, 160)),
            ]
        );
        // A restarted worker (the queue is drained) still gets both spans.
        assert_eq!(
            received("restarted"),
            [language(0, "en"), language(320, "pt")]
        );
    }

    #[tokio::test]
    async fn record_stopped_live_worker_keeps_journal_gap_free_bounds_queue_and_resumes() {
        use broscribe_capture::{
            journal::{JournalWriter, SessionMeta, Track as JournalTrack},
            timeline::{Timeline, TimelineOptions},
        };
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("started");
        let ready = LiveEvent::Ready {
            model: "synthetic".into(),
        }
        .to_line()
        .unwrap();
        let fixture = TimingWorker::new();
        let worker = fixture.command(&["stopped", marker.to_str().unwrap(), &ready]);
        let queue = Arc::new(LiveQueue::with_seconds(10));
        let bus = EventBus::new(128);
        let mut sub = bus.subscribe();
        let stop = Arc::new(Shutdown::new());
        let event_bus = bus.clone();
        let run = LiveRun {
            worker: WorkerCommand {
                data_dir_override: Some(tmp.path().join("data")),
                ..worker
            },
            library: tmp.path().join("library"),
            admission: Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.path().join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
            session_id: "synthetic-live".into(),
            settings: LiveSettings {
                silence_kill: Duration::from_millis(300),
                keepalive: Duration::from_millis(10),
                restart_backoff: Duration::from_millis(10),
                ..Default::default()
            },
            queue: queue.clone(),
            bus: bus.clone(),
            stop: stop.clone(),
            on_preview: Arc::new(move |session_id: &str, preview| {
                event_bus.publish(Event::Preview {
                    session_id: session_id.to_owned(),
                    preview,
                });
            }),
            preview: None,
        };
        let task = tokio::spawn(async move {
            supervise(run).await;
            drop(fixture);
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if matches!(
                    sub.next().await,
                    Some(Event::Preview {
                        preview: PreviewState::Ready { .. },
                        ..
                    })
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let mut journal = JournalWriter::create(
            tmp.path(),
            "synthetic-live",
            SessionMeta::default(),
            5.0,
            std::process::id(),
        )
        .unwrap();
        for track in JournalTrack::ALL {
            journal.begin_segment(track, 0, None, 16000, 0).unwrap();
        }
        let samples = vec![0.125; 80_000];
        for index in 0..20u64 {
            for (track, journal_track) in [
                (Track::Mic, JournalTrack::Mic),
                (Track::System, JournalTrack::System),
            ] {
                journal
                    .append(journal_track, index * 5_000_000_000, &samples)
                    .unwrap();
                queue.push(frame(track, index * 80_000, samples.len()));
                assert!(queue.queued_samples(track) <= 160_000);
            }
        }
        let journal_dir = journal.dir().to_owned();
        journal.finish(100_000_000_000, "user").unwrap();
        let mut gap = false;
        let mut resumed = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !gap || !resumed {
                match sub.next().await {
                    Some(Event::PreviewGap {
                        session_id,
                        from_s,
                        to_s,
                        ..
                    }) => {
                        assert_eq!(session_id, "synthetic-live");
                        assert!(to_s > from_s);
                        gap = true;
                    }
                    Some(Event::Preview {
                        preview: PreviewState::Ready { .. },
                        ..
                    }) => resumed = true,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        stop.trigger();
        queue.close();
        task.await.unwrap();
        let manifest = broscribe_capture::journal::load_manifest(&journal_dir).unwrap();
        let timeline =
            Timeline::plan(&journal_dir, &manifest, &TimelineOptions::default()).unwrap();
        assert_eq!(timeline.frames, 1_600_000);
        for track in JournalTrack::ALL {
            assert_eq!(timeline.tracks[track.index()].gap_frames, 0);
            assert!(
                timeline
                    .read_track(track)
                    .unwrap()
                    .iter()
                    .all(|&sample| sample == 0.125)
            );
        }
    }

    /// A supervised synthetic worker and every preview state it published,
    /// with when.
    struct Supervised {
        _tmp: tempfile::TempDir,
        states: Arc<Mutex<Vec<(Instant, PreviewState)>>>,
        stop: Arc<Shutdown>,
        task: tokio::task::JoinHandle<()>,
    }

    fn supervised(
        worker: WorkerCommand,
        settings: LiveSettings,
        fixture: Option<TimingWorker>,
    ) -> Supervised {
        let tmp = tempfile::tempdir().unwrap();
        let states = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Shutdown::new());
        let sink = Arc::clone(&states);
        let run = LiveRun {
            worker: WorkerCommand {
                data_dir_override: Some(tmp.path().join("data")),
                ..worker
            },
            library: tmp.path().join("library"),
            admission: Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.path().join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
            session_id: "synthetic-live".into(),
            settings,
            queue: Arc::new(LiveQueue::with_seconds(1)),
            bus: EventBus::new(64),
            stop: Arc::clone(&stop),
            on_preview: Arc::new(move |session_id: &str, p| {
                assert_eq!(session_id, "synthetic-live");
                lock(&sink).push((Instant::now(), p));
            }),
            preview: None,
        };
        let task = tokio::spawn(async move {
            supervise(run).await;
            // Keep the executable until every child has been reaped.
            drop(fixture);
        });
        Supervised {
            _tmp: tmp,
            states,
            stop,
            task,
        }
    }

    impl Supervised {
        fn states(&self) -> Vec<(Instant, PreviewState)> {
            lock(&self.states).clone()
        }

        async fn until(&self, limit: Duration, done: impl Fn(&PreviewState) -> bool) {
            let found = tokio::time::timeout(limit, async {
                loop {
                    if self.states().iter().any(|(_, p)| done(p)) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            assert!(found.is_ok(), "timed out: {:?}", self.states());
        }

        /// Ends the session; the states, the final `Off` included.
        async fn finish(self) -> Vec<(Instant, PreviewState)> {
            self.stop.trigger();
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .unwrap()
                .unwrap();
            lock(&self.states).clone()
        }
    }

    /// Loading lines keep a loading worker from the silence kill, but not
    /// past the total load deadline; consecutive load timeouts give up.
    #[tokio::test]
    async fn record_live_loading_worker_is_killed_at_the_load_deadline_not_for_silence() {
        let loading = LiveEvent::Loading {
            model: "synthetic".into(),
            elapsed_ms: 1,
        }
        .to_line()
        .unwrap();
        let fixture = TimingWorker::new();
        let worker = fixture.command(&["loading", &loading]);
        // Loading lines every 0.1 s, well inside the silence limit, itself
        // well inside the load deadline.
        let load_deadline = Duration::from_millis(2000);
        let run = supervised(
            worker,
            LiveSettings {
                silence_kill: Duration::from_millis(500),
                load_deadline,
                load_attempts: 2,
                restart_backoff: Duration::from_millis(10),
                restart_backoff_max: Duration::from_millis(10),
                ..Default::default()
            },
            Some(fixture),
        );
        run.until(Duration::from_secs(10), |p| {
            matches!(p, PreviewState::Unavailable { .. })
        })
        .await;
        // Given up: no worker is started again for this session.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let states = run.finish().await;
        let shown: Vec<PreviewState> = states.iter().map(|(_, p)| p.clone()).collect();
        assert_eq!(
            shown[0],
            PreviewState::Loading {
                model: None,
                elapsed_ms: 0
            }
        );
        assert!(
            shown.iter().any(|p| matches!(
                p,
                PreviewState::Loading { model: Some(m), .. } if m == "synthetic"
            )),
            "{shown:?}"
        );
        let problems: Vec<&PreviewState> = shown
            .iter()
            .filter(|p| {
                matches!(
                    p,
                    PreviewState::Restarting { .. } | PreviewState::Unavailable { .. }
                )
            })
            .collect();
        assert_eq!(
            problems,
            [
                &PreviewState::Restarting {
                    reason: WorkerProblem::LoadTimeout,
                    retry_in_ms: 10
                },
                &PreviewState::Unavailable {
                    reason: WorkerProblem::LoadTimeout
                },
            ]
        );
        // Killed at the load deadline, long after the silence limit.
        let restarted = states
            .iter()
            .find(|(_, p)| matches!(p, PreviewState::Restarting { .. }))
            .unwrap()
            .0;
        assert!(
            restarted - states[0].0 >= load_deadline,
            "{:?}",
            restarted - states[0].0
        );
        let given_up = shown
            .iter()
            .position(|p| matches!(p, PreviewState::Unavailable { .. }))
            .unwrap();
        assert_eq!(shown[given_up + 1..].to_vec(), vec![PreviewState::Off]);
    }

    /// A worker that cannot run (its live model is missing, or the profile
    /// or config it was given is invalid) says so with the typed terminal
    /// line: the preview is unavailable for that reason at once, never
    /// restarting, and no second worker starts for the session. Holds both
    /// when the worker exits right after the line and when it lingers.
    #[tokio::test]
    async fn record_live_terminal_worker_problem_is_unavailable_and_never_restarted() {
        let loading = LiveEvent::Loading {
            model: "synthetic".into(),
            elapsed_ms: 0,
        }
        .to_line()
        .unwrap();
        for reason in [WorkerProblem::ModelMissing, WorkerProblem::InvalidSettings] {
            let unavailable = LiveEvent::Unavailable { reason }.to_line().unwrap();
            for linger in [false, true] {
                let then = if linger {
                    "exec cat >/dev/null"
                } else {
                    "exit 1"
                };
                let spawns = tempfile::tempdir().unwrap();
                let count = spawns.path().join("spawns");
                let script = format!(
                    "echo spawn >> '{}'; printf '%s\\n' '{loading}' '{unavailable}'; {then}",
                    count.display()
                );
                let ps = format!(
                    "fx_append {count} ('spawn'+[char]10); \
                     fx_echo {loading}; fx_echo {unavailable}; {then}",
                    count = ps_str(count.to_str().unwrap()),
                    loading = ps_str(&loading),
                    unavailable = ps_str(&unavailable),
                    then = if linger { "fx_drain_stdin" } else { "exit 1" },
                );
                let run = supervised(
                    worker(&script, &ps),
                    LiveSettings {
                        restart_backoff: Duration::from_millis(10),
                        restart_backoff_max: Duration::from_millis(10),
                        ..Default::default()
                    },
                    None,
                );
                run.until(Duration::from_secs(10), |p| {
                    matches!(p, PreviewState::Unavailable { .. })
                })
                .await;
                let shown: Vec<PreviewState> =
                    run.finish().await.into_iter().map(|(_, p)| p).collect();
                let problems: Vec<&PreviewState> = shown
                    .iter()
                    .filter(|p| {
                        matches!(
                            p,
                            PreviewState::Restarting { .. } | PreviewState::Unavailable { .. }
                        )
                    })
                    .collect();
                assert_eq!(
                    problems,
                    [&PreviewState::Unavailable { reason }],
                    "{reason:?} {then}: {shown:?}"
                );
                assert_eq!(shown.last(), Some(&PreviewState::Off), "{reason:?} {then}");
                assert_eq!(
                    std::fs::read_to_string(&count).unwrap(),
                    "spawn\n",
                    "{reason:?} {then}"
                );
            }
        }
    }

    /// Before `ready`, warnings and unparseable lines are no sign of life:
    /// such a worker is killed as silent long before the load deadline.
    #[tokio::test]
    async fn record_live_warnings_before_ready_do_not_keep_a_worker_alive() {
        let warning = LiveEvent::Warning {
            message: "synthetic".into(),
        }
        .to_line()
        .unwrap();
        let fixture = TimingWorker::new();
        let worker = fixture.command(&["unready", &warning, "not-json"]);
        let run = supervised(
            worker,
            LiveSettings {
                silence_kill: Duration::from_millis(300),
                load_deadline: Duration::from_secs(60),
                restart_backoff: Duration::from_millis(10),
                restart_backoff_max: Duration::from_millis(10),
                ..Default::default()
            },
            Some(fixture),
        );
        run.until(Duration::from_secs(5), |p| {
            matches!(p, PreviewState::Restarting { .. })
        })
        .await;
        let shown: Vec<PreviewState> = run.finish().await.into_iter().map(|(_, p)| p).collect();
        let restarting = shown
            .iter()
            .find(|p| matches!(p, PreviewState::Restarting { .. }))
            .unwrap();
        assert_eq!(
            restarting,
            &PreviewState::Restarting {
                reason: WorkerProblem::Silent,
                retry_in_ms: 10
            }
        );
        assert!(!shown.iter().any(PreviewState::is_available), "{shown:?}");
        assert_eq!(shown.last(), Some(&PreviewState::Off));
    }
}
