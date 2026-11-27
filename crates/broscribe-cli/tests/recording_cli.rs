//! Synthetic command-line and daemon boundary coverage.
use broscribe_cli::{Deps, run};
use broscribe_models::fakes::FakeModelProvider;
use broscribe_platform::fakes::{
    FakeAutostart, FakeMicCapture, FakePaths, FakeProcessProbe, FakeProcessTreeControl,
    FakeSecretStore, FakeTrustStore,
};
use broscribe_platform::{Autostart, DecoderChain, LoginJob, LoginRegistrationGuard};
use broscribe_platform_native::ipc::{ConnectionReader, ConnectionWriter, Listener};
use serde_json::Value;
use std::ffi::OsString;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

/// A synthetic home: under `/tmp` on Unix, which keeps the agent's socket
/// path within the short macOS limit; the system temporary folder elsewhere.
fn temp_home() -> tempfile::TempDir {
    let parent = if cfg!(unix) {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    tempfile::tempdir_in(parent).unwrap()
}

fn fixture(root: &std::path::Path) -> Deps {
    Deps {
        clock: broscribe_cli::system_clock(),
        paths: Box::new(FakePaths::new(root)),
        updates: broscribe_cli::UpdateDeps::with_transport(
            std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                std::path::Path::new(&root).join("install"),
                "synthetic-test-target",
            )),
            std::sync::Arc::new(broscribe_updates::FakeTransport::default()),
        ),
        models: Box::new(FakeModelProvider::default()),
        decoder: DecoderChain::new(vec![]),
        process: Box::new(FakeProcessProbe::new(std::process::id())),
        mic: Box::new(FakeMicCapture::new(16_000)),
        secrets: std::sync::Arc::new(FakeSecretStore::default()),
        browser: std::sync::Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        agent_instance: Box::new(broscribe_platform_native::ipc::NativeAgentInstanceProbe),
        autostart: Box::new(FakeAutostart {
            login_job: LoginJob::Idle {
                serves_profile: Some(true),
            }
            .into(),
            ..Default::default()
        }),
        agents: broscribe_cli::AgentEnv::isolated(root),
        execution: broscribe_models::ExecutionContext::new(
            std::sync::Arc::new(FakeProcessTreeControl::default()),
            std::sync::Arc::new(FakeTrustStore::default()),
        ),
    }
}
/// Gives the tests and their synthetic OS manager access to the same job.
struct TestAutostart(Arc<FakeAutostart>);

impl Autostart for TestAutostart {
    fn is_enabled(&self) -> broscribe_core::Result<bool> {
        self.0.is_enabled()
    }

    fn set_enabled(&self, enabled: bool) -> broscribe_core::Result<()> {
        self.0.set_enabled(enabled)
    }

    fn set_enabled_with_registration(
        &self,
        enabled: bool,
        registration: &LoginRegistrationGuard,
    ) -> broscribe_core::Result<()> {
        self.0.set_enabled_with_registration(enabled, registration)
    }

    fn login_job(&self, data_dir: &std::path::Path) -> LoginJob {
        self.0.login_job(data_dir)
    }

    fn login_job_matches(&self, data_dir: &std::path::Path) -> Option<bool> {
        self.0.login_job_matches(data_dir)
    }

    fn stop(&self) -> broscribe_core::Result<()> {
        self.0.stop()
    }

    fn lock_login_registration(
        &self,
        wait: Duration,
    ) -> broscribe_core::Result<LoginRegistrationGuard> {
        self.0.lock_login_registration(wait)
    }

    fn stop_login_job(&self, registration: &LoginRegistrationGuard) -> broscribe_core::Result<()> {
        self.0.stop_login_job(registration)
    }
}

fn running_job(pid: u32) -> LoginJob {
    LoginJob::Running(NonZeroU32::new(pid).unwrap())
}

fn fake_login_job(deps: &mut Deps, job: LoginJob) -> Arc<FakeAutostart> {
    let autostart = Arc::new(FakeAutostart {
        login_job: job.into(),
        ..Default::default()
    });
    deps.autostart = Box::new(TestAutostart(autostart.clone()));
    autostart
}

fn command(deps: &Deps, args: &[&str]) -> (i32, Value) {
    let argv = ["broscribe", "--json"]
        .into_iter()
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (vec![], vec![]);
    let code = run(argv, deps, &mut out, &mut err);
    (
        code,
        serde_json::from_slice(&out)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out))),
    )
}
#[test]
fn recording_commands_require_a_daemon() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    for args in [
        vec![
            "record",
            "start",
            "--app",
            "synthetic.app",
            "--title",
            "Synthetic",
        ],
        vec!["record", "stop"],
        vec!["record", "status"],
        vec!["record", "pause"],
        vec!["record", "resume"],
        vec!["record", "lang", "pt-BR"],
        vec!["record", "bookmark"],
        vec!["record", "bookmark", "decision"],
        vec!["record", "note", "Synthetic note"],
        vec!["record", "tag", "+a", "-b"],
        vec!["record", "participant", "confirm", "Synthetic Person"],
        vec!["record", "participant", "remove", "Synthetic Person"],
        vec!["record", "title", "Synthetic title"],
        vec![
            "record",
            "title",
            "Synthetic title",
            "--session",
            "s-synthetic",
        ],
        vec!["record", "discard", "--session", "s-synthetic", "--yes"],
        vec!["record", "tag", "--session", "s-synthetic", "x"],
        vec!["record", "live"],
        vec!["record", "auto-stop", "--on"],
        vec![
            "record",
            "live-attribute",
            "--session",
            "s-synthetic",
            "--turn",
            "mic-0",
            "--revision",
            "1",
            "--epoch",
            "0",
            "--evidence",
            "ev",
            "--clear",
        ],
    ] {
        let (code, value) = command(&deps, &args);
        assert_eq!(code, 1, "{args:?}: {value}");
        assert_eq!(
            value["error"]["code"], "daemon_unavailable",
            "{args:?}: {value}"
        );
    }
}
#[test]
fn daemon_status_reports_absence() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let (code, value) = command(&deps, &["daemon", "status"]);
    assert_eq!(code, 0);
    assert_eq!(value["running"], false);
    assert_eq!(value["observation"], "absent");
    assert!(value["installed"].is_boolean());
    assert_eq!(value["service_status"], "not_installed");
}

#[test]
fn daemon_status_waits_for_a_just_started_agent_s_first_answer() {
    // `daemon start` reports status right after the OS started the agent,
    // which may take a while to answer its first request. A first answer
    // 3 s after the ping arrived (beyond the former 1 s probe) is still
    // verified. The delay starts when the request arrives, so scheduling of
    // the test threads cannot shorten it.
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let pid = std::process::id().wrapping_add(1);
    let server = spawn_agent(&deps, move |mut listener| async move {
        let (mut reader, mut writer, request) = next_request(&mut listener).await;
        assert_eq!(request.call.method(), "ping");
        tokio::time::sleep(Duration::from_secs(3)).await;
        let pong = broscribe_ipc::Response::ok(request.id, ping_value(pid, false));
        writer.write_line(pong.to_line().unwrap()).await.unwrap();
        let line = tokio::time::timeout(AGENT_WAIT, reader.read_line())
            .await
            .unwrap()
            .unwrap()
            .expect("the command asked for the status");
        let request = broscribe_ipc::Request::parse(&line).unwrap();
        assert_eq!(request.call.method(), "status");
        let state = broscribe_ipc::Response::ok(request.id, serde_json::json!({"synthetic": true}));
        writer.write_line(state.to_line().unwrap()).await.unwrap();
    });
    let (code, value) = command(&deps, &["daemon", "status"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["observation"], "verified", "{value}");
    assert_eq!(value["running"], true);
    assert_eq!(value["agent"]["pid"], pid);
    assert_eq!(value["inventory"], "assessed");
    server.join().unwrap();
}

#[test]
fn daemon_status_reports_held_claim_without_an_endpoint_as_unreachable() {
    use broscribe_platform::fakes::FakeAgentInstanceProbe;

    let tmp = temp_home();
    let mut deps = fixture(tmp.path());
    deps.agent_instance = Box::new(FakeAgentInstanceProbe { held: true.into() });
    let (code, value) = command(&deps, &["daemon", "status"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["observation"], "unreachable");
    assert_eq!(value["running"], true);
    assert_eq!(value["inventory"], "unassessed");
    assert!(value["state"].is_null());
}

#[test]
fn retention_policy_refuses_an_agent_on_another_library() {
    use broscribe_core::config::{Config, Retention};
    use broscribe_ipc::{Call, Response};

    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let paths = broscribe_platform::resolve_paths(deps.paths.as_ref()).unwrap();
    broscribe_core::Library::open(paths.default_library_dir).unwrap();
    let other = tmp.path().join("another-library");
    let server = spawn_agent(&deps, |mut listener| async move {
        let (_, mut writer, request) = next_request(&mut listener).await;
        assert_eq!(request.call, Call::Status);
        writer
            .write_line(
                Response::ok(request.id, serde_json::json!({"library": other}))
                    .to_line()
                    .unwrap(),
            )
            .await
            .unwrap();
    });
    let (code, result) = command(&deps, &["retention", "policy", "processed", "--yes"]);
    assert_eq!(code, 1, "{result}");
    assert_eq!(result["error"]["code"], "retention_agent_library_mismatch");
    assert_eq!(
        Config::load(&paths.data_dir).unwrap().retention,
        Retention::KeepForever
    );
    server.join().unwrap();
}

#[test]
fn app_show_and_record_prompt_commands_use_the_agent_and_report_json() {
    use broscribe_ipc::{Call, Response};
    use serde_json::json;
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let prompt = json!({
        "id": "ask-synthetic",
        "title": "Meeting detected",
        "body": "Synthetic meeting is using the microphone.",
        "actions": [{"id":"record", "label":"Record"}, {"id":"ignore", "label":"Ignore"}]
    });
    let expected_prompt = prompt.clone();
    let server = spawn_agent(&deps, |mut listener| async move {
        for (call, result) in [
            (Call::AppShow, json!({"subscribers": 1})),
            (
                Call::Status,
                json!({"pending_notifications": [
                    prompt,
                    {"id":"notice-synthetic", "title":"Recording stopped", "body":"Synthetic notice"}
                ]}),
            ),
            (
                Call::NotificationAction {
                    notification_id: "ask-synthetic".into(),
                    action: "ignore".into(),
                },
                json!({"ignored": true}),
            ),
            (
                Call::NotificationAction {
                    notification_id: "ask-second-synthetic".into(),
                    action: "record".into(),
                },
                json!({"recording": {"session_id":"s-synthetic"}}),
            ),
        ] {
            let (_, mut writer, request) = next_request(&mut listener).await;
            assert_eq!(request.call, call);
            writer
                .write_line(Response::ok(request.id, result).to_line().unwrap())
                .await
                .unwrap();
        }
    });
    assert_eq!(
        command(&deps, &["app", "show"]),
        (0, json!({"subscribers": 1}))
    );
    assert_eq!(
        command(&deps, &["record", "prompts"]),
        (0, json!({"prompts": [expected_prompt]}))
    );
    assert_eq!(
        command(&deps, &["record", "answer", "ask-synthetic", "ignore"]),
        (0, json!({"ignored": true}))
    );
    assert_eq!(
        command(
            &deps,
            &["record", "answer", "ask-second-synthetic", "record"]
        ),
        (0, json!({"recording": {"session_id":"s-synthetic"}}))
    );
    server.join().unwrap();
}

/// How long a fake agent waits for the command under test to connect, or
/// to send its request, before failing the test instead of hanging it.
const AGENT_WAIT: Duration = Duration::from_secs(10);

/// A fake agent at the data folder's real endpoint, bound as the agent
/// binds it (authenticated, private to this user), served by `serve` on its
/// own thread and runtime. Returns once it listens, so commands find it.
fn spawn_agent<F, Fut>(deps: &Deps, serve: F) -> std::thread::JoinHandle<()>
where
    F: FnOnce(Listener) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let (ready, listening) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let listener = Listener::bind(&data).unwrap();
                ready.send(()).unwrap();
                serve(listener).await;
            });
    });
    match listening.recv_timeout(AGENT_WAIT) {
        Ok(()) => thread,
        // It failed to bind: its own panic says why.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            std::panic::resume_unwind(thread.join().unwrap_err())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("the fake agent never listened"),
    }
}

/// The first request of the next connection, skipping connections closed
/// before sending one (a command's probes); each wait is bounded.
async fn next_request(
    listener: &mut Listener,
) -> (ConnectionReader, ConnectionWriter, broscribe_ipc::Request) {
    loop {
        let connection = tokio::time::timeout(AGENT_WAIT, listener.accept())
            .await
            .expect("the command did not contact the agent")
            .unwrap();
        let (mut reader, writer) = connection.into_split();
        let line = tokio::time::timeout(AGENT_WAIT, reader.read_line())
            .await
            .expect("the command sent no request")
            .unwrap();
        if let Some(line) = line {
            return (
                reader,
                writer,
                broscribe_ipc::Request::parse(&line).unwrap(),
            );
        }
    }
}

/// The session every fenced recording command must name: the one the fake
/// agent reports as active.
const ACTIVE: &str = "s-synthetic";

/// A fake agent answering `replies` in order, one request per connection.
/// It exits as it answers the last one: from then on nothing answers at
/// its endpoint (what `daemon stop` waits for).
fn fake_daemon(deps: &Deps, replies: Vec<(&'static str, Value)>) -> std::thread::JoinHandle<()> {
    spawn_agent(deps, move |listener| async move {
        let mut listener = Some(listener);
        let last = replies.len();
        let mut connection: Option<(ConnectionReader, ConnectionWriter)> = None;
        for (n, (method, result)) in replies.into_iter().enumerate() {
            let (reader, mut writer, request) = match connection.take() {
                Some((mut reader, writer)) => {
                    match tokio::time::timeout(AGENT_WAIT, reader.read_line())
                        .await
                        .unwrap()
                        .unwrap()
                    {
                        Some(line) => (
                            reader,
                            writer,
                            broscribe_ipc::Request::parse(&line).unwrap(),
                        ),
                        None => next_request(listener.as_mut().unwrap()).await,
                    }
                }
                None => next_request(listener.as_mut().unwrap()).await,
            };
            assert_eq!(request.call.method(), method);
            if let Some(session) = request.call.recording_session() {
                assert_eq!(session, ACTIVE, "{method} names the active recording");
            }
            if let broscribe_ipc::Call::RecordStart(params) = &request.call {
                assert_eq!(params.setup.as_deref(), Some("headphones"));
            }
            assert!(!request.id.as_str().is_empty());
            let response = broscribe_ipc::Response::ok(request.id, result)
                .to_line()
                .unwrap();
            if n + 1 == last {
                drop(listener.take());
            }
            writer.write_line(response).await.unwrap();
            connection = Some((reader, writer));
        }
    })
}

#[test]
fn setup_try_consumes_and_removes_the_running_daemons_sample() {
    use std::sync::{Arc, mpsc};

    struct NoRecording;
    impl broscribe_ipc::CaptureEngine for NoRecording {
        fn start(
            &self,
            _: broscribe_ipc::SessionConfig,
            _: broscribe_ipc::CaptureOutputs,
        ) -> broscribe_core::Result<Box<dyn broscribe_ipc::SessionHandle>> {
            Err(broscribe_core::Error::Unsupported(
                "this setup-only fixture must not start a recording".into(),
            ))
        }
        fn recover_orphans(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
        ) -> broscribe_core::Result<broscribe_ipc::OrphanRecovery> {
            Ok(broscribe_ipc::OrphanRecovery::default())
        }
    }

    let home = temp_home();
    let mut deps = fixture(home.path());
    deps.decoder = DecoderChain::new(vec![Box::new(
        broscribe_platform::symphonia_decoder::SymphoniaDecoder::new(),
    )]);
    let local_mic = FakeMicCapture::new(16_000);
    let local_stops = local_mic.stopped.clone();
    deps.mic = Box::new(local_mic);
    let paths = broscribe_platform::resolve_paths(deps.paths.as_ref()).unwrap();
    let mut cfg = broscribed::DaemonConfig::new(
        paths.default_library_dir.clone(),
        paths.data_dir.clone(),
        paths.cache_dir.clone(),
        broscribed::WorkerCommand {
            data_dir_override: None,
            program: home.path().join("unstarted-worker"),
            prefix: Vec::new(),
        },
    );
    cfg.data_dir_overridden = true;
    let admission = Arc::new(
        broscribe_core::maintenance::ModeLease::acquire(
            &cfg.data_dir,
            broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
        )
        .unwrap(),
    );
    let daemon = broscribed::Daemon::new(
        cfg,
        Arc::new(NoRecording),
        Arc::new(FakeProcessProbe::new(std::process::id())),
        admission,
    )
    .unwrap();
    let mut mic = FakeMicCapture::new(16_000);
    mic.chunks = 1;
    let mic = Arc::new(mic);
    daemon.set_setup_devices(
        Arc::new(broscribe_platform::devices::FakeDeviceInfo::default()),
        mic.clone(),
    );
    daemon.finish_startup();
    let (published, sample) = mpsc::channel();
    let agent = spawn_agent(&deps, move |mut listener| async move {
        for expected in ["ping", "capture_sample"] {
            let (_reader, mut writer, request) = next_request(&mut listener).await;
            assert_eq!(request.call.method(), expected);
            let response = daemon.handle(request).await;
            if expected == "capture_sample" {
                let path = std::path::PathBuf::from(
                    response.outcome.as_ref().unwrap()["path"].as_str().unwrap(),
                );
                assert!(path.is_file(), "the real daemon must publish its WAV");
                published.send(path).unwrap();
            }
            writer
                .write_line(response.to_line().unwrap())
                .await
                .unwrap();
        }
    });

    let (code, answer) = command(&deps, &["setup", "try", "--seconds", "0.1"]);
    agent.join().unwrap();
    assert_eq!(code, 0, "{answer}");
    assert!((answer["duration_s"].as_f64().unwrap() - 0.1).abs() < 0.001);
    assert!(answer["meeting_id"].is_null(), "try-it must remain unsaved");
    let path = sample.recv_timeout(AGENT_WAIT).unwrap();
    assert!(
        !path.parent().unwrap().exists(),
        "the requester must remove its sample"
    );
    assert_eq!(mic.started.lock().unwrap().len(), 1);
    assert_eq!(local_stops.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn named_sync_keeps_credential_recovery_codes_without_local_fallback() {
    for (remote_code, expected_code, message) in [
        (
            "invalid_secret_reference",
            "invalid_secret_reference",
            "credential recovery reference is outside its account namespace",
        ),
        (
            "account_registry_version",
            "account_registry_version",
            "account registry version is unsupported",
        ),
        (
            "google_stamp_required",
            "google_stamp_required",
            "Google calendar authority requires both generations",
        ),
        ("unknown_recovery_code", "internal", "unrecognized failure"),
    ] {
        let tmp = temp_home();
        let deps = fixture(tmp.path());
        let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
            .unwrap()
            .data_dir;
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(
            broscribe_core::config::Config::path(&data),
            "[integration.google]\nclient_id='synthetic-client'\n",
        )
        .unwrap();
        let server = spawn_agent(&deps, move |mut listener| async move {
            let (_reader, mut writer, request) = next_request(&mut listener).await;
            assert!(matches!(
                &request.call,
                broscribe_ipc::Call::IntegrationsSync {
                    integration: Some(id)
                } if id == "google"
            ));
            let response = broscribe_ipc::Response::err(
                request.id,
                broscribe_ipc::IpcError::new(remote_code, message),
            )
            .to_line()
            .unwrap();
            writer.write_line(response).await.unwrap();
        });
        let (code, value) = command(&deps, &["integrations", "sync", "google"]);
        server.join().unwrap();
        assert_eq!(code, 1, "{value}");
        assert_eq!(value["error"]["code"], expected_code, "{value}");
        if remote_code == expected_code {
            assert_eq!(value["error"]["message"], message, "{value}");
        }
    }
}

#[test]
fn controls_and_detection_use_ipc_and_preserve_json_results() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let active = serde_json::json!({"recording":{"session_id": ACTIVE}});
    let agent_pid = std::process::id().wrapping_add(1);
    let replies = vec![
        (
            "record.start",
            serde_json::json!({"recording":{"session_id":"s-synthetic"}}),
        ),
        ("record.status", active.clone()),
        ("record.pause", serde_json::json!({"paused":true})),
        ("record.status", active.clone()),
        ("record.resume", serde_json::json!({"resumed":true})),
        ("record.status", active.clone()),
        ("record.lang", serde_json::json!({"language":"pt-BR"})),
        ("record.status", active.clone()),
        ("record.bookmark", serde_json::json!({"label":"decision"})),
        ("record.status", active.clone()),
        ("record.note", serde_json::json!({"noted":true})),
        ("record.status", active.clone()),
        ("record.tag", serde_json::json!({"tags":["a"]})),
        (
            "record.status",
            serde_json::json!({"recording":{"session_id":"s-synthetic"}}),
        ),
        (
            "record_participant",
            serde_json::json!({"participants":["p-synthetic"]}),
        ),
        (
            "record.status",
            serde_json::json!({"recording":{"session_id":"s-synthetic"}}),
        ),
        ("record_participant", serde_json::json!({"participants":[]})),
        ("record.status", active.clone()),
        ("record.stop", serde_json::json!({"stopped":true})),
        ("detect.status", serde_json::json!({"apps":[]})),
        ("detect.reload", serde_json::json!({"reloaded":true})),
        (
            "audio_check",
            serde_json::json!({"delay_ms":12.5,"leakage_db":-35.0,"recommendation":"residue_gating","bluetooth_call_profile":false}),
        ),
        // The daemon checks for active work when handling this request.
        ("ping", ping_value(agent_pid, false)),
        ("daemon.stop_if_idle", serde_json::json!({"stopping":true})),
    ];
    let server = fake_daemon(&deps, replies);
    let mut stopped = Value::Null;
    for args in [
        vec!["record", "start", "--setup", "headphones"],
        vec!["record", "pause"],
        vec!["record", "resume"],
        vec!["record", "lang", "pt-BR"],
        vec!["record", "bookmark", "decision"],
        vec!["record", "note", "synthetic"],
        vec!["record", "tag", "+a", "-b"],
        vec!["record", "participant", "confirm", "p-synthetic"],
        vec!["record", "participant", "remove", "p-synthetic"],
        vec!["record", "stop"],
        vec!["detect", "status"],
        vec!["detect", "policy", "teams", "ask"],
        vec!["audio-check"],
        vec!["daemon", "stop"],
    ] {
        let (code, value) = command(&deps, &args);
        assert_eq!(code, 0, "{args:?}: {value}");
        assert!(value.is_object());
        stopped = value;
    }
    server.join().unwrap();
    assert_eq!(stopped["stopped"], true, "{stopped}");
    assert_eq!(stopped["pid"], agent_pid, "{stopped}");
    assert_eq!(stopped["login_job_unloaded"], true, "{stopped}");
    assert_eq!(stopped["running"], false, "{stopped}");
    assert_eq!(stopped["agent"]["stopping"], true, "{stopped}");
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    assert_eq!(
        broscribe_core::config::Config::load(&data)
            .unwrap()
            .detect
            .policies["teams"],
        "ask"
    );
}

#[test]
fn integration_commands_and_doctor_have_json_shapes() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    for id in ["microsoft-graph", "google"] {
        let (code, setup) = command(&deps, &["integrations", "setup", id]);
        assert_eq!(code, 0);
        assert_eq!(setup["integration"], id);
        assert!(setup["instructions"].is_string());
        let (code, status) = command(&deps, &["integrations", "status", id]);
        assert_eq!(code, 0);
        assert_eq!(status["configured"], false);
        assert_eq!(status["state"]["status"], "signed_out");
        let (code, login) = command(&deps, &["integrations", "login", id]);
        assert_eq!(code, 1);
        assert!(login["error"]["message"].is_string());
        let (code, logout) = command(&deps, &["integrations", "logout", id]);
        if id == "google" {
            assert_eq!(code, 0);
            assert_eq!(logout["signed_out"], true);
        } else {
            assert_eq!(code, 1);
            assert_eq!(logout["error"]["code"], "account_required");
        }
    }
    let (code, sync) = command(&deps, &["integrations", "sync"]);
    assert_eq!(code, 0);
    assert_eq!(sync["synced"], serde_json::json!([]));
    let (code, doctor) = command(&deps, &["doctor"]);
    assert_eq!(code, 0);
    assert!(doctor["checks"].is_array());
    assert!(doctor["daemon"]["running"].is_boolean());
    assert!(doctor["window_titles_available"].is_boolean());
    assert_eq!(doctor["pending_journal_bytes"], 0);
    assert_eq!(doctor["due_deletions"]["dry_run"], true);
    assert!(doctor["integrations"].is_array());
}

#[test]
fn lifecycle_uses_injected_autostart() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    for (action, installed) in [("install", true), ("start", true), ("uninstall", false)] {
        let (code, value) = command(&deps, &["daemon", action]);
        assert_eq!(code, 0, "{value}");
        assert_eq!(value["installed"], installed);
        assert_eq!(value["running"], false);
    }
}

fn ping_value(pid: u32, maintenance: bool) -> Value {
    serde_json::json!({
        "pid":pid,
        "version":env!("CARGO_PKG_VERSION"),
        "protocol":broscribe_ipc::PROTOCOL_VERSION,
        "build":null,
        "mode":if maintenance { "maintenance" } else { "normal" },
        "readiness":if maintenance { "maintenance_control_ready" } else { "normal_operational" },
    })
}

#[test]
fn maintenance_status_and_stop_use_control_ping_without_normal_state_or_install_admission() {
    use broscribe_core::maintenance::{Mode, ModeChange};
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let change = ModeChange::acquire(&data).unwrap();
    change.set(Mode::Maintenance).unwrap();
    drop(change);
    let pid = std::process::id().wrapping_add(1);
    let server = fake_daemon(
        &deps,
        vec![
            ("ping", ping_value(pid, true)),
            ("ping", ping_value(pid, true)),
            ("daemon.stop", serde_json::json!({"stopping":true})),
        ],
    );
    let (code, value) = command(&deps, &["daemon", "status"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["running"], true);
    assert_eq!(value["agent"]["mode"], "maintenance");
    assert_eq!(value["inventory"], "unassessed");
    assert!(value["state"].is_null());
    let (code, value) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["stopped"], true);
    server.join().unwrap();
}

#[test]
fn offline_control_refuses_a_legacy_endpoint_and_reports_unknown_actual_mode() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let server = fake_daemon(
        &deps,
        vec![(
            "ping",
            serde_json::json!({
                "pid":42, "version":"legacy", "protocol":6, "build":null
            }),
        )],
    );
    let (code, value) = command(&deps, &["maintenance", "enable"]);
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "maintenance_busy");
    let (code, value) = command(&deps, &["maintenance", "status"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["configured_mode"], "normal");
    assert_eq!(value["agent"]["observation"], "legacy_incompatible");
    assert!(value["agent"]["ping"].is_null());
    assert_eq!(value["inventory"], "unassessed");
    server.join().unwrap();
}

#[test]
fn stopping_restarted_maintenance_control_keeps_admission_through_service_unload() {
    use broscribe_core::maintenance::{Mode, ModeChange};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    struct MaintenanceJob {
        data: std::path::PathBuf,
        unloaded: Arc<AtomicBool>,
        pid: Arc<std::sync::atomic::AtomicU32>,
    }
    impl Autostart for MaintenanceJob {
        fn is_enabled(&self) -> broscribe_core::Result<bool> {
            Ok(true)
        }
        fn set_enabled(&self, _: bool) -> broscribe_core::Result<()> {
            unreachable!()
        }
        fn start(&self) -> broscribe_core::Result<()> {
            unreachable!()
        }
        fn login_job(&self, _: &std::path::Path) -> LoginJob {
            running_job(self.pid.load(Ordering::SeqCst))
        }
        fn stop(&self) -> broscribe_core::Result<()> {
            assert_eq!(
                ModeChange::acquire(&self.data).unwrap_err().code(),
                "maintenance_busy"
            );
            self.unloaded.store(true, Ordering::SeqCst);
            Ok(())
        }
    }
    let tmp = temp_home();
    let mut deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let change = ModeChange::acquire(&data).unwrap();
    change.set(Mode::Maintenance).unwrap();
    drop(change);
    let unloaded = Arc::new(AtomicBool::new(false));
    let first = std::process::id().wrapping_add(1);
    let login_pid = Arc::new(std::sync::atomic::AtomicU32::new(first));
    deps.autostart = Box::new(MaintenanceJob {
        data: data.clone(),
        unloaded: unloaded.clone(),
        pid: login_pid.clone(),
    });
    let finished = unloaded.clone();
    let server = spawn_agent(&deps, move |mut listener| async move {
        let mut restarted = false;
        while !finished.load(Ordering::SeqCst) {
            let Ok(connection) =
                tokio::time::timeout(Duration::from_millis(5), listener.accept()).await
            else {
                continue;
            };
            let (mut reader, mut writer) = connection.unwrap().into_split();
            while let Some(line) = tokio::time::timeout(AGENT_WAIT, reader.read_line())
                .await
                .unwrap()
                .unwrap()
            {
                let request = broscribe_ipc::Request::parse(&line).unwrap();
                let value = match request.call {
                    broscribe_ipc::Call::Ping => {
                        ping_value(if restarted { first + 1 } else { first }, true)
                    }
                    broscribe_ipc::Call::Shutdown => {
                        restarted = true;
                        login_pid.store(first + 1, Ordering::SeqCst);
                        serde_json::json!({"stopping":true})
                    }
                    broscribe_ipc::Call::ShutdownAcquire => {
                        assert!(restarted, "only the restarted peer is held for unload");
                        serde_json::json!({"held":true})
                    }
                    other => panic!(
                        "maintenance stop must not ask for normal state or install admission: {other:?}"
                    ),
                };
                writer
                    .write_line(
                        broscribe_ipc::Response::ok(request.id, value)
                            .to_line()
                            .unwrap(),
                    )
                    .await
                    .unwrap();
            }
        }
    });
    let (code, value) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["login_job_unloaded"], true);
    assert!(unloaded.load(Ordering::SeqCst));
    server.join().unwrap();
    ModeChange::acquire(&data)
        .unwrap()
        .set(Mode::Normal)
        .unwrap();
}

#[test]
fn stopping_with_no_endpoint_fences_startup_until_the_login_job_is_unloaded() {
    use broscribe_core::lock::DaemonStartupLock;
    use broscribe_platform::Result;
    use std::path::PathBuf;

    struct IdleJob(PathBuf);
    impl Autostart for IdleJob {
        fn is_enabled(&self) -> Result<bool> {
            Ok(true)
        }
        fn set_enabled(&self, _: bool) -> Result<()> {
            unreachable!()
        }
        fn login_job(&self, _: &std::path::Path) -> LoginJob {
            LoginJob::Idle {
                serves_profile: Some(true),
            }
        }
        fn stop(&self) -> Result<()> {
            // No endpoint answered the restart probe. Simulate launchd
            // starting the daemon as bootout is about to run.
            assert!(DaemonStartupLock::try_acquire(&self.0)?.is_none());
            Ok(())
        }
    }

    for had_agent in [false, true] {
        let tmp = temp_home();
        let mut deps = fixture(tmp.path());
        let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
            .unwrap()
            .data_dir;
        deps.autostart = Box::new(IdleJob(data.clone()));
        let server = had_agent.then(|| {
            let pid = std::process::id().wrapping_add(1);
            let mut starting = ping_value(pid, false);
            starting["readiness"] = serde_json::json!("normal_starting");
            fake_daemon(
                &deps,
                vec![
                    ("ping", starting),
                    ("ping", ping_value(pid, false)),
                    ("daemon.stop_if_idle", serde_json::json!({"stopping": true})),
                ],
            )
        });
        let (code, value) = command(&deps, &["daemon", "stop"]);
        if let Some(server) = server {
            server.join().unwrap();
        }
        assert_eq!(code, 0, "{value}");
        assert_eq!(value["stopped"], had_agent);
        assert_eq!(value["running"], false);
        assert_eq!(value["login_job_unloaded"], true);
        assert!(DaemonStartupLock::try_acquire(&data).unwrap().is_some());
    }
}

#[test]
fn other_profiles_install_or_start_waits_for_stop_to_unload_the_matched_job() {
    use broscribe_platform::Result;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};

    struct StoppingProfile {
        shared: Arc<FakeAutostart>,
        observations: AtomicUsize,
        matched: Sender<()>,
        writer_checked: Mutex<Receiver<bool>>,
    }

    impl Autostart for StoppingProfile {
        fn is_enabled(&self) -> Result<bool> {
            self.shared.is_enabled()
        }

        fn set_enabled(&self, _: bool) -> Result<()> {
            unreachable!()
        }

        fn lock_login_registration(&self, wait: Duration) -> Result<LoginRegistrationGuard> {
            self.shared.lock_login_registration(wait)
        }

        fn login_job(&self, data_dir: &Path) -> LoginJob {
            let job = self.shared.login_job(data_dir);
            if self.observations.fetch_add(1, Ordering::SeqCst) == 1 {
                assert_eq!(
                    job,
                    LoginJob::Idle {
                        serves_profile: Some(true)
                    }
                );
                self.matched.send(()).unwrap();
                assert!(
                    self.writer_checked
                        .lock()
                        .unwrap()
                        .recv_timeout(AGENT_WAIT)
                        .unwrap()
                );
            }
            job
        }

        fn stop_login_job(&self, registration: &LoginRegistrationGuard) -> Result<()> {
            let mut job = self.shared.login_job.lock().unwrap();
            assert_eq!(
                *job,
                LoginJob::Idle {
                    serves_profile: Some(true)
                }
            );
            *job = LoginJob::Absent;
            self.shared.stop_login_job(registration)
        }
    }

    struct StartingProfile {
        shared: Arc<FakeAutostart>,
        checked: Sender<bool>,
        pid: u32,
    }

    impl StartingProfile {
        fn replace_registration(&self) -> Result<()> {
            let busy = matches!(
                self.shared.lock_login_registration(Duration::ZERO),
                Err(error) if error.code() == "busy"
            );
            self.checked.send(busy).unwrap();
            let _registration = self.shared.lock_login_registration(AGENT_WAIT)?;
            let mut job = self.shared.login_job.lock().unwrap();
            assert_eq!(*job, LoginJob::Absent);
            *job = running_job(self.pid);
            Ok(())
        }
    }

    impl Autostart for StartingProfile {
        fn is_enabled(&self) -> Result<bool> {
            Ok(true)
        }

        fn set_enabled(&self, enabled: bool) -> Result<()> {
            assert!(enabled);
            self.replace_registration()
        }

        fn start(&self) -> Result<()> {
            self.replace_registration()
        }
    }

    for action in ["install", "start"] {
        let a_home = temp_home();
        let b_home = temp_home();
        let mut a = fixture(a_home.path());
        let mut b = fixture(b_home.path());
        let shared = Arc::new(FakeAutostart {
            login_registration_path: Some(a_home.path().join("login-registration.lock")),
            login_job: LoginJob::Idle {
                serves_profile: Some(true),
            }
            .into(),
            ..Default::default()
        });
        let (matched, matched_rx) = channel();
        let (checked, writer_checked) = channel();
        let b_pid = std::process::id().wrapping_add(1);
        a.autostart = Box::new(StoppingProfile {
            shared: shared.clone(),
            observations: AtomicUsize::new(0),
            matched,
            writer_checked: Mutex::new(writer_checked),
        });
        b.autostart = Box::new(StartingProfile {
            shared: shared.clone(),
            checked,
            pid: b_pid,
        });
        std::thread::scope(|scope| {
            let other = scope.spawn(move || {
                matched_rx.recv_timeout(AGENT_WAIT).unwrap();
                let (code, value) = command(&b, &["daemon", action]);
                assert_eq!(code, 0, "{value}");
            });
            let (code, value) = command(&a, &["daemon", "stop"]);
            assert_eq!(code, 0, "{value}");
            assert_eq!(value["login_job_unloaded"], true);
            other.join().unwrap();
        });
        assert_eq!(shared.stops.load(Ordering::SeqCst), 1);
        assert_eq!(*shared.login_job.lock().unwrap(), running_job(b_pid));
    }
}

#[test]
fn stop_reports_busy_when_the_login_registration_lock_times_out() {
    use broscribe_platform::Result;

    struct ContendedRegistration(FakeAutostart);
    impl Autostart for ContendedRegistration {
        fn is_enabled(&self) -> Result<bool> {
            Ok(true)
        }
        fn set_enabled(&self, _: bool) -> Result<()> {
            unreachable!()
        }
        fn lock_login_registration(&self, wait: Duration) -> Result<LoginRegistrationGuard> {
            assert!(wait > Duration::ZERO);
            self.0.lock_login_registration(Duration::from_millis(20))
        }
        fn login_job(&self, _: &std::path::Path) -> LoginJob {
            LoginJob::Idle {
                serves_profile: Some(true),
            }
        }
    }

    let home = temp_home();
    let mut deps = fixture(home.path());
    let path = home.path().join("login-registration.lock");
    let registration = LoginRegistrationGuard::acquire(&path, Duration::ZERO).unwrap();
    deps.autostart = Box::new(ContendedRegistration(FakeAutostart {
        login_registration_path: Some(path),
        ..Default::default()
    }));
    let (code, value) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "busy");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("login registration")
    );
    assert!(value.get("login_job_unloaded").is_none());
    drop(registration);
}

#[test]
fn stop_returns_a_manager_timeout_and_rechecks_the_job_on_retry() {
    use broscribe_platform::{Error, Result};
    use std::io::ErrorKind;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct TimedOutUnload {
        lock: PathBuf,
        requested: AtomicBool,
    }
    impl Autostart for TimedOutUnload {
        fn is_enabled(&self) -> Result<bool> {
            Ok(true)
        }
        fn set_enabled(&self, _: bool) -> Result<()> {
            unreachable!()
        }
        fn lock_login_registration(&self, _: Duration) -> Result<LoginRegistrationGuard> {
            LoginRegistrationGuard::acquire(&self.lock, Duration::ZERO)
        }
        fn login_job(&self, _: &Path) -> LoginJob {
            if self.requested.load(Ordering::SeqCst) {
                LoginJob::Absent
            } else {
                LoginJob::Idle {
                    serves_profile: Some(true),
                }
            }
        }
        fn stop_login_job(&self, _: &LoginRegistrationGuard) -> Result<()> {
            assert!(!self.requested.swap(true, Ordering::SeqCst));
            Err(Error::io("unloading login job", ErrorKind::TimedOut.into()))
        }
    }

    let home = temp_home();
    let mut deps = fixture(home.path());
    deps.autostart = Box::new(TimedOutUnload {
        lock: home.path().join("login-registration.lock"),
        requested: AtomicBool::new(false),
    });
    let (code, value) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "io");
    assert!(value.get("login_job_unloaded").is_none());

    let (code, value) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["login_job_unloaded"], false);
}

#[test]
fn stopping_leaves_an_idle_login_job_for_another_profile_loaded() {
    let home = temp_home();
    let mut deps = fixture(home.path());
    let login_job = fake_login_job(
        &mut deps,
        LoginJob::Idle {
            serves_profile: Some(false),
        },
    );
    let (code, result) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["running"], false);
    assert_eq!(result["login_job_unloaded"], false);
    assert_eq!(login_job.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn stopping_refuses_an_idle_login_job_with_an_unknown_profile() {
    let home = temp_home();
    let mut deps = fixture(home.path());
    let login_job = fake_login_job(
        &mut deps,
        LoginJob::Idle {
            serves_profile: None,
        },
    );
    let (code, result) = command(&deps, &["daemon", "stop"]);
    assert_stop_unverified(code, &result);
    assert_eq!(login_job.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn stopping_leaves_another_profiles_login_job_that_spawns_after_the_last_observation() {
    use std::sync::Mutex;

    struct SpawningJob {
        state: Arc<Mutex<(usize, LoginJob)>>,
        pid: u32,
    }

    impl Autostart for SpawningJob {
        fn is_enabled(&self) -> broscribe_core::Result<bool> {
            Ok(true)
        }

        fn set_enabled(&self, _: bool) -> broscribe_core::Result<()> {
            unreachable!()
        }

        fn login_job(&self, _: &std::path::Path) -> LoginJob {
            let mut state = self.state.lock().unwrap();
            let observed = state.1;
            state.0 += 1;
            if state.0 == 2 {
                state.1 = running_job(self.pid);
            }
            observed
        }

        fn stop_login_job(&self, _: &LoginRegistrationGuard) -> broscribe_core::Result<()> {
            let mut state = self.state.lock().unwrap();
            assert_eq!(state.1, running_job(self.pid));
            state.1 = LoginJob::Absent;
            Ok(())
        }
    }
    let home = temp_home();
    let mut deps = fixture(home.path());
    let state = Arc::new(Mutex::new((
        0,
        LoginJob::Idle {
            serves_profile: Some(false),
        },
    )));
    let pid = std::process::id().wrapping_add(1);
    deps.autostart = Box::new(SpawningJob {
        state: state.clone(),
        pid,
    });
    let (code, result) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["running"], false);
    assert_eq!(result["login_job_unloaded"], false);
    assert_eq!(state.lock().unwrap().1, running_job(pid));
}

#[cfg(unix)]
#[test]
fn stopping_cannot_claim_a_profile_with_a_held_pid_file_and_no_endpoint() {
    let home = temp_home();
    let mut deps = fixture(home.path());
    fake_login_job(&mut deps, LoginJob::Absent);
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let run = data.join("run");
    broscribe_platform_native::ipc::ensure_private_dir(&run).unwrap();
    let path = run.join("broscribed.pid");
    std::fs::write(&path, b"4242\n").unwrap();
    let lock = std::fs::File::open(&path).unwrap();
    lock.try_lock().unwrap();

    let (code, result) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 1, "{result}");
    assert_eq!(result["error"]["code"], "agent_unreachable", "{result}");
    assert!(result.get("running").is_none(), "{result}");

    drop(lock);
    let (code, result) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["running"], false);
    assert_eq!(std::fs::read(&path).unwrap(), b"4242\n");
}

#[test]
fn stopping_waits_for_a_fenced_login_restart_to_become_idle() {
    let pid = std::process::id().wrapping_add(1);
    for transient in [running_job(pid + 1), LoginJob::Unknown] {
        check_stop_after_login_job_becomes_idle(pid, Some(transient));
    }
}

#[test]
fn stopping_unloads_a_bundled_default_profile_job_that_goes_from_running_to_idle() {
    check_stop_after_login_job_becomes_idle(std::process::id().wrapping_add(1), None);
}

fn check_stop_after_login_job_becomes_idle(pid: u32, transient: Option<LoginJob>) {
    use broscribe_core::lock::DaemonStartupLock;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    struct RestartingJob(Mutex<VecDeque<LoginJob>>);

    impl Autostart for RestartingJob {
        fn is_enabled(&self) -> broscribe_core::Result<bool> {
            Ok(true)
        }

        fn set_enabled(&self, _: bool) -> broscribe_core::Result<()> {
            unreachable!()
        }

        fn login_job(&self, data_dir: &Path) -> LoginJob {
            assert!(DaemonStartupLock::try_acquire(data_dir).unwrap().is_none());
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .expect("extra observation")
        }

        fn login_job_matches(&self, _: &Path) -> Option<bool> {
            Some(true)
        }

        fn stop_login_job(&self, _: &LoginRegistrationGuard) -> broscribe_core::Result<()> {
            assert!(self.0.lock().unwrap().is_empty());
            Ok(())
        }
    }

    let home = temp_home();
    let mut deps = fixture(home.path());
    let observations = std::iter::once(running_job(pid))
        .chain(transient)
        .chain([LoginJob::Idle {
            serves_profile: Some(true),
        }]);
    deps.autostart = Box::new(RestartingJob(Mutex::new(observations.collect())));
    let server = fake_daemon(
        &deps,
        vec![
            ("ping", ping_value(pid, false)),
            ("daemon.stop_if_idle", serde_json::json!({"stopping": true})),
        ],
    );
    let (code, result) = command(&deps, &["daemon", "stop"]);
    server.join().unwrap();
    assert_eq!(code, 0, "{transient:?}: {result}");
    assert_eq!(result["stopped"], true);
    assert_eq!(result["running"], false);
    assert_eq!(result["login_job_unloaded"], true);
}

#[test]
fn stopping_leaves_a_running_job_configured_for_another_profile_loaded() {
    let home = temp_home();
    let mut deps = fixture(home.path());
    let login_job = fake_login_job(&mut deps, running_job(std::process::id().wrapping_add(1)));
    *login_job.login_job_profile.lock().unwrap() = Some(false);
    let (code, result) = command(&deps, &["daemon", "stop"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["running"], false);
    assert_eq!(result["login_job_unloaded"], false);
    assert_eq!(login_job.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn stopping_waits_when_a_restarted_endpoint_is_recovering() {
    check_restarted_stop_hold(false, RecoveryFollowUp::Starting);
}

#[test]
fn stopping_refuses_to_unload_when_the_login_pid_changes_after_the_hold() {
    check_restarted_stop_hold(true, RecoveryFollowUp::Starting);
}

#[test]
fn stopping_retries_the_hold_when_recovery_finishes_before_the_follow_up_ping() {
    check_restarted_stop_hold(false, RecoveryFollowUp::OperationalIdle);
}

#[test]
fn stopping_preserves_a_restarted_agents_busy_error_after_recovery() {
    check_restarted_stop_hold(false, RecoveryFollowUp::OperationalBusy);
}

#[derive(Clone, Copy)]
enum RecoveryFollowUp {
    Starting,
    OperationalIdle,
    OperationalBusy,
}

fn check_restarted_stop_hold(changes_during_hold: bool, follow_up: RecoveryFollowUp) {
    use broscribe_ipc::{AgentReadiness, Call, IpcError, Request, Response};

    const BUSY_MESSAGE: &str = "BroScribe Agent has work in progress";
    const STARTING_MESSAGE: &str = "BroScribe Agent is still recovering";
    let tmp = temp_home();
    let mut deps = fixture(tmp.path());
    let first = std::process::id().wrapping_add(1);
    let restarted = first.wrapping_add(1);
    let login_job = fake_login_job(&mut deps, running_job(first));
    let agent_job = login_job.clone();
    let server = spawn_agent(&deps, move |mut listener| async move {
        let (mut reader, mut writer, request) = next_request(&mut listener).await;
        assert_eq!(request.call, Call::Ping);
        writer
            .write_line(
                Response::ok(request.id, ping_value(first, false))
                    .to_line()
                    .unwrap(),
            )
            .await
            .unwrap();
        let request = Request::parse(&reader.read_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request.call, Call::ShutdownIfIdle);
        *agent_job.login_job.lock().unwrap() = running_job(restarted);
        writer
            .write_line(
                Response::ok(request.id, serde_json::json!({ "stopping": true }))
                    .to_line()
                    .unwrap(),
            )
            .await
            .unwrap();
        drop((reader, writer));

        let (mut reader, mut writer, request) = next_request(&mut listener).await;
        assert_eq!(request.call, Call::Ping);
        let mut starting = ping_value(restarted, false);
        starting["readiness"] = serde_json::to_value(AgentReadiness::NormalStarting).unwrap();
        writer
            .write_line(
                Response::ok(request.id, starting.clone())
                    .to_line()
                    .unwrap(),
            )
            .await
            .unwrap();
        let exchange = [
            (
                Call::Status,
                Ok(serde_json::json!({"recording": null, "install": null})),
            ),
            (
                Call::ShutdownAcquire,
                Err(IpcError::new(IpcError::BUSY, STARTING_MESSAGE)),
            ),
            (
                Call::Ping,
                Ok(match follow_up {
                    RecoveryFollowUp::Starting => starting,
                    _ => ping_value(restarted, false),
                }),
            ),
            (
                Call::ShutdownAcquire,
                match follow_up {
                    RecoveryFollowUp::OperationalBusy => {
                        Err(IpcError::new(IpcError::BUSY, BUSY_MESSAGE))
                    }
                    _ => Ok(serde_json::json!({ "fence": 1 })),
                },
            ),
        ];
        for (call, outcome) in exchange {
            let line = tokio::time::timeout(AGENT_WAIT, reader.read_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let request = Request::parse(&line).unwrap();
            assert_eq!(request.call, call);
            if changes_during_hold && request.call == Call::ShutdownAcquire && outcome.is_ok() {
                *agent_job.login_job.lock().unwrap() = running_job(restarted + 1);
            }
            writer
                .write_line(
                    Response {
                        id: request.id,
                        outcome,
                    }
                    .to_line()
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        drop(listener);
        // The CLI holds the connection through its final PID check and any unload.
        assert!(
            tokio::time::timeout(AGENT_WAIT, reader.read_line())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
    });
    let (code, value) = command(&deps, &["daemon", "stop"]);
    server.join().unwrap();
    if matches!(follow_up, RecoveryFollowUp::OperationalBusy) {
        assert_eq!(code, 1, "{value}");
        assert_eq!(value["error"]["code"], IpcError::BUSY);
        assert_eq!(value["error"]["message"], BUSY_MESSAGE);
        assert_eq!(login_job.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
    } else if changes_during_hold {
        assert_eq!(code, 1, "{value}");
        assert_eq!(value["error"]["code"], "login_job_unverified");
        assert!(
            value["error"]["message"]
                .as_str()
                .unwrap()
                .ends_with("the selected agent stopped and the login job was left loaded")
        );
        assert!(value.get("running").is_none(), "{value}");
        assert_eq!(login_job.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
    } else {
        assert_eq!(code, 0, "{value}");
        assert_eq!(value["login_job_unloaded"], true);
        assert_eq!(login_job.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[test]
fn stopping_leaves_a_restarted_agent_holding_an_install_admission_running() {
    use broscribe_ipc::{Call, InstallHold, IpcError, Request, Response};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// The agent's answer to `daemon.stop_acquire` while another admission is held.
    const HELD: &str = "another install admission is held";
    // An update confirmed its admission before the first agent stopped, and
    // the agent the login job restarted reloaded it.
    let durable = serde_json::to_value(InstallHold {
        fence: 4,
        durable: true,
    })
    .unwrap();
    for (restarted_install, expected_code, expected_calls) in [
        // Its status shows the admission: refused before asking for one.
        (
            durable,
            IpcError::INSTALLING,
            vec![Call::Ping, Call::ShutdownIfIdle, Call::Ping, Call::Status],
        ),
        // An update took its admission after the status was read: the
        // agent's refusal keeps its own code, never relabeled as recording.
        (
            Value::Null,
            IpcError::INSTALLING,
            vec![
                Call::Ping,
                Call::ShutdownIfIdle,
                Call::Ping,
                Call::Status,
                Call::ShutdownAcquire,
            ],
        ),
    ] {
        let tmp = temp_home();
        let mut deps = fixture(tmp.path());
        // Neither is this test's process, so the fake probe reports the
        // stopped agent gone at once.
        let first = std::process::id().wrapping_add(1);
        let restarted = std::process::id().wrapping_add(2);
        let login_job = fake_login_job(&mut deps, running_job(first));
        let after_restart =
            serde_json::json!({"pid": restarted, "recording": null, "install": restarted_install});
        let done = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let (done, calls) = (done.clone(), calls.clone());
            let agent_job = login_job.clone();
            spawn_agent(&deps, move |mut listener| async move {
                let mut stopped = false;
                while !done.load(Ordering::SeqCst) {
                    // A short wait, so the agent notices the command ended.
                    let Ok(accepted) =
                        tokio::time::timeout(Duration::from_millis(5), listener.accept()).await
                    else {
                        continue;
                    };
                    let (mut reader, mut writer) = accepted.unwrap().into_split();
                    // Every request of the connection, as the agent answers them.
                    while let Some(line) = tokio::time::timeout(AGENT_WAIT, reader.read_line())
                        .await
                        .expect("the command left its connection idle")
                        .unwrap()
                    {
                        let request = Request::parse(&line).unwrap();
                        let outcome = match &request.call {
                            Call::Ping => {
                                Ok(ping_value(if stopped { restarted } else { first }, false))
                            }
                            Call::Status => Ok(after_restart.clone()),
                            Call::ShutdownIfIdle => {
                                stopped = true;
                                *agent_job.login_job.lock().unwrap() = running_job(restarted);
                                Ok(serde_json::json!({"stopping": true}))
                            }
                            Call::ShutdownAcquire => Err(IpcError::new(IpcError::INSTALLING, HELD)),
                            other => {
                                Err(IpcError::new("internal", format!("unexpected {other:?}")))
                            }
                        };
                        let response = Response {
                            id: request.id,
                            outcome,
                        };
                        calls.lock().unwrap().push(request.call);
                        writer
                            .write_line(response.to_line().unwrap())
                            .await
                            .unwrap();
                    }
                }
            })
        };

        let (code, value) = command(&deps, &["daemon", "stop"]);
        done.store(true, Ordering::SeqCst);
        server.join().unwrap();
        assert_eq!(code, 1, "{value}");
        // The agent's own refusal, never relabeled as recording work.
        assert_eq!(value["error"]["code"], expected_code, "{value}");
        if expected_calls.contains(&Call::ShutdownAcquire) {
            assert_eq!(value["error"]["message"], HELD, "{value}");
        }
        // The restarted agent was asked for nothing else: it was not asked
        // to stop, and no admission was taken from it, confirmed or released.
        assert_eq!(*calls.lock().unwrap(), expected_calls, "{value}");
        // Its login job stays loaded, so the update's agent keeps running.
        assert_eq!(login_job.stops.load(Ordering::SeqCst), 0, "{value}");
    }
}

#[test]
fn stopping_another_profile_leaves_a_recording_login_agent_running() {
    use std::sync::atomic::Ordering;

    let home_a = temp_home();
    let home_b = temp_home();
    let deps_a = fixture(home_a.path());
    let mut deps_b = fixture(home_b.path());
    let pid = std::process::id().wrapping_add(1);
    let login_job = fake_login_job(&mut deps_b, running_job(pid));
    let server_a = fake_daemon(
        &deps_a,
        vec![
            ("ping", ping_value(pid, false)),
            (
                "status",
                serde_json::json!({"recording": {"session_id": "s-synthetic"}}),
            ),
        ],
    );
    // An answering endpoint with a different PID cannot authorize even an
    // initial idle stop while the unrelated login job is live.
    let server_b = fake_daemon(
        &deps_b,
        vec![("ping", ping_value(pid.wrapping_add(1), false))],
    );
    let (code, result) = command(&deps_b, &["daemon", "stop"]);
    server_b.join().unwrap();
    assert_stop_unverified(code, &result);
    assert_eq!(login_job.stops.load(Ordering::SeqCst), 0);

    let (code, result) = command(&deps_a, &["daemon", "status"]);
    server_a.join().unwrap();
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["state"]["recording"]["session_id"], "s-synthetic");
}

#[test]
fn uninstall_refuses_a_login_job_serving_another_profile() {
    let home_a = temp_home();
    let home_b = temp_home();
    let deps_a = fixture(home_a.path());
    let mut deps_b = fixture(home_b.path());
    let pid = std::process::id().wrapping_add(1);
    let login_job = fake_login_job(&mut deps_b, running_job(pid));
    *login_job.enabled.lock().unwrap() = true;
    let server_a = fake_daemon(
        &deps_a,
        vec![
            ("ping", ping_value(pid, false)),
            (
                "status",
                serde_json::json!({"recording": {"session_id": ACTIVE}}),
            ),
        ],
    );
    let server_b = fake_daemon(
        &deps_b,
        vec![("ping", ping_value(pid.wrapping_add(1), false))],
    );
    let (code, result) = command(&deps_b, &["daemon", "uninstall"]);
    server_b.join().unwrap();
    assert_stop_unverified(code, &result);
    assert!(*login_job.enabled.lock().unwrap());

    let (code, result) = command(&deps_a, &["daemon", "status"]);
    server_a.join().unwrap();
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["state"]["recording"]["session_id"], ACTIVE);
}

#[test]
fn uninstall_of_this_idle_profile_reuses_the_registration_guard() {
    let home = temp_home();
    let mut deps = fixture(home.path());
    let autostart = FakeAutostart {
        enabled: true.into(),
        login_registration_path: Some(home.path().join("registration.lock")),
        login_job: LoginJob::Idle {
            serves_profile: Some(true),
        }
        .into(),
        ..Default::default()
    };
    let autostart = Arc::new(autostart);
    deps.autostart = Box::new(TestAutostart(autostart.clone()));
    let (code, result) = command(&deps, &["daemon", "uninstall"]);
    assert_eq!(code, 0, "{result}");
    assert!(!*autostart.enabled.lock().unwrap());
}

#[test]
fn uninstall_refuses_an_unloaded_registration_for_another_profile() {
    let home = temp_home();
    let mut deps = fixture(home.path());
    let login = fake_login_job(
        &mut deps,
        LoginJob::Registered {
            serves_profile: Some(false),
        },
    );
    *login.enabled.lock().unwrap() = true;
    let (code, value) = command(&deps, &["daemon", "uninstall"]);
    assert_stop_unverified(code, &value);
    assert!(*login.enabled.lock().unwrap());
}

#[test]
fn daemon_uninstall_requires_the_answering_agents_durable_fence() {
    for (install, name) in [
        (
            serde_json::json!({"fence": 42, "durable": true}),
            "wrong fence",
        ),
        (
            serde_json::json!({"fence": 41, "durable": false}),
            "not durable",
        ),
        (serde_json::Value::Null, "admission gone"),
    ] {
        let home = temp_home();
        let mut deps = fixture(home.path());
        let pid = std::process::id().wrapping_add(1);
        let login = fake_login_job(&mut deps, running_job(pid));
        *login.enabled.lock().unwrap() = true;
        let replies = vec![
            ("ping", ping_value(pid, false)),
            (
                "status",
                serde_json::json!({
                    "pid": pid,
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol": broscribe_ipc::PROTOCOL_VERSION,
                    "library": "",
                    "recording": null,
                    "jobs": broscribe_ipc::JobsState::default(),
                    "preview": broscribe_ipc::PreviewState::Off,
                    "install": install,
                }),
            ),
        ];
        let fake = fake_daemon(&deps, replies);
        let (code, value) = command(&deps, &["daemon", "uninstall", "--admission-fence", "41"]);
        fake.join().unwrap();
        assert_eq!(code, 1, "{name}: {value}");
        assert_eq!(
            value["error"]["code"],
            broscribe_updates::codes::ADMISSION_LOST,
            "{name}"
        );
        assert!(*login.enabled.lock().unwrap(), "{name}");
    }
}

#[test]
fn daemon_uninstall_with_a_fence_refuses_when_the_agent_does_not_answer() {
    let home = temp_home();
    let mut deps = fixture(home.path());
    let login = fake_login_job(
        &mut deps,
        LoginJob::Registered {
            serves_profile: Some(true),
        },
    );
    *login.enabled.lock().unwrap() = true;
    let (code, value) = command(&deps, &["daemon", "uninstall", "--admission-fence", "41"]);
    assert_eq!(code, 1, "{value}");
    assert_eq!(
        value["error"]["code"],
        broscribe_updates::codes::ADMISSION_LOST
    );
    assert!(*login.enabled.lock().unwrap());
}

#[test]
fn daemon_uninstall_if_idle_checks_the_durable_fence_and_job_ownership() {
    for (job, held, expected) in [
        (
            LoginJob::Idle {
                serves_profile: Some(true),
            },
            Some((41, true)),
            None,
        ),
        (
            LoginJob::Registered {
                serves_profile: Some(true),
            },
            Some((41, true)),
            None,
        ),
        (
            LoginJob::Idle {
                serves_profile: Some(false),
            },
            Some((41, true)),
            Some("login_job_unverified"),
        ),
        (
            running_job(123),
            Some((41, true)),
            Some("login_job_unverified"),
        ),
        (
            LoginJob::Idle {
                serves_profile: Some(true),
            },
            Some((40, true)),
            Some("update_admission_lost"),
        ),
        (
            LoginJob::Idle {
                serves_profile: Some(true),
            },
            Some((41, false)),
            Some("update_admission_lost"),
        ),
        (
            LoginJob::Idle {
                serves_profile: Some(true),
            },
            None,
            Some("update_admission_lost"),
        ),
    ] {
        let home = temp_home();
        let mut deps = fixture(home.path());
        let login = fake_login_job(&mut deps, job);
        *login.enabled.lock().unwrap() = true;
        if let Some((fence, durable)) = held {
            let data_dir = deps.paths.data_dir();
            let path = broscribe_ipc::install_admission::Persisted::path(&data_dir);
            broscribe_platform_native::ipc::ensure_private_dir(path.parent().unwrap()).unwrap();
            let record = broscribe_ipc::install_admission::Persisted {
                last_fence: fence,
                held: Some(broscribe_ipc::InstallHold { fence, durable }),
            };
            std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        }
        let (code, value) = command(
            &deps,
            &["daemon", "uninstall", "--admission-fence-if-idle", "41"],
        );
        assert_eq!(code, i32::from(expected.is_some()), "{job:?}: {value}");
        assert_eq!(
            value["error"]["code"].as_str(),
            expected,
            "{job:?}: {value}"
        );
        assert_eq!(
            *login.enabled.lock().unwrap(),
            expected.is_some(),
            "{job:?}"
        );
    }
}

struct RemovalFixture {
    _home: tempfile::TempDir,
    deps: Deps,
    data: std::path::PathBuf,
    cache: std::path::PathBuf,
    library: std::path::PathBuf,
    install: std::path::PathBuf,
    platform: Arc<broscribe_platform::uninstall::FakeDesktopUninstall>,
}

fn claim_profile(data: &std::path::Path) {
    broscribe_core::maintenance::ModeLease::acquire(data, Duration::ZERO).unwrap();
}

impl RemovalFixture {
    fn new() -> Self {
        let home = temp_home();
        let root = home.path();
        let mut deps = fixture(root);
        deps.agent_instance =
            Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default());
        deps.autostart = Box::new(FakeAutostart::default());
        let data = deps.paths.data_dir();
        let cache = deps.paths.cache_dir();
        let library = deps.paths.default_library_dir();
        let install = root.join("install");
        let platform = Arc::new(broscribe_platform::uninstall::FakeDesktopUninstall {
            installation: Some(broscribe_platform::stable_path(&install)),
            ..Default::default()
        });
        deps.agents.uninstall = platform.clone();
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        broscribe_core::Library::open(&library).unwrap();
        broscribe_core::config::Config::default()
            .save(&data)
            .unwrap();
        claim_profile(&data);
        Self {
            _home: home,
            deps,
            data,
            cache,
            library,
            install,
            platform,
        }
    }
}

#[test]
fn whole_app_uninstall_previews_then_preserves_data_and_library_by_default() {
    use broscribe_platform::SecretStore as _;
    let fixture = RemovalFixture::new();
    let secret = "synthetic-do-not-report";
    fixture
        .platform
        .credentials
        .set("provider/demo/api_key", secret)
        .unwrap();
    let initial = std::fs::read_dir(&fixture.data).unwrap().count();
    for args in [&["uninstall"][..], &["uninstall", "--dry-run"]] {
        let (code, preview) = command(&fixture.deps, args);
        assert_eq!(code, 0, "{preview}");
        assert_eq!(preview["dry_run"], true);
        assert_eq!(preview["ready"], true);
        assert!(fixture.install.exists() && fixture.cache.exists());
        assert_eq!(std::fs::read_dir(&fixture.data).unwrap().count(), initial);
        assert_eq!(
            fixture
                .platform
                .credentials
                .get("provider/demo/api_key")
                .unwrap()
                .as_deref(),
            Some(secret)
        );
    }
    let (code, result) = command(&fixture.deps, &["uninstall", "--yes"]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["ready"], true);
    assert!(!result.to_string().contains(secret));
    assert!(fixture.library.is_dir() && fixture.data.is_dir());
    assert!(!fixture.install.exists() && !fixture.cache.exists());
    assert_eq!(
        fixture
            .platform
            .credentials
            .get("provider/demo/api_key")
            .unwrap(),
        None
    );
    let (code, second) = command(&fixture.deps, &["uninstall", "--yes"]);
    assert_eq!(code, 0, "{second}");
}

#[test]
fn uninstall_refuses_extra_installation_entries_and_unknown_profile_configs() {
    use broscribe_platform::release_layout::ReleaseLayout;
    for layout in [ReleaseLayout::Linux, ReleaseLayout::Windows] {
        let mut fixture = RemovalFixture::new();
        fixture.deps.agents.uninstall =
            Arc::new(broscribe_platform::uninstall::FakeDesktopUninstall {
                installation: fixture.platform.installation.clone(),
                release_layout: Some(layout),
                ..Default::default()
            });
        for file in layout.required_files() {
            let path = fixture.install.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "synthetic packaged resource").unwrap();
        }
        let library = fixture.install.join("meeting-library");
        std::fs::create_dir(&library).unwrap();
        let meeting = library.join("keep.md");
        std::fs::write(&meeting, "synthetic meeting").unwrap();
        let (code, result) = command(&fixture.deps, &["uninstall", "--yes"]);
        assert_eq!(code, 1, "{result}");
        assert!(result.to_string().contains("Move them out"));
        assert_eq!(
            std::fs::read_to_string(&meeting).unwrap(),
            "synthetic meeting"
        );
        assert!(fixture.cache.is_dir());
        std::fs::remove_dir_all(&library).unwrap();

        let secondary = fixture._home.path().join("foreign-profile");
        std::fs::create_dir_all(secondary.join("run")).unwrap();
        let config = secondary.join("config.toml");
        std::fs::write(&config, "[other_application]\ncolor = 'blue'\n").unwrap();
        let (code, result) = command(
            &fixture.deps,
            &[
                "uninstall",
                "--yes",
                "--remove-data",
                "--data-dir",
                secondary.to_str().unwrap(),
            ],
        );
        assert_eq!(code, 1, "{result}");
        assert!(
            result
                .to_string()
                .contains("no BroScribe configuration or profile files")
        );
        assert!(config.is_file() && secondary.join("run").is_dir());

        let (code, result) = command(&fixture.deps, &["uninstall", "--yes"]);
        assert_eq!(code, 0, "{result}");
        assert!(!fixture.install.exists());
    }
}

#[test]
fn uninstall_protects_libraries_named_only_by_login_registration() {
    use broscribe_platform::RegisteredProfile;
    use std::path::{Path, PathBuf};
    struct StoppedRegistration(PathBuf, PathBuf);
    impl Autostart for StoppedRegistration {
        fn is_enabled(&self) -> broscribe_core::Result<bool> {
            Ok(true)
        }
        fn set_enabled(&self, _: bool) -> broscribe_core::Result<()> {
            panic!("the overlap must be refused before changing registration")
        }
        fn login_job(&self, data: &Path) -> LoginJob {
            LoginJob::Registered {
                serves_profile: Some(data == self.0),
            }
        }
        fn registered_profiles(&self) -> broscribe_core::Result<Vec<RegisteredProfile>> {
            Ok(vec![RegisteredProfile {
                data_dir: self.0.clone(),
                library: Some(self.1.clone()),
            }])
        }
    }
    for linux in [false, true] {
        let mut fixture = RemovalFixture::new();
        let data = broscribe_platform::stable_path(&fixture._home.path().join("stopped-profile"));
        broscribe_core::config::Config::update(&data, |config| {
            config.library = Some(fixture._home.path().join("unrelated-configured-library"));
            Ok(())
        })
        .unwrap();
        claim_profile(&data);
        let container = if linux { &data } else { &fixture.install };
        let library = container.join("registered-library");
        std::fs::create_dir(&library).unwrap();
        let meeting = library.join("keep.md");
        std::fs::write(&meeting, "synthetic meeting").unwrap();
        fixture.deps.autostart = Box::new(StoppedRegistration(data, library));
        let mut args = vec!["uninstall", "--yes"];
        if linux {
            args.push("--remove-data");
        }
        let (code, report) = command(&fixture.deps, &args);
        assert_eq!(code, 1, "{report}");
        let action = if linux {
            "Validate data removal"
        } else {
            "Validate app removal"
        };
        assert!(report.to_string().contains(action), "{report}");
        assert!(meeting.is_file());
    }
}

#[test]
fn uninstall_keeps_the_app_when_credentials_fail_and_only_skips_them_on_request() {
    use broscribe_platform::SecretStore as _;
    let mut fixture = RemovalFixture::new();
    let platform = Arc::new(broscribe_platform::uninstall::FakeDesktopUninstall {
        installation: fixture.platform.installation.clone(),
        credentials_unreachable: true,
        ..Default::default()
    });
    platform.credentials.set("synthetic", "keep").unwrap();
    fixture.deps.agents.uninstall = platform.clone();

    let (code, result) = command(&fixture.deps, &["uninstall", "--yes"]);
    assert_ne!(code, 0, "{result}");
    assert!(fixture.install.is_dir(), "{result}");
    assert!(result["steps"].as_array().unwrap().iter().any(|step| {
        step["action"] == "Remove BroScribe credentials" && step["status"] == "failed"
    }));

    let (code, plan) = command(
        &fixture.deps,
        &["uninstall", "--dry-run", "--skip-credentials"],
    );
    assert_eq!(code, 0, "{plan}");
    assert!(plan["steps"].as_array().unwrap().iter().any(|step| {
        step["action"] == "Sign out of integrations and remove BroScribe credentials"
            && step["status"] == "skipped"
    }));
    let (code, result) = command(&fixture.deps, &["uninstall", "--yes", "--skip-credentials"]);
    assert_eq!(code, 0, "{result}");
    assert!(!fixture.install.exists());
    assert_eq!(
        platform.credentials.get("synthetic").unwrap().as_deref(),
        Some("keep")
    );
}

#[test]
fn uninstall_retry_from_default_recovers_a_secondary_profile_after_login_removal() {
    use broscribe_platform::uninstall::{DesktopUninstall, Layout};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct Login(Mutex<Option<PathBuf>>);
    impl Autostart for Login {
        fn is_enabled(&self) -> broscribe_core::Result<bool> {
            Ok(self.0.lock().unwrap().is_some())
        }
        fn set_enabled(&self, enabled: bool) -> broscribe_core::Result<()> {
            assert!(!enabled);
            self.0.lock().unwrap().take();
            Ok(())
        }
        fn login_job(&self, profile: &Path) -> LoginJob {
            self.0
                .lock()
                .unwrap()
                .as_ref()
                .map_or(LoginJob::Absent, |path| LoginJob::Registered {
                    serves_profile: Some(path == profile),
                })
        }
        fn registered_profiles(
            &self,
        ) -> broscribe_core::Result<Vec<broscribe_platform::RegisteredProfile>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .map(Into::into)
                .collect())
        }
    }

    struct Credentials {
        profile: PathBuf,
        pending: Mutex<bool>,
    }
    impl DesktopUninstall for Credentials {
        fn layout(&self, _: &Path) -> broscribe_core::Result<Layout> {
            Ok(Layout::default())
        }
        fn secret_store(&self, _: &Path) -> Arc<dyn broscribe_platform::SecretStore> {
            Arc::new(broscribe_platform::fakes::FakeSecretStore::default())
        }
        fn remove_installation(
            &self,
            _: &Path,
            _: &Path,
            _: &broscribe_platform::uninstall::ReleaseRemovalCheck<'_>,
        ) -> broscribe_core::Result<Option<PathBuf>> {
            unreachable!("the layout has no installation")
        }
        fn remove_credentials(
            &self,
            profiles: &[broscribe_platform::uninstall::CredentialProfile],
            _: bool,
        ) -> broscribe_core::Result<usize> {
            assert!(profiles.iter().any(
                |p| p.data_dir == self.profile && p.namespace == self.profile.to_string_lossy()
            ));
            let mut pending = self.pending.lock().unwrap();
            if std::mem::replace(&mut *pending, false) {
                return Err(broscribe_core::Error::Unsupported(
                    "synthetic credential failure".into(),
                ));
            }
            Ok(1)
        }
    }

    for scenario in [
        "intact",
        "replaced",
        "missing marker",
        "replaced marker",
        "legacy",
        "created-default",
    ] {
        let mut fixture = RemovalFixture::new();
        let secondary =
            broscribe_platform::stable_path(&fixture._home.path().join("login-profile"));
        broscribe_core::config::Config::default()
            .save(&secondary)
            .unwrap();
        claim_profile(&secondary);
        if scenario == "created-default" {
            std::fs::remove_dir_all(&fixture.data).unwrap();
        }
        fixture.deps.autostart = Box::new(Login(Mutex::new(Some(secondary.clone()))));
        fixture.deps.agents.uninstall = Arc::new(Credentials {
            profile: secondary.clone(),
            pending: Mutex::new(true),
        });

        let (code, result) = command(&fixture.deps, &["uninstall", "--yes", "--remove-data"]);
        assert_eq!(code, 1, "{scenario}: {result}");
        assert!(
            fixture
                .deps
                .autostart
                .registered_profiles()
                .unwrap()
                .is_empty()
        );
        let marker = fixture.data.join("uninstall-pending.json");
        assert!(marker.is_file());
        let entry = serde_json::from_slice::<Value>(&std::fs::read(&marker).unwrap()).unwrap();
        assert!(entry["profiles"][0]["identity"].is_object());
        assert_eq!(
            entry["profiles"][0]["marker"].as_str(),
            broscribe_core::maintenance::profile_id(&secondary)
                .unwrap()
                .as_deref()
        );

        let config = secondary.join("config.toml");
        let unrelated = "[other_application]\ncolor = 'blue'\n";
        match scenario {
            "replaced" => {
                std::fs::rename(&secondary, secondary.with_file_name("old-profile")).unwrap();
                std::fs::create_dir(&secondary).unwrap();
                // A genuine-looking configuration still cannot claim the old identity.
                broscribe_core::config::Config::default()
                    .save(&secondary)
                    .unwrap();
            }
            "missing marker" | "replaced marker" => {
                std::fs::remove_file(secondary.join(broscribe_core::maintenance::PROFILE_MARKER))
                    .unwrap();
                if scenario == "replaced marker" {
                    claim_profile(&secondary);
                }
            }
            "legacy" => {
                std::fs::write(&marker, serde_json::to_vec(&[&secondary]).unwrap()).unwrap();
                std::fs::write(&config, unrelated).unwrap();
            }
            _ => {}
        }
        std::fs::create_dir(secondary.join("cache")).unwrap();
        std::fs::write(secondary.join("cache/keep"), "synthetic file").unwrap();
        let configured = std::fs::read(&config).ok();

        let (code, result) = command(&fixture.deps, &["uninstall", "--yes", "--remove-data"]);
        assert_eq!(code, 0, "{scenario}: {result}");
        assert!(!marker.exists());
        if matches!(
            scenario,
            "replaced" | "missing marker" | "replaced marker" | "legacy"
        ) {
            assert!(result.to_string().contains("Clean up profile files"));
            assert_eq!(std::fs::read(&config).ok(), configured);
            assert!(secondary.join("cache/keep").is_file());
        } else {
            assert!(secondary.join("cache/keep").is_file());
            assert!(!config.exists());
            assert!(result.to_string().contains("not created by BroScribe"));
        }
        if scenario == "created-default" {
            assert!(!fixture.data.exists());
        }
    }
}

#[test]
fn uninstall_leaves_an_empty_discard_directory_with_retained_profile_data() {
    let fixture = RemovalFixture::new();
    let discarded = broscribe_core::lifecycle::journal_root(&fixture.data).join(".discard");
    std::fs::create_dir_all(&discarded).unwrap();
    let (code, result) = command(&fixture.deps, &["uninstall", "--yes"]);
    assert_eq!(code, 0, "{result}");
    assert!(!fixture.install.exists());
    assert!(fixture.library.is_dir());
    assert!(discarded.is_dir());
}

#[test]
fn uninstall_requires_both_flags_to_discard_journals_and_keeps_library() {
    let fixture = RemovalFixture::new();
    let journal = broscribe_core::lifecycle::journal_root(&fixture.data).join("s-synthetic");
    std::fs::create_dir_all(&journal).unwrap();
    std::fs::write(
        journal.join(broscribe_core::lifecycle::SESSION_MANIFEST),
        b"unreadable synthetic session manifest",
    )
    .unwrap();
    for args in [
        &["uninstall", "--yes"][..],
        &["uninstall", "--yes", "--remove-data"],
    ] {
        let (code, result) = command(&fixture.deps, args);
        assert_eq!(code, 1, "{result}");
        assert!(
            result
                .to_string()
                .contains("unsaved or processing recordings")
        );
        assert!(journal.is_dir() && fixture.install.is_dir());
    }
    let (code, result) = command(
        &fixture.deps,
        &[
            "uninstall",
            "--yes",
            "--remove-data",
            "--discard-unsaved",
            "--keep-app",
        ],
    );
    assert_eq!(code, 0, "{result}");
    assert!(!fixture.data.exists());
    assert!(fixture.install.is_dir() && fixture.library.is_dir());
}

#[test]
fn uninstall_leaves_a_locked_update_stage_in_place() {
    let fixture = RemovalFixture::new();
    let stage = fixture.install.with_file_name(".install.update");
    let lock = fixture.install.with_file_name(".install.update.lock");
    std::fs::create_dir_all(&stage).unwrap();
    let held = std::fs::File::create(&lock).unwrap();
    held.try_lock().unwrap();
    let (code, result) = command(&fixture.deps, &["uninstall", "--yes"]);
    assert_eq!(code, 1, "{result}");
    assert!(
        result
            .to_string()
            .contains("Check for an update in progress")
    );
    assert!(stage.is_dir() && fixture.install.is_dir() && fixture.cache.is_dir());
}

#[test]
fn a_temporary_library_selection_cannot_hide_the_default_portable_library() {
    let mut fixture = RemovalFixture::new();
    let meeting = fixture.library.join("synthetic-meeting");
    std::fs::write(&meeting, "keep this meeting").unwrap();
    fixture.deps.agents.uninstall = Arc::new(broscribe_platform::uninstall::FakeDesktopUninstall {
        installation: Some(broscribe_platform::stable_path(&fixture.library)),
        ..Default::default()
    });
    let temporary = fixture._home.path().join("temporary-library");
    for persisted in [
        fixture.library.clone(),
        fixture._home.path().join("configured-library"),
    ] {
        broscribe_core::config::Config::update(&fixture.data, |config| {
            config.library = Some(persisted.clone());
            Ok(())
        })
        .unwrap();
        let (code, result) = command(
            &fixture.deps,
            &[
                "--library",
                temporary.to_str().unwrap(),
                "uninstall",
                "--yes",
            ],
        );
        assert_eq!(code, 1, "{result}");
        assert!(
            result.to_string().contains("Validate app removal"),
            "{result}"
        );
        assert_eq!(
            std::fs::read_to_string(&meeting).unwrap(),
            "keep this meeting"
        );
    }
}

#[test]
fn data_and_runtime_removal_protect_a_persisted_library_hidden_by_an_override() {
    let fixture = RemovalFixture::new();
    let library = fixture.data.join("updates/library");
    broscribe_core::Library::open(&library).unwrap();
    broscribe_core::config::Config::update(&fixture.data, |config| {
        config.library = Some(library.clone());
        Ok(())
    })
    .unwrap();
    let temporary = fixture._home.path().join("temporary-library");
    let (code, result) = command(
        &fixture.deps,
        &[
            "--library",
            temporary.to_str().unwrap(),
            "uninstall",
            "--yes",
            "--remove-data",
        ],
    );
    assert_eq!(code, 1, "{result}");
    assert!(result.to_string().contains("Validate data removal"));
    assert!(
        result
            .to_string()
            .contains("Validate cache and runtime removal")
    );
    assert!(library.exists() && fixture.install.exists());
}

#[test]
fn uninstall_protects_a_library_reported_only_by_the_running_agent() {
    let fixture = RemovalFixture::new();
    let live = broscribe_platform::stable_path(&fixture.install);
    let server = fake_agent(
        &fixture.deps,
        vec![(
            broscribe_ipc::Call::Status,
            Ok(serde_json::json!({
                "pid": 70, "version": "1.0.0", "protocol": broscribe_ipc::PROTOCOL_VERSION,
                "library": live, "recording": null, "jobs": {"waiting": 0, "deferred": 0},
                "preview": {"state": "off"},
            })),
        )],
    );
    let (code, result) = command(&fixture.deps, &["uninstall", "--yes"]);
    server.join().unwrap();
    assert_eq!(code, 1, "{result}");
    assert!(
        result.to_string().contains("Validate app removal"),
        "{result}"
    );
    assert!(fixture.install.exists());
}

#[test]
fn uninstall_only_removes_unprotected_staging_with_a_lock_and_a_discovered_owner() {
    for (locked, owner, protected) in [
        (false, Some(true), false),
        (true, None, false),
        (true, Some(false), false),
        (true, Some(true), true),
        (true, Some(true), false),
    ] {
        let fixture = RemovalFixture::new();
        let stage = fixture.install.with_file_name(".install.update");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("download.zip"), "contents").unwrap();
        if locked {
            std::fs::File::create(fixture.install.with_file_name(".install.update.lock")).unwrap();
        }
        if let Some(ours) = owner {
            let data = if ours {
                fixture.data.clone()
            } else {
                fixture.data.with_file_name("other")
            };
            let state = broscribe_platform::stable_path(&data.join("updates"));
            let build = |build| {
                serde_json::json!({
                    "product": "BroScribe", "app_id": "org.broscribe.BroScribe",
                    "version": "1.0.0", "build": build, "target": "synthetic"
                })
            };
            std::fs::write(
                stage.join("owner.json"),
                serde_json::json!({"schema": 1, "state_dir": state, "from": build(10), "to": build(11)}).to_string(),
            ).unwrap();
        }
        if protected {
            broscribe_core::config::Config::update(&fixture.data, |config| {
                config.library = Some(stage.clone());
                Ok(())
            })
            .unwrap();
        }
        let (code, result) = command(&fixture.deps, &["uninstall", "--yes"]);
        assert_eq!(code, 0, "{result}");
        let step = result["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["action"] == "Remove update staging")
            .unwrap();
        let removed = locked && owner == Some(true) && !protected;
        assert_eq!(
            step["status"],
            if removed { "done" } else { "skipped" },
            "{result}"
        );
        assert_eq!(stage.exists(), !removed);
    }
}

#[test]
fn a_mac_uninstall_resumes_a_journaled_tombstone_after_the_bundle_seal_breaks() {
    use broscribe_platform::HostPlatform;
    use broscribe_platform::uninstall::DesktopUninstall as _;
    use broscribe_platform_native::NativePlatform;
    if NativePlatform::host_platform() != HostPlatform::MacOs {
        return;
    }

    let fixture = RemovalFixture::new();
    let staging = fixture.install.with_file_name(".install.update");
    let state = broscribe_platform::stable_path(&fixture.data.join("updates"));
    let tombstone = staging.join("deleting-incoming");
    let bundle = tombstone.join("BroScribe.app");
    let contents = bundle.join("Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::create_dir_all(contents.join("Resources")).unwrap();
    let resource = contents.join("Resources/fixture.txt");
    std::fs::write(&resource, "sealed synthetic file").unwrap();
    std::fs::copy("/usr/bin/true", contents.join("MacOS/fixture")).unwrap();
    std::fs::write(
        contents.join("Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>org.broscribe.BroScribe</string>
<key>CFBundleExecutable</key><string>fixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#,
    )
    .unwrap();
    let executable = contents.join("MacOS/fixture");
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let uninstaller = broscribe_platform_native::uninstall::NativeUninstall;
    uninstaller.validate_installation(&bundle, false).unwrap();
    let platform = NativePlatform::updater_for(executable);
    let build = |build| {
        serde_json::json!({
            "product": "BroScribe", "app_id": "org.broscribe.BroScribe",
            "version": "1.0.0", "build": build, "target": "aarch64-apple-darwin"
        })
    };
    std::fs::write(
        staging.join("owner.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": 1, "state_dir": &state, "from": build(10), "to": build(11)
        }))
        .unwrap(),
    )
    .unwrap();

    let remove = || {
        broscribe_updates::remove_owned_stage(
            &staging,
            fixture.install.file_name().unwrap(),
            std::slice::from_ref(&state),
            platform.as_ref(),
        )
    };
    std::fs::remove_file(resource).unwrap();
    assert_eq!(
        remove().unwrap_err().code(),
        broscribe_updates::codes::RELEASE_TREE_UNSAFE
    );
    assert!(bundle.exists());
    let proof = platform.directory_identity(&tombstone).unwrap().unwrap();
    std::fs::write(
        staging.join("deleting-incoming.json"),
        serde_json::to_vec(&serde_json::json!({
            "slot": proof,
            "release": platform.directory_identity(&bundle).unwrap()
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(remove().unwrap());
    assert!(!staging.exists());
}

struct WindowsSettingsRemoval {
    inner: Arc<broscribe_platform::uninstall::FakeDesktopUninstall>,
    expected: std::path::PathBuf,
    remove_data: bool,
    keep_app: bool,
    remove_update_runner: bool,
    default_cleanup: Option<(std::path::PathBuf, bool, bool)>,
}

impl broscribe_platform::uninstall::DesktopUninstall for WindowsSettingsRemoval {
    fn layout(
        &self,
        executable: &std::path::Path,
    ) -> broscribe_core::Result<broscribe_platform::uninstall::Layout> {
        self.inner.layout(executable)
    }
    fn shortcut_profiles(&self) -> broscribe_core::Result<Vec<std::path::PathBuf>> {
        self.inner.shortcut_profiles()
    }
    fn remove_credentials(
        &self,
        paths: &[broscribe_platform::uninstall::CredentialProfile],
        all: bool,
    ) -> broscribe_core::Result<usize> {
        self.inner.remove_credentials(paths, all)
    }
    fn secret_store(&self, profile: &std::path::Path) -> Arc<dyn broscribe_platform::SecretStore> {
        self.inner.secret_store(profile)
    }
    fn remove_installation(
        &self,
        _: &std::path::Path,
        _: &std::path::Path,
        _: &broscribe_platform::uninstall::ReleaseRemovalCheck<'_>,
    ) -> broscribe_core::Result<Option<std::path::PathBuf>> {
        panic!("Settings must use the after-exit helper");
    }
    fn defers_settings_cleanup(&self) -> bool {
        true
    }
    fn remove_after_exit(
        &self,
        _: &std::path::Path,
        root: Option<&std::path::Path>,
        _: Option<&broscribe_platform::uninstall::ReleaseRemovalCheck<'_>>,
        profile: &broscribe_platform::uninstall::DeferredProfile,
        before_launch: &mut dyn FnMut(
            &std::path::Path,
        ) -> broscribe_core::Result<std::path::PathBuf>,
    ) -> broscribe_core::Result<std::path::PathBuf> {
        assert_eq!(
            root,
            if self.keep_app {
                None
            } else {
                self.inner.installation.as_deref()
            }
        );
        assert_eq!(profile.data_dir, self.expected);
        assert_eq!(profile.remove_data, self.remove_data);
        assert_eq!(profile.remove_update_runner, self.remove_update_runner);
        assert!(profile.data_dir.join("run/app.lock").exists());
        assert!(profile.data_dir.join("maintenance.lock").exists());
        assert!(!profile.data_dir.join("daemon-start.lock").exists());
        let target = if profile.remove_data {
            profile.data_dir.clone()
        } else {
            profile.data_dir.join("run")
        };
        assert!(!profile.protection.conflicts(&target, true));
        let seen = profile.default_cleanup.as_ref().map(|cleanup| {
            assert_eq!(
                broscribe_platform::uninstall::directory_identity(&cleanup.data_dir).unwrap(),
                Some(cleanup.identity)
            );
            if let Some(protection) = &cleanup.remove_data {
                assert!(!protection.conflicts(&cleanup.data_dir, true));
            }
            assert!(cleanup.data_dir.join("uninstall-pending.json").is_file());
            (
                cleanup.data_dir.clone(),
                cleanup.remove_data.is_some(),
                cleanup.created_for_pending_list,
            )
        });
        assert_eq!(seen, self.default_cleanup);
        let receipt = self.expected.with_file_name("synthetic-result.txt");
        before_launch(&receipt)?;
        Ok(receipt)
    }
}

#[test]
fn windows_uninstall_refuses_a_working_directory_inside_the_installation() {
    const CHILD: &str = "BROSCRIBE_TEST_UNINSTALL_CALLER_CWD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env(CHILD, "1")
            .args([
                "--exact",
                "windows_uninstall_refuses_a_working_directory_inside_the_installation",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }

    let mut fixture = RemovalFixture::new();
    let other_data = fixture.data.with_file_name("discovered-profile");
    let other_library = fixture.library.with_file_name("discovered-library");
    let platform = Arc::new(broscribe_platform::uninstall::FakeDesktopUninstall {
        profiles: vec![other_data.clone()],
        installation: fixture.platform.installation.clone(),
        ..Default::default()
    });
    claim_profile(&other_data);
    broscribe_core::config::Config::update(&other_data, |config| {
        config.library = Some(other_library.clone());
        Ok(())
    })
    .unwrap();
    broscribe_core::Library::open(&other_library).unwrap();
    fixture.deps.agents.uninstall = Arc::new(WindowsSettingsRemoval {
        inner: platform,
        expected: broscribe_platform::stable_path(&fixture.data),
        remove_data: false,
        keep_app: false,
        remove_update_runner: false,
        default_cleanup: None,
    });
    let original_dir = std::env::current_dir().unwrap();
    let inside = fixture.install.join("nested");
    std::fs::create_dir(&inside).unwrap();
    std::env::set_current_dir(&inside).unwrap();
    let (preview_exit, preview) = command(&fixture.deps, &["uninstall", "--remove-library"]);
    let (apply_exit, apply) = command(&fixture.deps, &["uninstall", "--remove-data", "--yes"]);
    let (retain_exit, retain) = command(&fixture.deps, &["uninstall"]);
    let (keep_exit, keep) = command(&fixture.deps, &["uninstall", "--keep-app"]);
    std::env::set_current_dir(original_dir).unwrap();

    assert_eq!(preview_exit, 2, "{preview}");
    assert_eq!(apply_exit, 1, "{apply}");
    assert_eq!(retain_exit, 2, "{retain}");
    for (report, targets) in [
        (preview, vec![("Library", &fixture.library)]),
        (
            apply,
            vec![("Data folder", &fixture.data), ("Data folder", &other_data)],
        ),
        (retain, vec![]),
    ] {
        let refusal = report["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["code"] == "installation_working_directory")
            .unwrap();
        let detail = refusal["detail"].as_str().unwrap();
        let lines: Vec<_> = detail
            .lines()
            .filter(|line| line.starts_with("Data folder:") || line.starts_with("Library:"))
            .collect();
        assert_eq!(lines.len(), targets.len(), "{detail}");
        if targets.is_empty() {
            assert!(
                detail.contains("Check the plan before applying with --yes."),
                "{detail}"
            );
        } else {
            assert!(
                detail.contains("Check the data and library paths before applying with --yes."),
                "{detail}"
            );
        }
        for (label, path) in targets {
            let path = broscribe_platform::plain_verbatim(&broscribe_platform::stable_path(path));
            assert!(path.is_absolute());
            assert!(
                lines.contains(&format!("{label}: {}", path.display()).as_str()),
                "{detail}"
            );
        }
        assert!(detail.contains("Change to another folder"), "{detail}");
        assert!(
            detail.contains("--dry-run") && detail.contains("--yes"),
            "{detail}"
        );
    }
    assert_eq!(keep_exit, 0, "{keep}");
    assert!(fixture.install.is_dir());
    assert!(fixture.data.join("config.toml").is_file());
    assert!(
        !fixture
            .data
            .with_extension("uninstall-helper.json")
            .exists()
    );
}

#[test]
fn uninstall_refuses_relative_profile_selectors_from_either_working_folder() {
    use broscribe_core::{Library, config::Config, paths::INDEX_DB};
    use broscribe_platform::paths::{ENV_DATA_DIR, ENV_LIBRARY};
    use std::path::Path;

    const CHILD: &str = "BROSCRIBE_TEST_RELATIVE_UNINSTALL_DATA";
    let relative = Path::new("profile");
    let Some(source) = std::env::var_os(CHILD) else {
        for source in ["--data-dir", ENV_DATA_DIR] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .env(CHILD, source)
                .env_remove(ENV_DATA_DIR)
                .env_remove(ENV_LIBRARY)
                .args([
                    "--exact",
                    "uninstall_refuses_relative_profile_selectors_from_either_working_folder",
                ]);
            if source == ENV_DATA_DIR {
                child.env(ENV_DATA_DIR, relative);
            }
            let output = child.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{source}: {stdout}"
            );
        }
        return;
    };

    let fixture = RemovalFixture::new();
    let original_dir = std::env::current_dir().unwrap();
    let folders = ["one", "two"].map(|name| fixture.install.with_file_name(name));
    let files = folders.each_ref().map(|folder| {
        let data = folder.join(relative);
        let library = folder.join("meetings");
        claim_profile(&data);
        Config::update(&data, |config| {
            config.library = Some(library.clone());
            Ok(())
        })
        .unwrap();
        Library::open(&library).unwrap();
        let index = library.join(INDEX_DB);
        std::fs::write(&index, b"synthetic index").unwrap();
        [Config::path(&data), data.join(".broscribe-profile"), index]
    });
    for cwd in &folders {
        std::env::set_current_dir(cwd).unwrap();
        let mut args = vec!["uninstall", "--keep-app", "--skip-credentials"];
        if source == "--data-dir" {
            args.extend([
                "--data-dir",
                fixture.data.to_str().unwrap(),
                "--data-dir",
                relative.to_str().unwrap(),
            ]);
        }
        for removal in [None, Some("--remove-library"), Some("--remove-data")] {
            for (mode, expected_exit) in [(None, 2), (Some("--dry-run"), 2), (Some("--yes"), 1)] {
                let invocation = args
                    .iter()
                    .copied()
                    .chain(removal)
                    .chain(mode)
                    .collect::<Vec<_>>();
                let (exit, report) = command(&fixture.deps, &invocation);
                assert_eq!(exit, expected_exit, "{report}");
                assert_eq!(report["ready"], false, "{report}");
                let refusal = report["steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|step| {
                        step["action"] == "Validate removal selectors" && step["status"] == "failed"
                    })
                    .unwrap_or_else(|| panic!("{report}"));
                let detail = refusal["detail"].as_str().unwrap();
                assert!(detail.contains(source.to_str().unwrap()), "{detail}");
                assert!(detail.contains("absolute path"), "{detail}");
                for path in files.iter().flatten() {
                    assert!(path.is_file(), "{} was removed", path.display());
                }
            }
        }
    }
    std::env::set_current_dir(original_dir).unwrap();
}

#[test]
fn maintenance_refuses_a_relative_macos_default_data_folder() {
    use broscribe_platform::HostPlatform;
    use broscribe_platform::paths::{ENV_DATA_DIR, ENV_LIBRARY};
    use broscribe_platform_native::NativePlatform;

    if NativePlatform::host_platform() != HostPlatform::MacOs {
        return;
    }
    const CHILD: &str = "BROSCRIBE_TEST_RELATIVE_MAINTENANCE_HOME";
    if std::env::var_os(CHILD).is_none() {
        let profile = temp_home();
        for absolute_override in [false, true] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .env(CHILD, "1")
                .env("HOME", "relative-home")
                .env_remove(ENV_DATA_DIR)
                .env_remove(ENV_LIBRARY)
                .args([
                    "--exact",
                    "maintenance_refuses_a_relative_macos_default_data_folder",
                ]);
            if absolute_override {
                child.env(ENV_DATA_DIR, profile.path().join("profile"));
            }
            let output = child.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}"
            );
        }
        assert!(!profile.path().join("profile").exists());
        return;
    }

    let mut fixture = RemovalFixture::new();
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(fixture._home.path()).unwrap();
    fixture.deps.paths = NativePlatform::paths();
    let native = fixture.deps.paths.data_dir();
    assert_eq!(
        native,
        std::path::Path::new("relative-home/Library/Application Support/BroScribe")
    );
    let refusal = format!(
        "The default data folder ({}) from HOME is relative",
        native.display()
    );
    for args in [
        &["update", "install", "--yes"][..],
        &["update", "rollback", "--yes"],
        &["update", "recover"],
    ] {
        let (exit, reply) = command(&fixture.deps, args);
        assert_eq!(exit, 1, "{reply}");
        assert_eq!(reply["error"]["code"], "invalid_input", "{reply}");
        assert!(
            reply["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&refusal),
            "{reply}"
        );
    }
    for removal in [None, Some("--remove-library"), Some("--remove-data")] {
        for (mode, expected_exit) in [(None, 2), (Some("--dry-run"), 2), (Some("--yes"), 1)] {
            let args = ["uninstall", "--keep-app", "--skip-credentials"]
                .into_iter()
                .chain(removal)
                .chain(mode)
                .collect::<Vec<_>>();
            let (exit, report) = command(&fixture.deps, &args);
            assert_eq!(exit, expected_exit, "{report}");
            assert_eq!(report["ready"], false, "{report}");
            let steps = report["steps"].as_array().unwrap();
            assert_eq!(steps.len(), 1, "{report}");
            assert_eq!(steps[0]["action"], "Validate removal selectors", "{report}");
            assert_eq!(steps[0]["status"], "failed", "{report}");
            assert!(
                steps[0]["detail"].as_str().unwrap().contains(&refusal),
                "{report}"
            );
        }
    }
    assert!(!std::path::Path::new("relative-home").exists());
    std::env::set_current_dir(original_dir).unwrap();
}

#[test]
fn uninstall_refuses_a_relative_macos_default_library() {
    use broscribe_platform::paths::{ENV_DATA_DIR, ENV_LIBRARY};
    use broscribe_platform::{AppPaths, HostPlatform};
    use broscribe_platform_native::NativePlatform;
    use std::path::PathBuf;

    struct PathsWithRelativeLibrary {
        fake: FakePaths,
        library: PathBuf,
    }

    impl AppPaths for PathsWithRelativeLibrary {
        fn data_dir(&self) -> PathBuf {
            self.fake.data_dir()
        }
        fn cache_dir(&self) -> PathBuf {
            self.fake.cache_dir()
        }
        fn documents_dir(&self) -> PathBuf {
            self.fake.documents_dir()
        }
        fn default_library_dir(&self) -> PathBuf {
            self.library.clone()
        }
    }

    if NativePlatform::host_platform() != HostPlatform::MacOs {
        return;
    }
    const CHILD: &str = "BROSCRIBE_TEST_RELATIVE_UNINSTALL_HOME";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env(CHILD, "1")
            .env("HOME", "relative-home")
            .env_remove(ENV_DATA_DIR)
            .env_remove(ENV_LIBRARY)
            .args([
                "--exact",
                "uninstall_refuses_a_relative_macos_default_library",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}"
        );
        return;
    }

    let mut fixture = RemovalFixture::new();
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(fixture._home.path()).unwrap();
    let library = NativePlatform::paths().default_library_dir();
    assert_eq!(library, std::path::Path::new("relative-home/BroScribe"));
    // Keep the data folder absolute to reach the separate library check.
    fixture.deps.paths = Box::new(PathsWithRelativeLibrary {
        fake: FakePaths::new(fixture._home.path()),
        library: library.clone(),
    });
    broscribe_core::Library::open(&library).unwrap();
    let index = library.join(broscribe_core::paths::INDEX_DB);
    std::fs::write(&index, b"synthetic index").unwrap();
    for removal in ["--remove-library", "--remove-data"] {
        for (mode, expected_exit) in [("--dry-run", 2), ("--yes", 1)] {
            let (exit, report) = command(
                &fixture.deps,
                &[
                    "uninstall",
                    "--keep-app",
                    "--skip-credentials",
                    removal,
                    mode,
                ],
            );
            assert_eq!(exit, expected_exit, "{report}");
            assert_eq!(report["ready"], false, "{report}");
            assert!(
                report["steps"].as_array().unwrap().iter().any(|step| {
                    step["status"] == "failed"
                        && step["detail"].as_str().is_some_and(|text| {
                            text.contains(
                                "The default library path (relative-home/BroScribe) is relative",
                            )
                        })
                }),
                "{report}"
            );
            assert!(index.is_file());
        }
    }
    let (exit, report) = command(
        &fixture.deps,
        &[
            "--library",
            fixture.library.to_str().unwrap(),
            "uninstall",
            "--keep-app",
            "--skip-credentials",
            "--remove-library",
            "--dry-run",
        ],
    );
    assert_eq!(exit, 0, "{report}");
    std::env::set_current_dir(original_dir).unwrap();
}

#[test]
fn uninstall_library_removal_refuses_relative_selections_before_and_after_changing_folders() {
    use broscribe_core::config::Config;
    use broscribe_platform::paths::{ENV_LIBRARY, ENV_MAINTENANCE_CONFIG_BASE};
    use std::path::Path;

    const CHILD: &str = "BROSCRIBE_TEST_RELATIVE_UNINSTALL_LIBRARY";
    let relative = Path::new("..").join("Meetings");
    let Some(source) = std::env::var_os(CHILD) else {
        for source in [
            "config",
            "--library",
            ENV_LIBRARY,
            "secondary",
            "shadowed-config",
            "shadowed-env",
        ] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .env(CHILD, source)
                .env_remove(ENV_LIBRARY)
                .env_remove(ENV_MAINTENANCE_CONFIG_BASE)
                .args([
                    "--exact",
                    "uninstall_library_removal_refuses_relative_selections_before_and_after_changing_folders",
                ]);
            if source == ENV_LIBRARY || source == "shadowed-env" {
                child.env(ENV_LIBRARY, &relative);
            }
            let output = child.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{source}: {stdout}"
            );
        }
        return;
    };

    let mut fixture = RemovalFixture::new();
    fixture.deps.agents.uninstall = Arc::new(WindowsSettingsRemoval {
        inner: fixture.platform.clone(),
        expected: broscribe_platform::stable_path(&fixture.data),
        remove_data: false,
        keep_app: false,
        remove_update_runner: false,
        default_cleanup: None,
    });
    let original_dir = std::env::current_dir().unwrap();
    let outside = fixture.install.with_file_name("Work").join("Temp");
    std::fs::create_dir_all(&outside).unwrap();
    let folders = [&fixture.install, &outside];
    let libraries = folders.map(|cwd| cwd.parent().unwrap().join("Meetings"));
    for library in &libraries {
        broscribe_core::Library::open(library).unwrap();
    }
    let secondary = fixture.data.with_file_name("secondary-profile");
    let mut args = vec!["uninstall"];
    if source == "shadowed-config" || source == "shadowed-env" {
        args.splice(..0, ["--library", fixture.library.to_str().unwrap()]);
    }
    let expected_source = if source == "--library" {
        args.splice(..0, ["--library", relative.to_str().unwrap()]);
        "--library"
    } else if source == ENV_LIBRARY || source == "shadowed-env" {
        ENV_LIBRARY
    } else {
        let profile = if source == "secondary" {
            claim_profile(&secondary);
            args.extend(["--data-dir", secondary.to_str().unwrap()]);
            &secondary
        } else {
            &fixture.data
        };
        Config::update(profile, |config| {
            config.library = Some(relative.clone());
            Ok(())
        })
        .unwrap();
        "config `library`"
    };

    for cwd in folders {
        std::env::set_current_dir(cwd).unwrap();
        for removal in ["--remove-library", "--remove-data"] {
            for (mode, expected_exit) in [("--dry-run", 2), ("--yes", 1)] {
                args.extend([removal, mode]);
                let (exit, report) = command(&fixture.deps, &args);
                args.truncate(args.len() - 2);
                assert_eq!(exit, expected_exit, "{report}");
                let refusal = report["steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|step| {
                        step["action"] == "Validate removal selectors" && step["status"] == "failed"
                    })
                    .unwrap_or_else(|| panic!("{report}"));
                let detail = refusal["detail"].as_str().unwrap();
                assert!(detail.contains(expected_source), "{detail}");
                assert!(
                    detail.contains("absolute") && detail.contains("--dry-run"),
                    "{detail}"
                );
            }
        }
    }
    std::env::set_current_dir(original_dir).unwrap();
    for library in libraries {
        assert!(library.is_dir());
    }
}

#[test]
fn windows_settings_uninstall_defers_the_open_profile_files_and_can_schedule_app_removal() {
    for (remove_data, keep_app) in [(false, false), (true, false), (true, true)] {
        let mut fixture = RemovalFixture::new();
        broscribe_platform_native::ipc::ensure_private_dir(&fixture.data.join("run")).unwrap();
        let app_lock = std::fs::File::create(fixture.data.join("run/app.lock")).unwrap();
        app_lock.try_lock().unwrap();
        let maintenance = std::fs::File::create(fixture.data.join("maintenance.lock")).unwrap();
        maintenance.try_lock_shared().unwrap();
        let runner = fixture.data.join("update-runner");
        std::fs::create_dir(&runner).unwrap();
        let runner_file = runner.join("broscribe-update-runner.exe");
        std::fs::write(&runner_file, "synthetic").unwrap();
        // Simulate a Windows process still holding a runner file open.
        #[cfg(windows)]
        let _runner_lock = {
            use std::os::windows::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(runner_file)
                .unwrap()
        };
        let remove_update_runner = cfg!(windows);
        fixture.deps.agents.uninstall = Arc::new(WindowsSettingsRemoval {
            inner: fixture.platform.clone(),
            expected: broscribe_platform::stable_path(&fixture.data),
            remove_data,
            keep_app,
            remove_update_runner,
            default_cleanup: None,
        });
        let mut args = vec!["uninstall", "--yes", "--from-settings"];
        if remove_data {
            args.push("--remove-data");
        }
        if keep_app {
            args.push("--keep-app");
        }
        let (code, result) = command(&fixture.deps, &args);
        assert_eq!(code, 0, "{result}");
        let steps = result["steps"].as_array().unwrap();
        let pending = steps
            .iter()
            .filter(|step| step["status"] == "pending")
            .collect::<Vec<_>>();
        assert_eq!(
            pending.len(),
            2 + usize::from(!keep_app) + usize::from(remove_update_runner),
            "{result}"
        );
        let reference = pending.last().unwrap();
        assert_eq!(reference["action"], "Clear after-exit helper reference");
        assert_eq!(
            reference["path"].as_str(),
            fixture
                .data
                .with_extension("uninstall-helper.json")
                .to_str()
        );
        if remove_update_runner {
            assert_eq!(pending[1]["action"], "Remove update runner after exit");
            assert!(runner.is_dir());
        }
        let path = if remove_data {
            fixture.data.clone()
        } else {
            fixture.data.join("run")
        };
        assert_eq!(
            pending[0]["path"],
            broscribe_platform::stable_path(&path).display().to_string()
        );
        assert!(fixture.install.exists() && path.exists());
    }
}

#[test]
fn windows_settings_from_secondary_schedules_default_cleanup_before_app_removal() {
    const CASE: &str = "BROSCRIBE_TEST_SECONDARY_UNINSTALL";
    let Some(scenario) = std::env::var_os(CASE) else {
        let temp = temp_home();
        for scenario in ["delete-default", "created-default", "keep-default"] {
            let profile = temp.path().join(scenario);
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windows_settings_from_secondary_schedules_default_cleanup_before_app_removal",
                ])
                .env(CASE, scenario)
                .env("BROSCRIBE_DATA_DIR", profile)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        return;
    };
    let active = broscribe_platform::stable_path(std::path::Path::new(
        &std::env::var_os("BROSCRIBE_DATA_DIR").unwrap(),
    ));
    let mut fixture = RemovalFixture::new();
    let created = scenario == "created-default";
    let remove_data = scenario == "delete-default";
    if created {
        std::fs::remove_dir_all(&fixture.data).unwrap();
    }
    broscribe_core::config::Config::default()
        .save(&active)
        .unwrap();
    claim_profile(&active);
    broscribe_platform_native::ipc::ensure_private_dir(&active.join("run")).unwrap();
    let app = std::fs::File::create(active.join("run/app.lock")).unwrap();
    app.try_lock().unwrap();
    let default = broscribe_platform::stable_path(&fixture.data);
    fixture.deps.agents.uninstall = Arc::new(WindowsSettingsRemoval {
        inner: fixture.platform.clone(),
        expected: active,
        remove_data,
        keep_app: false,
        remove_update_runner: false,
        default_cleanup: Some((default, remove_data, created)),
    });
    let mut args = vec!["uninstall", "--yes", "--from-settings"];
    if remove_data {
        args.push("--remove-data");
    }
    let (code, report) = command(&fixture.deps, &args);
    assert_eq!(code, 0, "{report}");
    let pending = report["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["status"] == "pending")
        .map(|step| step["action"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        pending,
        if remove_data {
            vec![
                "Remove data folder after exit",
                "Remove default data folder after exit",
                "Clear pending uninstall profiles after exit",
                "Remove BroScribe app after exit",
                "Clear after-exit helper reference",
            ]
        } else {
            vec![
                "Remove profile runtime files after exit",
                "Clear pending uninstall profiles after exit",
                "Remove BroScribe app after exit",
                "Clear after-exit helper reference",
            ]
        }
    );
    assert!(fixture.data.join("uninstall-pending.json").is_file());
    assert!(fixture.install.is_dir());
}

#[test]
fn discovery_with_an_unmarked_ui_state_cleans_only_the_recorded_credentials() {
    use broscribe_platform::SecretStore as _;
    let mut fixture = RemovalFixture::new();
    let other = fixture._home.path().join("unrelated");
    std::fs::create_dir_all(other.join("cache")).unwrap();
    std::fs::write(other.join("ui-state.json"), "{}").unwrap();
    broscribe_core::config::Config::default()
        .save(&other)
        .unwrap();
    let uninstaller = Arc::new(broscribe_platform::uninstall::FakeDesktopUninstall {
        profiles: vec![other.clone()],
        ..Default::default()
    });
    uninstaller
        .credentials
        .set("profile-test", "synthetic")
        .unwrap();
    fixture.deps.agents.uninstall = uninstaller.clone();
    let (code, report) = command(&fixture.deps, &["uninstall", "--yes", "--remove-data"]);
    assert_eq!(code, 0, "{report}");
    assert!(report.to_string().contains("No BroScribe profile marker"));
    assert_eq!(
        std::fs::read_to_string(other.join("ui-state.json")).unwrap(),
        "{}"
    );
    assert!(other.join("config.toml").is_file() && other.join("cache").is_dir());
    assert!(
        uninstaller
            .credentials
            .get("profile-test")
            .unwrap()
            .is_none()
    );
}

#[test]
fn uninstall_keeps_an_unlisted_nested_library_unless_it_is_selected() {
    let fixture = RemovalFixture::new();
    let kept = fixture.data.join("archives/meeting-library");
    broscribe_core::Library::open(&kept).unwrap();
    let args = ["uninstall", "--yes", "--remove-data"];
    let (code, report) = command(&fixture.deps, &args);
    assert_eq!(code, 0, "{report}");
    assert!(report.to_string().contains("not created by BroScribe"));
    assert!(kept.is_dir());
    let (code, report) = command(
        &fixture.deps,
        &[
            "--library",
            kept.to_str().unwrap(),
            "uninstall",
            "--yes",
            "--remove-data",
            "--remove-library",
        ],
    );
    assert_eq!(code, 0, "{report}");
    assert!(!kept.exists());
}

#[test]
fn uninstall_protects_a_library_shadowed_by_an_explicit_override() {
    const CASE: &str = "BROSCRIBE_TEST_SHADOWED_UNINSTALL";
    let Some(root) = std::env::var_os(CASE) else {
        let temp = temp_home();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "uninstall_protects_a_library_shadowed_by_an_explicit_override",
            ])
            .env(CASE, temp.path())
            .env("BROSCRIBE_LIBRARY", temp.path().join("data/kept"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    };
    let root = std::path::PathBuf::from(root);
    let mut deps = fixture(&root);
    deps.agent_instance = Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default());
    deps.autostart = Box::new(FakeAutostart::default());
    let kept = std::path::PathBuf::from(std::env::var_os("BROSCRIBE_LIBRARY").unwrap());
    assert!(
        kept.starts_with(deps.paths.data_dir()),
        "{kept:?} / {:?}",
        deps.paths.data_dir()
    );
    std::fs::create_dir_all(&kept).unwrap();
    std::fs::write(kept.join("keep.md"), "synthetic meeting").unwrap();
    claim_profile(&deps.paths.data_dir());
    let library = root.join("different-library");
    let (code, report) = command(
        &deps,
        &[
            "--library",
            library.to_str().unwrap(),
            "uninstall",
            "--yes",
            "--remove-data",
        ],
    );
    assert_eq!(code, 1, "{report}");
    assert!(report.to_string().contains("overlaps BroScribe data"));
    assert!(kept.join("keep.md").is_file());
}

#[test]
fn uninstall_protects_a_library_shared_with_a_discovered_profile() {
    let mut fixture = RemovalFixture::new();
    let other = fixture._home.path().join("other-profile");
    broscribe_core::config::Config::default()
        .save(&other)
        .unwrap();
    claim_profile(&other);
    fixture.deps.agents.uninstall = Arc::new(broscribe_platform::uninstall::FakeDesktopUninstall {
        profiles: vec![other.clone()],
        ..Default::default()
    });
    let (code, result) = command(&fixture.deps, &["uninstall", "--yes", "--remove-library"]);
    assert_eq!(code, 1, "{result}");
    assert!(
        result
            .to_string()
            .contains("Another profile uses this library")
    );
    assert!(fixture.library.exists() && other.exists());
    let (code, result) = command(
        &fixture.deps,
        &[
            "uninstall",
            "--yes",
            "--remove-library",
            "--data-dir",
            other.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{result}");
    assert!(!fixture.library.exists());
    assert!(fixture.data.exists() && other.exists());
}

#[test]
fn uninstall_refuses_to_delete_a_library_inside_a_retained_cleanup_path() {
    let fixture = RemovalFixture::new();
    let nested = fixture.data.join("updates/library");
    broscribe_core::Library::open(&nested).unwrap();
    let mut config = broscribe_core::config::Config::load(&fixture.data).unwrap();
    config.library = Some(nested.clone());
    config.save(&fixture.data).unwrap();
    let (code, result) = command(&fixture.deps, &["uninstall", "--yes"]);
    assert_eq!(code, 1, "{result}");
    assert!(nested.exists() && fixture.install.exists());
}

fn assert_stop_unverified(code: i32, result: &Value) {
    assert_eq!(code, 1, "{result}");
    assert_eq!(result["error"]["code"], "login_job_unverified");
    assert_eq!(
        result["error"]["message"],
        "the BroScribe Agent login job could not be verified for this profile; nothing was stopped"
    );
    assert!(result.get("running").is_none(), "{result}");
    assert!(result.get("stopped").is_none(), "{result}");
}

#[test]
fn stopping_does_not_report_a_live_legacy_job_on_an_unknown_endpoint_as_stopped() {
    use std::sync::atomic::Ordering;

    let home = temp_home();
    let legacy_home = temp_home();
    let legacy_deps = fixture(legacy_home.path());
    let mut deps = fixture(home.path());
    let pid = std::process::id().wrapping_add(1);
    let login_job = fake_login_job(&mut deps, running_job(pid));
    let (check_legacy, ready_to_check) = tokio::sync::oneshot::channel();
    let legacy = spawn_agent(&legacy_deps, move |mut listener| async move {
        // The stop can spend its full manager deadline probing the other profile.
        ready_to_check.await.unwrap();
        let (_, mut writer, request) = next_request(&mut listener).await;
        assert_eq!(request.call, broscribe_ipc::Call::Ping);
        let reply =
            broscribe_ipc::Response::ok(request.id, serde_json::json!({"pid": pid, "protocol": 7}));
        writer.write_line(reply.to_line().unwrap()).await.unwrap();
    });

    let (code, result) = command(&deps, &["daemon", "stop"]);
    assert_stop_unverified(code, &result);
    assert_eq!(login_job.stops.load(Ordering::SeqCst), 0);

    // Check that the old process still listens on its separate endpoint.
    check_legacy.send(()).unwrap();
    let data = broscribe_platform::resolve_paths(legacy_deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let mut client = broscribe_platform_native::ipc::Client::connect_daemon(
        &broscribe_platform_native::ipc::Endpoint::new(&data),
        AGENT_WAIT,
    )
    .unwrap()
    .unwrap();
    assert_eq!(client.call(broscribe_ipc::Call::Ping).unwrap()["pid"], pid);
    legacy.join().unwrap();
}

#[test]
fn stopping_refuses_when_the_login_manager_cannot_identify_its_job() {
    let home = temp_home();
    let mut deps = fixture(home.path());
    let login_job = fake_login_job(&mut deps, LoginJob::Unknown);
    let (code, result) = command(&deps, &["daemon", "stop"]);
    assert_stop_unverified(code, &result);
    assert_eq!(login_job.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn stopping_a_one_shot_login_agent_leaves_its_registration_in_place() {
    let home = temp_home();
    let mut deps = fixture(home.path());
    let login_job = fake_login_job(&mut deps, LoginJob::Unsupported);
    let pid = std::process::id().wrapping_add(1);
    let server = fake_daemon(
        &deps,
        vec![
            ("ping", ping_value(pid, false)),
            ("daemon.stop_if_idle", serde_json::json!({"stopping": true})),
        ],
    );
    let (code, result) = command(&deps, &["daemon", "stop"]);
    server.join().unwrap();
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["stopped"], true);
    assert_eq!(result["login_job_unloaded"], false);
    assert_eq!(login_job.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn legacy_journal_cli_previews_and_requires_confirmation() {
    use broscribe_ipc::{Call, JournalResolution, Response};

    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let legacy = "s-legacy";
    let (code, value) = command(
        &deps,
        &["record", "resolve-journal", "discard", "--session", legacy],
    );
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["error"]["code"], "invalid_input");

    let root = tmp.path().to_path_buf();
    let server = spawn_agent(&deps, move |mut listener| async move {
        for (expected, resolution) in [
            (Call::Status, None),
            (Call::Status, None),
            (Call::Status, None),
            (Call::Status, None),
            (Call::Status, Some(JournalResolution::Adopt)),
            (Call::Status, Some(JournalResolution::Discard)),
        ] {
            let (_, mut writer, request) = next_request(&mut listener).await;
            assert_eq!(request.call, expected);
            let status = serde_json::json!({
                "library": root,
                "journal_issues": [
                    { "session_id": legacy, "code": "journal_library_unknown", "corrupt": false },
                    { "session_id": "s-foreign", "code": "journal_library_mismatch", "corrupt": false },
                ],
            });
            writer
                .write_line(Response::ok(request.id, status).to_line().unwrap())
                .await
                .unwrap();
            if let Some(resolution) = resolution {
                let (_, mut writer, request) = next_request(&mut listener).await;
                assert!(matches!(
                    request.call,
                    Call::RecordResolveJournal {
                        session_id,
                        resolution: requested,
                        library_root,
                        confirmed: true,
                        ..
                    } if session_id == legacy && requested == resolution && library_root == root
                ));
                let status = if resolution == JournalResolution::Adopt {
                    "adopted"
                } else {
                    "discarded"
                };
                writer
                    .write_line(
                        Response::ok(request.id, serde_json::json!({ "status": status }))
                            .to_line()
                            .unwrap(),
                    )
                    .await
                    .unwrap();
            }
        }
    });
    for action in ["adopt", "discard"] {
        let (code, value) = command(
            &deps,
            &[
                "record",
                "resolve-journal",
                action,
                "--session",
                legacy,
                "--dry-run",
            ],
        );
        assert_eq!(code, 0, "{value}");
        assert_eq!(value["dry_run"], true);
        assert_eq!(value["resolution"], action);
    }
    let (code, value) = command(
        &deps,
        &[
            "record",
            "resolve-journal",
            "adopt",
            "--session",
            "s-foreign",
            "--yes",
        ],
    );
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "journal_library_mismatch");
    let other = tempfile::tempdir().unwrap();
    let (code, value) = command(
        &deps,
        &[
            "--library",
            other.path().to_str().unwrap(),
            "record",
            "resolve-journal",
            "adopt",
            "--session",
            legacy,
            "--yes",
        ],
    );
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "library_changed");
    for (action, status) in [("adopt", "adopted"), ("discard", "discarded")] {
        let (code, value) = command(
            &deps,
            &[
                "record",
                "resolve-journal",
                action,
                "--session",
                legacy,
                "--yes",
            ],
        );
        assert_eq!(code, 0, "{value}");
        assert_eq!(value["status"], status);
    }
    server.join().unwrap();
}

#[test]
fn uncertain_mutation_is_reconciled_without_repeating_the_write() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let server = spawn_agent(&deps, |mut listener| async move {
        let mut mutation_id = None;
        loop {
            let (_reader, mut writer, request) = next_request(&mut listener).await;
            match request.call {
                broscribe_ipc::Call::RecordNote { session_id, text } => {
                    assert_eq!((session_id.as_str(), text.as_str()), (ACTIVE, "synthetic"));
                    assert!(mutation_id.is_none());
                    mutation_id = Some(request.id);
                    // Commit happened, but its response is lost.
                }
                broscribe_ipc::Call::RequestStatus { request_id } => {
                    assert_eq!(Some(request_id), mutation_id);
                    let response = broscribe_ipc::Response::ok(
                        request.id,
                        serde_json::json!({"state":"done","result":{"noted":true}}),
                    );
                    writer
                        .write_line(response.to_line().unwrap())
                        .await
                        .unwrap();
                    break;
                }
                other => panic!("unexpected call: {other:?}"),
            }
        }
    });
    let (code, value) = command(&deps, &["record", "note", "--session", ACTIVE, "synthetic"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["noted"], true);
    server.join().unwrap();
}

#[test]
fn trailing_json_is_not_consumed_as_a_recording_tag() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let server = fake_daemon(
        &deps,
        vec![
            (
                "record.status",
                serde_json::json!({"recording":{"session_id": ACTIVE}}),
            ),
            ("record.tag", serde_json::json!({"tags":["a"]})),
        ],
    );
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        ["broscribe", "record", "tag", "+a", "-b", "--json"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        &deps,
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert_eq!(
        serde_json::from_slice::<Value>(&out).unwrap()["tags"],
        serde_json::json!(["a"])
    );
    server.join().unwrap();
}

#[test]
fn integration_setup_preserves_other_provider_settings() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        broscribe_core::config::Config::path(&data),
        "[integration.google]\nclient_id='old-synthetic'\nfuture_setting=true\n",
    )
    .unwrap();
    let server = fake_daemon(
        &deps,
        vec![("integrations.reload", serde_json::json!({"reloaded":true}))],
    );
    let (code, value) = command(
        &deps,
        &[
            "integrations",
            "setup",
            "google",
            "--client-id",
            "new-synthetic",
        ],
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["agent_notified"], true, "{value}");
    server.join().unwrap();
    let config = broscribe_core::config::Config::load(&data).unwrap();
    assert_eq!(
        config.integration["google"]["client_id"].as_str(),
        Some("new-synthetic")
    );
    assert_eq!(
        config.integration["google"]["future_setting"].as_bool(),
        Some(true)
    );
}

#[test]
fn google_client_secret_removal_and_revoking_sign_out_have_json_shapes() {
    use broscribe_integrations::{IntegrationId, account_dir, client_secret_key};
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        broscribe_core::config::Config::path(&data),
        "[integration.google]\nclient_id='synthetic-client'\n",
    )
    .unwrap();
    let secret = client_secret_key(IntegrationId::Google);

    // Without a stored secret, sign-in fails before any browser or network.
    let (code, status) = command(&deps, &["integrations", "status", "google"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["client_secret_stored"], false, "{status}");
    let (code, login) = command(&deps, &["integrations", "login", "google"]);
    assert_eq!(code, 1, "{login}");
    assert_eq!(login["error"]["code"], "client_secret_missing", "{login}");

    deps.secrets
        .set(&secret, "synthetic-desktop-secret")
        .unwrap();
    let (_, status) = command(&deps, &["integrations", "status", "google"]);
    assert_eq!(status["client_secret_stored"], true, "{status}");

    // Refused while signed in; the secret stays.
    let state = account_dir(&data, IntegrationId::Google).join("state.json");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    std::fs::write(&state, r#"{"generation":1,"status":"active"}"#).unwrap();
    let (code, refused) = command(
        &deps,
        &["integrations", "setup", "google", "--clear-client-secret"],
    );
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["error"]["code"], "sign_out_first", "{refused}");
    assert!(deps.secrets.get(&secret).unwrap().is_some());

    // Nothing is stored to revoke, so nothing is sent to Google.
    let (code, logout) = command(
        &deps,
        &["integrations", "logout", "google", "--revoke-at-google"],
    );
    assert_eq!(code, 0, "{logout}");
    assert_eq!(logout["integration"], "google", "{logout}");
    assert_eq!(logout["signed_out"], true, "{logout}");
    assert_eq!(
        logout["remote_revocation"],
        serde_json::json!({"outcome":"no_stored_grant"}),
        "{logout}"
    );
    assert!(logout.get("error").is_none(), "{logout}");
    // Sign-out keeps the client secret.
    assert!(deps.secrets.get(&secret).unwrap().is_some());

    for expected in [true, false] {
        let (code, cleared) = command(
            &deps,
            &["integrations", "setup", "google", "--clear-client-secret"],
        );
        assert_eq!(code, 0, "{cleared}");
        assert_eq!(cleared["client_secret_removed"], expected, "{cleared}");
    }
    assert!(deps.secrets.get(&secret).unwrap().is_none());
    let (_, status) = command(&deps, &["integrations", "status", "google"]);
    assert_eq!(status["client_secret_stored"], false, "{status}");

    for args in [
        vec![
            "integrations",
            "logout",
            "microsoft-graph",
            "--revoke-at-google",
        ],
        vec![
            "integrations",
            "logout",
            "system-calendar",
            "--revoke-at-google",
        ],
        vec![
            "integrations",
            "setup",
            "microsoft-graph",
            "--clear-client-secret",
        ],
        vec![
            "integrations",
            "setup",
            "system-calendar",
            "--clear-client-secret",
        ],
    ] {
        let (code, value) = command(&deps, &args);
        assert_eq!(code, 1, "{args:?}: {value}");
        assert_eq!(value["error"]["code"], "invalid_input", "{args:?}: {value}");
    }
}

/// A saved change stays saved and is reported as such when the running
/// agent refuses to reload: never rolled back, never resent.
#[test]
fn a_refused_agent_reload_keeps_the_saved_setup() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let server = fake_agent(
        &deps,
        vec![(
            broscribe_ipc::Call::IntegrationsReload,
            Err(broscribe_ipc::IpcError::new(
                "internal",
                "synthetic refusal",
            )),
        )],
    );
    let (code, value) = command(
        &deps,
        &[
            "integrations",
            "setup",
            "microsoft-graph",
            "--client-id",
            "synthetic-client",
        ],
    );
    server.join().unwrap();
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["agent_notified"], false, "{value}");
    assert_eq!(value["configured"], true, "{value}");
    assert_eq!(value["integration"], "microsoft-graph", "{value}");
    let config = broscribe_core::config::Config::load(&data).unwrap();
    assert_eq!(
        config.integration["microsoft-graph"]["client_id"].as_str(),
        Some("synthetic-client")
    );
}

#[test]
fn registered_client_changes_retire_only_an_effective_change() {
    use broscribe_integrations::{AccountStatus, IntegrationId, MicrosoftAccount, account_dir};
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let id = IntegrationId::MicrosoftGraph;
    let handle = "11111111111111111111111111111111";
    let secret_key = format!(
        "integration/microsoft-graph/accounts/{handle}/tokens/33333333333333333333333333333333"
    );
    let slot_dir = account_dir(&data, id).join("accounts").join(handle);
    let signed_in = |client: &str| {
        deps.secrets
            .set(
                &secret_key,
                r#"{"access_token":"synthetic-access","refresh_token":"synthetic-refresh","expires_at":4102444800,"scope":"User.Read Calendars.Read offline_access"}"#,
            )
            .unwrap();
        let dir = account_dir(&data, id);
        std::fs::create_dir_all(&slot_dir).unwrap();
        let target = serde_json::json!({"kind":"own","calendar_id":"synthetic-calendar"});
        let registry = serde_json::json!({
            "version":1,
            "accounts":[{
                "view":{
                    "account":{"integration":"microsoft-graph","account_id":handle},
                    "identity":{
                        "authority_host":"login.microsoftonline.com",
                        "resource_tenant_id":"synthetic-tenant",
                        "object_id":"synthetic-user",
                        "client_id":client,
                        "verified_addresses":["synthetic@example.invalid"]
                    },
                    "generation":3,"selection_generation":1,"status":"active",
                    "granted_scopes":["User.Read","Calendars.Read","offline_access"],
                    "selected_calendars":[target],
                    "calendar":"available","directory":"not_requested",
                    "shared_calendars":"not_requested","microsoft365_groups":"not_requested",
                    "target_results":[]
                },
                "secret_key":secret_key,
                "readable_targets":[target],
                "authorized_scopes":["User.Read","Calendars.Read","offline_access"]
            }],
            "pending_secrets":[],"attempts":[],
            "legacy_quarantined":false,"legacy_checked":true
        });
        broscribe_core::fsutil::write_json_atomic(&dir.join("accounts.json"), &registry).unwrap();
        std::fs::write(slot_dir.join("calendar.json"), "{\"events\":[]}").unwrap();
    };
    let state = || -> MicrosoftAccount {
        let (code, value) = command(&deps, &["integrations", "accounts", "microsoft-graph"]);
        assert_eq!(code, 0, "{value}");
        let accounts: Vec<MicrosoftAccount> = serde_json::from_value(value).unwrap();
        accounts
            .into_iter()
            .find(|a| a.account.account_id.as_str() == handle)
            .unwrap()
    };
    // No daemon: setup still edits and retires locally; reload is optional.
    let mut config = broscribe_core::config::Config::default();
    config.integration.insert(
        id.as_str().into(),
        toml::Value::Table(toml::Table::from_iter([(
            "client_id".into(),
            toml::Value::String("synthetic-original".into()),
        )])),
    );
    config.save(&data).unwrap();
    signed_in("synthetic-original");
    // Saving the existing registered client leaves the account authorized.
    let (code, value) = command(
        &deps,
        &[
            "integrations",
            "setup",
            "microsoft-graph",
            "--client-id",
            "synthetic-original",
        ],
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["client_change"]["effective_changed"], false);
    assert_eq!(state().status, AccountStatus::Active);
    // A different registered client retires the sign-in, tokens and cache.
    let (code, value) = command(
        &deps,
        &[
            "integrations",
            "setup",
            "microsoft-graph",
            "--client-id",
            "synthetic-override",
        ],
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["client_change"]["effective_changed"], true);
    assert_eq!(value["client_change"]["sign_in_retired"], true);
    let retired = state();
    assert_eq!(retired.status, AccountStatus::ReauthRequired);
    assert!(retired.generation > 3);
    assert!(deps.secrets.get(&secret_key).unwrap().is_none());
    assert!(!slot_dir.join("calendar.json").exists());
    let config = broscribe_core::config::Config::load(&data).unwrap();
    assert_eq!(
        config.integration["microsoft-graph"]["client_id"].as_str(),
        Some("synthetic-override")
    );
    // Retirement asks for an explicit sign-in; it never starts one.
    let (code, status) = command(&deps, &["integrations", "status", "microsoft-graph"]);
    assert_eq!(code, 0);
    assert_eq!(status["microsoft_accounts"][0]["status"], "reauth_required");
}

/// Forgetting a person of an existing library also removes their cached
/// directory photos; a library that does not exist is `not_found` and is
/// neither created nor touched, and neither are the photos.
#[test]
fn forgetting_a_person_removes_directory_photos() {
    use broscribe_core::{Library, PersonId, schema::Person};
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let resolved = broscribe_platform::resolve_paths(deps.paths.as_ref()).unwrap();
    let (data, library) = (resolved.data_dir, resolved.default_library_dir);
    let mut photos = Vec::new();
    for id in broscribe_integrations::IntegrationId::ALL {
        let dir = broscribe_integrations::account_dir(&data, id).join("photos");
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("p-0a1b2c.img");
        std::fs::write(&photo, b"synthetic photo").unwrap();
        photos.push(photo);
    }
    let (code, value) = command(&deps, &["people", "forget", "p-0a1b2c", "--yes"]);
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "not_found", "{value}");
    assert!(!library.exists());
    assert!(photos.iter().all(|photo| photo.exists()));

    let person = PersonId::parse("p-0a1b2c").unwrap();
    let lib = Library::open(&library).unwrap();
    lib.write(|tx| tx.save_person(&Person::new(person.clone(), "Synthetic Person")))
        .unwrap();
    let (code, value) = command(&deps, &["people", "forget", "p-0a1b2c", "--yes"]);
    assert_eq!(code, 0, "{value}");
    assert!(photos.iter().all(|photo| !photo.exists()));
    assert_eq!(value["photos"]["status"], "complete", "{value}");
    assert!(value.get("photos_error").is_none(), "{value}");
    assert!(matches!(
        lib.load_person(&person),
        Err(broscribe_core::Error::NotFound(_))
    ));
}

/// `detect status` reports settings that do not load instead of failing,
/// with the policies detection actually runs on, and lists the apps when
/// the daemon's detection is off.
#[test]
fn detect_status_reports_broken_settings_and_lists_apps_when_detection_is_off() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        broscribe_detect::DetectRules::user_path(&data),
        "[[apps]]\nnot_a_field = 1\n",
    )
    .unwrap();
    std::fs::write(
        broscribe_core::config::Config::path(&data),
        "[detect.policies]\nzoom = \"off\"\nteams = \"sometimes\"\n",
    )
    .unwrap();
    let policy = |value: &Value, app: &str| {
        value["apps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["app_id"] == app)
            .unwrap_or_else(|| panic!("{app} missing: {value}"))["policy"]
            .clone()
    };
    let (code, value) = command(&deps, &["detect", "status"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["running"], false);
    assert_eq!(policy(&value, "zoom"), "off");
    assert_eq!(policy(&value, "teams"), "auto");
    let problem = value["problem"].as_str().unwrap();
    assert!(
        problem.contains("detect.toml") && problem.contains("\"teams\""),
        "{problem}"
    );

    let server = fake_daemon(
        &deps,
        vec![(
            "detect.status",
            serde_json::json!({"enabled":false,"apps":[],"problem":null}),
        )],
    );
    let (code, value) = command(&deps, &["detect", "status"]);
    server.join().unwrap();
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["enabled"], false);
    assert_eq!(policy(&value, "zoom"), "off");
    assert!(
        value["apps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["name"].is_string())
    );
    assert!(
        value["problem"]
            .as_str()
            .is_some_and(|p| p.contains("detect.toml"))
    );
}

type AgentReply = Result<Value, broscribe_ipc::IpcError>;

/// An agent that expects exactly `script`'s calls, in order, and answers each.
fn fake_agent(
    deps: &Deps,
    script: Vec<(broscribe_ipc::Call, AgentReply)>,
) -> std::thread::JoinHandle<()> {
    spawn_agent(deps, move |mut listener| async move {
        for (expected, reply) in script {
            let (_reader, mut writer, request) = next_request(&mut listener).await;
            assert_eq!(request.call, expected);
            let response = broscribe_ipc::Response {
                id: request.id,
                outcome: reply,
            };
            writer
                .write_line(response.to_line().unwrap())
                .await
                .unwrap();
        }
    })
}

fn calendar_call(action: broscribe_ipc::SystemCalendarAction) -> broscribe_ipc::Call {
    broscribe_ipc::Call::SystemCalendar { action }
}

/// What a current agent reports: explicit opt-in, granted, one of two
/// eligible calendars selected.
fn granted_status() -> Value {
    serde_json::json!({
        "integration": "system-calendar",
        "configured": true,
        "state": {"generation": 3, "status": "active", "native_calendar_ids": ["cal-work"]},
        "admin_controlled_scopes": [],
        "system_calendar": {
            "authorization": "granted",
            "helper_required": false,
            "calendars": [
                {"id": "cal-work", "title": "Synthetic Work", "source": "Synthetic"},
                {"id": "cal-home", "title": "Synthetic Home", "source": "Synthetic"}
            ]
        }
    })
}

#[test]
fn system_calendar_needs_no_oauth_client_and_never_runs_without_the_agent() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;

    let (code, setup) = command(&deps, &["integrations", "setup", "system-calendar"]);
    assert_eq!(code, 0, "{setup}");
    assert_eq!(setup["integration"], "system-calendar");
    assert!(setup["instructions"].is_string());

    for args in [
        vec![
            "integrations",
            "setup",
            "system-calendar",
            "--client-id",
            "synthetic",
        ],
        vec![
            "integrations",
            "setup",
            "system-calendar",
            "--client-secret-stdin",
        ],
        vec![
            "integrations",
            "setup",
            "google",
            "--calendar-id",
            "cal-work",
        ],
        vec![
            "integrations",
            "setup",
            "system-calendar",
            "--calendar-id",
            " ",
        ],
    ] {
        let (code, value) = command(&deps, &args);
        assert_eq!(code, 1, "{args:?}: {value}");
        assert_eq!(value["error"]["code"], "invalid_input", "{args:?}: {value}");
    }
    // No OAuth client was recorded for the native integration.
    let config = broscribe_core::config::Config::load(&data).unwrap();
    assert!(!config.integration.contains_key("system-calendar"));

    // Only the agent can connect, select or sync: no local fallback.
    for args in [
        vec!["integrations", "login", "system-calendar"],
        vec![
            "integrations",
            "setup",
            "system-calendar",
            "--calendar-id",
            "cal-work",
        ],
        vec!["integrations", "sync", "system-calendar"],
    ] {
        let (code, value) = command(&deps, &args);
        assert_eq!(code, 1, "{args:?}: {value}");
        assert_eq!(
            value["error"]["code"], "helper_required",
            "{args:?}: {value}"
        );
    }

    // Status says why permission is unknown instead of guessing it.
    let (code, status) = command(&deps, &["integrations", "status", "system-calendar"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["configured"], true);
    assert_eq!(status["system_calendar"]["authorization"], Value::Null);
    assert_eq!(status["system_calendar"]["helper_required"], true);
    assert_eq!(
        status["system_calendar"]["calendars"],
        serde_json::json!([])
    );
    assert_eq!(status["helper_error"]["code"], "helper_required");
    let (code, all) = command(&deps, &["integrations", "status"]);
    assert_eq!(code, 0, "{all}");
    let all = all["integrations"].as_array().unwrap();
    assert_eq!(all.len(), 3);
    // Only system calendars need the agent; remote account status stays local.
    assert_eq!(all[0]["integration"], "system-calendar");
    assert_eq!(all[0]["helper_error"]["code"], "helper_required");
    assert_eq!(all[0]["system_calendar"]["helper_required"], true);
    assert!(all[1..].iter().all(|s| s.get("helper_error").is_none()));

    // Disconnecting never needs the agent.
    let (code, logout) = command(&deps, &["integrations", "logout", "system-calendar"]);
    assert_eq!(code, 0, "{logout}");
    assert_eq!(logout["signed_out"], true);
    assert_eq!(logout["status"]["state"]["status"], "signed_out");

    // System calendars are no people directory.
    let (code, lookup) = command(
        &deps,
        &[
            "people",
            "lookup",
            "synthetic",
            "--integration",
            "system-calendar",
        ],
    );
    assert_eq!(code, 1, "{lookup}");
    assert_eq!(lookup["error"]["code"], "unsupported");
}

#[test]
fn native_calendar_actions_do_not_depend_on_advanced_oauth_configuration() {
    use broscribe_integrations::{IntegrationId, Integrations, IntegrationsConfig};
    use broscribe_platform::{
        CalendarAuthorization, CalendarProvider, FakeSystemCalendar, SystemCalendarEvent,
        SystemCalendarInfo,
    };
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let calendar = std::sync::Arc::new(FakeSystemCalendar::default());
    *calendar.authorization.lock().unwrap() = CalendarAuthorization::Granted;
    *calendar.calendars.lock().unwrap() = vec![SystemCalendarInfo {
        id: "cal-work".into(),
        title: "Synthetic Work".into(),
        source: "Synthetic".into(),
    }];
    let now = chrono::Utc::now();
    *calendar.events.lock().unwrap() = vec![SystemCalendarEvent {
        id: "synthetic-event".into(),
        calendar_id: "cal-work".into(),
        ical_uid: None,
        original_start_s: now.timestamp(),
        title: Some("Invented meeting".into()),
        start_s: now.timestamp(),
        end_s: now.timestamp() + 1800,
        links: vec![],
        attendees: vec![],
    }];
    let service = Integrations::new(
        &data,
        IntegrationsConfig::default(),
        deps.secrets.clone(),
        deps.browser.clone(),
        std::sync::Arc::new(|_| panic!("native calendars never reauthenticate")),
    )
    .unwrap()
    .with_system_calendar(calendar);
    let id = IntegrationId::SystemCalendar;
    service
        .connect_system_calendar(CalendarProvider::EventKit)
        .unwrap();
    service
        .configure_native_calendars(CalendarProvider::EventKit, &["cal-work".into()])
        .unwrap();
    let generation = service.status(id).unwrap().state.generation;
    assert_eq!(
        service
            .sync_calendar(
                id,
                now - chrono::Duration::days(1),
                now + chrono::Duration::days(30)
            )
            .unwrap()
            .events[0]
            .id,
        "synthetic-event"
    );
    fn has_cache(dir: &std::path::Path) -> bool {
        std::fs::read_dir(dir).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                has_cache(&path)
            } else {
                path.file_name() == Some(std::ffi::OsStr::new("calendar.json"))
            }
        })
    }
    assert!(has_cache(&data));
    // Core TOML is valid, but the unrelated advanced OAuth setting is not.
    std::fs::write(
        data.join("config.toml"),
        "[integration.google]\nclient_id=123\n",
    )
    .unwrap();
    assert!(IntegrationsConfig::load(&data).is_err());
    let (code, setup) = command(&deps, &["integrations", "setup", "system-calendar"]);
    assert_eq!(code, 0, "{setup}");
    let (code, status) = command(&deps, &["integrations", "status", "system-calendar"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["helper_error"]["code"], "helper_required");
    let (code, logout) = command(&deps, &["integrations", "logout", "system-calendar"]);
    assert_eq!(code, 0, "{logout}");
    let state: broscribe_integrations::AccountState =
        serde_json::from_value(logout["status"]["state"].clone()).unwrap();
    assert!(state.generation > generation);
    assert_eq!(
        state.status,
        broscribe_integrations::AccountStatus::SignedOut
    );
    assert!(state.native_calendar_ids.is_empty());
    assert!(!has_cache(&data));
}

#[test]
fn system_calendar_actions_go_to_the_agent_with_the_exact_selection() {
    use broscribe_ipc::{Call, SystemCalendarAction as A};
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let status = granted_status();
    let mut cleared = status.clone();
    cleared["state"]["native_calendar_ids"] = serde_json::json!([]);
    let server = fake_agent(
        &deps,
        vec![
            (calendar_call(A::Connect), Ok(status.clone())),
            (
                calendar_call(A::Configure {
                    calendar_ids: vec!["cal-work".into(), "cal-b".into()],
                }),
                Ok(status.clone()),
            ),
            (calendar_call(A::Status), Ok(status.clone())),
            (calendar_call(A::Status), Ok(status.clone())),
            (calendar_call(A::Status), Ok(status.clone())),
            (
                Call::IntegrationsSync {
                    integration: Some("system-calendar".into()),
                },
                Ok(serde_json::json!({"synced":[{"integration":"system-calendar","events":2}]})),
            ),
            (
                calendar_call(A::Configure {
                    calendar_ids: Vec::new(),
                }),
                Ok(cleared.clone()),
            ),
            (calendar_call(A::Disconnect), Ok(status.clone())),
        ],
    );
    let (code, connect) = command(&deps, &["integrations", "login", "system-calendar"]);
    assert_eq!(code, 0, "{connect}");
    assert_eq!(connect, status);
    // Repeats collapse; nothing is added or reordered.
    let (code, configure) = command(
        &deps,
        &[
            "integrations",
            "setup",
            "system-calendar",
            "--calendar-id",
            "cal-work",
            "--calendar-id",
            "cal-b",
            "--calendar-id",
            "cal-work",
        ],
    );
    assert_eq!(code, 0, "{configure}");
    let (code, one) = command(&deps, &["integrations", "status", "system-calendar"]);
    assert_eq!(code, 0, "{one}");
    assert_eq!(one, status);
    // The agent's native entry replaces the local one; the others stay local.
    let (code, all) = command(&deps, &["integrations", "status"]);
    assert_eq!(code, 0, "{all}");
    assert_eq!(all["integrations"][0], status);
    assert_eq!(all["integrations"][1]["integration"], "microsoft-graph");
    assert_eq!(all["integrations"][2]["integration"], "google");
    let (code, sync) = command(&deps, &["integrations", "sync", "system-calendar"]);
    assert_eq!(code, 0, "{sync}");
    assert_eq!(sync["synced"][0]["events"], 2);
    let (code, empty) = command(
        &deps,
        &[
            "integrations",
            "setup",
            "system-calendar",
            "--clear-calendars",
        ],
    );
    assert_eq!(code, 0, "{empty}");
    assert_eq!(empty["state"]["native_calendar_ids"], serde_json::json!([]));
    let (code, logout) = command(&deps, &["integrations", "logout", "system-calendar"]);
    assert_eq!(code, 0, "{logout}");
    assert_eq!(logout["signed_out"], true);
    server.join().unwrap();
}

#[test]
fn system_calendar_agent_errors_keep_stable_codes() {
    use broscribe_ipc::{IpcError, SystemCalendarAction as A};
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let codes = [
        "calendar_denied",
        "calendar_restricted",
        "calendar_write_only",
        "calendar_pending",
        "busy",
        "superseded",
        "signed_out",
    ];
    let server = fake_agent(
        &deps,
        codes
            .iter()
            .map(|code| {
                (
                    calendar_call(A::Connect),
                    Err(IpcError::new(*code, "synthetic refusal")),
                )
            })
            .collect(),
    );
    for code in codes {
        let (exit, value) = command(&deps, &["integrations", "login", "system-calendar"]);
        assert_eq!(exit, 1, "{value}");
        assert_eq!(value["error"]["code"], code, "{value}");
    }
    server.join().unwrap();
}

#[test]
fn an_agent_from_before_system_calendars_is_reported_outdated() {
    use broscribe_ipc::{IpcError, SystemCalendarAction as A};
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let unknown = || {
        Err(IpcError::new(
            IpcError::INVALID_REQUEST,
            "unknown variant `integrations.system_calendar`",
        ))
    };
    let server = fake_agent(
        &deps,
        vec![
            (calendar_call(A::Status), unknown()),
            (calendar_call(A::Connect), unknown()),
            (calendar_call(A::Status), unknown()),
            (calendar_call(A::Disconnect), unknown()),
        ],
    );
    let (code, status) = command(&deps, &["integrations", "status", "system-calendar"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["helper_error"]["code"], "helper_outdated");
    assert_eq!(status["system_calendar"]["authorization"], Value::Null);
    let (code, login) = command(&deps, &["integrations", "login", "system-calendar"]);
    assert_eq!(code, 1, "{login}");
    assert_eq!(login["error"]["code"], "helper_outdated");
    // Sync asks first, so it never sends the old agent an unknown account.
    let (code, sync) = command(&deps, &["integrations", "sync", "system-calendar"]);
    assert_eq!(code, 1, "{sync}");
    assert_eq!(sync["error"]["code"], "helper_outdated");
    // The old agent holds no system-calendar state: disconnect locally.
    let (code, logout) = command(&deps, &["integrations", "logout", "system-calendar"]);
    assert_eq!(code, 0, "{logout}");
    assert_eq!(logout["signed_out"], true);
    server.join().unwrap();
}

#[test]
fn an_uncertain_connect_is_reported_and_never_sent_again() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let server = spawn_agent(&deps, |mut listener| async move {
        let mut connect_id = None;
        loop {
            let (_reader, mut writer, request) = next_request(&mut listener).await;
            match request.call {
                broscribe_ipc::Call::SystemCalendar {
                    action: broscribe_ipc::SystemCalendarAction::Connect,
                } => {
                    assert!(connect_id.is_none(), "connect was sent again");
                    connect_id = Some(request.id);
                    // The agent restarts: the reply and the request's record are lost.
                }
                broscribe_ipc::Call::RequestStatus { request_id } => {
                    assert_eq!(Some(request_id), connect_id);
                    let response = broscribe_ipc::Response::ok(
                        request.id,
                        serde_json::json!({"state":"unknown"}),
                    );
                    writer
                        .write_line(response.to_line().unwrap())
                        .await
                        .unwrap();
                    break;
                }
                other => panic!("unexpected call: {other:?}"),
            }
        }
    });
    let (code, value) = command(&deps, &["integrations", "login", "system-calendar"]);
    server.join().unwrap();
    assert_eq!(code, 1, "{value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("may or may not")),
        "{value}"
    );
}

type AgentScript = Vec<(Box<dyn Fn(&broscribe_ipc::Call) + Send>, AgentReply)>;

/// An agent answering `script` in order: each entry checks the call it gets
/// (request IDs are random) and gives the reply.
fn checking_agent(deps: &Deps, script: AgentScript) -> std::thread::JoinHandle<()> {
    spawn_agent(deps, move |mut listener| async move {
        for (check, reply) in script {
            let (_reader, mut writer, request) = next_request(&mut listener).await;
            check(&request.call);
            let response = broscribe_ipc::Response {
                id: request.id,
                outcome: reply,
            };
            writer
                .write_line(response.to_line().unwrap())
                .await
                .unwrap();
        }
    })
}

#[test]
fn record_title_tag_and_discard_name_their_session_and_keep_stale_session() {
    use broscribe_ipc::Call;
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let script: AgentScript = vec![
        (
            Box::new(|call| {
                assert!(
                    matches!(call, Call::RecordTitle { session_id, title, .. }
                        if session_id == "s-stopped" && title == "Synthetic renamed"),
                    "{call:?}"
                )
            }),
            Ok(serde_json::json!({"status": "already_published", "meeting_id": "m-synthetic"})),
        ),
        (
            Box::new(|call| assert_eq!(call, &Call::RecordStatus)),
            Ok(serde_json::json!({"recording": {"session_id": "s-active"}})),
        ),
        (
            Box::new(|call| {
                assert!(
                    matches!(call, Call::RecordTitle { session_id, title, .. }
                        if session_id == "s-active" && title == "Synthetic live"),
                    "{call:?}"
                )
            }),
            Ok(
                serde_json::json!({"status": "saved", "session_id": "s-active", "title": "Synthetic live"}),
            ),
        ),
        (
            Box::new(|call| {
                assert_eq!(
                    call,
                    &Call::RecordTag {
                        tags: vec!["+-literal".into()],
                        session_id: "s-active".into(),
                    }
                )
            }),
            Ok(serde_json::json!({"tags": ["-literal"], "added": ["-literal"], "removed": []})),
        ),
        (
            Box::new(|call| {
                assert!(
                    matches!(call, Call::RecordDiscard { session_id, .. } if session_id == "s-old"),
                    "{call:?}"
                )
            }),
            Err(broscribe_ipc::IpcError::new(
                "stale_session",
                "session s-old is not active",
            )),
        ),
        // `record stop` resolves the recording shown now, then names it: a
        // stop that crossed into another recording is refused, not applied.
        (
            Box::new(|call| assert_eq!(call, &Call::RecordStatus)),
            Ok(serde_json::json!({"recording": {"session_id": "s-old"}})),
        ),
        (
            Box::new(|call| {
                assert_eq!(
                    call,
                    &Call::RecordStop {
                        session_id: "s-old".into()
                    }
                )
            }),
            Err(broscribe_ipc::IpcError::new(
                "stale_session",
                "session s-old is not active",
            )),
        ),
    ];
    let server = checking_agent(&deps, script);
    let (code, value) = command(
        &deps,
        &[
            "record",
            "title",
            "  Synthetic renamed ",
            "--session",
            "s-stopped",
        ],
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["status"], "already_published");
    assert_eq!(value["meeting_id"], "m-synthetic");
    let (code, value) = command(&deps, &["record", "title", "Synthetic live"]);
    assert_eq!((code, &value["status"]), (0, &serde_json::json!("saved")));
    let (code, value) = command(
        &deps,
        &["record", "tag", "--session", "s-active", "+-literal"],
    );
    assert_eq!(code, 0, "{value}");
    let (code, value) = command(&deps, &["record", "discard", "--session", "s-old", "--yes"]);
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "stale_session");
    let (code, value) = command(&deps, &["record", "stop"]);
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "stale_session");
    server.join().unwrap();
    // Refused before the agent: an empty title.
    let (code, value) = command(&deps, &["record", "title", "   ", "--session", "s"]);
    assert_eq!(
        (code, &value["error"]["code"]),
        (1, &serde_json::json!("invalid_input"))
    );
}

fn status_with_auto_stop(auto_stop: Option<serde_json::Value>) -> Value {
    let mut recording = serde_json::json!({ "session_id": "s-active" });
    if let Some(auto_stop) = auto_stop {
        recording["auto_stop"] = auto_stop;
    }
    serde_json::json!({ "recording": recording })
}

fn unbound_auto_stop(generation: u64) -> Value {
    serde_json::json!({
        "enabled": false,
        "app": null,
        "generation": generation,
        "reader_available": false,
        "pending": null,
        "candidates": [{ "app_id": "teams", "app_name": "Synthetic Teams" }]
    })
}

#[test]
fn record_auto_stop_reads_the_binding_then_sends_a_fenced_change() {
    use broscribe_ipc::Call;
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let linked = serde_json::json!({
        "enabled": true,
        "app": { "app_id": "teams", "app_name": "Synthetic Teams" },
        "generation": 8,
        "reader_available": true,
        "pending": null,
        "candidates": []
    });
    let script: AgentScript = vec![
        // Link the unbound recording to the detected call.
        (
            Box::new(|call| assert_eq!(call, &Call::RecordStatus)),
            Ok(status_with_auto_stop(Some(unbound_auto_stop(7)))),
        ),
        (
            Box::new(|call| {
                assert!(
                    matches!(call, Call::RecordAutoStop(p)
                        if p.session_id == "s-active" && p.expected_generation == 7
                            && p.enabled && p.app_id.as_deref() == Some("teams")),
                    "{call:?}"
                )
            }),
            Ok(linked.clone()),
        ),
        // Turn it off again: the binding is kept (no app).
        (
            Box::new(|call| assert_eq!(call, &Call::RecordStatus)),
            Ok(status_with_auto_stop(Some(linked.clone()))),
        ),
        (
            Box::new(|call| {
                assert!(
                    matches!(call, Call::RecordAutoStop(p)
                        if p.expected_generation == 8 && !p.enabled && p.app_id.is_none()),
                    "{call:?}"
                )
            }),
            Err(broscribe_ipc::IpcError::new(
                "stale_binding",
                "the binding changed",
            )),
        ),
        // Another session named: refused before anything is sent.
        (
            Box::new(|call| assert_eq!(call, &Call::RecordStatus)),
            Ok(status_with_auto_stop(Some(linked))),
        ),
        // An agent without automatic stop: unsupported, nothing sent.
        (
            Box::new(|call| assert_eq!(call, &Call::RecordStatus)),
            Ok(status_with_auto_stop(None)),
        ),
    ];
    let server = checking_agent(&deps, script);
    let (code, value) = command(&deps, &["record", "auto-stop", "--on", "--app", "teams"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["enabled"], true);
    assert_eq!(value["app"]["app_id"], "teams");
    let (code, value) = command(&deps, &["record", "auto-stop", "--off"]);
    assert_eq!(
        (code, &value["error"]["code"]),
        (1, &serde_json::json!("stale_binding"))
    );
    let (code, value) = command(
        &deps,
        &["record", "auto-stop", "--session", "s-old", "--off"],
    );
    assert_eq!(
        (code, &value["error"]["code"]),
        (1, &serde_json::json!("stale_session"))
    );
    let (code, value) = command(&deps, &["record", "auto-stop", "--on"]);
    assert_eq!(
        (code, &value["error"]["code"]),
        (1, &serde_json::json!("unsupported"))
    );
    server.join().unwrap();
}

#[test]
fn a_trailing_record_tag_session_names_that_session_never_the_active_one() {
    use broscribe_ipc::Call;
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let tag = |tags: &[&str], session: &str| {
        let expected = Call::RecordTag {
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            session_id: session.to_owned(),
        };
        let check: Box<dyn Fn(&Call) + Send> =
            Box::new(move |call: &Call| assert_eq!(call, &expected));
        let reply: AgentReply = Ok(serde_json::json!({ "tags": tags }));
        (check, reply)
    };
    // The agent reports `s-a` active; only the last command, with nothing
    // but literal changes, may resolve it.
    let script: AgentScript = vec![
        tag(&["+a"], "s-b"),
        tag(&["+a", "-b"], "s-b"),
        tag(&["+a", "--session", "s-c"], "s-b"),
        (
            Box::new(|call| assert_eq!(call, &Call::RecordStatus)),
            Ok(serde_json::json!({"recording": {"session_id": "s-a"}})),
        ),
        tag(&["--session", "s-b"], "s-a"),
    ];
    let server = checking_agent(&deps, script);
    // A second session, before or after the changes, is refused before
    // anything reaches the agent (whose first expected call is the tag
    // below).
    for args in [
        [
            "record",
            "tag",
            "+a",
            "--session",
            "s-b",
            "--session",
            "s-c",
        ],
        [
            "record",
            "tag",
            "--session",
            "s-b",
            "+a",
            "--session",
            "s-c",
        ],
    ] {
        let (code, value) = command(&deps, &args);
        assert_eq!(code, 2, "{args:?}: {value}");
    }
    for args in [
        &["record", "tag", "+a", "--session", "s-b"][..],
        &["record", "tag", "+a", "-b", "--session=s-b"],
        &[
            "record",
            "tag",
            "--session",
            "s-b",
            "--",
            "+a",
            "--session",
            "s-c",
        ],
        &["record", "tag", "--", "--session", "s-b"],
    ] {
        let (code, value) = command(&deps, args);
        assert_eq!(code, 0, "{args:?}: {value}");
    }
    server.join().unwrap();
}

/// The session the `record live --follow` tests follow.
const FOLLOWED: &str = "s-follow";

fn live_turn(session: &str, id: &str, revision: u64) -> broscribe_ipc::Event {
    broscribe_ipc::Event::LiveTurn(Box::new(
        serde_json::from_value(serde_json::json!({
            "session_id": session, "turn_id": id, "revision": revision, "epoch": 0,
            "track": "mic", "from_sample": 0, "to_sample": 16000, "start_s": 0.0,
            "end_s": 1.0, "text": "synthetic", "status": "preview", "evidence": "ev-0",
        }))
        .unwrap(),
    ))
}

fn live_snapshot(turns: &[(&str, u64)]) -> Value {
    let turns: Vec<Value> = turns
        .iter()
        .map(|&(id, revision)| {
            let broscribe_ipc::Event::LiveTurn(turn) = live_turn(FOLLOWED, id, revision) else {
                unreachable!()
            };
            serde_json::to_value(turn).unwrap()
        })
        .collect();
    serde_json::json!({
        "session_id": FOLLOWED,
        "identity": {"status": {"state": "idle"}, "epoch": 0},
        "evidence": "ev-0",
        "turns": turns,
    })
}

/// An agent's state with no recording: the followed session ended.
fn nothing_recording() -> broscribe_ipc::Event {
    broscribe_ipc::Event::State(Box::new(
        serde_json::from_value(serde_json::json!({
            "pid": 1, "version": "0", "protocol": broscribe_ipc::PROTOCOL_VERSION,
            "library": "/synthetic", "recording": null,
            "jobs": {"running": null, "waiting": 0, "deferred": 0},
            "preview": {"state": "off"},
        }))
        .unwrap(),
    ))
}

/// An agent serving one `record live --follow` of [`FOLLOWED`]: it answers
/// the subscription and writes `before` to it, and only then answers
/// `record.live` with `snapshot` (so `before` is what arrived between
/// subscribing and reading the snapshot); then it writes `after` to the
/// subscription until that ends or the follower is gone.
fn follow_agent(
    deps: &Deps,
    snapshot: Value,
    before: Vec<broscribe_ipc::Event>,
    after: impl Iterator<Item = broscribe_ipc::Event> + Send + 'static,
) -> std::thread::JoinHandle<()> {
    use broscribe_ipc::{Call, Response};
    spawn_agent(deps, move |mut listener| async move {
        // The subscription stays open while the snapshot is answered.
        let (_events_reader, mut events, subscribe) = next_request(&mut listener).await;
        assert_eq!(subscribe.call, Call::Subscribe);
        let answer = Response::ok(subscribe.id, serde_json::json!({"subscribed": true}));
        events.write_line(answer.to_line().unwrap()).await.unwrap();
        for event in before {
            events.write_line(event.to_line().unwrap()).await.unwrap();
        }
        let (live_reader, mut live, request) = next_request(&mut listener).await;
        assert_eq!(
            request.call,
            Call::RecordLive {
                session_id: FOLLOWED.into()
            }
        );
        live.write_line(Response::ok(request.id, snapshot).to_line().unwrap())
            .await
            .unwrap();
        drop((live_reader, live));
        for event in after {
            if events.write_line(event.to_line().unwrap()).await.is_err() {
                return;
            }
        }
    })
}

/// `record live --session FOLLOWED --follow <extra>`, which must succeed:
/// its NDJSON lines.
fn follow(deps: &Deps, extra: &[&str]) -> Vec<Value> {
    let argv = [
        "broscribe",
        "record",
        "live",
        "--session",
        FOLLOWED,
        "--follow",
    ]
    .iter()
    .chain(extra)
    .map(OsString::from)
    .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(argv, deps, &mut out, &mut err);
    let lines = serde_json::Deserializer::from_slice(&out)
        .into_iter()
        .collect::<Result<Vec<Value>, _>>()
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out)));
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    lines
}

#[test]
fn following_writes_each_newer_turn_once_within_the_agents_window() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let window = broscribe_ipc::live::LiveIdentitySettings::default().max_turns;
    let total = window * 3;
    // The turn the agent still holds longest after `total` new ones.
    let oldest = format!("mic-{}", total - window + 1);
    let mut after: Vec<_> = (1..=total)
        .map(|n| live_turn(FOLLOWED, &format!("mic-{n}"), 2))
        .collect();
    after.extend([
        live_turn(FOLLOWED, &oldest, 1),
        live_turn(FOLLOWED, &oldest, 2),
        live_turn("s-other", "mic-other", 9),
        live_turn(FOLLOWED, &oldest, 3),
        nothing_recording(),
    ]);
    let server = follow_agent(
        &deps,
        live_snapshot(&[("mic-0", 3)]),
        // Published before the snapshot was read: it already holds them.
        vec![
            live_turn(FOLLOWED, "mic-0", 2),
            live_turn(FOLLOWED, "mic-0", 3),
        ],
        after.into_iter(),
    );
    let lines = follow(&deps, &[]);
    server.join().unwrap();
    assert_eq!(lines.len(), total + 3);
    assert_eq!(lines[0]["type"], "snapshot");
    assert_eq!(lines[0]["session_id"], FOLLOWED);
    assert_eq!(lines[0]["turns"][0]["turn_id"], "mic-0");
    for (n, line) in lines[1..=total].iter().enumerate() {
        assert_eq!(line["type"], "turn");
        assert_eq!(line["session_id"], FOLLOWED);
        assert_eq!(line["turn"]["turn_id"], format!("mic-{}", n + 1));
        assert_eq!(line["turn"]["revision"], 2);
    }
    assert_eq!(lines[total + 1]["turn"]["turn_id"], oldest);
    assert_eq!(lines[total + 1]["turn"]["revision"], 3);
    assert_eq!(
        lines[total + 2],
        serde_json::json!({"type": "end", "session_id": FOLLOWED, "reason": "session_ended"})
    );
}

fn live_identity(session: &str, status: Value, epoch: u64) -> broscribe_ipc::Event {
    broscribe_ipc::Event::LiveIdentity {
        session_id: session.into(),
        identity: serde_json::from_value(serde_json::json!({"status": status, "epoch": epoch}))
            .unwrap(),
    }
}

fn live_transcript(session: &str, text: &str) -> broscribe_ipc::Event {
    broscribe_ipc::Event::LiveTranscript {
        session_id: session.into(),
        segment: serde_json::from_value(serde_json::json!({
            "track": "mic", "start_s": 0.0, "end_s": 1.0, "text": text,
            "speaker": {"kind": "me"},
        }))
        .unwrap(),
    }
}

/// A stopped recording's supervisors may still publish after the next one
/// started (its buffered transcript, its final `idle`): following the new
/// one never writes them as its own, while its own transcript and `idle`
/// still arrive and turn revisions stay bounded by the followed session.
#[test]
fn following_never_writes_a_stopped_recordings_late_output_as_the_followed_one() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let old = "s-stopped";
    let idle = || serde_json::json!({"state": "idle"});
    let server = follow_agent(
        &deps,
        live_snapshot(&[("mic-0", 3)]),
        vec![
            live_transcript(old, "late from the stopped recording"),
            live_identity(old, idle(), 1),
        ],
        vec![
            live_identity(old, serde_json::json!({"state": "ready"}), 1),
            live_transcript(FOLLOWED, "from the followed recording"),
            live_transcript(old, "late from the stopped recording"),
            // Same turn ID, higher revision, other recording: not this
            // turn, and it never raises the followed turn's revision.
            live_turn(old, "mic-0", 9),
            live_turn(FOLLOWED, "mic-0", 4),
            live_identity(old, idle(), 1),
            live_identity(FOLLOWED, idle(), 2),
            nothing_recording(),
        ]
        .into_iter(),
    );
    let lines = follow(&deps, &[]);
    server.join().unwrap();
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert_eq!(lines[0]["type"], "snapshot");
    assert_eq!(
        lines[1],
        serde_json::json!({"type": "transcript", "session_id": FOLLOWED, "segment": {
            "track": "mic", "start_s": 0.0, "end_s": 1.0,
            "text": "from the followed recording", "speaker": {"kind": "me"},
        }})
    );
    assert_eq!(lines[2]["type"], "turn");
    assert_eq!(lines[2]["turn"]["session_id"], FOLLOWED);
    assert_eq!(lines[2]["turn"]["revision"], 4);
    assert_eq!(
        lines[3],
        serde_json::json!({"type": "identity", "session_id": FOLLOWED,
            "identity": {"status": {"state": "idle"}, "epoch": 2}})
    );
    assert_eq!(
        lines[4],
        serde_json::json!({"type": "end", "session_id": FOLLOWED, "reason": "session_ended"})
    );
}

#[test]
fn a_follow_ends_once_at_max_seconds_while_events_keep_coming() {
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    // An endless stream for the followed session (repeats of a revision
    // already shown): the follow must still stop at its deadline.
    let server = follow_agent(
        &deps,
        live_snapshot(&[("mic-0", 3)]),
        Vec::new(),
        std::iter::repeat_with(|| live_turn(FOLLOWED, "mic-0", 3)),
    );
    let lines = follow(&deps, &["--max-seconds", "1"]);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["type"], "snapshot");
    assert_eq!(
        lines[1],
        serde_json::json!({"type": "end", "session_id": FOLLOWED, "reason": "max_seconds"})
    );
    // The agent stops writing once the follower has gone.
    server.join().unwrap();
}

#[test]
fn snapshot_failures_end_follow_with_one_compact_terminal_line() {
    use broscribe_ipc::{Call, Event, IpcError, Response};

    for resync in [false, true] {
        for json_mode in [false, true] {
            let tmp = temp_home();
            let deps = fixture(tmp.path());
            let server = spawn_agent(&deps, move |mut listener| async move {
                let fail = |id| {
                    Response::err(
                        id,
                        IpcError::new("invalid_input", "private synthetic snapshot detail"),
                    )
                };
                let (_events_reader, mut events, request) = next_request(&mut listener).await;
                assert_eq!(request.call, Call::Subscribe);
                events
                    .write_line(
                        Response::ok(request.id, serde_json::json!({"subscribed": true}))
                            .to_line()
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let (live_reader, mut live, request) = next_request(&mut listener).await;
                assert_eq!(
                    request.call,
                    Call::RecordLive {
                        session_id: FOLLOWED.into()
                    }
                );
                let reply = if resync {
                    Response::ok(request.id, live_snapshot(&[("mic-0", 3)]))
                } else {
                    fail(request.id)
                };
                live.write_line(reply.to_line().unwrap()).await.unwrap();
                drop((live_reader, live));
                if resync {
                    events
                        .write_line(Event::Lagged { dropped: 1 }.to_line().unwrap())
                        .await
                        .unwrap();
                    let (_live_reader, mut live, request) = next_request(&mut listener).await;
                    assert_eq!(
                        request.call,
                        Call::RecordLive {
                            session_id: FOLLOWED.into()
                        }
                    );
                    live.write_line(fail(request.id).to_line().unwrap())
                        .await
                        .unwrap();
                }
            });
            let mut args = vec![OsString::from("broscribe")];
            if json_mode {
                args.push("--json".into());
            }
            args.extend(
                [
                    "record",
                    "live",
                    "--session",
                    FOLLOWED,
                    "--follow",
                    "--max-seconds",
                    "5",
                ]
                .map(OsString::from),
            );
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = run(args, &deps, &mut out, &mut err);
            server.join().unwrap();
            assert_eq!(code, 1, "{}", String::from_utf8_lossy(&err));
            let text = String::from_utf8(out).unwrap();
            let lines: Vec<Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).expect("each NDJSON line is one document"))
                .collect();
            assert_eq!(lines.len(), if resync { 2 } else { 1 }, "{text}");
            if resync {
                assert_eq!(lines[0]["type"], "snapshot");
                assert_eq!(lines[0]["turns"][0]["revision"], 3);
            }
            assert_eq!(
                lines.last().unwrap(),
                &serde_json::json!({
                    "type": "end", "session_id": FOLLOWED, "reason": "error",
                    "error": {"code": "invalid_input"},
                })
            );
            assert!(!text.contains("private synthetic snapshot detail"));
        }
    }
}

#[test]
fn parity_watch_streams_state_meters_and_job_progress() {
    use broscribe_ipc::{Call, Event, JobProgress, NotificationRequest, Response};
    use serde_json::json;
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let initial: Event = serde_json::from_value(json!({
        "event": "state", "pid": 10, "version": "0.0.0", "protocol": broscribe_ipc::PROTOCOL_VERSION,
        "library": "synthetic-library", "jobs": {"waiting":0,"deferred":0}, "preview":{"state":"off"},
        "recording": {
            "session_id":"synthetic-session", "status":"recording", "elapsed_s":3.0,
            "started_at":"2026-11-25T00:00:00Z", "health": broscribe_ipc::capture::CaptureHealth::starting(),
            "auto_stop":{"enabled":true,"generation":1,"reader_available":true,
                         "pending":{"reason":"call_ended","remaining_s":5}},
        },
        "pending_notifications":[{"id":"synthetic-prompt","title":"Record?","body":"Synthetic prompt"}],
        "saved":[{"session_id":"synthetic-previous", "meeting_id":"m-000001", "saved_at":"2026-11-25T00:00:00Z"}],
    })).unwrap();
    let events = [
        initial,
        Event::Meters {
            me_dbfs: -20.0,
            others_dbfs: -42.0,
        },
        Event::JobProgress(JobProgress {
            job_id: "synthetic-job".into(),
            meeting_id: Some(broscribe_core::MeetingId::parse("m-000001").unwrap()),
            kind: "record-final".into(),
            fraction: Some(0.7),
            phase: "progress".into(),
            message: None,
        }),
        Event::Notification(NotificationRequest {
            id: "synthetic-notice".into(),
            title: "Transcript ready".into(),
            body: "Synthetic transcript ready.".into(),
            actions: vec![],
            meeting_id: Some("m-000001".into()),
        }),
        Event::Lagged { dropped: 2 },
    ];
    let expected = events.clone();
    let server = spawn_agent(&deps, |mut listener| async move {
        let (_, mut writer, request) = next_request(&mut listener).await;
        assert_eq!(request.call, Call::Subscribe);
        writer
            .write_line(
                Response::ok(request.id, json!({"subscribed":true}))
                    .to_line()
                    .unwrap(),
            )
            .await
            .unwrap();
        for event in events {
            writer.write_line(event.to_line().unwrap()).await.unwrap();
        }
    });
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let args = ["broscribe", "daemon", "watch", "--json"]
        .into_iter()
        .map(OsString::from)
        .collect();
    assert_eq!(
        run(args, &deps, &mut out, &mut err),
        0,
        "{}",
        String::from_utf8_lossy(&err)
    );
    server.join().unwrap();
    let rows: Vec<Event> = std::str::from_utf8(&out)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows, expected);
}

#[test]
fn parity_app_permissions_are_queried_from_the_running_app() {
    use broscribe_platform::shell::{HandoffRequest, handoff_channel, next_handoff_blocking};
    use broscribe_platform_native::NativePlatform;
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let (send, mut requests) = handoff_channel();
    let channel = NativePlatform::instance_channel(&data);
    let _listen = channel.listen(send).unwrap();
    let server = std::thread::spawn(move || {
        for (wanted, reply) in [
            (HandoffRequest::NotificationStatus, "not_determined"),
            (HandoffRequest::AccessibilityStatus, "denied"),
            (HandoffRequest::RequestNotifications, "done"),
            (HandoffRequest::RequestAccessibility, "done"),
        ] {
            let received = next_handoff_blocking(&mut requests).unwrap();
            assert_eq!(received.request, wanted);
            received.reply(reply);
        }
    });
    let (code, status) = command(&deps, &["app", "permissions"]);
    assert_eq!(code, 0, "{status}");
    assert_eq!(
        status,
        serde_json::json!({"notifications":"not_determined","accessibility":"denied"})
    );
    assert_eq!(
        command(&deps, &["app", "permissions", "--request", "notifications"]).0,
        0
    );
    assert_eq!(
        command(&deps, &["app", "permissions", "--request", "accessibility"]).0,
        0
    );
    server.join().unwrap();
}

#[test]
fn parity_queue_reports_live_progress_from_its_library() {
    use broscribe_core::{
        Library, MeetingId,
        queue::{Queue, VECTORS_KIND},
    };
    use broscribe_ipc::{Call, Response};
    use serde_json::json;
    let tmp = temp_home();
    let deps = fixture(tmp.path());
    let root = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .default_library_dir;
    let lib = Library::open(&root).unwrap();
    let mid = MeetingId::parse("m-000001").unwrap();
    let job = lib
        .write(|tx| {
            Queue::open(tx.root())?.enqueue(
                tx,
                VECTORS_KIND,
                mid.as_str(),
                json!({"meeting_id":mid}),
            )
        })
        .unwrap();
    let expected = job.clone();
    let server = spawn_agent(&deps, |mut listener| async move {
        let (_, mut writer, request) = next_request(&mut listener).await;
        assert_eq!(request.call, Call::Status);
        writer.write_line(Response::ok(request.id, json!({
            "library":root, "jobs":{"running":{"job_id":job,"meeting_id":mid,"kind":VECTORS_KIND,"phase":"progress","fraction":0.63}}
        })).to_line().unwrap()).await.unwrap();
    });
    let (code, value) = command(&deps, &["queue", "status"]);
    assert_eq!(code, 0, "{value}");
    let row = &value["jobs"][0];
    assert_eq!(row["id"], expected.as_str());
    assert_eq!(row["meeting_id"], "m-000001");
    assert_eq!(row["fraction"], 0.63);
    server.join().unwrap();
}

#[test]
fn default_uninstall_keeps_unknown_cache_library() {
    let fixture = RemovalFixture::new();
    let kept = fixture.cache.join("old-library");
    broscribe_core::Library::open(&kept).unwrap();
    let (code, report) = command(&fixture.deps, &["uninstall", "--yes"]);
    assert_eq!(code, 0, "{report}");
    assert!(kept.is_dir() && !fixture.install.exists());
    assert!(report["steps"].as_array().unwrap().iter().any(|step| {
        step["status"] == "kept"
            && step["path"]
                .as_str()
                .is_some_and(|p| std::path::Path::new(p).ends_with("old-library"))
    }));
}

#[test]
fn remove_data_keeps_ordinary_year_archives_and_refuses_a_library_in_an_owned_tree() {
    for library in [false, true] {
        let fixture = RemovalFixture::new();
        let archive = fixture.data.join("archives/2024");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("notes.txt"), "synthetic").unwrap();
        let model = fixture.data.join("models/synthetic-model");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(
            model.join(".verified"),
            r#"{"id":"synthetic-model","files":[]}"#,
        )
        .unwrap();
        if library {
            let meeting = model.join("2024/11/synthetic");
            std::fs::create_dir_all(&meeting).unwrap();
            std::fs::write(meeting.join("meeting.json"), "{}").unwrap();
        }
        let (code, report) = command(&fixture.deps, &["uninstall", "--yes", "--remove-data"]);
        assert_eq!(code, i32::from(library), "{report}");
        assert!(archive.join("notes.txt").is_file());
        assert_eq!(model.exists(), library);
        assert_eq!(fixture.install.exists(), library);
        assert!(report.to_string().contains(if library {
            "contains a BroScribe library"
        } else {
            "not created by BroScribe"
        }));
    }
}

#[test]
fn unjournaled_stage_failure_keeps_update_state_and_retry_uses_its_owner() {
    let fixture = RemovalFixture::new();
    let updates = fixture.data.join("updates");
    let stage = fixture.install.with_file_name(".install.update");
    let payload = stage.join("incoming/BroScribe");
    std::fs::create_dir_all(&updates).unwrap();
    std::fs::create_dir_all(&payload).unwrap();
    std::fs::write(payload.join("LICENSE"), "synthetic partial release").unwrap();
    std::fs::write(payload.join("foreign.txt"), "keep").unwrap();
    std::fs::write(updates.join("seen.json"), r#"{"highest_build":11}"#).unwrap();
    std::fs::File::create(fixture.install.with_file_name(".install.update.lock")).unwrap();
    let identity = |build| {
        serde_json::json!({
            "product":"BroScribe", "app_id":"org.broscribe.BroScribe", "version":"1.0.0",
            "build":build, "target":"x86_64-pc-windows-msvc"
        })
    };
    std::fs::write(
        stage.join("owner.json"),
        serde_json::json!({
            "schema": 1, "state_dir": updates, "from": identity(10), "to": identity(11)
        })
        .to_string(),
    )
    .unwrap();
    let args = ["uninstall", "--yes", "--remove-data"];
    let (code, report) = command(&fixture.deps, &args);
    assert_eq!(code, 1, "{report}");
    assert!(report.to_string().contains("broscribe update recover"));
    assert!(updates.join("seen.json").exists() && fixture.install.exists());
    std::fs::remove_file(payload.join("foreign.txt")).unwrap();
    let (code, report) = command(&fixture.deps, &args);
    assert_eq!(code, 0, "{report}");
    assert!(!stage.exists() && !updates.exists() && !fixture.install.exists());
}

#[test]
fn failed_staging_cleanup_keeps_the_update_journal_for_a_retry() {
    let fixture = RemovalFixture::new();
    let updates = fixture.data.join("updates");
    let stage = fixture.install.with_file_name(".install.update");
    std::fs::create_dir_all(&updates).unwrap();
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("unexpected.txt"), "keep").unwrap();
    std::fs::File::create(fixture.install.with_file_name(".install.update.lock")).unwrap();
    let platform = broscribe_platform::stable_path;
    let id = |build| {
        serde_json::json!({
            "product": "BroScribe", "app_id": "BroScribe", "version": "1.0.0",
            "build": build, "target": "x86_64-pc-windows-msvc",
        })
    };
    let signer =
        serde_json::json!({"facility": "manifest_only", "signer":null, "requirement":null});
    std::fs::write(
        stage.join("owner.json"),
        serde_json::json!({
            "schema": 1, "state_dir": platform(&updates), "from": id(10), "to": id(11),
        })
        .to_string(),
    )
    .unwrap();
    let journal = serde_json::json!({
        "schema": 2, "phase": "staged", "install_root": platform(&fixture.install),
        "staging": platform(&stage), "payload_root": "BroScribe",
        "from": id(10), "to": id(11), "installed_signer": signer, "signer": signer,
        "owner_pid": 1, "fence": null, "updated_at": "2026-11-26T08:00:00Z"
    })
    .to_string();
    std::fs::write(updates.join("journal.json"), &journal).unwrap();
    let args = ["uninstall", "--yes", "--remove-data"];
    let (code, report) = command(&fixture.deps, &args);
    assert_eq!(code, 1, "{report}");
    assert!(
        report
            .to_string()
            .contains("Unexpected update staging entry")
    );
    assert_eq!(
        std::fs::read_to_string(updates.join("journal.json")).unwrap(),
        journal
    );
    assert!(fixture.install.exists());
    std::fs::remove_file(stage.join("unexpected.txt")).unwrap();
    let (code, report) = command(&fixture.deps, &args);
    assert_eq!(code, 0, "{report}");
    assert!(!stage.exists() && !updates.exists() && !fixture.install.exists());
}

#[test]
fn default_uninstall_removes_only_recognized_integration_photos() {
    use broscribe_integrations::{IntegrationId, account_dir};
    let fixture = RemovalFixture::new();
    let mut kept = Vec::new();
    let mut cached = Vec::new();
    for id in [IntegrationId::Google, IntegrationId::SystemCalendar] {
        let photos = account_dir(&fixture.data, id).join("photos");
        std::fs::create_dir_all(photos.join("7")).unwrap();
        let path = photos.join("7/notes.txt");
        std::fs::write(&path, "synthetic user file").unwrap();
        kept.push(path);
        for path in [photos.join("p-987abc.img"), photos.join("7/p-123abc.img")] {
            std::fs::write(&path, "synthetic photo").unwrap();
            cached.push(path);
        }
    }
    let (code, report) = command(&fixture.deps, &["uninstall", "--yes"]);
    assert_eq!(code, 0, "{report}");
    assert!(!fixture.install.exists());
    assert!(fixture.data.is_dir() && fixture.library.is_dir());
    assert!(cached.iter().all(|path| !path.exists()));
    for path in kept {
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "synthetic user file"
        );
    }
}

#[test]
fn library_removal_requires_markers_of_a_library_and_keeps_an_ordinary_archive() {
    use broscribe_core::{Library, MeetingId, schema};
    let fixture = RemovalFixture::new();
    let archive = fixture._home.path().join("personal-archive");
    std::fs::create_dir_all(archive.join("2024")).unwrap();
    std::fs::write(archive.join("2024/notes.txt"), "synthetic archive").unwrap();
    let planning = archive.join("2024/11/planning");
    std::fs::create_dir_all(&planning).unwrap();
    std::fs::write(
        planning.join("meeting.md"),
        "---\nid: planning\nstarted: November\n---\nSynthetic personal notes",
    )
    .unwrap();
    let (code, report) = command(
        &fixture.deps,
        &[
            "--library",
            archive.to_str().unwrap(),
            "uninstall",
            "--remove-library",
            "--yes",
        ],
    );
    assert_eq!(code, 1, "{report}");
    assert!(
        report
            .to_string()
            .contains("could not verify a meeting library")
    );
    assert!(archive.join("2024/notes.txt").is_file() && fixture.install.is_dir());

    let lib = Library::open(&fixture.library).unwrap();
    let meeting = schema::Meeting::new(
        MeetingId::parse("m-000001").unwrap(),
        schema::Source::Import,
        schema::now(),
    );
    lib.write(|tx| {
        tx.create_meeting_dir(&meeting)?;
        tx.save_meeting(&meeting)
    })
    .unwrap();
    let (code, report) = command(&fixture.deps, &["uninstall", "--remove-library", "--yes"]);
    assert_eq!(code, 0, "{report}");
    assert!(!fixture.library.exists() && !fixture.install.exists());
    assert!(archive.join("2024/notes.txt").exists());
}

#[cfg(unix)]
#[test]
fn uninstall_dry_run_and_settings_preview_keep_legacy_update_modes() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = RemovalFixture::new();
    let state = fixture.data.join("updates");
    let seen = state.join("seen.json");
    let report = state.join("install-record.json");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(&seen, r#"{"highest_build":11}"#).unwrap();
    let id = broscribe_platform::update::PayloadIdentity {
        product: "BroScribe".into(),
        app_id: "synthetic".into(),
        version: "1.0.0".into(),
        build: 11,
        target: "synthetic".into(),
    };
    broscribe_update_runner::InstallRecord {
        schema: broscribe_update_runner::InstallRecord::SCHEMA,
        fence: 1,
        agent_pid: 42,
        agent_library: None,
        expected: id.clone(),
        unchanged: id,
        runner_pid: None,
    }
    .save(&fixture.data)
    .unwrap();
    for (path, mode) in [(&state, 0o755), (&seen, 0o644), (&report, 0o644)] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    for args in [
        &["uninstall", "--dry-run"][..],
        &["uninstall", "--dry-run", "--from-settings"],
    ] {
        let (code, result) = command(&fixture.deps, args);
        assert_eq!(code, 0, "{result}");
        for (path, mode) in [(&state, 0o755), (&seen, 0o644), (&report, 0o644)] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
    }
}

#[test]
fn uninstall_warns_about_unusable_secondary_inventory_and_keeps_its_recovery_files() {
    let mut fixture = RemovalFixture::new();
    let second = fixture._home.path().join("secondary");
    let updates = second.join("updates");
    std::fs::create_dir_all(&updates).unwrap();
    claim_profile(&second);
    broscribe_core::config::Config::default()
        .save(&second)
        .unwrap();
    let inventory = updates.join("installed-inventory.json");
    std::fs::write(&inventory, "synthetic invalid inventory").unwrap();
    fixture.deps.agents.uninstall = Arc::new(broscribe_platform::uninstall::FakeDesktopUninstall {
        installation: fixture.platform.installation.clone(),
        profiles: vec![second.clone()],
        ..Default::default()
    });
    let (code, result) = command(&fixture.deps, &["uninstall", "--yes", "--remove-data"]);
    assert_eq!(code, 0, "{result}");
    assert!(!fixture.install.exists() && !fixture.data.exists());
    assert!(inventory.exists());
    let steps = result["steps"].as_array().unwrap();
    assert!(steps.iter().any(|step| {
        step["status"] == "warning"
            && step["path"].as_str().is_some_and(|path| {
                broscribe_platform::same_path(std::path::Path::new(path), &updates)
            })
    }));
    assert!(steps.iter().any(|step| {
        step["status"] == "partial"
            && step["path"].as_str().is_some_and(|path| {
                broscribe_platform::same_path(std::path::Path::new(path), &second)
            })
    }));
    for name in ["updates", ".broscribe-profile", "config.toml"] {
        let retained = second.join(name);
        assert!(retained.exists());
        assert!(steps.iter().any(|step| {
            step["status"] == "kept"
                && step["path"].as_str().is_some_and(|path| {
                    broscribe_platform::same_path(std::path::Path::new(path), &retained)
                })
        }));
    }
}

#[test]
fn uninstall_lists_quarantined_update_siblings_without_removing_them() {
    let fixture = RemovalFixture::new();
    let kept = fixture
        .install
        .with_file_name(".install.update-kept-123-1795700000000");
    std::fs::create_dir(&kept).unwrap();
    let file = kept.join("personal.txt");
    std::fs::write(&file, b"synthetic user file").unwrap();
    let unrelated = fixture.install.with_file_name(".install.update-kept-notes");
    std::fs::create_dir(&unrelated).unwrap();
    for option in ["--dry-run", "--yes"] {
        let (code, report) = command(&fixture.deps, &["uninstall", option]);
        assert_eq!(code, 0, "{report}");
        let kept_steps: Vec<_> = report["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|step| step["action"] == "Keep quarantined update staging")
            .collect();
        assert_eq!(kept_steps.len(), 1, "{report}");
        assert_eq!(kept_steps[0]["status"], "kept");
        assert!(
            kept_steps[0]["path"]
                .as_str()
                .unwrap()
                .ends_with(".install.update-kept-123-1795700000000")
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"synthetic user file");
    }
}
