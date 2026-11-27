//! Generation jobs the user started from the app (design §4 meeting view,
//! §8.4): a summary (`enrich`) or a template run, each one queue job.
//!
//! [`GenerationWatcher`] lives in [`crate::state::AppState`] for the app's
//! lifetime, so leaving the meeting (its view is dropped) or closing the
//! window does not lose track of a job. It remembers the jobs the user
//! started; job events (the app's worker and the agent's) make the state read
//! the queue again, and once a job is done or failed the watcher's sink posts
//! a local notification ("Summary ready") unless that meeting's view
//! is the route's view *and* its window is the active one, checked when the
//! result is delivered. Titles stay on this machine.

use std::collections::BTreeMap;
use std::rc::Rc;

use broscribe_core::queue::{ENRICH_KIND, JobId, JobState, Queue, TEMPLATE_KIND, template_job_key};
use broscribe_core::schema::Meeting;
use broscribe_core::{Error, Library, MeetingId, Result};
use broscribe_ipc::NotificationRequest;
use chrono::{DateTime, FixedOffset};
use gpui::App;

/// What a generation produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenerationTarget {
    /// Summary, decisions and actions (`enrich`).
    Summary,
    /// One template's result; `name` as shown when the run started.
    Template { id: String, name: String },
}

impl GenerationTarget {
    /// The queue job of this target for `meeting` (one per meeting and
    /// target, derived from its key).
    pub fn job_id(&self, meeting: &MeetingId) -> JobId {
        match self {
            Self::Summary => JobId::for_key(ENRICH_KIND, meeting.as_str()),
            Self::Template { id, .. } => {
                JobId::for_key(TEMPLATE_KIND, &template_job_key(meeting, id))
            }
        }
    }

    /// "Summary", or the template's name.
    pub fn noun(&self) -> &str {
        match self {
            Self::Summary => "Summary",
            Self::Template { name, .. } => name,
        }
    }

    /// `created_at` of the meeting's current result for this target.
    pub fn result_created(&self, meeting: &Meeting) -> Option<DateTime<FixedOffset>> {
        match self {
            Self::Summary => meeting.enrichment.as_ref().map(|e| e.created_at),
            Self::Template { id, .. } => meeting.template_result(id).map(|r| r.created_at),
        }
    }
}

/// A queued or running job as read from the queue.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingJob {
    pub generation_run: Option<String>,
    pub running: bool,
    /// The job's `updated_at`: set when it was claimed or restarted, not by
    /// heartbeats, so it is when the current run started.
    pub since: DateTime<FixedOffset>,
    /// Claimable jobs the scheduler runs before it (0 once running).
    pub ahead: usize,
    /// `provider` and `model` of the job's input, when set.
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// What a read of a generation job found.
#[derive(Clone, Debug, PartialEq)]
pub enum JobRead {
    Cancelled,
    Superseded,
    Pending(PendingJob),
    /// Failed for good (attempts used up, or a run the worker never
    /// retries), with the queue's error.
    Failed(Option<String>),
    /// An attempt failed and the scheduler will run it again: not an end.
    Retrying(Option<String>),
    /// Done or removed (the worker removes done jobs): the target's result
    /// `created_at` now.
    Finished(Option<DateTime<FixedOffset>>),
}

fn input_text(input: &serde_json::Value, key: &str) -> Option<String> {
    input[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Reads `job` and, once it is finished, the meeting's result for `target`.
/// Reads `job.json` and `meeting.json`: background executor only.
pub fn read_job(
    lib: &Library,
    job: &JobId,
    meeting: &MeetingId,
    target: &GenerationTarget,
    expected: Option<&str>,
) -> Result<JobRead> {
    let queue = Queue::open(lib.root())?;
    let finished = || {
        Ok(JobRead::Finished(
            target.result_created(&lib.load_meeting(meeting)?),
        ))
    };
    match queue.get(job) {
        Ok(j) if expected.is_some_and(|run| j.generation_run.as_deref() != Some(run)) => {
            Ok(JobRead::Superseded)
        }
        Ok(j) if j.error.as_deref() == Some(broscribe_core::queue::GENERATION_CANCELLED) => {
            Ok(JobRead::Cancelled)
        }
        Ok(j) => match j.state {
            JobState::Queued | JobState::Running | JobState::Analyzed => {
                let running = j.state != JobState::Queued;
                Ok(JobRead::Pending(PendingJob {
                    generation_run: j.generation_run,
                    running,
                    since: j.updated_at,
                    ahead: if running { 0 } else { queue.jobs_ahead(job)? },
                    provider: input_text(&j.input, "provider"),
                    model: input_text(&j.input, "model"),
                }))
            }
            JobState::Failed if j.is_claimable() => Ok(JobRead::Retrying(j.error)),
            JobState::Failed => Ok(JobRead::Failed(j.error)),
            JobState::Done => finished(),
        },
        Err(Error::NotFound(_)) => finished(),
        Err(e) => Err(e),
    }
}

/// A model or provider that answered badly, as a generation's cause.
pub const AI_UNUSABLE: &str = "The AI provider didn't give a usable answer.";
/// A generation's stored error that says nothing BroScribe can put in words.
pub const SOMETHING_WRONG: &str = "Something went wrong.";

/// A summary or template run's stored error as the {Cause} of its failure
/// line (WRT-04), mapped where it is shown: the queue's record and a job
/// event's message are `e.to_string()`, which can name files or quote a
/// provider, and stay as they are (the job's record).
/// - A model or provider that answered badly (`inference`, `schema`,
///   `decode`) is [`AI_UNUSABLE`].
/// - The queue's own words (a run interrupted, a crashed worker, attempts
///   used up, a meeting deleted meanwhile) and every other library error are
///   said in words ([`crate::jobs::core_user_text`]).
/// - Anything else is [`SOMETHING_WRONG`], never the text itself.
///
/// Empty when nothing was recorded: the cause is left out.
pub fn generation_cause(stored: &str) -> String {
    let stored = stored.trim();
    if stored.is_empty() {
        return String::new();
    }
    let queue_words = match stored {
        "interrupted" => Some("It was stopped before it finished."),
        "worker crashed" => Some("BroScribe's background worker stopped unexpectedly."),
        "worker exhausted its attempts" => Some("Every attempt stopped unexpectedly."),
        broscribe_core::queue::REVOKED => Some("Its meeting was deleted."),
        // The run's own error when it was interrupted ("enrichment of m-…
        // was interrupted; nothing was saved").
        _ if stored.ends_with(" was interrupted; nothing was saved") => {
            Some("It was stopped before it finished. Nothing was saved.")
        }
        _ => None,
    };
    if let Some(words) = queue_words {
        return words.to_owned();
    }
    match crate::jobs::stored_error(stored) {
        Some(Error::Inference(_) | Error::Schema(_) | Error::Decode(_)) => AI_UNUSABLE.to_owned(),
        Some(error) => crate::jobs::core_user_text(&error),
        None => SOMETHING_WRONG.to_owned(),
    }
}

/// How a finished generation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ready,
    /// Why it failed, the cause of the view's failure line.
    Failed(String),
}

/// A finished (done or removed) job: ready when the result's `created_at`
/// moved since `baseline`; else failed, with the failure reported since the
/// run started if any.
pub fn finished_outcome(
    target: &GenerationTarget,
    baseline: Option<DateTime<FixedOffset>>,
    created: Option<DateTime<FixedOffset>>,
    failure: Option<String>,
) -> Outcome {
    match created {
        Some(created) if Some(created) != baseline => Outcome::Ready,
        _ => Outcome::Failed(failure.unwrap_or_else(|| {
            let what = match target {
                GenerationTarget::Summary => "a new summary".to_owned(),
                GenerationTarget::Template { name, .. } => format!("a new {name} result"),
            };
            format!("it finished without {what}; check the provider and try again")
        })),
    }
}

/// A job the user started, as the watcher remembers it.
#[derive(Clone, Debug, PartialEq)]
pub struct WatchedJob {
    pub generation_run: Option<String>,
    pub meeting_id: MeetingId,
    pub target: GenerationTarget,
    /// The target's result `created_at` when the run was requested.
    pub baseline: Option<DateTime<FixedOffset>>,
    /// Failures the app's job events had reported for the job then.
    pub failures_before: usize,
}

/// Receipt for one accepted submission, including its durable pre-run baseline.
#[derive(Clone, Debug)]
pub struct GenerationSubmission {
    pub generation_run: Option<String>,
    pub job_id: JobId,
    pub run_id: u64,
    pub baseline: Option<DateTime<FixedOffset>>,
    pub failures_before: usize,
}

/// A finished job's notice puts the meeting title only in the body; opening
/// it opens the meeting. Each meeting and generation target have one
/// replacement ID, so a newer result replaces the older notice.
pub fn notification(
    watched: &WatchedJob,
    title: Option<&str>,
    outcome: &Outcome,
) -> NotificationRequest {
    let meeting = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Untitled meeting");
    let cause = |message: &str| match crate::jobs::sentence(message) {
        cause if cause.is_empty() => "Open the meeting to try again.".to_owned(),
        cause => cause,
    };
    let (mac_title, title, body) = match (&watched.target, outcome) {
        (GenerationTarget::Summary, Outcome::Ready) => (
            "Summary Ready",
            "Summary ready",
            format!("The summary, decisions and action items for “{meeting}” are ready."),
        ),
        (GenerationTarget::Summary, Outcome::Failed(message)) => (
            "Summary Not Generated",
            "Summary not generated",
            format!(
                "BroScribe couldn't generate the summary for “{meeting}”. {}",
                cause(message)
            ),
        ),
        (GenerationTarget::Template { name, .. }, Outcome::Ready) => (
            "Template Result Ready",
            "Template result ready",
            format!("The “{name}” result for “{meeting}” is ready."),
        ),
        (GenerationTarget::Template { name, .. }, Outcome::Failed(message)) => (
            "Template Result Not Generated",
            "Template result not generated",
            format!(
                "BroScribe couldn't run “{name}” for “{meeting}”. {}",
                cause(message)
            ),
        ),
    };
    let id = match &watched.target {
        GenerationTarget::Summary => format!("generation-{}-summary", watched.meeting_id),
        GenerationTarget::Template { id, .. } => {
            format!("generation-{}-template-{id}", watched.meeting_id)
        }
    };
    NotificationRequest {
        id,
        title: broscribe_platform_native::NativePlatform::notification_title(mac_title, title),
        body,
        actions: Vec::new(),
        meeting_id: Some(watched.meeting_id.to_string()),
    }
}

/// Where finished generations are announced ([`crate::notify::NotificationHost::post_local`]
/// in the app; a recorder in tests).
pub type NotificationSink = Rc<dyn Fn(NotificationRequest, &mut App)>;

/// The jobs the user started and not finished yet.
#[derive(Default)]
pub struct GenerationWatcher {
    jobs: BTreeMap<JobId, (u64, WatchedJob)>,
    pending: BTreeMap<u64, JobId>,
    latest: BTreeMap<JobId, u64>,
    next_run: u64,
    sink: Option<NotificationSink>,
    /// A read of the jobs is in flight; `recheck` asks for another after it.
    pub(crate) checking: bool,
    pub(crate) recheck: bool,
}

impl GenerationWatcher {
    pub fn set_sink(&mut self, sink: NotificationSink) {
        self.sink = Some(sink);
    }

    pub(crate) fn sink(&self) -> Option<NotificationSink> {
        self.sink.clone()
    }

    /// Starts watching an already accepted job.
    pub fn watch(&mut self, job: JobId, watched: WatchedJob) {
        let run = self.reserve(job.clone());
        self.accept(job, run, watched);
    }

    pub(crate) fn reserve(&mut self, job: JobId) -> u64 {
        self.next_run += 1;
        self.pending.insert(self.next_run, job);
        self.next_run
    }

    /// A delayed enqueue reply must not replace a newer accepted run.
    pub(crate) fn accept(&mut self, job: JobId, run: u64, watched: WatchedJob) -> bool {
        let expected = self.pending.remove(&run);
        if expected.as_ref() != Some(&job) {
            if let Some(expected) = expected {
                self.cleanup(&expected);
            }
            return false;
        }
        if self.latest.get(&job).is_some_and(|latest| *latest > run) {
            self.cleanup(&job);
            return false;
        }
        self.latest.insert(job.clone(), run);
        self.jobs.insert(job, (run, watched));
        true
    }

    pub(crate) fn cancel_submission(&mut self, run: u64) {
        if let Some(job) = self.pending.remove(&run) {
            self.cleanup(&job);
        }
    }

    fn cleanup(&mut self, job: &JobId) {
        if !self.jobs.contains_key(job) && !self.pending.values().any(|pending| pending == job) {
            self.latest.remove(job);
        }
    }

    #[cfg(test)]
    pub(crate) fn is_watching(&self, job: &JobId) -> bool {
        self.jobs.contains_key(job)
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub(crate) fn get(&self, job: &JobId, run: u64) -> Option<&WatchedJob> {
        self.jobs
            .get(job)
            .filter(|(current, _)| *current == run)
            .map(|(_, watched)| watched)
    }

    pub(crate) fn snapshot(&self) -> Vec<(JobId, u64, WatchedJob)> {
        self.jobs
            .iter()
            .map(|(id, (run, watched))| (id.clone(), *run, watched.clone()))
            .collect()
    }

    pub(crate) fn finish(&mut self, job: &JobId, run: u64) -> Option<WatchedJob> {
        self.get(job, run)?;
        let (_, watched) = self.jobs.remove(job)?;
        self.cleanup(job);
        Some(watched)
    }

    /// The template name of a watched template job (job progress events
    /// carry only the kind; the footer names the template with it).
    pub fn template_name(&self, job_id: &str) -> Option<&str> {
        self.jobs
            .iter()
            .find(|(id, _)| id.as_str() == job_id)
            .and_then(|(_, (_, w))| match &w.target {
                GenerationTarget::Template { name, .. } => Some(name.as_str()),
                GenerationTarget::Summary => None,
            })
    }

    pub(crate) fn meeting_for(&self, job_id: &str) -> Option<&MeetingId> {
        self.jobs
            .iter()
            .find_map(|(id, (_, job))| (id.as_str() == job_id).then_some(&job.meeting_id))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use broscribe_core::schema::Meeting;
    use broscribe_platform::fakes::FakeProcessProbe;
    use gpui::{Entity, TestAppContext, VisualTestContext, WindowHandle};
    use serde_json::json;

    use super::*;
    use crate::jobs::{ProgressEvent, ProgressEventKind, WorkerBatch, WorkerMsg};
    use crate::state::{AppState, Route};
    use crate::views::meeting::MeetingView;
    use crate::views::person_picker::fixtures::{self, Synthetic};

    type Posted = Rc<RefCell<Vec<NotificationRequest>>>;

    /// Queues the meeting's summary job, has the app watch it as started
    /// by the user, then finishes it the way the worker does (publish,
    /// release, remove, report). Returns what was posted.
    fn run_summary(
        n: u32,
        fail_first: bool,
        app: &Entity<AppState>,
        lib: &Library,
        s: &Synthetic,
        posted: &Posted,
        cx: &mut TestAppContext,
    ) -> Vec<NotificationRequest> {
        let meeting = s.meeting.clone();
        let job = lib
            .write(|tx| {
                Queue::open(tx.root())?.enqueue(
                    tx,
                    ENRICH_KIND,
                    meeting.as_str(),
                    json!({ "meeting_id": meeting }),
                )
            })
            .unwrap();
        let baseline = lib
            .load_meeting(&meeting)
            .unwrap()
            .enrichment
            .map(|e| e.created_at);
        app.update(cx, |st, cx| {
            st.watch_generation(
                job.clone(),
                WatchedJob {
                    generation_run: None,
                    meeting_id: meeting.clone(),
                    target: GenerationTarget::Summary,
                    baseline,
                    failures_before: 0,
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert!(posted.borrow().is_empty(), "nothing before the job ends");
        let queue = Queue::open(lib.root()).unwrap();
        if fail_first {
            // The first attempt fails; attempts are left, so the scheduler
            // runs it again: no notice, still watched.
            let lease = queue
                .claim(lib, &job, &FakeProcessProbe::new(4242))
                .unwrap()
                .unwrap();
            lease.fail(lib, "synthetic timeout").unwrap();
            lease.release(lib).unwrap();
            assert!(queue.get(&job).unwrap().is_claimable());
            report(
                app,
                &job,
                ProgressEventKind::JobFailed,
                Some("synthetic timeout"),
                cx,
            );
            app.read_with(cx, |st, _| assert!(st.generations().is_watching(&job)));
            assert!(
                posted.borrow().is_empty(),
                "a retried attempt is not the end"
            );
        }
        let lease = queue
            .claim(lib, &job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        lease
            .commit(lib, JobState::Done, |tx, _| {
                let mut m: Meeting = tx.library().load_meeting(&meeting)?;
                m.enrichment = Some(
                    serde_json::from_value(json!({
                        "provider": "agent-a",
                        "created_at": format!("2026-11-15T00:0{n}:00Z"),
                        "summary": "Synthetic summary."
                    }))
                    .unwrap(),
                );
                tx.save_meeting(&m)
            })
            .unwrap();
        lease.release(lib).unwrap();
        lib.write(|tx| Queue::open(tx.root())?.remove_finished(tx, ENRICH_KIND))
            .unwrap();
        let mut batch = WorkerBatch::default();
        batch.push(WorkerMsg::Progress(ProgressEvent {
            event: ProgressEventKind::JobDone,
            job_id: Some(job.to_string()),
            kind: Some(ENRICH_KIND.into()),
            meeting_id: None,
            fraction: None,
            message: None,
        }));
        app.update(cx, |st, cx| st.apply_worker_batch(batch, cx));
        cx.run_until_parked();
        app.read_with(cx, |st, _| assert!(!st.generations().is_watching(&job)));
        std::mem::take(&mut *posted.borrow_mut())
    }

    /// Reports one worker event for `job`.
    fn report(
        app: &Entity<AppState>,
        job: &JobId,
        event: ProgressEventKind,
        message: Option<&str>,
        cx: &mut TestAppContext,
    ) {
        let mut batch = WorkerBatch::default();
        batch.push(WorkerMsg::Progress(ProgressEvent {
            event,
            job_id: Some(job.to_string()),
            kind: Some(ENRICH_KIND.into()),
            meeting_id: None,
            fraction: None,
            message: message.map(str::to_owned),
        }));
        app.update(cx, |st, cx| st.apply_worker_batch(batch, cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn an_attempt_the_scheduler_retries_is_not_announced_as_a_failure(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) =
            fixtures::state_with_cli(cx, dir.path(), dir.path().join("no-such-cli"));
        let posted: Posted = Rc::default();
        app.update(cx, |st, _| {
            let posted = posted.clone();
            st.set_generation_sink(Rc::new(move |request, _| posted.borrow_mut().push(request)))
        });
        let got = run_summary(1, true, &app, &lib, &s, &posted, cx);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(
            got[0].title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Summary Ready",
                "Summary ready"
            )
        );
    }

    fn activate(window: WindowHandle<MeetingView>, cx: &mut TestAppContext) {
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.active_window()), Some(window.into()));
    }

    #[gpui::test]
    fn finished_generations_notify_unless_the_meeting_is_in_view(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) =
            fixtures::state_with_cli(cx, dir.path(), dir.path().join("no-such-cli"));
        let posted: Posted = Rc::default();
        app.update(cx, |st, _| {
            let posted = posted.clone();
            st.set_generation_sink(Rc::new(move |request, _| posted.borrow_mut().push(request)))
        });
        let id = s.meeting.clone();
        app.update(cx, |st, cx| st.navigate(Route::meeting(id.clone()), cx));
        let window = cx.add_window({
            let app = app.clone();
            let id = id.clone();
            move |window, cx| MeetingView::new(app, id, window, cx)
        });
        cx.run_until_parked();

        // Shown and focused: the panel updates, nothing is posted.
        activate(window, cx);
        assert!(run_summary(1, false, &app, &lib, &s, &posted, cx).is_empty());

        // Shown but another app is in front: notified.
        VisualTestContext::from_window(window.into(), cx).deactivate_window();
        let got = run_summary(2, false, &app, &lib, &s, &posted, cx);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Summary Ready",
                "Summary ready"
            )
        );
        assert_eq!(got[0].meeting_id.as_deref(), Some(id.as_str()));
        assert!(got[0].actions.is_empty());

        // Focused again but the user navigated away: notified.
        activate(window, cx);
        app.update(cx, |st, cx| st.navigate(Route::library(), cx));
        assert_eq!(run_summary(3, false, &app, &lib, &s, &posted, cx).len(), 1);

        // Back on the meeting, then its window is closed (the view is
        // dropped): notified.
        app.update(cx, |st, cx| st.navigate(Route::meeting(id.clone()), cx));
        assert!(run_summary(4, false, &app, &lib, &s, &posted, cx).is_empty());
        window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(run_summary(5, false, &app, &lib, &s, &posted, cx).len(), 1);
    }

    #[test]
    fn outcomes_and_notifications() {
        let m = MeetingId::parse("m-0000a1").unwrap();
        let at: DateTime<FixedOffset> = "2026-11-15T10:00:00+00:00".parse().unwrap();
        let later: DateTime<FixedOffset> = "2026-11-15T10:05:00+00:00".parse().unwrap();
        let summary = GenerationTarget::Summary;
        assert_eq!(
            finished_outcome(&summary, Some(at), Some(later), None),
            Outcome::Ready
        );
        assert_eq!(
            finished_outcome(&summary, None, Some(at), Some("x".into())),
            Outcome::Ready
        );
        match finished_outcome(&summary, Some(at), Some(at), None) {
            Outcome::Failed(text) => assert!(text.contains("without a new summary"), "{text}"),
            other => panic!("{other:?}"),
        }
        let bant = GenerationTarget::Template {
            id: "synthetic-bant".into(),
            name: "Synthetic BANT".into(),
        };
        assert_eq!(
            finished_outcome(&bant, None, None, Some("provider said no".into())),
            Outcome::Failed("provider said no".into())
        );
        assert_ne!(summary.job_id(&m), bant.job_id(&m));

        let watched = WatchedJob {
            generation_run: None,
            meeting_id: m.clone(),
            target: bant.clone(),
            baseline: None,
            failures_before: 0,
        };
        let ready = notification(&watched, Some("Synthetic sync"), &Outcome::Ready);
        assert_eq!(
            ready.title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Template Result Ready",
                "Template result ready"
            )
        );
        assert_eq!(
            ready.body,
            "The “Synthetic BANT” result for “Synthetic sync” is ready."
        );
        assert_eq!(ready.meeting_id.as_deref(), Some(m.as_str()));
        assert!(ready.actions.is_empty());
        let failed = notification(&watched, None, &Outcome::Failed("boom".into()));
        assert_eq!(
            failed.title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Template Result Not Generated",
                "Template result not generated"
            )
        );
        assert_eq!(
            failed.body,
            "BroScribe couldn't run “Synthetic BANT” for “Untitled meeting”. Boom."
        );
        assert_eq!(ready.id, failed.id);
        assert_eq!(ready.id, format!("generation-{m}-template-synthetic-bant"));
        // An unknown cause says what to do, never "unknown error".
        let unknown = notification(&watched, None, &Outcome::Failed(String::new()));
        assert_eq!(
            unknown.body,
            "BroScribe couldn't run “Synthetic BANT” for “Untitled meeting”. Open the meeting to try again."
        );
        let watched_summary = WatchedJob {
            target: summary,
            ..watched.clone()
        };
        let ready_summary = notification(&watched_summary, Some("Synthetic sync"), &Outcome::Ready);
        assert_eq!(
            ready_summary.title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Summary Ready",
                "Summary ready"
            )
        );
        assert_eq!(
            ready_summary.body,
            "The summary, decisions and action items for “Synthetic sync” are ready."
        );
        assert_eq!(ready_summary.id, format!("generation-{m}-summary"));
        let failed_summary = notification(
            &watched_summary,
            Some("Synthetic sync"),
            &Outcome::Failed("Try another provider.".into()),
        );
        assert_eq!(
            failed_summary.title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Summary Not Generated",
                "Summary not generated"
            )
        );
        assert_eq!(
            failed_summary.body,
            "BroScribe couldn't generate the summary for “Synthetic sync”. Try another provider."
        );
        assert_eq!(failed_summary.id, ready_summary.id);

        let job = bant.job_id(&m);
        let mut watcher = GenerationWatcher::default();
        watcher.watch(job.clone(), watched);
        assert_eq!(watcher.template_name(job.as_str()), Some("Synthetic BANT"));
        let run = watcher.snapshot()[0].1;
        assert!(watcher.finish(&job, run).is_some());
        assert!(watcher.is_empty());
    }
}

/// WRT-04: a generation's stored error (the queue's record or a job
/// event's message) is said in words: never its path, its provider's
/// words, the system's error or its own prefix.
#[cfg(test)]
mod stored_error_tests {
    use super::*;

    #[test]
    fn stored_errors_are_said_in_words() {
        const PRIVATE: &str = "/Users/synthetic-private/Library/BroScribe/meetings/zq5";
        // Win32 and POSIX assign different numbers to the same error kinds.
        let denied = std::io::Error::from_raw_os_error(if cfg!(windows) { 5 } else { 13 });
        let disk_full = std::io::Error::from_raw_os_error(if cfg!(windows) { 112 } else { 28 });
        for (stored, words) in [
            (String::new(), ""),
            (
                format!("inference error: synthetic-zq5 returned {PRIVATE}/reply (x)"),
                AI_UNUSABLE,
            ),
            (
                "schema error: synthetic-zq5: the reply is not JSON".to_owned(),
                AI_UNUSABLE,
            ),
            (format!("decode error: {PRIVATE}/reply"), AI_UNUSABLE),
            (
                format!("{PRIVATE}/meeting.json: {disk_full}"),
                "BroScribe couldn't read or write a file.",
            ),
            (
                format!("{PRIVATE}/meeting.json: {denied}"),
                "BroScribe isn't allowed to open it.",
            ),
            (format!("not found: {PRIVATE}"), "It no longer exists."),
            (
                "consent required: synthetic-zq5 is not enabled".to_owned(),
                "The provider isn't turned on.",
            ),
            (
                format!("model missing: {PRIVATE}/model.onnx"),
                "A model it needs isn't installed.",
            ),
            (
                format!("invalid input: {PRIVATE}/template.md: the body is empty"),
                "template.md: the body is empty.",
            ),
            (
                format!("invalid input: {PRIVATE}/config.toml: TOML parse error at line 2"),
                crate::jobs::SETTINGS_FILE_UNREADABLE,
            ),
            (
                "interrupted".to_owned(),
                "It was stopped before it finished.",
            ),
            (
                "enrichment of m-0000a1 was interrupted; nothing was saved".to_owned(),
                "It was stopped before it finished. Nothing was saved.",
            ),
            (
                "worker crashed".to_owned(),
                "BroScribe's background worker stopped unexpectedly.",
            ),
            (
                "worker exhausted its attempts".to_owned(),
                "Every attempt stopped unexpectedly.",
            ),
            (
                broscribe_core::queue::REVOKED.to_owned(),
                "Its meeting was deleted.",
            ),
            (
                format!("synthetic provider said zq5 at {PRIVATE}"),
                SOMETHING_WRONG,
            ),
        ] {
            let text = generation_cause(&stored);
            assert_eq!(text, words, "{stored}");
            for raw in ["synthetic-private", "zq5", "os error", "error:", "/Users"] {
                assert!(!text.contains(raw), "{raw} shown for {stored}: {text}");
            }
        }
    }
}

#[cfg(test)]
mod submission_tests {
    use super::*;

    fn watched() -> WatchedJob {
        WatchedJob {
            generation_run: None,
            meeting_id: MeetingId::parse("m-000001").unwrap(),
            target: GenerationTarget::Summary,
            baseline: None,
            failures_before: 0,
        }
    }

    #[test]
    fn a_previous_completion_cannot_finish_a_new_run_of_the_same_job() {
        let mut watcher = GenerationWatcher::default();
        let watched = watched();
        let job = watched.target.job_id(&watched.meeting_id);
        let first = watcher.reserve(job.clone());
        assert!(watcher.accept(job.clone(), first, watched.clone()));
        let second = watcher.reserve(job.clone());
        assert!(watcher.accept(job.clone(), second, watched));
        assert!(watcher.finish(&job, first).is_none());
        assert!(watcher.is_watching(&job));
        assert!(watcher.finish(&job, second).is_some());
    }

    #[test]
    fn a_delayed_enqueue_reply_cannot_resurrect_a_finished_newer_run() {
        let mut watcher = GenerationWatcher::default();
        let watched = watched();
        let job = watched.target.job_id(&watched.meeting_id);
        let first = watcher.reserve(job.clone());
        let second = watcher.reserve(job.clone());
        assert!(watcher.accept(job.clone(), second, watched.clone()));
        assert!(watcher.finish(&job, second).is_some());
        assert!(!watcher.accept(job, first, watched));
        assert!(watcher.is_empty());
    }

    #[test]
    fn a_refused_new_request_does_not_hide_an_earlier_accepted_job() {
        let mut watcher = GenerationWatcher::default();
        let watched = watched();
        let job = watched.target.job_id(&watched.meeting_id);
        let first = watcher.reserve(job.clone());
        let second = watcher.reserve(job.clone());
        watcher.cancel_submission(second);
        assert!(watcher.accept(job.clone(), first, watched));
        assert!(watcher.finish(&job, first).is_some());
    }
}
