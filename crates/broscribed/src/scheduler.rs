//! Serial job scheduling; child waits and retry delays hold no library lock.
use crate::daemon::{Shared, lock};
use broscribe_core::{
    Result, ops,
    queue::{
        Job, JobId, JobState, MAX_ATTEMPTS, Queue, RECORD_FINAL_KIND, RECORD_PROVISIONAL_KIND,
        StoppedRelease,
    },
    schema,
};
use broscribe_ipc::{Event, JobProgress, NotificationRequest, StoppedRecordingState, WorkerLine};
use chrono::{DateTime, FixedOffset};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::io::BufReader;

/// Jobs a worker left unfinished on purpose, with when to offer them again:
/// a manifest entry its still-running import has not reached, or an
/// analyzed entry waiting for the rest of its batch. They count no attempt.
static WAITING: LazyLock<Mutex<HashMap<JobId, Instant>>> = LazyLock::new(Mutex::default);

/// The longest retry delay, however many attempts were spent.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(3600);

/// The error of a job given up at the daemon's own attempt limit.
const EXHAUSTED: &str = "worker exhausted its attempts";

/// How long a run waits for its exited worker's last stdout lines.
const DRAIN: Duration = Duration::from_secs(1);

/// Until when `id` waits; a passed deadline is forgotten.
fn waiting_until(id: &JobId) -> Option<Instant> {
    let mut waiting = lock(&WAITING);
    match waiting.get(id) {
        Some(until) if Instant::now() < *until => Some(*until),
        Some(_) => {
            waiting.remove(id);
            None
        }
        None => None,
    }
}

/// The delay before a job that spent `attempts` runs again.
fn retry_delay(s: &Shared, attempts: u32) -> Duration {
    if attempts == 0 {
        Duration::ZERO
    } else {
        crate::live::backoff(s.cfg.retry_backoff, MAX_RETRY_DELAY, attempts)
    }
}

/// The recording session a record job works on.
fn session_of(job: &Job) -> &str {
    job.input["session_id"].as_str().unwrap_or(&job.key)
}

/// How long from `at` until `t`; zero once it passed.
fn until(at: DateTime<FixedOffset>, t: DateTime<FixedOffset>) -> Duration {
    t.signed_duration_since(at).to_std().unwrap_or_default()
}

/// What one read of the queue found.
#[derive(Debug, Default)]
struct Scan {
    /// The job to run now.
    job: Option<Job>,
    /// Runnable jobs (the chosen one included).
    waiting: usize,
    /// Jobs held back: not before a time, in retry backoff, left waiting by
    /// their worker, or not run while capturing.
    deferred: usize,
    /// How soon a held-back job may become runnable; `None` when none
    /// waits for a time.
    wake_in: Option<Duration>,
    /// Every final recording job not done, by session.
    stopped: HashMap<String, StoppedRecordingState>,
}

/// One snapshot of the queue: the next job, the counts, the next deadline
/// and the stopped recordings' states. Unreadable job records are left out
/// (the snapshot reports them); they never block healthy jobs.
fn next(s: &Shared) -> Result<Scan> {
    let queue = Queue::open(s.lib.root())?;
    let probe = s.probe.as_ref();
    let delay = |attempts| retry_delay(s, attempts);
    let mut snap = queue.snapshot()?;
    let mut pending = snap.pending_work(probe, &delay);
    if !pending.exhausted.is_empty() {
        // Crashed holders whose run was the last attempt: failed for good,
        // then read again so every state below sees it.
        s.lib.write(|tx| queue.fail_exhausted(tx, probe))?;
        snap = queue.snapshot()?;
        pending = snap.pending_work(probe, &delay);
    }
    let at = snap.at();
    let runnable: HashSet<&JobId> = pending.runnable.iter().collect();
    let recording = s.is_recording();
    let mut scan = Scan {
        wake_in: pending.wake_at.map(|t| until(at, t)),
        ..Scan::default()
    };
    let mut exhausted = HashSet::new();
    for j in &snap.jobs {
        // The queue's own rules decide what may run now (claimable, its
        // lease not held by a live process, `not_before` and the retry delay
        // passed); never a second copy of them.
        if !runnable.contains(&j.id) {
            if j.not_before.is_some_and(|t| at < t) || snap.is_waiting(j) {
                scan.deferred += 1;
            }
            continue;
        }
        if j.attempts >= s.cfg.max_attempts {
            // The daemon's limit, when below the queue's own.
            if j.state != JobState::Failed {
                s.lib.write(|tx| {
                    queue.set_state(tx, &j.id, JobState::Failed, Some(EXHAUSTED.into()))
                })?;
                exhausted.insert(j.id.clone());
            }
            continue;
        }
        if let Some(until) = waiting_until(&j.id) {
            let left = until.saturating_duration_since(Instant::now());
            scan.wake_in = Some(scan.wake_in.map_or(left, |w| w.min(left)));
            scan.deferred += 1;
            continue;
        }
        // Only an explicit durable process-now snapshot may run during capture.
        if recording && j.kind != RECORD_PROVISIONAL_KIND {
            scan.deferred += 1;
            continue;
        }
        scan.waiting += 1;
        if scan.job.is_none() {
            scan.job = Some(j.clone());
        }
    }
    let ranks = snap.ranks();
    let running = lock(&s.jobs).running.clone();
    let given_up = s.cfg.max_attempts.min(MAX_ATTEMPTS);
    for j in &snap.jobs {
        if j.kind != RECORD_FINAL_KIND || j.state == JobState::Done {
            continue;
        }
        // A candidate has not started. In particular, a library-switch
        // hold can still refuse its reservation; progress marks it running.
        let state = if queue.is_held(j, probe) {
            StoppedRecordingState::Running {
                fraction: running
                    .as_ref()
                    .filter(|r| r.job_id == j.id.as_str())
                    .and_then(|r| r.fraction),
            }
        } else if exhausted.contains(&j.id) {
            StoppedRecordingState::Failed {
                error: Some(EXHAUSTED.into()),
            }
        } else if j.keeps_file()
            || (j.state == JobState::Failed && (j.attempts >= given_up || j.is_interactive()))
        {
            StoppedRecordingState::Failed {
                error: j.error.clone(),
            }
        } else if j.state == JobState::Failed {
            let left = if runnable.contains(&j.id) {
                Duration::ZERO
            } else {
                let age = at
                    .signed_duration_since(j.updated_at)
                    .to_std()
                    .unwrap_or_default();
                retry_delay(s, j.attempts).saturating_sub(age)
            };
            StoppedRecordingState::Retrying {
                attempts: j.attempts,
                retry_in_s: left.as_secs() + u64::from(left.subsec_nanos() > 0),
                error: j.error.clone(),
            }
        } else {
            StoppedRecordingState::Queued {
                ahead: ranks.get(&j.id).copied().unwrap_or(0),
            }
        };
        scan.stopped.insert(session_of(j).to_owned(), state);
    }
    Ok(scan)
}

/// Applies a scan's counts and stopped states; whether anything changed.
fn apply(s: &Shared, scan: &Scan) -> bool {
    let jobs_changed = {
        let mut state = lock(&s.jobs);
        let changed = state.waiting != scan.waiting || state.deferred != scan.deferred;
        state.waiting = scan.waiting;
        state.deferred = scan.deferred;
        changed
    };
    let stopped_changed = crate::daemon::set_stopped_states(s, &scan.stopped);
    jobs_changed || stopped_changed
}

pub(crate) async fn run(s: Arc<Shared>) {
    while !s.shutdown.is_triggered() {
        let result = tokio::task::spawn_blocking({
            let s = s.clone();
            move || next(&s)
        })
        .await;
        // Jobs enqueued by other processes are found at the next poll.
        let mut sleep = s.cfg.poll_every;
        match result {
            Ok(Ok(scan)) => {
                let changed = apply(&s, &scan);
                if let Some(job) = scan.job
                    && run_job(&s, job).await
                {
                    continue;
                }
                if changed {
                    s.publish_state();
                }
                if let Some(wake) = scan.wake_in {
                    sleep = sleep.min(wake);
                }
            }
            Ok(Err(e)) => tracing::warn!("queue scheduling failed: {}", e.code()),
            Err(_) => tracing::warn!("queue scheduling task failed"),
        }
        tokio::select! { () = s.shutdown.wait() => break, () = s.kick.notified() => {}, () = tokio::time::sleep(sleep) => {} }
    }
}

fn job_progress(
    job: &Job,
    phase: &str,
    fraction: Option<f64>,
    message: Option<String>,
) -> JobProgress {
    JobProgress {
        job_id: job.id.to_string(),
        kind: job.kind.clone(),
        meeting_id: job.meeting_id(),
        phase: phase.into(),
        fraction,
        message,
    }
}

/// Publishes a job's progress. `message` is the worker's own text (counts
/// and phases, never transcript text or names).
fn progress(s: &Shared, job: &Job, phase: &str, fraction: Option<f64>, message: Option<String>) {
    let event = job_progress(job, phase, fraction, message);
    lock(&s.jobs).running = Some(event.clone());
    if job.kind == RECORD_FINAL_KIND && matches!(phase, "started" | "progress") {
        // The stopped list follows the run; this event carries each step to
        // subscribers, the next state published carries the latest.
        let states = HashMap::from([(
            session_of(job).to_owned(),
            StoppedRecordingState::Running { fraction },
        )]);
        crate::daemon::set_stopped_states(s, &states);
    }
    s.bus.publish(Event::JobProgress(event));
}

/// How a run of a job ended, as the queue records it afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reconciled {
    Done,
    /// The worker left it unfinished on purpose (a batch waiting for the
    /// rest of its group, a speaker recheck continuing in its next turn),
    /// or the agent stopped it to shut down: no attempt spent.
    Waiting,
    Failed,
}

impl Reconciled {
    fn phase(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Waiting => "waiting",
            Self::Failed => "crashed",
        }
    }
}

/// How a worker child ended.
enum Exit {
    /// It exited on its own (`true`: successfully).
    Exited(bool),
    /// The agent shut down and stopped it (planned): `pid` was the child,
    /// `token` its lease as read while it was still alive.
    Stopped {
        pid: Option<u32>,
        token: Option<String>,
    },
}

async fn run_job(s: &Arc<Shared>, job: Job) -> bool {
    let started = job_progress(&job, "started", Some(0.0), None);
    if !s.reserve_job(started) {
        return false;
    }
    progress(s, &job, "started", Some(0.0), None);
    s.publish_state();
    let id = job.id.to_string();
    let mut cmd = s
        .cfg
        .worker
        .tokio_command(s.lib.root(), &[OsStr::new("run-one"), OsStr::new(&id)]);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    // The worker's last message, carried into the final event so the
    // result (a speaker recheck's counts) outlives the run.
    let last_message: Arc<Mutex<Option<String>>> = Arc::default();
    let exit = match cmd.spawn() {
        Err(_) => Exit::Exited(false),
        Ok(mut child) => {
            let pid = child.id();
            let out = child.stdout.take().expect("piped child stdout");
            let s2 = s.clone();
            let job2 = job.clone();
            let last = Arc::clone(&last_message);
            let mut reader = tokio::spawn(async move {
                let mut lines = BufReader::new(out);
                while let Ok(Some(line)) = broscribe_platform_native::ipc::read_line(
                    &mut lines,
                    broscribe_ipc::MAX_LINE_BYTES,
                )
                .await
                {
                    // Idle, unknown and non-worker lines are no progress.
                    let Some(line) = WorkerLine::parse(&line) else {
                        continue;
                    };
                    let Some(phase) = line.phase() else {
                        continue;
                    };
                    if line.message.is_some() {
                        *lock(&*last) = line.message.clone();
                    }
                    progress(&s2, &job2, phase, line.fraction, line.message);
                }
            });
            let exit = tokio::select! {
                st = child.wait() => Exit::Exited(st.is_ok_and(|st| st.success())),
                () = s.shutdown.wait() => {
                    // Read while the child is alive, so its PID is nobody
                    // else's and the token names exactly its claim.
                    let token = match pid {
                        Some(pid) => {
                            let s3 = s.clone();
                            let id = job.id.clone();
                            tokio::task::spawn_blocking(move || holder_token(&s3, &id, pid))
                                .await
                                .ok()
                                .flatten()
                        }
                        None => None,
                    };
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    Exit::Stopped { pid, token }
                }
            };
            // Lines still in the pipe when the child exited are its last
            // words (its `job_done` message); a pipe a grandchild keeps open
            // never holds the run up for long.
            let drained = matches!(exit, Exit::Exited(_))
                && tokio::time::timeout(DRAIN, &mut reader).await.is_ok();
            if !drained {
                reader.abort();
                let _ = reader.await;
            }
            exit
        }
    };
    let s2 = s.clone();
    let job2 = job.clone();
    let complete = tokio::task::spawn_blocking(move || match exit {
        // A planned stop is no crash: its claim is released without an
        // attempt, and no crash takeover runs.
        Exit::Stopped { pid, token } => release_stopped(&s2, &job2.id, pid, token.as_deref()),
        Exit::Exited(success) => reconcile(&s2, &job2, success),
    })
    .await;
    let ended = match complete {
        Ok(Ok(ended)) => ended,
        Ok(Err(e)) => {
            tracing::warn!("reconciling a worker run failed: {}", e.code());
            Reconciled::Failed
        }
        Err(_) => Reconciled::Failed,
    };
    let done = ended == Reconciled::Done;
    let message = lock(&*last_message).take();
    progress(s, &job, ended.phase(), done.then_some(1.0), message);
    lock(&s.jobs).running = None;
    if done && job.kind == RECORD_FINAL_KIND {
        let s2 = s.clone();
        let job2 = job.clone();
        let ready = tokio::task::spawn_blocking(move || {
            let session_id = session_of(&job2);
            // One scan serves both the saved list and the notification.
            let meeting = saved_meeting(&s2, session_id);
            crate::daemon::recording_saved_as(
                &s2,
                session_id,
                meeting
                    .as_ref()
                    .map(|m| (m.id.to_string(), m.title.clone())),
            );
            meeting.map(|m| ready_notification(&m))
        })
        .await;
        if let Ok(Some(notification)) = ready {
            s.notify(notification);
        }
    }
    s.publish_state();
    true
}

/// The token of job `id`'s lease when process `pid` holds it.
fn holder_token(s: &Shared, id: &JobId, pid: u32) -> Option<String> {
    let job = Queue::open(s.lib.root()).and_then(|q| q.get(id)).ok()?;
    job.lease.filter(|l| l.pid == pid).map(|l| l.token)
}

/// After the agent stopped worker `pid` to shut down: releases its claim of
/// job `id` without counting an attempt (an interactive run is failed for
/// good, never rerun by a worker). A claim it never made, or one another
/// process holds, is left alone.
fn release_stopped(
    s: &Shared,
    id: &JobId,
    pid: Option<u32>,
    token: Option<&str>,
) -> Result<Reconciled> {
    let Some(pid) = pid else {
        return Ok(Reconciled::Waiting);
    };
    let queue = Queue::open(s.lib.root())?;
    let released = s
        .lib
        .write(|tx| queue.release_stopped_holder(tx, id, pid, token, s.probe.as_ref()))?;
    Ok(match released {
        StoppedRelease::Interrupted => Reconciled::Failed,
        StoppedRelease::HolderAlive => {
            tracing::warn!("a stopped worker's PID is alive again; its lease is left to go stale");
            Reconciled::Waiting
        }
        StoppedRelease::Released | StoppedRelease::NotHeld => Reconciled::Waiting,
    })
}

fn reconcile(s: &Shared, old: &Job, success: bool) -> Result<Reconciled> {
    let queue = Queue::open(s.lib.root())?;
    let current = match queue.get(&old.id) {
        Ok(j) => j,
        Err(broscribe_core::Error::NotFound(_)) if success => return Ok(Reconciled::Done),
        Err(e) => return Err(e),
    };
    if current.state == JobState::Done {
        lock(&WAITING).remove(&old.id);
        return Ok(Reconciled::Done);
    }
    // Another worker may have won the lease between selection and spawn.
    if queue.is_held(&current, s.probe.as_ref()) {
        return Ok(Reconciled::Waiting);
    }
    if success {
        // A worker that exits cleanly without completing the job left it
        // waiting on purpose: no attempt is spent, and it is offered again
        // after a delay instead of right away.
        lock(&WAITING).insert(old.id.clone(), Instant::now() + s.cfg.retry_backoff);
        return Ok(Reconciled::Waiting);
    }
    if current.lease.is_some() {
        // The child crashed holding the lease. Taking the dead lease over
        // counts the attempt (and fails the job once none are left).
        if let Some(lease) = queue.claim(&s.lib, &old.id, s.probe.as_ref())? {
            lease.release(&s.lib)?;
            s.lib.write(|tx| {
                queue.set_state(tx, &old.id, JobState::Failed, Some("worker crashed".into()))
            })?;
        }
    } else if current.updated_at == old.updated_at || current.state != JobState::Failed {
        // The child crashed before claiming (plain claims count nothing) or
        // after releasing the job unfinished: either way the crash consumes
        // an attempt, recorded as a failure.
        let message = if current.updated_at == old.updated_at {
            "worker exited without completing the job"
        } else {
            "worker crashed"
        };
        if let Some(lease) = queue.claim(&s.lib, &old.id, s.probe.as_ref())? {
            lease.fail(&s.lib, message)?;
            lease.release(&s.lib)?;
        }
    }
    Ok(Reconciled::Failed)
}

/// The saved (done) meeting of recording `session_id`, from one scan of the
/// library. Unreadable or unrelated meetings are skipped: they never hide
/// the saved one.
fn saved_meeting(s: &Shared, session_id: &str) -> Option<schema::Meeting> {
    let source_id = format!("recording:{session_id}");
    let dirs = match s.lib.list_meeting_dirs() {
        Ok(dirs) => dirs,
        Err(e) => {
            tracing::warn!(
                "listing meetings for a saved recording failed: {}",
                e.code()
            );
            return None;
        }
    };
    dirs.iter()
        .find_map(|dir| match s.lib.load_meeting_at(dir) {
            Ok(m) => (m.source == schema::Source::Recording
                && m.source_id.as_deref() == Some(&source_id)
                && m.processing.status == schema::ProcessingStatus::Done)
                .then_some(m),
            Err(e) => {
                tracing::debug!(code = e.code(), "skipping an unreadable meeting");
                None
            }
        })
}

/// The speakers of `meeting` still waiting for a name, as the meeting shows
/// them: a merged group counts once (through its kept speaker), and one
/// dismissed from review ("Can't tell") no longer waits.
fn speakers_needing_name(meeting: &schema::Meeting) -> usize {
    ops::effective_speakers(meeting)
        .filter(|speaker| speaker.named_person().is_none())
        .filter(|speaker| !ops::review_dismissed(meeting, speaker.id))
        .count()
}

/// "Final transcript ready" for the meeting final job `job` saved. The
/// meeting's title goes in the body only, in the curly quotes user-supplied
/// text gets inside a sentence.
fn ready_notification(meeting: &schema::Meeting) -> NotificationRequest {
    let title = meeting
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("Untitled meeting");
    let body = match speakers_needing_name(meeting) {
        0 => format!("Every speaker in “{title}” has a name."),
        1 => format!("1 speaker in “{title}” needs a name."),
        n => format!("{n} speakers in “{title}” need a name."),
    };
    NotificationRequest {
        id: format!("final-{}", meeting.id),
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Final Transcript Ready",
            "Final transcript ready",
        ),
        body,
        actions: Vec::new(),
        meeting_id: Some(meeting.id.to_string()),
    }
}

pub(crate) fn pending(shared: &Shared) -> Result<Vec<JobId>> {
    Ok(Queue::open(shared.lib.root())?
        .jobs()?
        .into_iter()
        .filter(|job| job.is_claimable() && job.attempts < shared.cfg.max_attempts)
        .map(|job| job.id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::fakes::FakeProcessProbe;

    struct Engine;
    impl broscribe_ipc::CaptureEngine for Engine {
        fn start(
            &self,
            _: broscribe_ipc::SessionConfig,
            _: broscribe_ipc::CaptureOutputs,
        ) -> Result<Box<dyn broscribe_ipc::SessionHandle>> {
            Err(broscribe_core::Error::Unsupported(
                "no synthetic capture requested".into(),
            ))
        }
        fn recover_orphans(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
        ) -> Result<broscribe_ipc::OrphanRecovery> {
            Ok(broscribe_ipc::OrphanRecovery::default())
        }
    }

    fn daemon_with(tmp: &std::path::Path, worker: crate::WorkerCommand) -> crate::Daemon {
        let mut cfg = crate::DaemonConfig::new(
            tmp.join("library"),
            tmp.join("data"),
            tmp.join("cache"),
            worker,
        );
        cfg.data_dir_overridden = true;
        crate::Daemon::new(
            cfg,
            Arc::new(Engine),
            Arc::new(FakeProcessProbe::new(std::process::id())),
            Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
        )
        .unwrap()
    }

    fn daemon(tmp: &std::path::Path) -> crate::Daemon {
        daemon_with(tmp, crate::WorkerCommand::new("/bin/false"))
    }

    #[tokio::test]
    async fn stopped_recording_waits_until_its_job_is_reserved() {
        use broscribe_ipc::{Call, Request};

        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path());
        d.finish_startup();
        let connection = d.connect();
        connection
            .handle(Request::new(Call::LibrarySwitchAcquire))
            .await
            .outcome
            .unwrap();
        let lib = &d.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        let session = "synthetic-session";
        lib.write(|tx| queue.enqueue(tx, RECORD_FINAL_KIND, session, serde_json::json!({})))
            .unwrap();

        let scan = next(&d.shared).unwrap();
        assert_eq!(
            scan.stopped[session],
            StoppedRecordingState::Queued { ahead: 0 }
        );
        let candidate = scan.job.unwrap();
        assert!(
            !d.shared
                .reserve_job(job_progress(&candidate, "started", Some(0.0), None))
        );
        drop(connection);
        assert!(
            d.shared
                .reserve_job(job_progress(&candidate, "started", Some(0.0), None))
        );
    }

    /// A synthetic worker with equivalent bodies for each platform.
    fn script_worker(dir: &std::path::Path, sh: &str, ps: &str) -> crate::WorkerCommand {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = ps;
            let path = dir.join("worker.sh");
            std::fs::write(&path, format!("#!/bin/sh\n{sh}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            crate::WorkerCommand::new(path)
        }
        #[cfg(windows)]
        {
            let _ = (dir, sh);
            crate::WorkerCommand {
                program: std::path::PathBuf::from(
                    std::env::var_os("SystemRoot").expect("Windows system directory"),
                )
                .join("System32/WindowsPowerShell/v1.0/powershell.exe"),
                prefix: [
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    // The daemon appends worker arguments; these synthetic
                    // bodies deliberately ignore them, as the Unix scripts do.
                    format!("& {{ {ps} }} #").into(),
                ]
                .into(),
                data_dir_override: None,
            }
        }
    }

    /// A done meeting saved from recording `session` and its final job.
    fn saved_recording(daemon: &crate::Daemon, session: &str) -> (JobId, schema::Meeting) {
        let lib = &daemon.shared.lib;
        let mut meeting = schema::Meeting::new(
            lib.new_meeting_id().unwrap(),
            schema::Source::Recording,
            schema::now(),
        );
        meeting.source_id = Some(format!("recording:{session}"));
        meeting.processing.status = schema::ProcessingStatus::Done;
        let queue = Queue::open(lib.root()).unwrap();
        let id = lib
            .write(|tx| {
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                queue.enqueue(
                    tx,
                    RECORD_FINAL_KIND,
                    session,
                    serde_json::json!({ "session_id": session }),
                )
            })
            .unwrap();
        (id, meeting)
    }

    /// The job progress events published for job `id`, as `(phase,
    /// fraction, message)`.
    fn progress_of(
        events: &crate::bus::Subscriber,
        id: &JobId,
    ) -> Vec<(String, Option<f64>, Option<String>)> {
        std::iter::from_fn(|| events.try_next())
            .filter_map(|e| match e {
                Event::JobProgress(p) if p.job_id == id.as_str() => {
                    Some((p.phase, p.fraction, p.message))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn job_progress_carries_the_queue_meeting_id_without_its_title() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let events = daemon.subscribe();
        let (_, meeting) = saved_recording(&daemon, "synthetic-progress");
        let library = &daemon.shared.lib;
        let queue = Queue::open(library.root()).unwrap();
        let id = library
            .write(|tx| {
                queue.enqueue(tx, "enrich", meeting.id.as_str(),
            serde_json::json!({"meeting_id": meeting.id, "title": "Synthetic telescope review"}))
            })
            .unwrap();
        for phase in ["started", "progress", "failed"] {
            progress(&daemon.shared, &queue.get(&id).unwrap(), phase, None, None);
            let event = events.try_next().unwrap();
            let Event::JobProgress(job) = event else {
                panic!("expected job progress")
            };
            assert_eq!(job.meeting_id.as_ref(), Some(&meeting.id));
            assert_eq!(job.phase, phase);
            assert_eq!(job.message, None);
        }
    }

    #[tokio::test]
    async fn record_final_notification_opens_meeting_and_clears_pending_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let _subscriber = daemon.subscribe();
        let (_, meeting) = saved_recording(&daemon, "synthetic");
        let saved = saved_meeting(&daemon.shared, "synthetic").unwrap();
        let notification = ready_notification(&saved);
        assert!(notification.actions.is_empty());
        assert_eq!(
            notification.meeting_id.as_deref(),
            Some(meeting.id.as_str())
        );
        daemon.shared.notify(notification.clone());
        assert_eq!(daemon.state().pending_notifications.len(), 1);
        let response = daemon
            .handle(broscribe_ipc::Request::new(
                broscribe_ipc::Call::NotificationAction {
                    notification_id: notification.id.clone(),
                    action: "open".into(),
                },
            ))
            .await
            .outcome
            .unwrap();
        assert_eq!(response["meeting_id"], meeting.id.as_str());
        assert!(daemon.state().pending_notifications.is_empty());

        // The focused meeting consumes the same pending row without routing.
        daemon.shared.notify(notification.clone());
        let response = daemon
            .handle(broscribe_ipc::Request::new(
                broscribe_ipc::Call::NotificationAction {
                    notification_id: notification.id,
                    action: "dismiss".into(),
                },
            ))
            .await
            .outcome
            .unwrap();
        assert_eq!(response["dismissed"], true);
        assert!(daemon.state().pending_notifications.is_empty());
    }

    /// A meeting titled `title` with speakers S1…, each set up by
    /// `speakers[i]`.
    fn meeting_with(title: Option<&str>, speakers: &[fn(&mut schema::Speaker)]) -> schema::Meeting {
        let mut meeting = schema::Meeting::new(
            broscribe_core::MeetingId::parse("m-0000aa").unwrap(),
            schema::Source::Recording,
            schema::now(),
        );
        meeting.title = title.map(str::to_owned);
        for (i, set_up) in speakers.iter().enumerate() {
            let mut speaker = schema::Speaker::new(sid(i as u32 + 1), "system");
            set_up(&mut speaker);
            meeting.speakers.push(speaker);
        }
        meeting
    }

    /// "Final transcript ready" for [`meeting_with`]'s meeting.
    fn announced(
        title: Option<&str>,
        speakers: &[fn(&mut schema::Speaker)],
    ) -> NotificationRequest {
        ready_notification(&meeting_with(title, speakers))
    }

    fn sid(n: u32) -> broscribe_core::SpeakerId {
        broscribe_core::SpeakerId::new(n).unwrap()
    }

    fn unnamed(_: &mut schema::Speaker) {}
    fn suggested(speaker: &mut schema::Speaker) {
        speaker.assignment = schema::Assignment::Suggested;
        speaker.person_id = Some(person("p-0000a1"));
    }
    fn confirmed(speaker: &mut schema::Speaker) {
        speaker.assignment = schema::Assignment::Confirmed;
        speaker.person_id = Some(person("p-0000a2"));
    }
    fn automatic(speaker: &mut schema::Speaker) {
        speaker.assignment = schema::Assignment::Auto;
        speaker.person_id = Some(person("p-0000a3"));
    }
    fn dismissed(speaker: &mut schema::Speaker) {
        speaker.review_dismissed_at = Some(schema::now());
    }
    fn person(id: &str) -> broscribe_core::PersonId {
        broscribe_core::PersonId::parse(id).unwrap()
    }

    #[test]
    fn final_transcript_notification_counts_speakers_and_names_the_meeting() {
        let every = announced(Some("Weekly sync"), &[confirmed, automatic]);
        assert_eq!(
            every.title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Final Transcript Ready",
                "Final transcript ready"
            )
        );
        assert_eq!(every.body, "Every speaker in “Weekly sync” has a name.");
        // No speakers at all is the same sentence, never "0 speakers".
        assert_eq!(
            announced(Some("Weekly sync"), &[]).body,
            "Every speaker in “Weekly sync” has a name."
        );
        // A suggestion is not a name yet.
        assert_eq!(
            announced(Some("Weekly sync"), &[confirmed, suggested]).body,
            "1 speaker in “Weekly sync” needs a name."
        );
        assert_eq!(
            announced(Some("Weekly sync"), &[unnamed, confirmed, suggested]).body,
            "2 speakers in “Weekly sync” need a name."
        );
        assert_eq!(
            announced(
                Some("Weekly sync"),
                &[unnamed as fn(&mut schema::Speaker); 12]
            )
            .body,
            "12 speakers in “Weekly sync” need a name."
        );
        for notification in [
            announced(Some("Weekly sync"), &[]),
            announced(Some("Weekly sync"), &[unnamed]),
            announced(None, &[unnamed, unnamed]),
        ] {
            assert_eq!(
                notification.title,
                broscribe_platform_native::NativePlatform::notification_title(
                    "Final Transcript Ready",
                    "Final transcript ready"
                )
            );
            assert!(notification.actions.is_empty());
            assert_eq!(notification.meeting_id.as_deref(), Some("m-0000aa"));
            assert_eq!(notification.id, "final-m-0000aa");
        }
    }

    #[test]
    fn final_transcript_notification_calls_a_meeting_without_title_untitled() {
        for title in [None, Some(""), Some("   "), Some("\t\n")] {
            assert_eq!(
                announced(title, &[unnamed, unnamed]).body,
                "2 speakers in “Untitled meeting” need a name.",
                "{title:?}"
            );
            assert_eq!(
                announced(title, &[unnamed]).body,
                "1 speaker in “Untitled meeting” needs a name.",
                "{title:?}"
            );
            assert_eq!(
                announced(title, &[confirmed]).body,
                "Every speaker in “Untitled meeting” has a name.",
                "{title:?}"
            );
        }
        // Space around a title is not shown inside the quotes.
        assert_eq!(
            announced(Some("  Weekly sync "), &[unnamed]).body,
            "1 speaker in “Weekly sync” needs a name."
        );
    }

    #[test]
    fn final_transcript_notification_leaves_out_speakers_dismissed_from_review() {
        assert_eq!(
            announced(Some("Weekly sync"), &[unnamed, dismissed, confirmed]).body,
            "1 speaker in “Weekly sync” needs a name."
        );
        assert_eq!(
            announced(Some("Weekly sync"), &[unnamed, unnamed, dismissed]).body,
            "2 speakers in “Weekly sync” need a name."
        );
        // Nobody is left waiting: the dismissed ones are not counted as
        // needing a name.
        assert_eq!(
            announced(Some("Weekly sync"), &[dismissed, dismissed, confirmed]).body,
            "Every speaker in “Weekly sync” has a name."
        );
    }

    /// A merged group is one speaker in the meeting: it counts once, by its
    /// kept speaker's name, and is dismissed only while all of it is.
    #[test]
    fn final_transcript_notification_counts_a_merged_group_once() {
        let grouped = |speakers: &[fn(&mut schema::Speaker)]| {
            let mut meeting = meeting_with(Some("Weekly sync"), speakers);
            // S1 and S2 are shown as S1.
            meeting.speaker_groups.push(schema::SpeakerGroup::new(
                sid(1),
                [sid(2)],
                schema::GroupOrigin::User,
            ));
            ready_notification(&meeting).body
        };
        assert_eq!(
            grouped(&[unnamed, unnamed, unnamed]),
            "2 speakers in “Weekly sync” need a name."
        );
        // The kept speaker's name is the group's.
        assert_eq!(
            grouped(&[confirmed, unnamed, confirmed]),
            "Every speaker in “Weekly sync” has a name."
        );
        // One undismissed member keeps the group in review.
        assert_eq!(
            grouped(&[dismissed, unnamed, confirmed]),
            "1 speaker in “Weekly sync” needs a name."
        );
        assert_eq!(
            grouped(&[dismissed, dismissed, confirmed]),
            "Every speaker in “Weekly sync” has a name."
        );
    }

    #[tokio::test]
    async fn an_unreadable_unrelated_meeting_never_hides_the_saved_one() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let (_, meeting) = saved_recording(&daemon, "readable");
        // Sorted before the saved meeting, so the scan meets it first.
        let broken = daemon
            .shared
            .lib
            .root()
            .join("2001")
            .join("01")
            .join("broken-0000aa");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(
            broken.join(broscribe_core::paths::MEETING_JSON),
            b"{ not a meeting",
        )
        .unwrap();
        let saved = saved_meeting(&daemon.shared, "readable").unwrap();
        assert_eq!(saved.id, meeting.id);
        assert!(saved_meeting(&daemon.shared, "never-saved").is_none());
    }

    #[tokio::test]
    async fn crashed_worker_counts_one_attempt_whether_or_not_it_claimed() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let lib = &daemon.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        // A child process that dies: its PID is not alive for the daemon.
        let child = FakeProcessProbe::new(4242);
        for case in ["before_claim", "holding_lease", "after_release"] {
            let id = lib
                .write(|tx| queue.enqueue(tx, "synthetic", case, serde_json::json!({})))
                .unwrap();
            let old = queue.get(&id).unwrap();
            match case {
                "holding_lease" => {
                    // Dropping the handle stops the heartbeat but keeps the
                    // lease, as a crash would.
                    drop(queue.claim(lib, &id, &child).unwrap().unwrap());
                }
                "after_release" => {
                    let lease = queue.claim(lib, &id, &child).unwrap().unwrap();
                    lease
                        .commit(lib, JobState::Analyzed, |_, _| Ok(()))
                        .unwrap();
                    lease.release(lib).unwrap();
                }
                _ => {}
            }
            assert_eq!(
                reconcile(&daemon.shared, &old, false).unwrap(),
                Reconciled::Failed
            );
            let job = queue.get(&id).unwrap();
            assert_eq!(
                (job.state, job.attempts, job.lease.is_none()),
                (JobState::Failed, 1, true),
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn a_worker_stopped_by_shutdown_spends_no_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon_with(
            tmp.path(),
            script_worker(tmp.path(), "exec sleep 30", "Start-Sleep -Seconds 30"),
        );
        let lib = &daemon.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        let id = lib
            .write(|tx| queue.enqueue(tx, "synthetic", "stopped-run", serde_json::json!({})))
            .unwrap();
        let job = queue.get(&id).unwrap();
        let events = daemon.subscribe();
        let mut starting = daemon.subscribe();
        // Shut down after reservation, when the worker may stop and hand
        // the job back. A shut-down agent cannot reserve a new worker.
        let stop = async {
            while let Some(event) = starting.next().await {
                if matches!(event, Event::JobProgress(p) if p.job_id == id.as_str() && p.phase == "started")
                {
                    daemon.trigger_shutdown();
                    return;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(run_job(&daemon.shared, job), stop)
        })
        .await
        .unwrap();
        let after = queue.get(&id).unwrap();
        assert_eq!(
            (after.state, after.attempts, after.error),
            (JobState::Queued, 0, None)
        );
        assert_eq!(
            progress_of(&events, &id).last().map(|p| p.0.as_str()),
            Some("waiting")
        );
    }

    #[tokio::test]
    async fn a_claim_of_a_worker_stopped_by_shutdown_is_released_without_an_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let lib = &daemon.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        let id = lib
            .write(|tx| queue.enqueue(tx, "synthetic", "stopped-claim", serde_json::json!({})))
            .unwrap();
        // The child claimed the job, then was stopped and reaped: its lease
        // stays behind, as the agent's kill leaves it.
        let child = FakeProcessProbe::new(4243);
        let lease = queue.claim(lib, &id, &child).unwrap().unwrap();
        let token = holder_token(&daemon.shared, &id, 4243).unwrap();
        assert_eq!(token, lease.token());
        drop(lease);
        assert_eq!(
            release_stopped(&daemon.shared, &id, Some(4243), Some(&token)).unwrap(),
            Reconciled::Waiting
        );
        let job = queue.get(&id).unwrap();
        assert_eq!(
            (job.state, job.attempts, job.lease.is_none()),
            (JobState::Queued, 0, true)
        );
        // The next start's claim takes no crashed lease over: still none spent.
        let next_run = queue
            .claim(lib, &id, daemon.shared.probe.as_ref())
            .unwrap()
            .unwrap();
        assert_eq!(next_run.claimed_job().attempts, 0);
    }

    #[tokio::test]
    async fn a_job_keeping_the_users_only_copy_is_never_scheduled() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let lib = &daemon.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        let id = lib
            .write(|tx| queue.enqueue(tx, "import", "conflicted", serde_json::json!({})))
            .unwrap();
        let kept = queue.job_dir(&id).join("audio.wav");
        std::fs::write(&kept, b"synthetic").unwrap();
        let lease = queue
            .claim(lib, &id, daemon.shared.probe.as_ref())
            .unwrap()
            .unwrap();
        lease.fail_keeping(lib, "kept", kept.clone()).unwrap();
        lease.release(lib).unwrap();
        // Even reset to queued by hand, it stays out of the schedule.
        lib.write(|tx| queue.set_state(tx, &id, JobState::Queued, None))
            .unwrap();
        assert!(next(&daemon.shared).unwrap().job.is_none());
        assert!(pending(&daemon.shared).unwrap().is_empty());
        assert!(kept.is_file());
    }

    #[tokio::test]
    async fn a_corrupt_job_record_never_blocks_a_healthy_job() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let lib = &daemon.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        let corrupt = JobId::for_key("synthetic", "corrupt-record");
        std::fs::create_dir_all(queue.job_dir(&corrupt)).unwrap();
        std::fs::write(
            queue
                .job_dir(&corrupt)
                .join(broscribe_core::queue::JOB_JSON),
            b"{ not a job",
        )
        .unwrap();
        let healthy = lib
            .write(|tx| queue.enqueue(tx, "synthetic", "healthy", serde_json::json!({})))
            .unwrap();
        let scan = next(&daemon.shared).unwrap();
        assert_eq!(scan.job.map(|j| j.id), Some(healthy.clone()));
        assert_eq!(scan.waiting, 1);
        assert_eq!(pending(&daemon.shared).unwrap(), vec![healthy]);
    }

    #[tokio::test]
    async fn a_job_the_worker_left_waiting_spends_no_attempt_and_is_not_rerun_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let lib = &daemon.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        // A manifest entry whose import is still running, or an analyzed
        // entry waiting for its batch: the worker leaves it as it was.
        for state in [JobState::Queued, JobState::Analyzed] {
            let id = lib
                .write(|tx| {
                    let id = queue.enqueue(
                        tx,
                        "manifest",
                        &format!("{state:?}"),
                        serde_json::json!({}),
                    )?;
                    queue.set_state(tx, &id, state, None)?;
                    Ok(id)
                })
                .unwrap();
            let old = queue.get(&id).unwrap();
            assert_eq!(next(&daemon.shared).unwrap().job.unwrap().id, id);
            assert_eq!(
                reconcile(&daemon.shared, &old, true).unwrap(),
                Reconciled::Waiting
            );
            let job = queue.get(&id).unwrap();
            assert_eq!((job.state, job.attempts), (state, 0), "{state:?}");
            // Held back until its wait ends, and the scheduler sleeps no
            // longer than that.
            let scan = next(&daemon.shared).unwrap();
            assert!(scan.job.is_none(), "{state:?}");
            assert_eq!(scan.deferred, 1, "{state:?}");
            assert!(
                scan.wake_in
                    .is_some_and(|w| w <= daemon.shared.cfg.retry_backoff),
                "{state:?}"
            );
            lib.write(|tx| queue.set_state(tx, &id, JobState::Done, None))
                .unwrap();
        }
    }

    #[tokio::test]
    async fn an_interrupted_interactive_job_never_starves_later_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let lib = &daemon.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        let meeting = broscribe_core::MeetingId::parse("m-0000e1").unwrap();
        let interrupted = lib
            .write(|tx| {
                queue.restart_finished(
                    tx,
                    broscribe_core::queue::ENRICH_KIND,
                    meeting.as_str(),
                    serde_json::json!({"meeting_id": meeting, "interactive": true}),
                )
            })
            .unwrap();
        // The terminal's run was interrupted: one failure, attempts left.
        let lease = queue
            .claim(lib, &interrupted, daemon.shared.probe.as_ref())
            .unwrap()
            .unwrap();
        lease.fail(lib, "interrupted").unwrap();
        lease.release(lib).unwrap();
        let job = queue.get(&interrupted).unwrap();
        assert!(job.state == JobState::Failed && job.attempts < daemon.shared.cfg.max_attempts);
        let later = lib
            .write(|tx| queue.enqueue(tx, "synthetic", "later", serde_json::json!({})))
            .unwrap();
        assert_eq!(next(&daemon.shared).unwrap().job.unwrap().id, later);
        assert_eq!(
            pending(&daemon.shared).unwrap(),
            std::slice::from_ref(&later)
        );
        lib.write(|tx| queue.set_state(tx, &later, JobState::Done, None))
            .unwrap();
        assert!(next(&daemon.shared).unwrap().job.is_none());
    }

    #[tokio::test]
    async fn final_jobs_map_to_retrying_while_attempts_remain_and_failed_after() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let lib = &daemon.shared.lib;
        let probe = daemon.shared.probe.as_ref();
        let queue = Queue::open(lib.root()).unwrap();
        let enqueue_final = |session: &str, input: serde_json::Value| {
            lib.write(|tx| queue.enqueue(tx, RECORD_FINAL_KIND, session, input))
                .unwrap()
        };
        let fail_once = |id: &JobId| {
            let lease = queue.claim(lib, id, probe).unwrap().unwrap();
            lease.fail(lib, "synthetic failure").unwrap();
            lease.release(lib).unwrap();
        };
        let retrying = enqueue_final("retrying", serde_json::json!({"session_id": "retrying"}));
        fail_once(&retrying);
        let spent = enqueue_final("spent", serde_json::json!({"session_id": "spent"}));
        for _ in 0..daemon.shared.cfg.max_attempts {
            fail_once(&spent);
        }
        // An interactive run is never retried automatically.
        let interactive = enqueue_final(
            "interactive",
            serde_json::json!({"session_id": "interactive", "interactive": true}),
        );
        fail_once(&interactive);

        let scan = next(&daemon.shared).unwrap();
        let failed = StoppedRecordingState::Failed {
            error: Some("synthetic failure".into()),
        };
        assert_eq!(scan.stopped["spent"], failed);
        assert_eq!(scan.stopped["interactive"], failed);
        let backoff = daemon.shared.cfg.retry_backoff.as_secs();
        assert!(
            matches!(
                &scan.stopped["retrying"],
                StoppedRecordingState::Retrying { attempts: 1, retry_in_s, error: Some(e) }
                    if (1..=backoff).contains(retry_in_s) && e == "synthetic failure"
            ),
            "{:?}",
            scan.stopped["retrying"]
        );
        // Nothing runs before the backoff ends; the scheduler wakes for it.
        assert!(scan.job.is_none());
        assert_eq!((scan.waiting, scan.deferred), (0, 1));
        assert!(
            scan.wake_in
                .is_some_and(|w| w <= daemon.shared.cfg.retry_backoff)
        );
    }

    #[tokio::test]
    async fn queued_final_jobs_report_how_many_run_first() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon(tmp.path());
        let lib = &daemon.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        let first = lib
            .write(|tx| queue.enqueue(tx, "synthetic", "ahead-of-finals", serde_json::json!({})))
            .unwrap();
        for session in ["queued-a", "queued-b"] {
            // Distinct creation times keep the scheduler order certain.
            std::thread::sleep(Duration::from_millis(5));
            lib.write(|tx| {
                queue.enqueue(
                    tx,
                    RECORD_FINAL_KIND,
                    session,
                    serde_json::json!({ "session_id": session }),
                )
            })
            .unwrap();
        }
        let scan = next(&daemon.shared).unwrap();
        assert_eq!(scan.job.map(|j| j.id), Some(first));
        assert_eq!(scan.waiting, 3);
        assert_eq!(
            scan.stopped["queued-a"],
            StoppedRecordingState::Queued { ahead: 1 }
        );
        assert_eq!(
            scan.stopped["queued-b"],
            StoppedRecordingState::Queued { ahead: 2 }
        );
    }

    #[tokio::test]
    async fn worker_lines_become_progress_and_keep_their_message() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = daemon_with(
            tmp.path(),
            script_worker(
                tmp.path(),
                r#"printf '%s\n' '{"event":"job_started"}' '{"event":"progress","fraction":0.5,"message":"2 of 4 meetings checked"}' '{"event":"idle"}' 'not a worker line'"#,
                r#"Write-Output '{"event":"job_started"}'; Write-Output '{"event":"progress","fraction":0.5,"message":"2 of 4 meetings checked"}'; Write-Output '{"event":"idle"}'; Write-Output 'not a worker line'"#,
            ),
        );
        let lib = &daemon.shared.lib;
        let queue = Queue::open(lib.root()).unwrap();
        let id = lib
            .write(|tx| queue.enqueue(tx, "synthetic", "worker-lines", serde_json::json!({})))
            .unwrap();
        let job = queue.get(&id).unwrap();
        let events = daemon.subscribe();
        tokio::time::timeout(Duration::from_secs(10), run_job(&daemon.shared, job))
            .await
            .unwrap();
        let message = Some("2 of 4 meetings checked".to_owned());
        assert_eq!(
            progress_of(&events, &id),
            vec![
                ("started".to_owned(), Some(0.0), None),
                ("started".to_owned(), None, None),
                ("progress".to_owned(), Some(0.5), message.clone()),
                // Exited without completing: left waiting, no attempt.
                ("waiting".to_owned(), None, message),
            ]
        );
        assert_eq!(queue.get(&id).unwrap().attempts, 0);
    }
}
