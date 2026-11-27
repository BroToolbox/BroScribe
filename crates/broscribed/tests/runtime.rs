//! Public daemon seams, synthetic journals and fake child processes only.
use broscribe_core::{
    Result,
    process::ProcessProbe,
    queue::{JobState, Queue, RECORD_FINAL_KIND},
};
use broscribe_ipc::*;
use broscribed::{Daemon, DaemonConfig, WorkerCommand};
use std::{path::Path, sync::Arc, time::Duration};

struct Engine;
struct Session {
    id: String,
    dir: std::path::PathBuf,
    paused: bool,
    journal: Option<broscribe_capture::journal::JournalWriter>,
}
impl CaptureEngine for Engine {
    fn start(&self, c: SessionConfig, _: CaptureOutputs) -> Result<Box<dyn SessionHandle>> {
        use broscribe_capture::journal::{JournalWriter, SessionMeta, Track};
        let mut journal = JournalWriter::create(
            &c.journal_root,
            &c.session_id,
            SessionMeta {
                library_root: Some(c.library_root),
                setup: c.setup,
                title: c.title,
                app: c.app,
                language: c.language,
                tags: c.tags,
                title_detected: c.title_detected,
                echo_handling: c.echo_handling,
                in_person_setup: c.in_person_setup,
                meeting_app_seen: false,
                pinned_mic: c.mic_device,
            },
            5.0,
            std::process::id(),
        )?;
        journal.set_started_at(c.started_at.to_utc())?;
        for track in Track::ALL {
            journal.begin_segment(track, 0, None, 16000, 0)?;
            journal.append(track, 0, &vec![0.1; 32000])?;
        }
        let dir = journal.dir().to_owned();
        Ok(Box::new(Session {
            id: c.session_id,
            dir,
            paused: false,
            journal: Some(journal),
        }))
    }
    fn recover_orphans(&self, _: &Path, _: &Path) -> Result<OrphanRecovery> {
        Ok(OrphanRecovery::default())
    }
}
impl SessionHandle for Session {
    fn session_id(&self) -> &str {
        &self.id
    }
    fn health(&self) -> CaptureHealth {
        CaptureHealth::starting()
    }
    fn status(&self) -> SessionStatus {
        SessionStatus {
            status: if self.paused {
                RecordingStatus::Paused
            } else {
                RecordingStatus::Recording
            },
            elapsed_s: 10.0,
            language: None,
            setup: None,
            participants: Vec::new(),
            tags: Vec::new(),
        }
    }
    fn pause(&mut self) -> Result<()> {
        self.paused = true;
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        self.paused = false;
        Ok(())
    }
    fn set_language(&mut self, _: &str) -> Result<f64> {
        Ok(10.0)
    }
    fn bookmark(&mut self, _: Option<&str>) -> Result<f64> {
        Ok(10.0)
    }
    fn note(&mut self, _: &str) -> Result<f64> {
        Ok(10.0)
    }
    fn add_tags(&mut self, _: &[String]) -> Result<()> {
        Ok(())
    }
    fn remove_tags(&mut self, _: &[String]) -> Result<()> {
        Ok(())
    }
    fn mark_meeting_app_seen(&mut self) -> Result<()> {
        match self.journal.as_mut() {
            Some(journal) => journal.mark_meeting_app_seen(),
            None => Ok(()),
        }
    }
    fn snapshot_cutoff(&mut self) -> Result<ChunkCutoff> {
        Ok(ChunkCutoff {
            chunks: [(Track::Mic, 1), (Track::System, 2)].into(),
            recording_s: 10.0,
        })
    }
    fn participant(&mut self, _: &str, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    fn stop(mut self: Box<Self>) -> Result<StoppedSession> {
        if let Some(journal) = self.journal.take() {
            journal.finish(2_000_000_000, "user")?;
        }
        Ok(StoppedSession {
            session_id: self.id,
            journal_dir: self.dir,
        })
    }
}
struct Probe;
impl ProcessProbe for Probe {
    fn current_pid(&self) -> u32 {
        std::process::id()
    }
    fn is_alive(&self, p: u32) -> bool {
        p == std::process::id()
    }
}
/// Equivalent synthetic child commands; no external service is contacted.
fn synthetic_command(dir: &Path, name: &str, sh: &str, cmd: &str) -> WorkerCommand {
    #[cfg(unix)]
    {
        let _ = (dir, cmd);
        WorkerCommand {
            program: "/bin/sh".into(),
            prefix: vec!["-c".into(), sh.into(), name.into()],
            data_dir_override: None,
        }
    }
    #[cfg(windows)]
    {
        let _ = sh;
        let path = dir.join(format!("{name}.cmd"));
        std::fs::write(
            &path,
            format!("@echo off\r\nsetlocal DisableDelayedExpansion\r\n{cmd}\r\nexit /b 0\r\n"),
        )
        .unwrap();
        WorkerCommand::new(path)
    }
}

fn daemon(tmp: &Path) -> Daemon {
    daemon_with_accessory(tmp, None)
}
fn daemon_with_accessory(tmp: &Path, accessory: Option<WorkerCommand>) -> Daemon {
    let worker = synthetic_command(tmp, "worker", "exit 1", "exit /b 1");
    daemon_with(tmp, worker, accessory)
}
fn daemon_with(tmp: &Path, worker: WorkerCommand, accessory: Option<WorkerCommand>) -> Daemon {
    let mut cfg = DaemonConfig::new(
        tmp.join("library"),
        tmp.join("data"),
        tmp.join("cache"),
        worker,
    );
    cfg.data_dir_overridden = true;
    cfg.retry_backoff = Duration::from_millis(5);
    cfg.poll_every = Duration::from_millis(5);
    cfg.accessory = accessory;
    let admission = Arc::new(
        broscribe_core::maintenance::ModeLease::acquire(
            &cfg.data_dir,
            broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
        )
        .unwrap(),
    );
    let daemon = Daemon::new(cfg, Arc::new(Engine), Arc::new(Probe), admission).unwrap();
    daemon.set_setup_devices(
        Arc::new(broscribe_platform::devices::FakeDeviceInfo::default()),
        Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000)),
    );
    daemon.set_reader_monitor(Arc::new(
        broscribe_platform::fakes::FakeMicUsageMonitor::default(),
    ));
    // Captures are admitted once startup recovery finished.
    daemon.finish_startup();
    daemon
}
/// The session the daemon shows as recording now (empty when none).
fn current(d: &Daemon) -> String {
    d.state()
        .recording
        .map(|r| r.session_id)
        .unwrap_or_default()
}
/// "Stop and save" of the recording shown now, as every client sends it.
fn stop_active(d: &Daemon) -> Request {
    Request::new(Call::RecordStop {
        session_id: current(d),
    })
}
#[tokio::test]
async fn request_reconciliation_does_not_repeat_a_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let req = Request::new(Call::RecordStart(Default::default()));
    let id = req.id.clone();
    let first = d.handle(req.clone()).await;
    assert!(first.outcome.is_ok());
    assert_eq!(d.handle(req).await, first);
    let status = d
        .handle(Request::new(Call::RequestStatus { request_id: id }))
        .await;
    let state: RequestState = serde_json::from_value(status.outcome.unwrap()).unwrap();
    assert!(matches!(state, RequestState::Done { .. }));
    let provisional = d
        .handle(Request::new(Call::RecordProcessNow {
            session_id: current(&d),
        }))
        .await
        .outcome
        .unwrap();
    let queue = Queue::open(tmp.path().join("library")).unwrap();
    let job = queue
        .get(&broscribe_core::queue::JobId::parse(provisional["job_id"].as_str().unwrap()).unwrap())
        .unwrap();
    let input: RecordJobInput = serde_json::from_value(job.input).unwrap();
    assert_eq!(input.cutoff.unwrap().chunks[&Track::System], 2);
    assert!(d.state().recording.is_some());
    d.finish().await;
    assert_eq!(queue.list(RECORD_FINAL_KIND).unwrap().len(), 1);
}
#[tokio::test]
async fn crashed_worker_retries_then_fails_after_three_attempts() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let lib = broscribe_core::Library::open(tmp.path().join("library")).unwrap();
    let q = Queue::open(lib.root()).unwrap();
    let id = lib
        .write(|tx| q.enqueue(tx, "synthetic-crash", "fixture", serde_json::json!({})))
        .unwrap();
    d.start_background();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let job = q.get(&id).unwrap();
            if job.attempts == 3 && job.state == JobState::Failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    d.trigger_shutdown();
    d.finish().await;
    assert_eq!(q.get(&id).unwrap().attempts, 3);
}

#[tokio::test]
async fn socket_subscription_replays_pending_prompts() {
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let d = daemon(tmp.path());
    let prompt = d.ask_to_record(RecordStartParams {
        app: Some("synthetic-app".into()),
        ..Default::default()
    });
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let listener = broscribe_platform_native::ipc::Listener::bind(&data).unwrap();
    let endpoint = listener.endpoint().clone();
    let running = tokio::spawn(broscribed::server::serve(d.clone(), listener));
    let received = tokio::task::spawn_blocking(move || {
        let client = broscribe_platform_native::ipc::Client::connect_daemon(
            &endpoint,
            Duration::from_secs(10),
        )
        .unwrap()
        .expect("the agent listens");
        let mut sub = client.subscribe().unwrap();
        assert!(matches!(sub.next_event().unwrap(), Some(Event::State(_))));
        match sub.next_event().unwrap().unwrap() {
            Event::Notification(n) => n.id,
            other => panic!("unexpected event {other:?}"),
        }
    })
    .await
    .unwrap();
    assert_eq!(received, prompt);
    d.trigger_shutdown();
    tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn kick_queue_reports_pending_ids_without_finished_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let lib = broscribe_core::Library::open(tmp.path().join("library")).unwrap();
    let queue = Queue::open(lib.root()).unwrap();
    let (pending, done) = lib
        .write(|tx| {
            let pending = queue.enqueue(tx, "synthetic", "pending", serde_json::json!({}))?;
            let done = queue.enqueue(tx, "synthetic", "finished", serde_json::json!({}))?;
            queue.set_state(tx, &done, JobState::Done, None)?;
            Ok((pending, done))
        })
        .unwrap();
    let result = d
        .handle(Request::new(Call::KickQueue))
        .await
        .outcome
        .unwrap();
    assert_eq!(result["job_ids"], serde_json::json!([pending]));
    assert!(
        !result["job_ids"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(done))
    );
}

/// The scheduler forwards the worker's own progress text (counts, never
/// names) to subscribers and keeps it on the run's final event; a run that
/// leaves its job unfinished on purpose (a speaker recheck continuing in
/// its next turn) ends `waiting`, never as a failure, and spends no attempt.
#[tokio::test]
async fn worker_messages_reach_subscribers_and_a_continuation_is_not_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let script = r#"printf '%s\n' '{"event":"progress","fraction":0.5,"message":"1 of 2 meetings checked"}'"#;
    let worker = synthetic_command(
        tmp.path(),
        "worker",
        script,
        r#"echo {"event":"progress","fraction":0.5,"message":"1 of 2 meetings checked"}"#,
    );
    let d = daemon_with(tmp.path(), worker, None);
    let lib = broscribe_core::Library::open(tmp.path().join("library")).unwrap();
    let queue = Queue::open(lib.root()).unwrap();
    let id = lib
        .write(|tx| queue.enqueue(tx, "synthetic", "continuing", serde_json::json!({})))
        .unwrap();
    let mut events = d.subscribe();
    d.start_background();
    let (progress, last) = tokio::time::timeout(Duration::from_secs(5), async {
        let mut seen = None;
        loop {
            let Some(Event::JobProgress(p)) = events.next().await else {
                continue;
            };
            if p.job_id != id.to_string() {
                continue;
            }
            match p.phase.as_str() {
                "progress" if p.message.is_some() => seen = Some(p),
                "waiting" => return (seen, p),
                "crashed" | "failed" => panic!("a continuation is not a failure: {p:?}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let progress = progress.expect("the worker's progress line");
    assert_eq!(
        (
            progress.kind.as_str(),
            progress.fraction,
            progress.message.as_deref()
        ),
        ("synthetic", Some(0.5), Some("1 of 2 meetings checked"))
    );
    assert_eq!(last.message.as_deref(), Some("1 of 2 meetings checked"));
    let job = queue.get(&id).unwrap();
    assert_eq!((job.state, job.attempts), (JobState::Queued, 0));
    d.trigger_shutdown();
    d.finish().await;
}

/// Naming over IPC returns at once with the queued speaker recheck (no
/// candidates computed in the request); the recheck the scheduler runs
/// names the eligible sibling in the same meeting first, then the other
/// meeting, while the naming's `confirm-samples` job is still queued, and
/// its committed counts stay readable after it finished.
#[tokio::test]
async fn naming_over_ipc_queues_a_recheck_that_updates_siblings_source_first() {
    use broscribe_core::learning;
    use broscribe_core::schema::{
        Assignment, Centroid, Meeting, ProcessingStatus, Segment, Source, Speaker,
    };
    use broscribe_core::{MeetingId, SpeakerId};
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let data = tmp.path().join("data");
    let mut config = broscribe_core::config::Config::default();
    let model = config.profile.speaker_embedding.clone();
    config.identity.insert(
        model.clone(),
        broscribe_core::config::Thresholds {
            auto_name: Some(0.90),
            suggest: Some(0.80),
            margin: Some(0.10),
            ..Default::default()
        },
    );
    config.save(&data).unwrap();
    let lib = broscribe_core::Library::open(tmp.path().join("library")).unwrap();
    let meeting = |speakers: u32| {
        let mut m = Meeting::new(
            lib.new_meeting_id().unwrap(),
            Source::Import,
            broscribe_core::schema::now(),
        );
        m.processing.status = ProcessingStatus::Done;
        for n in 1..=speakers {
            let id = SpeakerId::new(n).unwrap();
            let mut s = Speaker::new(id, "t0");
            s.talk_s = 20.0;
            s.centroid = Some(Centroid::new(&model, &[1.0, 0.0], 2));
            m.speakers.push(s);
            m.segments.push(Segment {
                speech_overlap: Vec::new(),
                id: n,
                track: "t0".into(),
                speaker: Some(id),
                start: f64::from(n) * 5.0,
                end: f64::from(n) * 5.0 + 3.0,
                text: "synthetic words".into(),
                words: Vec::new(),
            });
        }
        m
    };
    let (a, b) = (meeting(2), meeting(1));
    lib.write(|tx| {
        for m in [&a, &b] {
            tx.create_meeting_dir(m)?;
            tx.save_meeting(m)?;
        }
        broscribe_core::index::Index::rebuild(tx).map(|_| ())
    })
    .unwrap();

    let named = d
        .handle(Request::new(Call::SpeakerName {
            meeting_id: a.id.to_string(),
            speaker_id: "S1".into(),
            person: "Ana Example".into(),
        }))
        .await
        .outcome
        .unwrap();
    let recheck = learning::job_id(&model);
    assert_eq!(named["learning"]["job_id"], serde_json::json!(recheck));
    assert_eq!(named["learning"]["source_meeting"], serde_json::json!(a.id));
    assert!(named.get("retro_candidates").is_none());
    let person = named["person_id"].as_str().unwrap().to_owned();
    let (s1, s2) = (SpeakerId::new(1).unwrap(), SpeakerId::new(2).unwrap());
    let assignment = |id: &MeetingId, speaker: SpeakerId| {
        let m = lib.load_meeting(id).unwrap();
        let s = m.speaker(speaker).unwrap();
        (s.assignment, s.person_id.as_ref().map(|p| p.to_string()))
    };
    // Saved at once; nobody else is named before the worker runs.
    assert_eq!(
        assignment(&a.id, s1),
        (Assignment::Confirmed, Some(person.clone()))
    );
    assert_eq!(assignment(&a.id, s2), (Assignment::None, None));
    assert_eq!(assignment(&b.id, s1), (Assignment::None, None));
    let waiting = learning::status(&lib, &model).unwrap().unwrap();
    assert!(waiting.requested > waiting.processed, "{waiting:?}");
    assert_eq!(waiting.source_meetings, vec![a.id.clone()]);
    let queue = Queue::open(lib.root()).unwrap();
    let stored = queue.get(&recheck).unwrap().input.to_string();
    assert!(!stored.contains("Ana Example"), "{stored}");

    let chain = broscribe_platform::DecoderChain::new(vec![Box::new(
        broscribe_platform::encoder::FakeEncodedDecoder,
    )]);
    let execution = broscribe_models::ExecutionContext::new(
        Arc::new(broscribe_platform::fakes::FakeProcessTreeControl::default()),
        Arc::new(broscribe_platform::fakes::FakeTrustStore::default()),
    );
    let env = broscribe_pipeline::Env {
        lib: &lib,
        config: &config,
        decoders: &chain,
        probe: &Probe,
        progress: None,
        execution: &execution,
    };
    let options = broscribe_pipeline::worker::WorkerOptions::new(
        tmp.path().join("cache"),
        Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
        data.clone(),
        Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
        Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
    );
    // Each run checks a bounded number of meetings; a continuation runs
    // again in its next turn.
    let mut messages = Vec::new();
    for _ in 0..5 {
        broscribe_pipeline::worker::work_job(
            &env,
            &broscribe_models::fakes::FakeModelProvider::default(),
            &options,
            &recheck,
            &mut |e: &broscribe_pipeline::worker::WorkerEvent| messages.extend(e.message.clone()),
        )
        .unwrap();
        if queue.get(&recheck).unwrap().state == JobState::Done {
            break;
        }
    }
    assert_eq!(queue.get(&recheck).unwrap().state, JobState::Done);
    assert!(
        messages.iter().all(|m| !m.contains("Ana Example")),
        "{messages:?}"
    );
    assert_eq!(
        assignment(&a.id, s2),
        (Assignment::Auto, Some(person.clone()))
    );
    assert_eq!(assignment(&b.id, s1), (Assignment::Auto, Some(person)));
    // The confirmation's sample job is not a competing job for the recheck.
    let confirm = queue.list(broscribe_core::queue::CONFIRM_KIND).unwrap();
    assert_eq!(confirm.len(), 1);
    assert_eq!(confirm[0].state, JobState::Queued);

    let finished = learning::status(&lib, &model).unwrap().unwrap();
    assert_eq!(finished.processed, finished.requested);
    let report = finished.report.expect("counts kept after completion");
    assert_eq!(
        (report.newly_named, report.replaced, report.demoted),
        (2, 0, 0)
    );
    assert_eq!(report.meetings[0].meeting_id, a.id);
    assert!(report.meetings[0].source);
    assert_eq!(report.meetings[0].newly_named, 1);
    let other = report
        .meetings
        .iter()
        .find(|m| m.meeting_id == b.id)
        .unwrap();
    assert!(!other.source);
    assert_eq!(other.newly_named, 1);
    d.finish().await;
}

struct Check(std::sync::atomic::AtomicUsize);
impl broscribed::audio_check::AudioCheck for Check {
    fn run(
        &self,
        _: f64,
        _: &Path,
        _: Option<&str>,
        _: &mut dyn FnMut(&[f32], &[f32]),
    ) -> Result<broscribe_dsp::AudioCheckReport> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(broscribe_dsp::AudioCheckReport {
            delay_ms: Some(12.5),
            leakage_db: Some(-30.0),
            correlation_peak: 0.9,
            noise_floor_dbfs: -70.0,
            recommendation: broscribe_dsp::EchoHandling::ResidueGating,
            bluetooth_call_profile: false,
        })
    }
}
#[tokio::test]
async fn audio_check_is_reconciled_and_refuses_active_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let check = Arc::new(Check(Default::default()));
    d.set_audio_check(check.clone());
    let request = Request::new(Call::AudioCheck { seconds: 3.0 });
    assert_eq!(
        d.handle(request.clone()).await.outcome.unwrap()["delay_ms"],
        12.5
    );
    assert!(d.handle(request).await.outcome.is_ok());
    assert_eq!(check.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        d.handle(Request::new(Call::RecordStart(Default::default())))
            .await
            .outcome
            .is_ok()
    );
    assert_eq!(
        d.handle(Request::new(Call::AudioCheck { seconds: 3.0 }))
            .await
            .outcome
            .unwrap_err()
            .code,
        "busy"
    );
    assert_eq!(check.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    d.finish().await;
}

struct MeetingMic;
impl broscribe_platform::MicUsageMonitor for MeetingMic {
    fn mic_users(&self) -> broscribe_platform::Result<Vec<broscribe_platform::MicUser>> {
        Ok(vec![broscribe_platform::MicUser {
            pid: 12345,
            bundle_id: Some("com.microsoft.teams2".into()),
            ..Default::default()
        }])
    }
}
#[tokio::test]
async fn detection_reload_applies_config_policies_and_reports_per_app_state() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("config.toml"),
        "[detect.policies]\nteams=\"ask\"\n",
    )
    .unwrap();
    std::fs::write(data.join("detect.toml"),"version=1\n[[apps]]\nid=\"synthetic\"\nname=\"Synthetic\"\nbundle_ids=[\"invalid.synthetic\"]\npolicy=\"off\"\n").unwrap();
    d.set_detector(broscribe_detect::Detector::new(
        broscribe_detect::DetectRules::builtin().unwrap(),
        Box::new(MeetingMic),
        Box::new(broscribe_platform::fakes::FakeWindowInspector::default()),
        Box::new(broscribe_detect::FakeClock::default()),
    ));
    assert!(
        d.handle(Request::new(Call::DetectReload))
            .await
            .outcome
            .is_ok()
    );
    let mut events = d.subscribe();
    d.start_background();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(events.next().await, Some(Event::Notification(_))) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let status = d
        .handle(Request::new(Call::DetectStatus))
        .await
        .outcome
        .unwrap();
    let apps = status["apps"].as_array().unwrap();
    assert!(
        apps.iter()
            .any(|app| app["app_id"] == "teams" && app["state"] == "asking")
    );
    assert!(
        apps.iter()
            .any(|app| app["app_id"] == "synthetic" && app["state"] == "idle")
    );
    assert!(d.state().recording.is_none());
    d.trigger_shutdown();
    d.finish().await;
}

struct Accounts {
    reloads: std::sync::atomic::AtomicUsize,
    calls: std::sync::atomic::AtomicUsize,
}
impl broscribed::integrations::IntegrationMaintenance for Accounts {
    fn list_accounts(
        &self,
    ) -> std::result::Result<Vec<broscribe_integrations::MicrosoftAccount>, IpcError> {
        Ok(Vec::new())
    }
    fn remote_authority(
        &self,
        _: &broscribe_core::calendar::AccountRef,
    ) -> std::result::Result<Option<broscribe_core::calendar::AccountAuthority>, IpcError> {
        Ok(None)
    }
    fn remote_calendar_events(
        &self,
    ) -> std::result::Result<Vec<broscribe_integrations::CalendarEvent>, IpcError> {
        Ok(Vec::new())
    }
    fn reload(&self) -> std::result::Result<(), IpcError> {
        self.reloads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn keep_alive(&self) -> std::result::Result<Vec<NotificationRequest>, IpcError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(vec![NotificationRequest {
            id: "integration-reauth-google".into(),
            title: "Sign-in required".into(),
            body: "Synthetic account needs sign-in".into(),
            actions: vec![NotificationButton {
                id: "open".into(),
                label: "Sign in".into(),
            }],
            meeting_id: None,
        }])
    }
    fn sync(
        &self,
        _: Option<&str>,
        _: bool,
    ) -> std::result::Result<Vec<broscribe_integrations::SyncRow>, IpcError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }
    fn native_calendar(
        &self,
        _: broscribe_platform::CalendarProvider,
        _: SystemCalendarAction,
    ) -> std::result::Result<broscribe_integrations::IntegrationStatus, IpcError> {
        Err(IpcError::new("not_configured", "synthetic maintenance"))
    }
    fn native_generation(
        &self,
        _: broscribe_platform::CalendarProvider,
    ) -> std::result::Result<Option<u64>, IpcError> {
        Err(IpcError::new("not_configured", "synthetic maintenance"))
    }
    fn calendar_generation(
        &self,
        _: broscribe_integrations::IntegrationId,
    ) -> std::result::Result<Option<u64>, IpcError> {
        Err(IpcError::new("not_configured", "synthetic maintenance"))
    }
    fn google_authority(
        &self,
    ) -> std::result::Result<Option<broscribe_core::calendar::GoogleCalendarStamp>, IpcError> {
        Err(IpcError::new("not_configured", "synthetic maintenance"))
    }
}
#[tokio::test]
async fn integration_reload_wakes_maintenance_and_forwards_reauth() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let accounts = Arc::new(Accounts {
        reloads: Default::default(),
        calls: Default::default(),
    });
    d.set_integrations(accounts.clone());
    let mut events = d.subscribe();
    d.start_background();
    let request = Request::new(Call::IntegrationsReload);
    assert!(d.handle(request.clone()).await.outcome.is_ok());
    assert!(d.handle(request).await.outcome.is_ok());
    assert_eq!(
        accounts.reloads.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(Event::Notification(n)) = events.next().await
                && n.id == "integration-reauth-google"
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(accounts.calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    let opened = d
        .handle(Request::new(Call::NotificationAction {
            notification_id: "integration-reauth-google".into(),
            action: "open".into(),
        }))
        .await
        .outcome
        .unwrap();
    assert_eq!(opened["integration"], "google");
    d.trigger_shutdown();
    d.finish().await;
}

#[tokio::test]
async fn shutdown_reply_has_already_closed_and_queued_capture() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    assert!(
        d.handle(Request::new(Call::RecordStart(Default::default())))
            .await
            .outcome
            .is_ok()
    );
    assert!(d.handle(Request::new(Call::Shutdown)).await.outcome.is_ok());
    assert!(d.state().recording.is_none());
    let queue = Queue::open(tmp.path().join("library")).unwrap();
    assert_eq!(queue.list(RECORD_FINAL_KIND).unwrap().len(), 1);
    assert_eq!(
        d.handle(Request::new(Call::RecordStart(Default::default())))
            .await
            .outcome
            .unwrap_err()
            .code,
        IpcError::SHUTTING_DOWN
    );
    d.finish().await;
}

#[tokio::test]
async fn record_helper_sample_is_private_scoped_and_refuses_active_tap() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let request = Request::new(Call::CaptureSample {
        session: "sample-one".into(),
        seconds: 0.1,
        purpose: "enrollment".into(),
    });
    let result = d.handle(request.clone()).await.outcome.unwrap();
    assert_eq!(d.handle(request).await.outcome.unwrap(), result);
    let path = std::path::PathBuf::from(result["path"].as_str().unwrap());
    // Unix modes; Windows keeps the files in the per-user profile ACLs.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert_eq!(&std::fs::read(&path).unwrap()[..4], b"RIFF");
    d.handle(Request::new(Call::CaptureSampleStop {
        session: "cancel-before-start".into(),
        discard: true,
    }))
    .await
    .outcome
    .unwrap();
    assert_eq!(
        d.handle(Request::new(Call::CaptureSample {
            session: "cancel-before-start".into(),
            seconds: 0.1,
            purpose: "try_it".into()
        }))
        .await
        .outcome
        .unwrap_err()
        .code,
        "cancelled"
    );
    let status = d
        .handle(Request::new(Call::PermissionsRequest {
            permission: "microphone".into(),
        }))
        .await
        .outcome
        .unwrap();
    assert_eq!(status["microphone"], "granted");
    d.handle(Request::new(Call::RecordStart(Default::default())))
        .await
        .outcome
        .unwrap();
    assert_eq!(
        d.handle(Request::new(Call::CaptureSample {
            session: "during-recording".into(),
            seconds: 0.1,
            purpose: "try_it".into()
        }))
        .await
        .outcome
        .unwrap_err()
        .code,
        "busy"
    );
    assert_eq!(
        d.handle(Request::new(Call::PermissionsRequest {
            permission: "system_audio".into()
        }))
        .await
        .outcome
        .unwrap_err()
        .code,
        "busy"
    );
    d.finish().await;
}

#[tokio::test]
async fn record_pending_prompts_clear_after_action_and_account_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let first = d.ask_to_record(Default::default());
    assert_eq!(d.state().pending_notifications.len(), 1);
    d.handle(Request::new(Call::NotificationAction {
        notification_id: first,
        action: "ignore".into(),
    }))
    .await
    .outcome
    .unwrap();
    assert!(d.state().pending_notifications.is_empty());
    let accounts = Arc::new(Accounts {
        reloads: Default::default(),
        calls: Default::default(),
    });
    d.set_integrations(accounts);
    let mut events = d.subscribe();
    d.start_background();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(events.next().await, Some(Event::Notification(_))) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let dir = broscribe_integrations::account_dir(
        &tmp.path().join("data"),
        broscribe_integrations::IntegrationId::Google,
    );
    std::fs::create_dir_all(&dir).unwrap();
    broscribe_core::fsutil::write_json_atomic(
        &dir.join("state.json"),
        &broscribe_integrations::AccountState::default(),
    )
    .unwrap();
    d.handle(Request::new(Call::IntegrationsSync {
        integration: Some("google".into()),
    }))
    .await
    .outcome
    .unwrap();
    assert!(d.state().pending_notifications.is_empty());
    d.trigger_shutdown();
    d.finish().await;
}

#[tokio::test]
async fn record_worker_consumes_actual_daemon_final_job() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    d.handle(Request::new(Call::RecordStart(RecordStartParams {
        setup: Some("headphones".into()),
        ..Default::default()
    })))
    .await
    .outcome
    .unwrap();
    let result = d.handle(stop_active(&d)).await.outcome.unwrap();
    let lib = broscribe_core::Library::open(tmp.path().join("library")).unwrap();
    let queue = Queue::open(lib.root()).unwrap();
    let job = queue
        .get(&broscribe_core::queue::JobId::parse(result["job_id"].as_str().unwrap()).unwrap())
        .unwrap();
    assert_eq!(job.kind, RECORD_FINAL_KIND);
    let chain = broscribe_platform::DecoderChain::new(vec![Box::new(
        broscribe_platform::encoder::FakeEncodedDecoder,
    )]);
    let config = broscribe_core::config::Config::default();
    let execution = broscribe_models::ExecutionContext::new(
        Arc::new(broscribe_platform::fakes::FakeProcessTreeControl::default()),
        Arc::new(broscribe_platform::fakes::FakeTrustStore::default()),
    );
    let env = broscribe_pipeline::Env {
        lib: &lib,
        config: &config,
        decoders: &chain,
        probe: &Probe,
        progress: None,
        execution: &execution,
    };
    let options = broscribe_pipeline::worker::WorkerOptions::new(
        tmp.path().join("cache"),
        Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
        tmp.path().join("data"),
        Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
        Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
    );
    let summary = broscribe_pipeline::worker::work_job(
        &env,
        &broscribe_models::fakes::FakeModelProvider::default(),
        &options,
        &job.id,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(summary.done, 1);
    assert_eq!(queue.get(&job.id).unwrap().state, JobState::Done);
    assert_eq!(lib.list_meeting_dirs().unwrap().len(), 1);
    let meeting = lib
        .load_meeting_at(&lib.list_meeting_dirs().unwrap()[0])
        .unwrap();
    assert_eq!(meeting.source, broscribe_core::schema::Source::Recording);
    assert_eq!(meeting.tracks.len(), 2);
    d.finish().await;
}

#[tokio::test]
async fn record_reader_launches_accessory_restarts_after_exit_and_publishes_observations() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("accessory-starts");
    let accessory = synthetic_command(
        tmp.path(),
        "app",
        &format!("printf x >> '{}'", marker.display()),
        r#"<nul set /p "=x" >> "%~dp0accessory-starts""#,
    );
    let d = daemon_with_accessory(tmp.path(), Some(accessory));
    d.set_reader_monitor(Arc::new(MeetingMic));
    let result = d
        .handle(Request::new(Call::RecordStart(RecordStartParams {
            app: Some("teams".into()),
            ..Default::default()
        })))
        .await
        .outcome
        .unwrap();
    let id = result["session_id"].as_str().unwrap().to_owned();
    d.start_background();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if std::fs::metadata(&marker).is_ok_and(|m| m.len() >= 2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let member = broscribe_readers::RosterMember {
        name: "Alex Example".into(),
        email: Some("alex@example.invalid".into()),
        is_self: false,
    };
    for (sequence, members) in [(1, vec![member]), (2, Vec::new())] {
        d.handle(Request::new(Call::ReaderEvents {
            session_id: id.clone(),
            batch: broscribe_readers::ReaderBatch {
                session_id: id.clone(),
                app: "teams".into(),
                pid: 12345,
                // The first reader incarnation this daemon issued.
                reader_generation: 1,
                call_observation: broscribe_readers::CallObservation {
                    sequence,
                    snapshot_started_at_ms: 0,
                    state: broscribe_readers::CallState::Unknown,
                    complete: false,
                },
                events: vec![broscribe_readers::ReaderEvent::Roster(
                    broscribe_readers::RosterEvent {
                        observed_s: 1.0,
                        members,
                    },
                )],
                health: Default::default(),
            },
        }))
        .await
        .outcome
        .unwrap();
    }
    let recording = d.state().recording.unwrap();
    assert_eq!(recording.observed_roster.len(), 1);
    assert!(recording.participants.is_empty());
    let saved: broscribe_readers::RosterEvent = broscribe_core::fsutil::read_json(
        &tmp.path().join("data/journal").join(id).join("roster.json"),
    )
    .unwrap();
    assert_eq!(saved.members, recording.observed_roster);
    d.trigger_shutdown();
    d.finish().await;
}

#[tokio::test]
async fn record_sample_cancel_is_scoped_and_discards_without_persisting_audio() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let mic = Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000));
    d.set_setup_devices(
        Arc::new(broscribe_platform::devices::FakeDeviceInfo::default()),
        mic.clone(),
    );
    let sample_daemon = d.clone();
    let sample = tokio::spawn(async move {
        sample_daemon
            .handle(Request::new(Call::CaptureSample {
                session: "active-sample".into(),
                seconds: 30.0,
                purpose: "enrollment".into(),
            }))
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while mic.started.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    d.handle(Request::new(Call::CaptureSampleStop {
        session: "other-sample".into(),
        discard: true,
    }))
    .await
    .outcome
    .unwrap();
    assert!(!sample.is_finished());
    d.handle(Request::new(Call::CaptureSampleStop {
        session: "active-sample".into(),
        discard: true,
    }))
    .await
    .outcome
    .unwrap();
    assert_eq!(sample.await.unwrap().outcome.unwrap_err().code, "cancelled");
    assert!(!tmp.path().join("cache/helper-capture").exists());
}

fn backdated_sample(root: &Path, name: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sample.wav"), b"RIFF").unwrap();
    filetime::set_file_mtime(
        &dir,
        filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - Duration::from_secs(3600),
        ),
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn record_helper_samples_are_reclaimed_when_requester_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache/helper-capture");
    let crashed = backdated_sample(&root, "sample-crashed-app-Ab12Cd");
    let d = daemon(tmp.path());
    assert!(!crashed.exists());

    // A finished sample its requester discarded is removed with it.
    let done = d
        .handle(Request::new(Call::CaptureSample {
            session: "done:sample.one".into(),
            seconds: 0.1,
            purpose: "try_it".into(),
        }))
        .await
        .outcome
        .unwrap();
    let done = std::path::PathBuf::from(done["path"].as_str().unwrap());
    assert!(done.is_file(), "published sample must exist before discard");
    d.handle(Request::new(Call::CaptureSampleStop {
        session: "done:sample.one".into(),
        discard: true,
    }))
    .await
    .outcome
    .unwrap();
    assert!(!done.parent().unwrap().exists());

    // The sweep before admitting a capture spares the active capture's samples.
    let mic = Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16000));
    d.set_setup_devices(
        Arc::new(broscribe_platform::devices::FakeDeviceInfo::default()),
        mic.clone(),
    );
    let sample_daemon = d.clone();
    let active = tokio::spawn(async move {
        sample_daemon
            .handle(Request::new(Call::CaptureSample {
                session: "active-sample".into(),
                seconds: 30.0,
                purpose: "enrollment".into(),
            }))
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while mic.started.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let kept = backdated_sample(
        &root,
        &format!(
            "sample-Ef34Gh{}",
            broscribe_platform::paths::helper_sample_session_key("active-sample")
        ),
    );
    let gone = backdated_sample(&root, "sample-gone-Ij56Kl");
    assert_eq!(
        d.handle(Request::new(Call::CaptureSample {
            session: "second-sample".into(),
            seconds: 0.1,
            purpose: "try_it".into(),
        }))
        .await
        .outcome
        .unwrap_err()
        .code,
        "busy"
    );
    assert!(!gone.exists());
    assert!(kept.exists());
    assert!(!active.is_finished());
    d.handle(Request::new(Call::CaptureSampleStop {
        session: "active-sample".into(),
        discard: true,
    }))
    .await
    .outcome
    .unwrap();
    assert_eq!(active.await.unwrap().outcome.unwrap_err().code, "cancelled");
    d.finish().await;
}

#[derive(Default)]
struct CalendarTicks(
    std::sync::atomic::AtomicUsize,
    std::sync::Mutex<Vec<broscribe_integrations::CalendarEvent>>,
    std::sync::Mutex<
        std::collections::BTreeMap<
            broscribe_core::calendar::AccountRef,
            broscribe_core::calendar::AccountAuthority,
        >,
    >,
);
impl broscribed::integrations::IntegrationMaintenance for CalendarTicks {
    fn list_accounts(
        &self,
    ) -> std::result::Result<Vec<broscribe_integrations::MicrosoftAccount>, IpcError> {
        Ok(Vec::new())
    }
    fn remote_authority(
        &self,
        account: &broscribe_core::calendar::AccountRef,
    ) -> std::result::Result<Option<broscribe_core::calendar::AccountAuthority>, IpcError> {
        Ok(self.2.lock().unwrap().get(account).cloned())
    }
    fn remote_calendar_events(
        &self,
    ) -> std::result::Result<Vec<broscribe_integrations::CalendarEvent>, IpcError> {
        Ok(self.1.lock().unwrap().clone())
    }
    fn reload(&self) -> std::result::Result<(), IpcError> {
        Ok(())
    }
    fn keep_alive(&self) -> std::result::Result<Vec<NotificationRequest>, IpcError> {
        Ok(Vec::new())
    }
    fn sync(
        &self,
        _: Option<&str>,
        _: bool,
    ) -> std::result::Result<Vec<broscribe_integrations::SyncRow>, IpcError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }
    fn native_calendar(
        &self,
        _: broscribe_platform::CalendarProvider,
        _: SystemCalendarAction,
    ) -> std::result::Result<broscribe_integrations::IntegrationStatus, IpcError> {
        Err(IpcError::new("not_configured", "synthetic maintenance"))
    }
    fn native_generation(
        &self,
        provider: broscribe_platform::CalendarProvider,
    ) -> std::result::Result<Option<u64>, IpcError> {
        Ok((provider == broscribe_platform::CalendarProvider::EventKit).then_some(7))
    }
    /// Synthetic EventKit authority; Google answers only its account/selection pair
    /// below and Microsoft remains account-qualified.
    fn calendar_generation(
        &self,
        id: broscribe_integrations::IntegrationId,
    ) -> std::result::Result<Option<u64>, IpcError> {
        Ok((id == broscribe_integrations::IntegrationId::SystemCalendar).then_some(7))
    }
    fn google_authority(
        &self,
    ) -> std::result::Result<Option<broscribe_core::calendar::GoogleCalendarStamp>, IpcError> {
        Ok(Some(GOOGLE_STAMP))
    }
}

/// The live Google account and calendar-selection generations of [`CalendarTicks`].
const GOOGLE_STAMP: broscribe_core::calendar::GoogleCalendarStamp =
    broscribe_core::calendar::GoogleCalendarStamp {
        generation: 7,
        selection_generation: 11,
    };

/// The Google priors' read-only authority: exactly the account's live account and
/// calendar-selection generations through the maintenance service,
/// `{"authority": {"generation", "selection_generation"} | null}`, never logged as a
/// mutation; without maintenance it fails closed.
#[tokio::test]
async fn google_calendar_authority_reports_only_the_google_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let err = d
        .handle(Request::new(Call::GoogleCalendarAuthority))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(err.code, "not_configured");
    d.set_integrations(Arc::new(CalendarTicks::default()));
    let request = Request::new(Call::GoogleCalendarAuthority);
    assert!(!request.call.is_mutating());
    let out = d.handle(request).await.outcome.unwrap();
    assert_eq!(
        out,
        serde_json::json!({ "authority": { "generation": 7, "selection_generation": 11 } })
    );
}

#[tokio::test]
async fn record_start_kicks_stale_calendar_maintenance_and_snapshots_only_signed_in_caches() {
    use broscribe_integrations::{
        AccountState, AccountStatus, CalendarCache, CalendarEvent, IntegrationId,
    };
    use std::sync::atomic::Ordering;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("data")).unwrap();
    std::fs::write(
        tmp.path().join("data/config.toml"),
        "[integration.google]\nclient_id=\"synthetic-google-client\"\n",
    )
    .unwrap();
    let d = daemon(tmp.path());
    let now = chrono::Utc::now();
    for id in IntegrationId::ALL {
        let dir = broscribe_integrations::account_dir(&tmp.path().join("data"), id);
        std::fs::create_dir_all(&dir).unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &dir.join("state.json"),
            &AccountState {
                generation: GOOGLE_STAMP.generation,
                selection_generation: if id == IntegrationId::Google {
                    GOOGLE_STAMP.selection_generation
                } else {
                    0
                },
                client_id: (id == IntegrationId::Google).then(|| "synthetic-google-client".into()),
                status: if id == IntegrationId::Google {
                    AccountStatus::Active
                } else {
                    AccountStatus::SignedOut
                },
                ..Default::default()
            },
        )
        .unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &dir.join("calendar.json"),
            &CalendarCache {
                events: vec![CalendarEvent {
                    id: format!("synthetic-{}", id.as_str()),
                    integration: id,
                    title: Some("Synthetic planning".into()),
                    start: now,
                    end: now + chrono::Duration::hours(1),
                    join_links: Vec::new(),
                    app: None,
                    attendees: Vec::new(),
                    // Stamped as each provider's sync stamps them: native events with
                    // their opt-in generation, Google with its account/selection pair.
                    account_generation: (id == IntegrationId::SystemCalendar).then_some(7),
                    remote: None,
                    google: (id == IntegrationId::Google).then_some(GOOGLE_STAMP),
                    ical_uid: None,
                    occurrence_id: None,
                }],
            },
        )
        .unwrap();
    }
    let ticks = Arc::new(CalendarTicks::default());
    d.set_integrations(ticks.clone());
    d.start_background();
    tokio::time::timeout(Duration::from_secs(10), async {
        while ticks.0.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let initial = ticks.0.load(Ordering::SeqCst);
    let result = d
        .handle(Request::new(Call::RecordStart(RecordStartParams {
            setup: Some("headphones".into()),
            ..Default::default()
        })))
        .await
        .outcome
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while ticks.0.load(Ordering::SeqCst) <= initial {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let journal = tmp
        .path()
        .join("data/journal")
        .join(result["session_id"].as_str().unwrap());
    let cache: CalendarCache =
        broscribe_core::fsutil::read_json(&journal.join("calendar.json")).unwrap();
    assert_eq!(cache.events.len(), 1);
    assert_eq!(cache.events[0].integration, IntegrationId::Google);
    assert_eq!(cache.events[0].title.as_deref(), Some("Synthetic planning"));
    assert_eq!(
        broscribe_capture::journal::load_manifest(&journal)
            .unwrap()
            .meta
            .setup
            .as_deref(),
        Some("headphones")
    );
    d.trigger_shutdown();
    d.finish().await;
}

#[tokio::test]
async fn recording_snapshot_revokes_only_the_target_account_and_keeps_native_google_and_other_account()
 {
    use broscribe_core::calendar::{
        AccountAuthority, AccountId, AccountRef, CalendarTarget, RemoteCalendarStamp,
        RemoteIntegration,
    };
    use broscribe_integrations::{
        AccountState, AccountStatus, CalendarCache, CalendarEvent, IntegrationId,
    };
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let now = chrono::Utc::now();
    let ticks = Arc::new(CalendarTicks::default());
    let account = |handle: &str| AccountRef {
        integration: RemoteIntegration::MicrosoftGraph,
        account_id: AccountId::parse(handle).unwrap(),
    };
    let a = account("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let b = account("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let target = CalendarTarget::Own {
        calendar_id: "same-id".into(),
    };
    let event = |id: &str, integration, remote| CalendarEvent {
        id: id.into(),
        integration,
        title: Some("Synthetic meeting".into()),
        start: now,
        end: now + chrono::Duration::hours(1),
        join_links: Vec::new(),
        app: None,
        attendees: Vec::new(),
        // Native caches are stamped with their opt-in generation, Google with its
        // account/selection pair; Microsoft events carry their account stamp instead.
        account_generation: (integration == IntegrationId::SystemCalendar).then_some(7),
        remote,
        google: (integration == IntegrationId::Google).then_some(GOOGLE_STAMP),
        ical_uid: None,
        occurrence_id: None,
    };
    for (id, integration) in [
        ("google", IntegrationId::Google),
        ("native", IntegrationId::SystemCalendar),
    ] {
        let dir = broscribe_integrations::account_dir(&tmp.path().join("data"), integration);
        std::fs::create_dir_all(&dir).unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &dir.join("state.json"),
            &AccountState {
                generation: 7,
                selection_generation: if integration == IntegrationId::Google {
                    GOOGLE_STAMP.selection_generation
                } else {
                    0
                },
                status: AccountStatus::Active,
                client_id: (integration == IntegrationId::Google)
                    .then(|| "synthetic-google-client".into()),
                ..Default::default()
            },
        )
        .unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &dir.join("calendar.json"),
            &CalendarCache {
                events: vec![event(id, integration, None)],
            },
        )
        .unwrap();
    }
    std::fs::write(
        tmp.path().join("data/config.toml"),
        "[integration.google]\nclient_id=\"synthetic-google-client\"\n",
    )
    .unwrap();
    for (id, account) in [("account-a", a.clone()), ("account-b", b.clone())] {
        ticks.2.lock().unwrap().insert(
            account.clone(),
            AccountAuthority {
                account: account.clone(),
                generation: 7,
                selection_generation: 11,
                selected_calendars: vec![target.clone()],
            },
        );
        ticks.1.lock().unwrap().push(event(
            id,
            IntegrationId::MicrosoftGraph,
            Some(RemoteCalendarStamp {
                account,
                generation: 7,
                selection_generation: 11,
                target: target.clone(),
            }),
        ));
    }
    d.set_integrations(ticks.clone());
    d.start_background();
    let started = d
        .handle(Request::new(Call::RecordStart(Default::default())))
        .await
        .outcome
        .unwrap();
    let journal = tmp
        .path()
        .join("data/journal")
        .join(started["session_id"].as_str().unwrap());
    let ids = || {
        let Ok(cache) =
            broscribe_core::fsutil::read_json::<CalendarCache>(&journal.join("calendar.json"))
        else {
            return Vec::new();
        };
        let mut ids = cache
            .events
            .into_iter()
            .map(|event| event.id)
            .collect::<Vec<_>>();
        ids.sort();
        ids
    };
    eventually("all independent calendar contexts", || {
        ids() == ["account-a", "account-b", "google", "native"]
    })
    .await;
    ticks.2.lock().unwrap().remove(&a);
    ticks.1.lock().unwrap().retain(|event| {
        event
            .remote
            .as_ref()
            .is_some_and(|stamp| stamp.account != a)
    });
    d.handle(Request::new(Call::IntegrationsReload))
        .await
        .outcome
        .unwrap();
    eventually("only A removed", || {
        ids() == ["account-b", "google", "native"]
    })
    .await;
    let live_b = d
        .handle(Request::new(Call::MicrosoftCalendarAuthority {
            account: b.clone(),
        }))
        .await
        .outcome
        .unwrap();
    assert_eq!(
        live_b["authority"]["account"]["account_id"],
        b.account_id.as_str()
    );
    assert_eq!(live_b["authority"]["generation"], 7);
    let live_a = d
        .handle(Request::new(Call::MicrosoftCalendarAuthority {
            account: a,
        }))
        .await
        .outcome
        .unwrap();
    assert!(live_a["authority"].is_null());
    assert!(d.state().recording.is_some());
    d.handle(stop_active(&d)).await.outcome.unwrap();
    d.finish().await;
}

/// The platform fake, except that the next permission request waits for the test: an open
/// macOS prompt the user has not answered yet. No real calendar or permission is touched.
struct SlowPrompt {
    fake: broscribe_platform::FakeSystemCalendar,
    prompt: std::sync::Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}
impl broscribe_platform::SystemCalendar for SlowPrompt {
    fn authorization(&self) -> Result<broscribe_platform::CalendarAuthorization> {
        self.fake.authorization()
    }
    fn request_access(&self) -> Result<broscribe_platform::CalendarAuthorization> {
        let prompt = self.prompt.lock().unwrap().take();
        if let Some((shown, answered)) = prompt {
            shown.send(()).unwrap();
            answered.recv().unwrap();
        }
        self.fake.request_access()
    }
    fn calendars(&self) -> Result<Vec<broscribe_platform::SystemCalendarInfo>> {
        self.fake.calendars()
    }
    fn events(
        &self,
        from_s: i64,
        to_s: i64,
        calendar_ids: &[String],
    ) -> Result<Vec<broscribe_platform::SystemCalendarEvent>> {
        self.fake.events(from_s, to_s, calendar_ids)
    }
}

async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test]
async fn record_system_calendar_prompt_never_blocks_capture_and_revocation_fences_snapshots() {
    use broscribe_integrations::{AccountStatus, CalendarCache, IntegrationStatus};
    use broscribe_platform::{CalendarAuthorization, SystemCalendarEvent, SystemCalendarInfo};
    use std::sync::atomic::Ordering;
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let events = d.subscribe();
    let (shown_tx, shown) = std::sync::mpsc::channel();
    let (answer, answered_rx) = std::sync::mpsc::channel();
    let calendar = Arc::new(SlowPrompt {
        fake: Default::default(),
        prompt: std::sync::Mutex::new(Some((shown_tx, answered_rx))),
    });
    *calendar.fake.request_result.lock().unwrap() = CalendarAuthorization::Granted;
    *calendar.fake.calendars.lock().unwrap() = vec![SystemCalendarInfo {
        id: "synthetic-work".into(),
        title: "Synthetic work".into(),
        source: "Synthetic".into(),
    }];
    let start = chrono::Utc::now().timestamp() + 300;
    *calendar.fake.events.lock().unwrap() = vec![SystemCalendarEvent {
        id: "synthetic-event".into(),
        calendar_id: "synthetic-work".into(),
        ical_uid: None,
        original_start_s: start,
        title: Some("Synthetic sync".into()),
        start_s: start,
        end_s: start + 1800,
        links: Vec::new(),
        attendees: Vec::new(),
    }];
    let secrets = Arc::new(broscribe_platform::fakes::FakeSecretStore::default());
    let browser = Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default());
    let accounts = Arc::new(
        broscribed::integrations::AccountMaintenance::new(
            tmp.path().join("data"),
            secrets.clone(),
            browser.clone(),
        )
        .with_system_calendar(calendar.clone()),
    );
    broscribed::integrations::IntegrationMaintenance::reload(accounts.as_ref()).unwrap();
    d.set_integrations(accounts);
    d.start_background();
    let native = |action| Request::new(Call::SystemCalendar { action });
    let status =
        |value: serde_json::Value| -> IntegrationStatus { serde_json::from_value(value).unwrap() };
    let authority = || {
        let d = d.clone();
        async move {
            d.handle(Request::new(Call::SystemCalendarAuthority))
                .await
                .outcome
                .unwrap()
        }
    };

    // An explicit connect opens the macOS prompt, which the user leaves unanswered for now.
    let connect = native(SystemCalendarAction::Connect);
    let pending = tokio::spawn({
        let d = d.clone();
        let connect = connect.clone();
        async move { d.handle(connect).await }
    });
    tokio::task::spawn_blocking(move || shown.recv().unwrap())
        .await
        .unwrap();
    // The same request is reconciled, never prompted twice; another request is busy.
    assert_eq!(
        d.handle(connect.clone()).await.outcome.unwrap_err().code,
        IpcError::IN_PROGRESS
    );
    assert_eq!(
        d.handle(native(SystemCalendarAction::Connect))
            .await
            .outcome
            .unwrap_err()
            .code,
        "busy"
    );
    let read = d
        .handle(native(SystemCalendarAction::Status))
        .await
        .outcome
        .unwrap();
    assert_eq!(status(read).state.status, AccountStatus::SignedOut);
    // Capture admission is independent of the calendar prompt.
    d.handle(Request::new(Call::RecordStart(Default::default())))
        .await
        .outcome
        .unwrap();
    d.handle(stop_active(&d)).await.outcome.unwrap();
    answer.send(()).unwrap();
    let connected = pending.await.unwrap();
    let opted_in = status(connected.outcome.clone().unwrap());
    assert_eq!(opted_in.state.status, AccountStatus::Active);
    assert!(opted_in.state.native_calendar_ids.is_empty());
    assert_eq!(
        opted_in.system_calendar.unwrap().calendars[0].id,
        "synthetic-work"
    );
    assert_eq!(d.handle(connect).await, connected);
    assert_eq!(calendar.fake.access_requests.load(Ordering::SeqCst), 1);

    // Choose a calendar, read it and snapshot it into a recording.
    let configured = d
        .handle(native(SystemCalendarAction::Configure {
            calendar_ids: vec!["synthetic-work".into()],
        }))
        .await
        .outcome
        .unwrap();
    let generation = status(configured).state.generation;
    d.handle(Request::new(Call::IntegrationsSync {
        integration: Some("system-calendar".into()),
    }))
    .await
    .outcome
    .unwrap();
    assert_eq!(
        authority().await,
        serde_json::json!({ "generation": generation })
    );
    let session = d
        .handle(Request::new(Call::RecordStart(Default::default())))
        .await
        .outcome
        .unwrap();
    let snapshot = tmp
        .path()
        .join("data/journal")
        .join(session["session_id"].as_str().unwrap())
        .join("calendar.json");
    let stamps = || -> Vec<Option<u64>> {
        let cache: CalendarCache = broscribe_core::fsutil::read_json(&snapshot).unwrap();
        cache.events.iter().map(|e| e.account_generation).collect()
    };
    assert_eq!(stamps(), [Some(generation)]);

    // Revoked in System Settings: the live authority answers at once, and the next
    // snapshot leaves the cached events out without waiting for a sync.
    *calendar.fake.authorization.lock().unwrap() = CalendarAuthorization::Denied;
    assert_eq!(authority().await, serde_json::json!({ "generation": null }));
    d.handle(Request::new(Call::IntegrationsReload))
        .await
        .outcome
        .unwrap();
    eventually("revoked snapshot", || stamps().is_empty()).await;

    // The revocation fenced the opt-in durably: granting again never revives the old copy or
    // its stamp. The reload's sync reads the calendar again under the new generation, and
    // only that fresh copy reaches the recording.
    *calendar.fake.authorization.lock().unwrap() = CalendarAuthorization::Granted;
    d.handle(Request::new(Call::IntegrationsReload))
        .await
        .outcome
        .unwrap();
    eventually(
        "re-granted snapshot",
        || matches!(stamps()[..], [Some(stamp)] if stamp > generation),
    )
    .await;
    let regranted = authority().await["generation"].as_u64().unwrap();
    assert!(regranted > generation);
    assert_eq!(stamps(), [Some(regranted)]);
    let disconnected = d
        .handle(native(SystemCalendarAction::Disconnect))
        .await
        .outcome
        .unwrap();
    let disconnected = status(disconnected);
    assert_eq!(disconnected.state.status, AccountStatus::SignedOut);
    assert!(disconnected.state.generation > generation);
    assert_eq!(authority().await, serde_json::json!({ "generation": null }));
    eventually("disconnected snapshot", || stamps().is_empty()).await;

    d.handle(stop_active(&d)).await.outcome.unwrap();
    // The fixture's failing worker can post recording failures. Calendar access
    // itself never uses credentials, opens a browser, or posts a sign-in notice.
    assert!(secrets.secrets.lock().unwrap().is_empty());
    assert!(browser.opened.lock().unwrap().is_empty());
    let is_sign_in = |notice: &NotificationRequest| notice.id.starts_with("integration-reauth-");
    assert!(!d.state().pending_notifications.iter().any(is_sign_in));
    d.trigger_shutdown();
    d.finish().await;
    assert!(
        !std::iter::from_fn(|| events.try_next())
            .any(|event| { matches!(event, Event::Notification(notice) if is_sign_in(&notice)) })
    );
}

fn show_main_window_count(subscriber: &broscribed::bus::Subscriber) -> usize {
    std::iter::from_fn(|| subscriber.try_next())
        .filter(|event| matches!(event, Event::ShowMainWindow))
        .count()
}

#[tokio::test]
async fn app_show_reaches_each_subscriber_once_per_call_and_counts_them() {
    let tmp = tempfile::tempdir().unwrap();
    let d = daemon(tmp.path());
    let show = || d.handle(Request::new(Call::AppShow));
    assert_eq!(
        show().await.outcome.unwrap(),
        serde_json::json!({ "subscribers": 0 })
    );
    let holder = d.subscribe();
    for _ in 0..2 {
        assert_eq!(
            show().await.outcome.unwrap(),
            serde_json::json!({ "subscribers": 1 })
        );
        assert_eq!(show_main_window_count(&holder), 1);
    }
    let second = d.subscribe();
    assert_eq!(
        show().await.outcome.unwrap(),
        serde_json::json!({ "subscribers": 2 })
    );
    assert_eq!(show_main_window_count(&holder), 1);
    assert_eq!(show_main_window_count(&second), 1);
    // Not remembered by request ID: every call shows the window again.
    let repeated = Request::new(Call::AppShow);
    d.handle(repeated.clone()).await.outcome.unwrap();
    d.handle(repeated).await.outcome.unwrap();
    assert_eq!(show_main_window_count(&holder), 2);
    d.finish().await;
}

/// Starts the agent with `config_toml` and reports how often the synthetic
/// accessory app was started.
async fn accessory_starts_at_login(config_toml: Option<&str>, subscribed: bool) -> usize {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    if let Some(text) = config_toml {
        std::fs::write(data.join("config.toml"), text).unwrap();
    }
    let marker = tmp.path().join("accessory-starts");
    let accessory = synthetic_command(
        tmp.path(),
        "app",
        &format!(
            "[ \"$1\" = --library ] && [ \"$2\" = '{}' ] && [ \"$3\" = --accessory ] && [ \"$BROSCRIBE_DATA_DIR\" = '{}' ] && printf x >> '{}'",
            tmp.path().join("library").display(),
            data.display(),
            marker.display()
        ),
        r#"if "%~1"=="--library" (if "%~2"=="%~dp0library" (if "%~3"=="--accessory" (if "%BROSCRIBE_DATA_DIR%"=="%~dp0data" (<nul set /p "=x" >> "%~dp0accessory-starts") else (exit /b 9)) else (exit /b 9)) else (exit /b 9)) else (exit /b 9)"#,
    );
    let d = daemon_with_accessory(tmp.path(), Some(accessory));
    let _app = subscribed.then(|| d.subscribe());
    d.launch_at_login(&broscribed::daemon::config_or_default(&data));
    // Wait for a started child's marker, then leave time for a duplicate.
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let starts = std::fs::metadata(&marker).map_or(0, |m| m.len() as usize);
    d.finish().await;
    starts
}

#[tokio::test]
async fn agent_start_opens_the_menu_bar_app_once_when_open_at_login_is_on() {
    assert_eq!(accessory_starts_at_login(None, false).await, 1);
    assert_eq!(
        accessory_starts_at_login(Some("[app]\nopen_at_login = true\n"), false).await,
        1
    );
    assert_eq!(
        accessory_starts_at_login(Some("[app]\nopen_at_login = false\n"), false).await,
        0
    );
    // An app already attached: nothing to open.
    assert_eq!(accessory_starts_at_login(None, true).await, 0);
}

/// Saves and indexes one synthetic meeting in `tmp/library` whose title a
/// search for "zebra" finds.
fn indexed_meeting(tmp: &Path) -> broscribe_core::MeetingId {
    indexed_meeting_titled(tmp, "Synthetic zebra review", "zebra")
}

/// Saves and indexes one synthetic meeting titled `title` in `tmp/library`,
/// the only one a search for `word` finds.
fn indexed_meeting_titled(tmp: &Path, title: &str, word: &str) -> broscribe_core::MeetingId {
    use broscribe_core::schema::{Meeting, Source};
    let lib = broscribe_core::Library::open(tmp.join("library")).unwrap();
    let mut meeting = Meeting::new(
        lib.new_meeting_id().unwrap(),
        Source::Import,
        broscribe_core::schema::now(),
    );
    meeting.title = Some(title.into());
    lib.write(|tx| {
        tx.create_meeting_dir(&meeting)?;
        tx.save_meeting(&meeting)?;
        broscribe_core::index::Index::open(tx.root())?.upsert_meeting(tx, &meeting)
    })
    .unwrap();
    assert_eq!(search(tmp, word), Some(vec![meeting.id.clone()]));
    meeting.id
}

/// The meetings a keyword search finds, read as the CLI and MCP read them;
/// `None` while the index cannot answer searches.
fn search(tmp: &Path, text: &str) -> Option<Vec<broscribe_core::MeetingId>> {
    use broscribe_core::index::{Index, SearchQuery};
    let index = Index::open_readonly(&tmp.join("library")).ok()?;
    let hits = index
        .search_meetings(&SearchQuery {
            text: Some(text.into()),
            ..SearchQuery::default()
        })
        .unwrap();
    Some(hits.into_iter().map(|hit| hit.meeting_id).collect())
}

/// The index state the daemon reports over `status`, which its state agrees with.
async fn index_state(d: &Daemon) -> IndexState {
    let status = d.handle(Request::new(Call::Status)).await.outcome.unwrap();
    let reported: IndexState = serde_json::from_value(status["index"].clone()).unwrap();
    assert_eq!(reported, d.state().index);
    reported
}

/// A library whose index was deleted, or recreated incomplete, before the
/// agent starts holds a saved, indexed meeting the empty pending list no
/// longer names: startup rebuilds it under the library lock before
/// reporting it current, and search finds the meeting again.
#[tokio::test]
async fn startup_rebuilds_a_missing_or_incomplete_index_before_reporting_it_current() {
    use broscribe_core::index::{Index, IndexDatabase};
    for incomplete in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let id = indexed_meeting(tmp.path());
        let root = tmp.path().join("library");
        std::fs::remove_dir_all(root.join(broscribe_core::paths::INDEX_DIR)).unwrap();
        let expected = if incomplete {
            drop(Index::open(&root).unwrap());
            IndexDatabase::Incomplete
        } else {
            IndexDatabase::Missing
        };
        let broken = Index::health(&root).unwrap();
        assert_eq!(broken.database, expected);
        assert!(
            !broken.is_current() && broken.meetings.is_empty(),
            "{broken:?}"
        );
        assert_eq!(search(tmp.path(), "zebra"), None);

        let d = daemon(tmp.path());
        assert_eq!(index_state(&d).await, IndexState::Current);
        assert_eq!(search(tmp.path(), "zebra"), Some(vec![id]));
        let healthy = Index::health(&root).unwrap();
        assert_eq!(healthy.database, IndexDatabase::Ready);
        assert!(healthy.is_current(), "{healthy:?}");
    }
}

/// After a healthy start, index maintenance reads the index on disk: an
/// index deleted, or recreated incomplete by another process's write, is
/// rebuilt so search finds the meeting again, and a save whose index
/// update never ran is re-indexed. The empty library it started with was
/// current with nothing to index.
#[tokio::test]
async fn index_maintenance_repairs_an_index_lost_after_a_healthy_start() {
    use broscribe_core::index::{Index, IndexDatabase};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("library");
    let d = daemon(tmp.path());
    assert_eq!(index_state(&d).await, IndexState::Current);
    assert!(Index::health(&root).unwrap().is_current());

    let id = indexed_meeting(tmp.path());
    d.maintain_index();
    assert_eq!(index_state(&d).await, IndexState::Current);

    let index_dir = root.join(broscribe_core::paths::INDEX_DIR);
    for recreated in [false, true] {
        std::fs::remove_dir_all(&index_dir).unwrap();
        if recreated {
            // Any write bootstraps a missing database, incomplete here.
            broscribe_core::Library::open(&root)
                .unwrap()
                .write(|_| Ok(()))
                .unwrap();
            assert_eq!(
                Index::health(&root).unwrap().database,
                IndexDatabase::Incomplete
            );
        } else {
            assert_eq!(
                Index::health(&root).unwrap().database,
                IndexDatabase::Missing
            );
        }
        assert_eq!(search(tmp.path(), "zebra"), None);

        d.maintain_index();
        assert_eq!(index_state(&d).await, IndexState::Current);
        assert_eq!(search(tmp.path(), "zebra"), Some(vec![id.clone()]));
        assert!(Index::health(&root).unwrap().is_current());
    }

    // A save whose index update never ran (a crash between the two).
    let lib = broscribe_core::Library::open(&root).unwrap();
    let mut meeting = lib.load_meeting(&id).unwrap();
    meeting.title = Some("Synthetic zebra quokka review".into());
    lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
    let pending = Index::health(&root).unwrap();
    assert_eq!(pending.database, IndexDatabase::Ready);
    assert_eq!(
        pending.meetings,
        std::collections::BTreeSet::from([id.clone()])
    );
    assert_eq!(search(tmp.path(), "quokka"), Some(Vec::new()));

    d.maintain_index();
    assert_eq!(index_state(&d).await, IndexState::Current);
    assert_eq!(search(tmp.path(), "quokka"), Some(vec![id]));
    assert!(Index::health(&root).unwrap().is_current());
}

/// A rebuild that could not read one of two meetings keeps asking for a
/// rebuild: the agent shows the index pending, never current, at every
/// maintenance pass while the file stays unreadable, the other meeting
/// answering searches all along; once the file reads again (restored as it
/// was, with no write listing it anew) the next pass indexes it and shows
/// the index current.
#[tokio::test]
async fn index_maintenance_keeps_a_meeting_a_rebuild_skipped_pending_until_it_reads() {
    use broscribe_core::index::{Index, IndexDatabase};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("library");
    let d = daemon(tmp.path());
    let kept = indexed_meeting(tmp.path());
    let broken = indexed_meeting_titled(tmp.path(), "Synthetic okapi plan", "okapi");
    d.maintain_index();
    assert_eq!(index_state(&d).await, IndexState::Current);

    let json = broscribe_core::Library::open(&root)
        .unwrap()
        .meeting_dir(&broken)
        .unwrap()
        .join(broscribe_core::paths::MEETING_JSON);
    let saved = std::fs::read(&json).unwrap();
    std::fs::write(&json, "{ not json").unwrap();
    // A lost index: each pass rebuilds it and skips the unreadable meeting.
    std::fs::remove_dir_all(root.join(broscribe_core::paths::INDEX_DIR)).unwrap();
    for _ in 0..3 {
        d.maintain_index();
        assert_eq!(index_state(&d).await, IndexState::Pending { skipped: 1 });
        let health = Index::health(&root).unwrap();
        assert_eq!(health.database, IndexDatabase::Ready);
        assert!(health.rebuild && !health.is_current(), "{health:?}");
        assert_eq!(search(tmp.path(), "zebra"), Some(vec![kept.clone()]));
        assert_eq!(search(tmp.path(), "okapi"), Some(Vec::new()));
    }

    std::fs::write(&json, &saved).unwrap();
    d.maintain_index();
    assert_eq!(index_state(&d).await, IndexState::Current);
    assert_eq!(search(tmp.path(), "okapi"), Some(vec![broken]));
    assert_eq!(search(tmp.path(), "zebra"), Some(vec![kept]));
    assert!(Index::health(&root).unwrap().is_current());
}

#[tokio::test]
async fn detached_normal_daemon_owner_retains_admission_until_its_index_work_finishes() {
    use broscribe_core::maintenance::{Mode, ModeChange};
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let id = indexed_meeting(tmp.path());
    let d = daemon(tmp.path());
    let index = tmp.path().join("library/.index");
    std::fs::remove_dir_all(&index).unwrap();
    let (parked_tx, parked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = tokio::task::spawn_blocking(move || {
        parked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        d.maintain_index();
    });
    parked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let refused = ModeChange::acquire(&data).is_err();
    release_tx.send(()).unwrap();
    writer.await.unwrap();
    assert!(
        refused,
        "an actual detached library effect owner must exclude mode changes"
    );
    assert_eq!(search(tmp.path(), "zebra"), Some(vec![id]));
    ModeChange::acquire(&data)
        .unwrap()
        .set(Mode::Maintenance)
        .unwrap();
}
