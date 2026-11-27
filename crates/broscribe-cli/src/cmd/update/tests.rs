//! `broscribe update` end to end over synthetic parts: a deterministic
//! update platform on real folders, an in-memory feed signed with a
//! synthetic release key kept in a fake secret store, and a synthetic agent
//! that speaks the real protocol over the real endpoint of a temporary data
//! folder and models install admission (connection-bound until confirmed,
//! durable afterwards, capture refused while held). Where the platform
//! replaces only after exit, the handoff the command writes is run by the
//! update runner's own [`run_handoff`] in this process instead of a runner
//! process. No real key, feed, signature, agent, runner process or
//! installation is involved.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use broscribe_core::config::{Config, UpdateSettings};
use broscribe_ipc::{Call, IpcError, Request, Response};
use broscribe_platform::fakes::{
    FakeAutostart, FakeMicCapture, FakePaths, FakeProcessProbe, FakeProcessTreeControl,
    FakeSecretStore, FakeTrustStore,
};
use broscribe_platform::update::{
    FakeFault, FakeUpdatePlatform, InstallationProcess, PayloadIdentity, Replacement,
    UpdatePlatform,
};
use broscribe_platform::{Autostart, DecoderChain};
use broscribe_platform_native::ipc::Listener;
use broscribe_update_runner::{
    Handoff, RunnerDeps, RunnerOutcome, RunnerReport, run_handoff, state_dir,
};
use broscribe_updates::{
    Cancel, ExistingKey, FakeTransport, MANIFEST_FORMAT, ReleaseKeyLabel, generate_release_key,
    sign_manifest,
};
use chrono::{Duration as Span, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::Pause;
use crate::{Deps, UpdateDeps, run};

const TARGET: &str = "aarch64-apple-darwin";
const APP_ID: &str = "org.broscribe.BroScribe";
const FEED: &str = "https://updates.example/stable.json";
const ARTIFACT: &str = "https://updates.example/BroScribe-11.zip";
const SIGNER: &str = "TEAMID1234";
/// Confirmation requires the confirming command to be the installed
/// version, so the synthetic releases carry this crate's version and move
/// forward by build number.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The process ID the synthetic update runner is started as.
const RUNNER_PID: u32 = 31_337;
/// The agent the synthetic world starts first.
const FIRST_AGENT_PID: u32 = 4242;

/// The update runner of a handed-off step: the test runs the written
/// handoff itself ([`World::run_runner`]).
fn started_runner(data_dir: &Path, command: &std::process::Command) -> broscribe_core::Result<u32> {
    let dir = broscribe_update_runner::runner_dir(data_dir);
    let working = broscribe_platform_native::NativePlatform::update_runner_working_directory(&dir)?;
    assert_eq!(command.get_current_dir(), Some(working.as_path()));
    assert!(command.get_envs().any(|(name, value)| {
        name == "BROSCRIBE_DATA_DIR" && value == Some(data_dir.as_os_str())
    }));
    assert!(command.get_envs().any(|(name, value)| {
        name == broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE && value.is_none()
    }));
    let handoff = Handoff::load(data_dir)?.unwrap();
    let library = handoff.agent_library.unwrap();
    assert!(library.is_absolute());
    assert!(command.get_envs().any(|(name, value)| {
        name == "BROSCRIBE_LIBRARY" && value == Some(library.as_os_str())
    }));
    assert_eq!(
        Path::new(command.get_program()).parent(),
        Some(dir.as_path())
    );
    Ok(RUNNER_PID)
}

fn identity(build: u64) -> PayloadIdentity {
    PayloadIdentity {
        product: "BroScribe".into(),
        app_id: APP_ID.into(),
        version: VERSION.into(),
        build,
        target: TARGET.into(),
    }
}

// ---- the synthetic agent ----

#[derive(Default)]
struct AgentModel {
    pid: u32,
    /// The build it read from its installation at start-up; `None`: unknown.
    build: Option<u64>,
    recording: bool,
    library: PathBuf,
    shutting_down: bool,
    last_fence: u64,
    /// `(fence, durable, holder connection)`.
    held: Option<(u64, bool, u64)>,
    calls: Vec<Call>,
    /// The IPC protocol it speaks; any other than this crate's refuses
    /// every request in its own protocol, as a real agent of it does.
    speaks: u32,
    /// The protocol it speaks after its next restarts (the build in place
    /// then is of that protocol); `None`: unchanged.
    restarts_speaking: Option<u32>,
    /// Requests refused for their protocol.
    refused: usize,
}

impl AgentModel {
    fn handle(&mut self, connection: u64, call: &Call) -> Result<Value, IpcError> {
        self.calls.push(call.clone());
        match call {
            Call::Ping => Ok(json!({
                "pid":self.pid, "version":VERSION, "protocol":broscribe_ipc::PROTOCOL_VERSION,
                "build":self.build.map(|build| json!({"build":build,"target":TARGET})),
                "mode":"normal", "readiness":"normal_operational",
            })),
            Call::Status => Ok(json!({
                "pid": self.pid,
                "version": VERSION,
                "protocol": broscribe_ipc::PROTOCOL_VERSION,
                "library": self.library,
                "recording": null,
                "jobs": { "running": null, "waiting": 0, "deferred": 0 },
                "preview": { "state": "off" },
                "install": self.held.map(|(fence, durable, _)| json!({"fence": fence, "durable": durable})),
                "build": self.build.map(|build| json!({"build": build, "target": TARGET})),
            })),
            Call::Shutdown
                if !self.recording && self.held.is_some_and(|(_, durable, _)| durable) =>
            {
                self.shutting_down = true;
                Ok(json!({"stopping": true}))
            }
            Call::InstallAcquire => {
                if self.recording {
                    return Err(IpcError::new(
                        IpcError::RECORDING_ACTIVE,
                        "a recording is active",
                    ));
                }
                if self.held.is_some() {
                    return Err(IpcError::new(
                        IpcError::BUSY,
                        "another install admission is held",
                    ));
                }
                self.last_fence += 1;
                self.held = Some((self.last_fence, false, connection));
                Ok(json!({ "fence": self.last_fence }))
            }
            Call::InstallConfirm { fence } => match self.held {
                Some((held, _, holder)) if held == *fence && !self.recording => {
                    self.held = Some((held, true, holder));
                    Ok(json!({ "fence": fence, "durable": true }))
                }
                _ => Err(lost(*fence)),
            },
            Call::InstallRelease { fence } => match self.held {
                Some((held, ..)) if held == *fence => {
                    self.held = None;
                    Ok(json!({ "released": fence }))
                }
                _ => Err(lost(*fence)),
            },
            other => Err(IpcError::new(
                IpcError::INVALID_REQUEST,
                format!("the synthetic agent does not handle {other:?}"),
            )),
        }
    }

    fn closed(&mut self, connection: u64) {
        if matches!(self.held, Some((_, false, holder)) if holder == connection) {
            self.held = None;
        }
    }

    /// A capture start, refused while an admission is held.
    fn start_capture(&self) -> Result<(), &'static str> {
        match self.held {
            Some(_) => Err(IpcError::INSTALLING),
            None => Ok(()),
        }
    }

    /// The agent exits and starts again on `build`: a new process that
    /// keeps only a durable admission.
    fn restart(&mut self, build: Option<u64>) {
        self.pid += 1000;
        self.build = build;
        self.held = self.held.filter(|(_, durable, _)| *durable);
        if let Some(protocol) = self.restarts_speaking {
            self.speaks = protocol;
        }
    }

    /// The reply line to `request`: its outcome, or the version refusal of
    /// an agent of another protocol, written in that protocol.
    fn answer(&mut self, connection: u64, request: Request) -> String {
        if self.speaks == broscribe_ipc::PROTOCOL_VERSION {
            let outcome = self.handle(connection, &request.call);
            return Response {
                id: request.id,
                outcome,
            }
            .to_line()
            .unwrap();
        }
        self.refused += 1;
        json!({
            "v": self.speaks,
            "id": request.id,
            "ok": false,
            "error": {
                "code": IpcError::UNSUPPORTED_VERSION,
                "message": format!("protocol version {} is not supported", broscribe_ipc::PROTOCOL_VERSION),
            },
        })
        .to_string()
    }

    fn acquires(&self) -> usize {
        self.calls
            .iter()
            .filter(|call| matches!(call, Call::InstallAcquire))
            .count()
    }
}

fn lost(fence: u64) -> IpcError {
    IpcError::new(
        IpcError::ADMISSION_LOST,
        format!("install admission {fence} is not held"),
    )
}

struct Agent {
    model: Arc<Mutex<AgentModel>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Agent {
    fn start(data_dir: &Path) -> Self {
        std::fs::create_dir_all(data_dir).unwrap();
        let model = Arc::new(Mutex::new(AgentModel {
            pid: FIRST_AGENT_PID,
            build: Some(10),
            library: data_dir.parent().unwrap().join("library"),
            speaks: broscribe_ipc::PROTOCOL_VERSION,
            ..AgentModel::default()
        }));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let shared = Arc::clone(&model);
        let data_dir = data_dir.to_path_buf();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut listener = Listener::bind(&data_dir).unwrap();
                ready_tx.send(()).unwrap();
                let mut connections = tokio::task::JoinSet::new();
                let mut next = 0u64;
                loop {
                    let connection = tokio::select! {
                        _ = &mut stopped => break,
                        accepted = listener.accept() => accepted.unwrap(),
                    };
                    next += 1;
                    let id = next;
                    let model = Arc::clone(&shared);
                    connections.spawn(async move {
                        let (mut reader, mut writer) = connection.into_split();
                        while let Ok(Some(line)) = reader.read_line().await {
                            let request = Request::parse(&line).unwrap();
                            let response = model.lock().unwrap().answer(id, request);
                            if writer.write_line(response).await.is_err() {
                                break;
                            }
                        }
                        model.lock().unwrap().closed(id);
                    });
                }
                connections.abort_all();
            });
        });
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        Self {
            model,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn model(&self) -> std::sync::MutexGuard<'_, AgentModel> {
        self.model.lock().unwrap()
    }

    fn hold(&self) -> Option<(u64, bool)> {
        self.model()
            .held
            .map(|(fence, durable, _)| (fence, durable))
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The agent's service registration: stop and start restart the synthetic
/// agent, on the build installed at that moment, when registered.
struct AgentService {
    model: Arc<Mutex<AgentModel>>,
    registered: bool,
    start_fails: bool,
    job_without_manager: Option<broscribe_platform::LoginJob>,
    platform: Arc<FakeUpdatePlatform>,
    root: PathBuf,
}

impl Autostart for AgentService {
    fn is_enabled(&self) -> broscribe_core::Result<bool> {
        Ok(self.registered)
    }
    fn set_enabled(&self, _enabled: bool) -> broscribe_core::Result<()> {
        panic!("starting an update must not change the login preference")
    }
    fn login_job(&self, _: &Path) -> broscribe_platform::LoginJob {
        if let Some(job) = self.job_without_manager {
            return job;
        }
        let pid = self.model.lock().unwrap().pid;
        broscribe_platform::LoginJob::Running(std::num::NonZeroU32::new(pid).unwrap())
    }
    fn stop(&self) -> broscribe_core::Result<()> {
        Ok(())
    }
    fn start(&self) -> broscribe_core::Result<()> {
        if self.start_fails {
            return Err(broscribe_core::Error::Service {
                code: "update_agent_unproven",
                message: "synthetic agent restart failed".into(),
            });
        }
        let build = self.platform.inspect(&self.root).ok().map(|id| id.build);
        let mut model = self.model.lock().unwrap();
        if self.job_without_manager.is_none() || model.shutting_down {
            model.shutting_down = false;
            model.restart(build);
        }
        Ok(())
    }
}

/// The agent's service as the update runner drives it: a stop closes the
/// endpoint (told to the runner through `listening`; the synthetic
/// listener stays up), a start of the stopped agent starts it again on the
/// build installed at that moment, and a start of a running agent changes
/// nothing.
struct RunnerService {
    model: Arc<Mutex<AgentModel>>,
    platform: Arc<FakeUpdatePlatform>,
    root: PathBuf,
    listening: AtomicBool,
    stops: AtomicUsize,
    restart_library: Mutex<Option<PathBuf>>,
}

impl Autostart for RunnerService {
    fn is_enabled(&self) -> broscribe_core::Result<bool> {
        Ok(true)
    }
    fn set_enabled(&self, _enabled: bool) -> broscribe_core::Result<()> {
        Ok(())
    }
    fn stop(&self) -> broscribe_core::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.listening.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn start(&self) -> broscribe_core::Result<()> {
        if !self.listening.swap(true, Ordering::SeqCst) {
            let build = self.platform.inspect(&self.root).ok().map(|id| id.build);
            self.model.lock().unwrap().restart(build);
        }
        Ok(())
    }
    fn start_for_update(
        &self,
        _: &broscribe_platform::LoginRegistrationGuard,
        _: &Path,
        library: &Path,
    ) -> broscribe_core::Result<()> {
        *self.restart_library.lock().unwrap() = Some(library.to_path_buf());
        self.start()
    }
}

// ---- the world ----

struct World {
    tmp: tempfile::TempDir,
    root: PathBuf,
    platform: Arc<FakeUpdatePlatform>,
    transport: FakeTransport,
    secrets: Arc<FakeSecretStore>,
    public_key: String,
    agent: Option<Agent>,
}

impl World {
    fn new() -> Self {
        Self::replacing(Replacement::InPlace)
    }

    /// A platform that replaces only once every process running from the
    /// installation exited (Windows): `update install` hands off.
    fn after_exit() -> Self {
        Self::replacing(Replacement::AfterExit)
    }

    fn replacing(replacement: Replacement) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Applications").join("BroScribe.app");
        Self::replacing_at(replacement, tmp, root)
    }

    fn replacing_at(replacement: Replacement, tmp: tempfile::TempDir, root: PathBuf) -> Self {
        FakeUpdatePlatform::write_payload(&root, &identity(10), SIGNER).unwrap();
        let secrets = Arc::new(FakeSecretStore::default());
        // A synthetic key in a fake store, never a real release key.
        let public_key = generate_release_key(
            secrets.as_ref(),
            &ReleaseKeyLabel::parse("test-release").unwrap(),
            ExistingKey::Refuse,
        )
        .unwrap()
        .key
        .public_key_hex();
        let mut platform = FakeUpdatePlatform::new(root.clone(), TARGET);
        platform.replacement = replacement;
        Self {
            platform: Arc::new(platform),
            transport: FakeTransport::new(),
            secrets,
            public_key,
            agent: None,
            root,
            tmp,
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.tmp.path().join("root").join("data")
    }

    fn enable(&self) {
        std::fs::create_dir_all(self.data_dir()).unwrap();
        std::fs::write(
            self.data_dir().join("config.toml"),
            format!(
                "[updates]\nenabled = true\nfeed_url = '{FEED}'\ntrusted_keys = ['{}']\nsigner = '{SIGNER}'\n",
                self.public_key
            ),
        )
        .unwrap();
    }

    fn start_agent(&mut self) -> &Agent {
        self.agent = Some(Agent::start(&self.data_dir()));
        self.agent.as_ref().unwrap()
    }

    fn agent(&self) -> &Agent {
        self.agent.as_ref().unwrap()
    }

    /// Publishes build `build` as a signed release with an honest archive.
    fn publish(&self, build: u64) {
        let (bytes, entries, installed) = archive(&identity(build));
        let now = Utc::now();
        let manifest = json!({
            "format": MANIFEST_FORMAT,
            "schema": 1,
            "product": "BroScribe",
            "channel": "stable",
            "version": VERSION,
            "build": build,
            "published_at": (now - Span::hours(1)).to_rfc3339(),
            "expires_at": (now + Span::days(30)).to_rfc3339(),
            "artifacts": [{
                "target": TARGET,
                "app_id": APP_ID,
                "url": ARTIFACT,
                "size": bytes.len(),
                "sha256": Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>(),
                "archive": "zip",
                "root": "BroScribe.app",
                "entries": entries,
                "installed_size": installed,
            }],
        });
        let signed = sign_manifest(
            self.secrets.as_ref(),
            &[ReleaseKeyLabel::parse("test-release").unwrap()],
            &manifest.to_string(),
            now,
            &Cancel::new(),
        )
        .unwrap();
        self.transport.serve(FEED, signed.envelope.into_bytes());
        self.transport.serve(ARTIFACT, bytes);
    }

    fn deps(&self, registered: bool) -> Deps {
        let model = self
            .agent
            .as_ref()
            .map(|agent| Arc::clone(&agent.model))
            .unwrap_or_default();
        let autostart: Box<dyn Autostart> = if self.agent.is_some() {
            Box::new(AgentService {
                model,
                registered,
                start_fails: false,
                job_without_manager: None,
                platform: self.platform.clone(),
                root: self.root.clone(),
            })
        } else {
            Box::new(FakeAutostart::default())
        };
        let platform: Arc<dyn UpdatePlatform> = self.platform.clone();
        Deps {
            clock: crate::system_clock(),
            paths: Box::new(FakePaths::new(self.tmp.path().join("root"))),
            models: Box::new(broscribe_models::fakes::FakeModelProvider::default()),
            decoder: DecoderChain::default(),
            process: Box::new(FakeProcessProbe::new(std::process::id())),
            mic: Box::new(FakeMicCapture::new(16_000)),
            secrets: self.secrets.clone(),
            browser: Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
            agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
            autostart,
            agents: crate::AgentEnv::isolated(self.tmp.path()),
            execution: broscribe_models::ExecutionContext::new(
                Arc::new(FakeProcessTreeControl::default()),
                Arc::new(FakeTrustStore::default()),
            ),
            updates: UpdateDeps::with_transport(platform, Arc::new(self.transport.clone())),
        }
    }

    /// Runs `broscribe --json <args…>` with the agent registered.
    fn run(&self, args: &[&str]) -> (i32, Value) {
        self.run_with(&self.deps(true), args)
    }

    /// Runs the command on this thread, where a handed-off step starts no
    /// runner process ([`started_runner`]).
    fn run_with(&self, deps: &Deps, args: &[&str]) -> (i32, Value) {
        run_cli(deps, args)
    }

    /// The enabled settings [`World::enable`] saves: the synthetic feed,
    /// release key and signer.
    fn settings(&self) -> UpdateSettings {
        UpdateSettings {
            enabled: true,
            feed_url: Some(FEED.into()),
            channel: "stable".into(),
            trusted_keys: vec![self.public_key.clone()],
            signer: Some(SIGNER.into()),
        }
    }

    /// The saved revision and `[updates]` table (`update settings show`).
    fn shown_settings(&self) -> (u64, Value) {
        let (status, value) = self.run(&["update", "settings", "show"]);
        assert_eq!(status, 0, "{value}");
        (
            value["revision"].as_u64().unwrap(),
            value["updates"].clone(),
        )
    }

    fn installed(&self) -> PayloadIdentity {
        self.platform.inspect(&self.root).unwrap()
    }

    fn staging(&self) -> PathBuf {
        self.root.with_file_name(".BroScribe.app.update")
    }

    fn stage(&self, build: u64) {
        self.publish(build);
        let (code, value) = self.run(&["update", "download"]);
        assert_eq!(code, 0, "{value}");
        assert_eq!(value["state"], "staged");
    }

    /// Stages 11 over 10 and installs it through the command, which hands
    /// the install off under a durable admission. Returns its fence.
    fn handed_off_install(&mut self) -> u64 {
        self.enable();
        self.start_agent();
        self.stage(11);
        let (status, value) = self.run(&["update", "install", "--yes"]);
        assert_eq!(status, 0, "{value}");
        assert_eq!(value["state"], "handed_off");
        assert_eq!(value["runner_pid"], RUNNER_PID);
        let fence = value["fence"].as_u64().unwrap();
        assert_eq!(
            self.agent().hold(),
            Some((fence, true)),
            "durable before the command exits"
        );
        fence
    }

    /// Runs the handoff the command left, as the runner process does, with
    /// `probe` telling which processes it waits for still run.
    fn run_runner(&self, probe: &FakeProcessProbe) -> (RunnerReport, RunnerService) {
        let handoff = Handoff::load(&self.data_dir())
            .unwrap()
            .expect("a handed-off step");
        let service = RunnerService {
            model: Arc::clone(&self.agent().model),
            platform: Arc::clone(&self.platform),
            root: self.root.clone(),
            listening: AtomicBool::new(true),
            stops: AtomicUsize::new(0),
            restart_library: Mutex::new(None),
        };
        let report = {
            let listening = |_: &Path| -> broscribe_core::Result<bool> {
                Ok(service.listening.load(Ordering::SeqCst))
            };
            run_handoff(
                &handoff,
                &RunnerDeps {
                    platform: self.platform.as_ref(),
                    autostart: &service,
                    probe,
                    agent_listening: &listening,
                    timeout: Duration::from_millis(300),
                    poll: Duration::from_millis(10),
                },
            )
            .unwrap()
        };
        (report, service)
    }

    /// The agent is held at `fence`, so no capture starts.
    fn assert_held(&self, fence: u64) {
        assert_eq!(self.agent().hold(), Some((fence, true)));
        assert_eq!(
            self.agent().model().start_capture(),
            Err(IpcError::INSTALLING)
        );
    }

    /// Every one of `changes` to the update settings, made at the
    /// `revision` shown, is refused with `refusal` while the step holding
    /// `fence` is unfinished: the saved settings and revision stay, and so
    /// does the admission.
    fn assert_settings_kept(
        &self,
        changes: &[UpdateSettings],
        revision: u64,
        refusal: &str,
        fence: u64,
    ) {
        for change in changes {
            let (status, value) = set_settings(&self.deps(true), change, Some(revision));
            assert_eq!(status, 1, "{change:?}: {value}");
            assert_eq!(code(&value), refusal, "{value}");
        }
        assert_eq!(
            self.shown_settings(),
            (revision, settings_value(&self.settings()))
        );
        self.assert_held(fence);
    }
}

fn runner_failure(report: &RunnerReport) -> &str {
    match &report.outcome {
        RunnerOutcome::Failed { code, .. } => code.as_str(),
        other => panic!("expected a failed step, got {other:?}"),
    }
}

fn archive(id: &PayloadIdentity) -> (Vec<u8>, u64, u64) {
    use zip::write::SimpleFileOptions;
    let info = json!({
        "schema": 1, "product": id.product, "app_id": id.app_id,
        "version": id.version, "build": id.build, "target": id.target,
    })
    .to_string();
    let files: [(&str, &[u8], u32); 3] = [
        ("BroScribe.app/build-info.json", info.as_bytes(), 0o644),
        ("BroScribe.app/signature", SIGNER.as_bytes(), 0o644),
        ("BroScribe.app/bin/broscribe", b"#!tool", 0o755),
    ];
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .add_directory("BroScribe.app/", SimpleFileOptions::default())
        .unwrap();
    writer
        .add_directory("BroScribe.app/bin/", SimpleFileOptions::default())
        .unwrap();
    let mut installed = 0u64;
    for (name, body, mode) in files {
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(mode);
        writer.start_file(name, options).unwrap();
        writer.write_all(body).unwrap();
        installed += body.len() as u64;
    }
    let bytes = writer.finish().unwrap().into_inner();
    (bytes, 5, installed)
}

fn code(value: &Value) -> &str {
    value["error"]["code"].as_str().unwrap_or("")
}

/// Runs `broscribe --json <args…>` on this thread with `deps`, where a
/// handed-off step starts no runner process ([`started_runner`]).
fn run_cli(deps: &Deps, args: &[&str]) -> (i32, Value) {
    super::TEST_RUNNER.set(Some(started_runner));
    let mut argv: Vec<OsString> = vec!["broscribe".into(), "--json".into()];
    argv.extend(args.iter().map(OsString::from));
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(argv, deps, &mut out, &mut err);
    let value = serde_json::from_slice(&out).unwrap_or_else(|_| {
        panic!(
            "no JSON for {args:?}: {}{}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        )
    });
    (code, value)
}

/// `update settings set --spec-stdin [--expect-revision N]` with `settings`
/// as its input; the app's toggle always names the revision it showed.
fn set_settings(
    deps: &Deps,
    settings: &UpdateSettings,
    expect_revision: Option<u64>,
) -> (i32, Value) {
    super::TEST_SETTINGS.set(Some(settings.clone()));
    let revision = expect_revision.map(|revision| revision.to_string());
    let mut args = vec!["update", "settings", "set", "--spec-stdin"];
    if let Some(revision) = &revision {
        args.extend(["--expect-revision", revision.as_str()]);
    }
    run_cli(deps, &args)
}

fn assert_working_directory_refusal(value: &Value, data: &Path, library: &Path) {
    assert_eq!(code(value), "installation_working_directory");
    let message = value["error"]["message"].as_str().unwrap();
    for (label, path) in [("Data folder", data), ("Library", library)] {
        let path = broscribe_platform::plain_verbatim(&broscribe_platform::stable_path(path));
        assert!(path.is_absolute());
        assert!(
            message
                .lines()
                .any(|line| line == format!("{label}: {}", path.display())),
            "{message}"
        );
    }
    assert_eq!(
        message.lines().last(),
        Some(
            "Change to a folder outside the installation and run the same update command again, without changing BROSCRIBE_DATA_DIR."
        )
    );
    assert!(!message.contains("--library"), "{message}");
}

#[test]
fn update_rejects_a_relative_profile_selector_from_either_working_folder() {
    use broscribe_platform::paths::ENV_DATA_DIR;

    const CHILD: &str = "BROSCRIBE_TEST_RELATIVE_UPDATE_DATA";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env(CHILD, "1")
            .env(ENV_DATA_DIR, "profile")
            .args([
                "--exact",
                "cmd::update::tests::update_rejects_a_relative_profile_selector_from_either_working_folder",
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

    let world = World::after_exit();
    let original = std::env::current_dir().unwrap();
    let outside = world.tmp.path().join("retry");
    std::fs::create_dir(&outside).unwrap();
    for cwd in [&world.root, &outside] {
        std::env::set_current_dir(cwd).unwrap();
        for action in ["install", "rollback", "recover"] {
            let mut args = vec!["update", action];
            if action != "recover" {
                args.push("--yes");
            }
            let (status, value) = world.run(&args);
            assert_eq!(status, 1, "{value}");
            assert_eq!(code(&value), "invalid_input");
            let message = value["error"]["message"].as_str().unwrap();
            assert!(
                message.contains(&format!("Use an absolute path in {ENV_DATA_DIR}")),
                "{value}"
            );
            assert!(!cwd.join("profile").exists());
        }
    }
    std::env::set_current_dir(original).unwrap();
}

#[test]
fn update_refuses_each_relative_library_selector_before_admission() {
    use broscribe_platform::paths::{ENV_DATA_DIR, ENV_LIBRARY, ENV_MAINTENANCE_CONFIG_BASE};

    const CHILD: &str = "BROSCRIBE_TEST_RELATIVE_UPDATE_LIBRARY";
    const RELATIVE: &str = "retry-library";
    let Some(source) = std::env::var_os(CHILD) else {
        let config_base = tempfile::tempdir().unwrap();
        for source in [
            "--library",
            ENV_LIBRARY,
            "config",
            "shadowed-env",
            "shadowed-config",
        ] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .env(CHILD, source)
                .env(ENV_MAINTENANCE_CONFIG_BASE, config_base.path())
                .env_remove(ENV_DATA_DIR)
                .env_remove(ENV_LIBRARY)
                .args([
                    "--exact",
                    "cmd::update::tests::update_refuses_each_relative_library_selector_before_admission",
                ]);
            if source == ENV_LIBRARY || source == "shadowed-env" {
                child.env(ENV_LIBRARY, RELATIVE);
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

    // A shadowed source (overridden by an absolute --library) still counts:
    // the updater protects it too, read against the current folder.
    let shadowed = source.to_str().unwrap().starts_with("shadowed-");
    let world = World::after_exit();
    let data = world.data_dir();
    if source == "config" || source == "shadowed-config" {
        Config::update(&data, |config| {
            config.library = Some(PathBuf::from(RELATIVE));
            Ok(())
        })
        .unwrap();
    }
    let expected_source = match source.to_str().unwrap() {
        "config" | "shadowed-config" => format!("`library` in {}", Config::path(&data).display()),
        "shadowed-env" => ENV_LIBRARY.to_owned(),
        other => other.to_owned(),
    };
    let absolute = data.with_file_name("absolute-library");
    for action in ["install", "rollback", "recover"] {
        let mut args = vec!["update", action];
        if action != "recover" {
            args.push("--yes");
        }
        if expected_source == "--library" {
            args.extend(["--library", RELATIVE]);
        } else if shadowed {
            args.extend(["--library", absolute.to_str().unwrap()]);
        }
        let (exit, value) = world.run(&args);
        assert_eq!(exit, 1, "{action}: {value}");
        assert_eq!(code(&value), "invalid_input", "{action}: {value}");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains(&expected_source), "{action}: {message}");
        assert!(message.contains(RELATIVE), "{action}: {message}");
        assert!(message.contains("absolute"), "{action}: {message}");
        assert!(
            broscribe_core::maintenance::profile_id(&data)
                .unwrap()
                .is_none(),
            "{action} acquired a mode lease"
        );
    }
}

#[test]
fn windows_install_and_rollback_refuse_the_installation_working_directory() {
    const CHILD: &str = "BROSCRIBE_TEST_RUNNER_CALLER_CWD";
    use broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE;
    if std::env::var_os(CHILD).is_none() {
        let base = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env(CHILD, "1")
            .env(ENV_MAINTENANCE_CONFIG_BASE, base.path())
            .args([
                "--exact",
                "cmd::update::tests::windows_install_and_rollback_refuse_the_installation_working_directory",
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
    let mut world = World::after_exit();
    let original_dir = std::env::current_dir().unwrap();
    world.enable();
    world.start_agent();
    world.stage(11);
    let config_base = super::super::maintenance_config_base().unwrap();
    let library = config_base.join("retry-library");
    let data = world.data_dir();
    let args = [
        "update",
        "install",
        "--yes",
        "--library",
        library.to_str().unwrap(),
    ];
    std::env::set_current_dir(&world.root).unwrap();
    let (exit, value) = world.run(&args);
    assert_ne!(exit, 0, "{value}");
    assert_working_directory_refusal(&value, &data, &library);
    assert!(Handoff::load(&world.data_dir()).unwrap().is_none());
    assert!(world.agent().hold().is_none());

    std::env::set_current_dir(&original_dir).unwrap();
    let (exit, value) = world.run(&args);
    assert_eq!(exit, 0, "{value}");
    assert_eq!(value["state"], "handed_off");
    let handoff = Handoff::load(&world.data_dir()).unwrap().unwrap();
    assert_eq!(
        handoff.config_base,
        super::super::maintenance_config_base().unwrap()
    );
    assert!(handoff.wait_pids.contains(&std::process::id()));
    let (report, _) = world.run_runner(&FakeProcessProbe::new(RUNNER_PID));
    assert!(
        matches!(report.outcome, RunnerOutcome::Installed { .. }),
        "{report:?}"
    );

    std::env::set_current_dir(&world.root).unwrap();
    let (exit, value) = world.run(&["update", "rollback", "--yes"]);
    std::env::set_current_dir(&original_dir).unwrap();
    assert_ne!(exit, 0, "{value}");
    assert_working_directory_refusal(&value, &data, &world.deps(true).paths.default_library_dir());
    assert!(Handoff::load(&world.data_dir()).unwrap().is_none());
    world.assert_held(handoff.fence);

    let (exit, value) = world.run(&["update", "rollback", "--yes"]);
    assert_eq!(exit, 0, "{value}");
    assert_eq!(value["state"], "handed_off");
    let (report, _) = world.run_runner(&FakeProcessProbe::new(RUNNER_PID));
    assert!(
        matches!(report.outcome, RunnerOutcome::RolledBack { .. }),
        "{report:?}"
    );
}

#[test]
fn relocated_windows_install_refuses_a_shadowed_relative_config_library() {
    use broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE;
    const CHILD: &str = "BROSCRIBE_TEST_RELOCATED_INSTALL";
    if std::env::var_os(CHILD).is_none() {
        let base = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env(CHILD, "1")
            .env(ENV_MAINTENANCE_CONFIG_BASE, base.path())
            .current_dir(cwd.path())
            .args([
                "--exact",
                "cmd::update::tests::relocated_windows_install_refuses_a_shadowed_relative_config_library",
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

    let base = PathBuf::from(std::env::var_os(ENV_MAINTENANCE_CONFIG_BASE).unwrap());
    let tmp = tempfile::tempdir_in(&base).unwrap();
    let root = tmp.path().join("BroScribe.app");
    let mut world = World::replacing_at(Replacement::AfterExit, tmp, root);
    world.enable();
    world.start_agent();
    world.stage(11);
    let library = world.root.join("synthetic-library");
    std::fs::create_dir(&library).unwrap();
    let relative = library.strip_prefix(&base).unwrap();
    assert!(!std::env::current_dir().unwrap().join(relative).exists());
    broscribe_core::config::Config::update(&world.data_dir(), |config| {
        config.library = Some(relative.to_path_buf());
        Ok(())
    })
    .unwrap();

    // A relative config library is refused before anything is resolved,
    // even when an absolute --library shadows it.
    let selected = world.data_dir().with_file_name("selected-library");
    let (exit, value) = world.run(&[
        "update",
        "install",
        "--yes",
        "--library",
        selected.to_str().unwrap(),
    ]);
    assert_eq!(exit, 1, "{value}");
    assert_eq!(code(&value), "invalid_input", "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("`library` in"), "{value}");
    assert!(message.contains(&*relative.to_string_lossy()), "{value}");
    assert!(Handoff::load(&world.data_dir()).unwrap().is_none());
    assert!(world.agent().hold().is_none());
}

#[test]
fn runner_explains_a_sharing_violation_moving_the_windows_installation() {
    let mut world = World::after_exit();
    Arc::get_mut(&mut world.platform).unwrap().atomic_exchange = false;
    world.handed_off_install();
    world
        .platform
        .set_fault(Some(FakeFault::RenameInstallation(32)));
    let (report, _) = world.run_runner(&FakeProcessProbe::new(RUNNER_PID));
    let RunnerOutcome::Failed { code, message } = report.outcome else {
        panic!("expected the installation to stay in place: {report:?}");
    };
    assert_eq!(code, "update_installation_in_use");
    assert!(message.contains("Another program or terminal"), "{message}");
    assert!(
        message.contains("change the terminal's current folder"),
        "{message}"
    );
    assert_eq!(world.platform.inspect(&world.root).unwrap().build, 10);
}

#[test]
fn a_modified_stage_is_refused_before_windows_install_takes_admission() {
    let mut world = World::after_exit();
    world.enable();
    world.start_agent();
    world.stage(11);
    std::fs::write(
        world.staging().join("incoming/BroScribe.app/extra"),
        "changed",
    )
    .unwrap();
    let (status, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), broscribe_updates::codes::VERIFICATION_FAILED);
    assert_eq!(world.agent().model().acquires(), 0);
    assert!(
        world
            .staging()
            .join("incoming/BroScribe.app/extra")
            .is_file()
    );
    assert!(Handoff::load(&world.data_dir()).unwrap().is_none());
}

#[test]
fn check_preference_preserves_overrides_without_printing_the_private_feed() {
    let world = World::new();
    let deps = world.deps(false);
    let (status, defaults) = run_cli(&deps, &["update", "settings", "show"]);
    assert_eq!(status, 0, "{defaults}");
    assert_eq!(defaults["source"], "built_in");
    assert_eq!(defaults["updates"]["enabled"], true);
    let settings = UpdateSettings {
        feed_url: Some("https://mirror.example/a-private-capability".into()),
        trusted_keys: vec![world.public_key.clone()],
        ..UpdateSettings::default()
    };
    let (status, shown) = set_settings(&deps, &settings, None);
    assert_eq!(status, 0, "{shown}");
    assert_eq!(shown["source"], "overridden");
    assert!(!shown.to_string().contains("private-capability"));
    let revision = shown["revision"].as_u64().unwrap().to_string();
    let (status, disabled) = run_cli(
        &deps,
        &[
            "update",
            "settings",
            "set",
            "--enabled",
            "false",
            "--expect-revision",
            &revision,
        ],
    );
    assert_eq!(status, 0, "{disabled}");
    assert_eq!(disabled["updates"]["enabled"], false);
    assert_eq!(
        broscribe_core::config::Config::load(&world.data_dir())
            .unwrap()
            .updates,
        UpdateSettings {
            enabled: false,
            ..settings
        }
    );
}

/// The answers of the commands a race runs at its pause.
type Answers = Rc<RefCell<Vec<(i32, Value)>>>;

/// Pauses the next command on this thread once it reaches `at` and runs
/// `competitor` there, as a racing process would; its answers are in the
/// returned list once the paused command went on.
fn race_at(at: Pause, competitor: impl FnOnce() -> Vec<(i32, Value)> + 'static) -> Answers {
    let answers = Answers::default();
    let sink = Rc::clone(&answers);
    let racer: Box<dyn FnOnce()> = Box::new(move || {
        let answered = competitor();
        sink.borrow_mut().extend(answered);
    });
    super::TEST_PAUSE.set(Some((at, racer)));
    answers
}

fn settings_value(settings: &UpdateSettings) -> Value {
    let mut public = serde_json::to_value(settings).unwrap();
    public["feed_url"] = if settings.effective_feed() == broscribe_core::config::RELEASE_FEED {
        json!(broscribe_core::config::RELEASE_FEED)
    } else {
        Value::Null
    };
    public
}

// ---- scenarios ----

#[test]
fn defaults_check_the_builtin_source_and_explicit_opt_out_makes_no_requests() {
    let world = World::new();
    let (status, value) = world.run(&["update", "status"]);
    assert_eq!(status, 0);
    assert_eq!(value["update"]["state"], "idle");
    assert_eq!(value["settings"]["enabled"], true);
    assert_eq!(value["settings"]["source"], "built_in");
    let (status, value) = world.run(&["update", "check"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "no_verified_update");
    let feed = broscribe_core::config::RELEASE_FEED;
    assert_eq!(world.transport.requests(), [feed]);
    let (status, value) = world.run(&["update", "settings", "set", "--enabled", "false"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["updates"]["enabled"], false);
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "off");
    let (status, value) = world.run(&["update", "check"]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "not_configured");
    assert_eq!(
        world.transport.requests(),
        [feed],
        "opt-out made no extra request"
    );
}

#[test]
fn install_and_rollback_need_explicit_yes() {
    let world = World::new();
    for args in [["update", "install"], ["update", "rollback"]] {
        let (status, value) = world.run(&args);
        assert_eq!(status, 2, "{args:?}");
        assert_eq!(code(&value), "invalid_input");
    }
}

#[test]
fn signed_update_restart_leaves_another_profiles_recording_login_job_running() {
    use broscribe_core::Result as CoreResult;
    use broscribe_platform::{LoginJob, LoginRegistrationGuard};
    use std::num::NonZeroU32;

    struct SharedLoginJob {
        path: PathBuf,
        pid: u32,
    }

    impl Autostart for SharedLoginJob {
        fn is_enabled(&self) -> CoreResult<bool> {
            Ok(true)
        }
        fn lock_login_registration(&self, wait: Duration) -> CoreResult<LoginRegistrationGuard> {
            LoginRegistrationGuard::acquire(&self.path, wait)
        }
        fn login_job(&self, _: &Path) -> LoginJob {
            let error = LoginRegistrationGuard::acquire(&self.path, Duration::ZERO)
                .expect_err("the login job must be checked under the registration guard");
            assert_eq!(error.code(), "busy");
            LoginJob::Running(NonZeroU32::new(self.pid).unwrap())
        }
        fn set_enabled(&self, _: bool) -> CoreResult<()> {
            panic!("the other profile's login registration must not change")
        }
        fn stop(&self) -> CoreResult<()> {
            panic!("the other profile's recording must not be stopped")
        }
        fn start(&self) -> CoreResult<()> {
            panic!("the other profile's login agent must not be started")
        }
    }

    let mut owner = World::new();
    let owner_pid = FIRST_AGENT_PID + 1;
    {
        let mut agent = owner.start_agent().model();
        agent.pid = owner_pid;
        agent.recording = true;
    }
    let mut other = World::new();
    other.enable();
    other.start_agent();
    other.publish(11);
    other.stage(11);

    let mut deps = other.deps(true);
    deps.autostart = Box::new(SharedLoginJob {
        path: owner.data_dir().join("shared-agent-registration.lock"),
        pid: owner_pid,
    });
    let (status, value) = other.run_with(&deps, &["update", "install", "--yes"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "login_job_unverified");
    assert!(owner.agent().model().recording);
    assert_eq!(other.installed(), identity(10));
}

#[test]
fn signed_update_restarts_a_managed_agent_with_login_autostart_off() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    let deps = world.deps(false);
    assert!(!deps.autostart.is_enabled().unwrap());

    let (status, value) = world.run_with(&deps, &["update", "install", "--yes"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["agent"]["proven"], true, "{value}");
    assert_ne!(world.agent().model().pid, FIRST_AGENT_PID);
    assert_eq!(world.installed(), identity(11));
    assert!(!deps.autostart.is_enabled().unwrap());
}

#[test]
fn a_signed_update_installs_under_the_agents_admission_and_confirms() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.publish(11);
    let (status, value) = world.run(&["update", "check"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "available");
    assert_eq!(value["build"], 11);
    assert_eq!(value["signer_pin"], SIGNER);
    world.stage(11);
    assert_eq!(world.installed(), identity(10), "download replaces nothing");

    let (status, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "awaiting_confirmation");
    assert_eq!(value["agent"]["proven"], true, "{value}");
    assert_eq!(world.installed(), identity(11));
    let fence = value["fence"].as_u64().unwrap();
    assert_eq!(
        world.agent().hold(),
        Some((fence, true)),
        "durable after restart"
    );
    assert_eq!(
        world.agent().model().start_capture(),
        Err(IpcError::INSTALLING),
        "no capture between replacement and confirmation"
    );

    let (status, value) = world.run(&["update", "confirm"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "confirmed");
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());
    assert!(!world.staging().exists(), "the previous copy is removed");
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "idle");
}

#[test]
fn portable_installation_at_the_default_library_is_refused_before_staging() {
    let mut world = World::new();
    world.root = world.tmp.path().join("root/library");
    Arc::get_mut(&mut world.platform).unwrap().installed_root = world.root.clone();
    FakeUpdatePlatform::write_payload(&world.root, &identity(10), SIGNER).unwrap();
    let meeting = world.root.join("synthetic-meeting.md");
    std::fs::write(&meeting, "Keep this meeting").unwrap();
    world.enable();
    world.publish(11);

    let (status, value) = world.run(&["update", "download"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "update_location_unsupported");
    assert!(!world.root.with_file_name(".library.update").exists());
    assert_eq!(
        std::fs::read_to_string(meeting).unwrap(),
        "Keep this meeting"
    );
    assert_eq!(world.installed(), identity(10));
}

#[test]
fn an_installation_inside_the_home_folder_updates() {
    // The standard per-user layouts (`~/.local/lib/broscribe`,
    // `%LOCALAPPDATA%\\Programs\\BroScribe`) live inside the home folder.
    let mut world = World::new();
    world.root = world.tmp.path().join("home/.local/lib/broscribe");
    Arc::get_mut(&mut world.platform).unwrap().installed_root = world.root.clone();
    FakeUpdatePlatform::write_payload(&world.root, &identity(10), SIGNER).unwrap();
    world.enable();
    world.publish(11);

    let (status, value) = world.run(&["update", "download"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "staged");
}

#[test]
fn an_installation_that_is_the_home_folder_is_refused() {
    let mut world = World::new();
    world.root = world.tmp.path().join("home");
    Arc::get_mut(&mut world.platform).unwrap().installed_root = world.root.clone();
    FakeUpdatePlatform::write_payload(&world.root, &identity(10), SIGNER).unwrap();
    let note = world.root.join("synthetic-note.txt");
    std::fs::write(&note, "Keep this file").unwrap();
    world.enable();
    world.publish(11);

    let (status, value) = world.run(&["update", "download"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "update_location_unsupported");
    assert_eq!(std::fs::read_to_string(note).unwrap(), "Keep this file");
    assert_eq!(world.installed(), identity(10));
}

#[test]
fn install_checks_the_library_of_the_running_agent_again() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    let meeting = world.root.join("library/synthetic-meeting.md");
    std::fs::create_dir(meeting.parent().unwrap()).unwrap();
    std::fs::write(&meeting, "Keep this meeting").unwrap();
    world.agent().model.lock().unwrap().library = meeting.parent().unwrap().to_path_buf();

    let (status, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "update_location_unsupported");
    assert_eq!(world.installed(), identity(10));
    assert_eq!(
        std::fs::read_to_string(meeting).unwrap(),
        "Keep this meeting"
    );
    assert!(world.staging().is_dir());
    assert!(world.agent().hold().is_none());
}

#[test]
fn secondary_profile_cannot_replace_the_default_profiles_configured_library() {
    let mut world = World::new();
    let secondary = world.tmp.path().join("secondary");
    let mut config = broscribe_core::config::Config::default();
    config.updates = world.settings();
    config.save(&secondary).unwrap();
    world.agent = Some(Agent::start(&secondary));
    world.publish(11);
    let deps = world.deps(true);
    let paths = broscribe_platform::paths::resolve_paths_from(deps.paths.as_ref(), |key| {
        (key == "BROSCRIBE_DATA_DIR").then(|| secondary.clone().into_os_string())
    })
    .unwrap();
    let run_secondary = |command: fn(&mut crate::Ctx<'_>) -> broscribe_core::Result<()>| {
        crate::Ctx::scoped(
            &deps,
            true,
            None,
            paths.clone(),
            None,
            &mut Vec::new(),
            &mut Vec::new(),
            command,
        )
    };
    run_secondary(super::download).unwrap();

    let library = world.root.join("default-library");
    std::fs::create_dir(&library).unwrap();
    let meeting = library.join("synthetic.md");
    std::fs::write(&meeting, "keep").unwrap();
    broscribe_core::config::Config::update(&deps.paths.data_dir(), |config| {
        config.library = Some(library);
        Ok(())
    })
    .unwrap();
    let error = run_secondary(|ctx| super::install(ctx, None)).unwrap_err();
    assert_eq!(error.code(), broscribe_updates::codes::LOCATION_UNSUPPORTED);
    assert_eq!(world.installed(), identity(10));
    assert_eq!(std::fs::read_to_string(meeting).unwrap(), "keep");
}

#[test]
fn env_selected_profile_rollback_protects_the_default_profiles_library() {
    use broscribe_platform::paths::{ENV_DATA_DIR, ENV_LIBRARY};

    const CHILD: &str = "BROSCRIBE_TEST_CROSS_PROFILE_ROLLBACK";
    if std::env::var_os(CHILD).is_none() {
        let temp = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env(CHILD, "1")
            .env(ENV_DATA_DIR, temp.path().join("secondary"))
            .env_remove(ENV_LIBRARY)
            .args([
                "--exact",
                "cmd::update::tests::env_selected_profile_rollback_protects_the_default_profiles_library",
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

    let secondary = PathBuf::from(std::env::var_os(ENV_DATA_DIR).unwrap());
    assert!(secondary.is_absolute());
    let mut world = World::new();
    let agent = Agent::start(&secondary);
    Config::update(&secondary, |config| {
        config.updates = world.settings();
        config.library = Some(agent.model().library.clone());
        Ok(())
    })
    .unwrap();
    world.agent = Some(agent);
    world.stage(11);
    let (exit, report) = world.run(&["update", "install", "--yes"]);
    assert_eq!(exit, 0, "{report}");
    assert_eq!(report["state"], "awaiting_confirmation", "{report}");
    let fence = report["fence"].as_u64().unwrap();
    assert_eq!(world.installed(), identity(11));

    // Protect a library only the default profile knows about. It must
    // fence rollback from the environment-selected profile.
    let library = world
        .staging()
        .join("previous/BroScribe.app/meeting-library");
    std::fs::create_dir(&library).unwrap();
    let meeting = library.join("keep.md");
    std::fs::write(&meeting, "synthetic meeting").unwrap();
    Config::update(&world.data_dir(), |config| {
        config.library = Some(library);
        Ok(())
    })
    .unwrap();
    let (exit, report) = world.run(&["update", "rollback", "--yes"]);
    assert_eq!(exit, 1, "{report}");
    assert_eq!(
        code(&report),
        broscribe_updates::codes::LOCATION_UNSUPPORTED
    );
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap()
            .contains("meeting-library")
    );
    assert_eq!(world.installed(), identity(11));
    assert_eq!(
        std::fs::read_to_string(meeting).unwrap(),
        "synthetic meeting"
    );
    world.assert_held(fence);
}

#[test]
fn in_place_update_refuses_a_library_the_service_restart_would_not_use() {
    // launchd and systemd both restart in place. Also exercise a Linux
    // direct agent that will be replaced by a service-manager start.
    for job in [None, Some(broscribe_platform::LoginJob::Absent)] {
        let mut world = World::new();
        world.enable();
        world.start_agent();
        world.stage(11);
        let old = world.tmp.path().join("old-live-library");
        world.agent().model().library = old;
        let configured = world.tmp.path().join("configured-library");
        Config::update(&world.data_dir(), |config| {
            config.library = Some(configured.clone());
            Ok(())
        })
        .unwrap();
        let mut deps = world.deps(true);
        deps.autostart = Box::new(AgentService {
            model: Arc::clone(&world.agent().model),
            registered: job.is_none(),
            start_fails: false,
            job_without_manager: job,
            platform: world.platform.clone(),
            root: world.root.clone(),
        });
        let (status, value) = world.run_with(&deps, &["update", "install", "--yes"]);
        assert_eq!(status, 1, "{value}");
        assert_eq!(code(&value), super::RESTART_LIBRARY_MISMATCH);
        assert_eq!(
            value["error"]["message"].as_str().unwrap(),
            format!(
                "Restart BroScribe Agent first so it uses {}, then update.",
                configured.display()
            )
        );
        assert!(world.agent().hold().is_none());
        assert_eq!(world.agent().model().acquires(), 0);
        assert!(world.platform.log().is_empty());
        assert_eq!(world.installed(), identity(10));
        world.agent().model().library = configured;
        assert_eq!(world.run_with(&deps, &["update", "install", "--yes"]).0, 0);
        assert_eq!(world.installed(), identity(11));
    }
}

#[test]
fn stopped_windows_run_library_is_protected_by_cli_and_rediscovered_by_runner() {
    use broscribe_platform::RegisteredProfile;
    struct Registry(RegisteredProfile);
    impl Autostart for Registry {
        fn is_enabled(&self) -> broscribe_core::Result<bool> {
            Ok(true)
        }
        fn set_enabled(&self, _: bool) -> broscribe_core::Result<()> {
            panic!("a Run entry must not change during update")
        }
        fn registered_profiles(&self) -> broscribe_core::Result<Vec<RegisteredProfile>> {
            Ok(vec![RegisteredProfile {
                data_dir: self.0.data_dir.clone(),
                library: self.0.library.clone(),
            }])
        }
        fn stop(&self) -> broscribe_core::Result<()> {
            panic!("the runner must refuse the overlap before stopping the agent")
        }
    }
    let mut world = World::after_exit();
    world.enable();
    world.start_agent();
    world.stage(11);
    let profile = world.tmp.path().join("stopped-profile");
    Config::update(&profile, |config| {
        config.library = Some(world.tmp.path().join("safe-configured-library"));
        Ok(())
    })
    .unwrap();
    let library = world.root.join("run-entry-library");
    std::fs::create_dir(&library).unwrap();
    let meeting = library.join("keep.md");
    std::fs::write(&meeting, "synthetic meeting").unwrap();
    let registry = || {
        Registry(RegisteredProfile {
            data_dir: profile.clone(),
            library: Some(library.clone()),
        })
    };
    let mut deps = world.deps(true);
    deps.autostart = Box::new(registry());
    let (status, value) = world.run_with(&deps, &["update", "install", "--yes"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), broscribe_updates::codes::LOCATION_UNSUPPORTED);
    assert!(world.agent().hold().is_none());

    // Simulate a registration made after the handoff snapshot. The runner must
    // discover it before it touches the installation or the running agent.
    let (status, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(status, 0, "{value}");
    let handoff = Handoff::load(&world.data_dir()).unwrap().unwrap();
    let report = run_handoff(
        &handoff,
        &RunnerDeps {
            platform: world.platform.as_ref(),
            autostart: &registry(),
            probe: &FakeProcessProbe::new(RUNNER_PID),
            agent_listening: &|_| Ok(true),
            timeout: Duration::from_millis(300),
            poll: Duration::from_millis(10),
        },
    )
    .unwrap();
    assert!(matches!(report.outcome, RunnerOutcome::Failed { code, .. }
        if code == broscribe_updates::codes::LOCATION_UNSUPPORTED));
    assert!(world.platform.log().is_empty());
    assert_eq!(
        std::fs::read_to_string(meeting).unwrap(),
        "synthetic meeting"
    );
}

#[test]
fn windows_secondary_uninstall_keeps_the_record_until_the_helper_receipt_succeeds() {
    use broscribe_platform::uninstall::{
        CredentialProfile, DeferredProfile, DesktopUninstall, Layout,
    };

    struct Helper {
        receipt: PathBuf,
        pending: PathBuf,
        launches: AtomicUsize,
    }
    impl DesktopUninstall for Helper {
        fn layout(&self, _: &Path) -> broscribe_core::Result<Layout> {
            Ok(Layout::default())
        }
        fn secret_store(&self, _: &Path) -> Arc<dyn broscribe_platform::SecretStore> {
            Arc::new(FakeSecretStore::default())
        }
        fn remove_credentials(
            &self,
            _: &[CredentialProfile],
            _: bool,
        ) -> broscribe_core::Result<usize> {
            Ok(0)
        }
        fn remove_installation(
            &self,
            _: &Path,
            _: &Path,
            _: &broscribe_platform::uninstall::ReleaseRemovalCheck<'_>,
        ) -> broscribe_core::Result<Option<PathBuf>> {
            unreachable!("this fixture keeps the app");
        }
        fn defers_settings_cleanup(&self) -> bool {
            true
        }
        fn remove_after_exit(
            &self,
            _: &Path,
            _: Option<&Path>,
            _: Option<&broscribe_platform::uninstall::ReleaseRemovalCheck<'_>>,
            profile: &DeferredProfile,
            before_launch: &mut dyn FnMut(&Path) -> broscribe_core::Result<PathBuf>,
        ) -> broscribe_core::Result<PathBuf> {
            before_launch(&self.receipt)?;
            let state: Value =
                serde_json::from_slice(&std::fs::read(&self.pending).unwrap()).unwrap();
            assert_eq!(state["profiles"][0]["data"], json!(profile.data_dir));
            assert_eq!(state["profiles"][0]["receipt"], json!(self.receipt));
            if self.launches.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(broscribe_core::Error::Unsupported(
                    "synthetic helper launch failure".into(),
                ));
            }
            let run = self.receipt.parent().unwrap();
            std::fs::create_dir(run)?;
            for name in ["uninstall-after-exit.ps1", "release-inventory.json"] {
                std::fs::write(run.join(name), "synthetic helper file")?;
            }
            std::fs::write(
                &self.receipt,
                "Waiting up to 15 minutes for BroScribe processes to exit.",
            )?;
            Ok(self.receipt.clone())
        }
    }

    let world = World::new();
    let default = broscribe_platform::stable_path(&world.data_dir());
    let secondary = broscribe_platform::stable_path(&world.tmp.path().join("secondary"));
    Config::default().save(&secondary).unwrap();
    broscribe_core::maintenance::ModeLease::acquire(&secondary, Duration::ZERO).unwrap();
    let pending = default.join("uninstall-pending.json");
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};
    let mut id = DefaultHasher::new();
    secondary.hash(&mut id);
    let run = broscribe_platform::stable_path(&std::env::temp_dir())
        .join(format!("broscribe-uninstall-{:032x}", id.finish()));
    let receipt = run.join("receipt.txt");
    let details = run.join("receipt.txt.details");
    let helper = Arc::new(Helper {
        receipt: receipt.clone(),
        pending: pending.clone(),
        launches: AtomicUsize::new(0),
    });
    let mut deps = world.deps(false);
    deps.agents.uninstall = helper.clone();
    let paths = broscribe_platform::paths::resolve_paths_from(deps.paths.as_ref(), |key| {
        (key == "BROSCRIBE_DATA_DIR").then(|| secondary.clone().into_os_string())
    })
    .unwrap();
    let uninstall = || {
        let mut out = Vec::new();
        let code = crate::Ctx::scoped(
            &deps,
            true,
            None,
            paths.clone(),
            None,
            &mut out,
            &mut Vec::new(),
            |ctx| {
                crate::cmd::uninstall::run(
                    ctx,
                    crate::cli::UninstallArgs {
                        yes: true,
                        from_settings: true,
                        keep_app: true,
                        dry_run: false,
                        remove_data: false,
                        remove_library: false,
                        discard_unsaved: false,
                        data_dir: Vec::new(),
                        all_profiles_secrets: false,
                        skip_credentials: false,
                        revoke_at_google: false,
                    },
                )
                .unwrap();
                ctx.exit_code.unwrap_or(0)
            },
        );
        (code, String::from_utf8(out).unwrap())
    };
    let (code, report) = uninstall();
    assert_eq!(code, 1, "{report}");
    assert!(report.contains("synthetic helper launch failure"));
    assert!(pending.is_file());
    let (code, report) = uninstall();
    assert_eq!(code, 0, "{report}");
    assert!(pending.is_file());
    let (code, report) = uninstall();
    assert_eq!(code, 1, "{report}");
    assert!(report.contains("still running"));
    assert_eq!(helper.launches.load(Ordering::SeqCst), 2);
    assert!(run.is_dir(), "a waiting helper must keep its files");
    // Simulate a helper that died before it could finish its receipt.
    std::fs::File::options()
        .write(true)
        .open(&receipt)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(17 * 60))
        .unwrap();
    let (code, report) = uninstall();
    assert_eq!(code, 0, "{report}");
    assert!(pending.is_file());
    assert_eq!(helper.launches.load(Ordering::SeqCst), 3);
    std::fs::write(&details, "synthetic completed profile cleanup").unwrap();
    std::fs::write(&receipt, "Done. Removed synthetic profile runtime.").unwrap();
    let (code, report) = uninstall();
    assert_eq!(code, 0, "{report}");
    assert!(report.contains("synthetic completed profile cleanup"));
    assert_eq!(helper.launches.load(Ordering::SeqCst), 3);
    assert!(!run.exists());
    assert!(!default.exists());
    assert!(secondary.join("config.toml").is_file());
}

#[test]
fn windows_cli_and_default_settings_uninstall_retire_interrupted_helper_runs_after_preflight() {
    use broscribe_platform::uninstall::{
        CredentialProfile, DeferredProfile, DesktopUninstall, Layout, ReleaseRemovalCheck,
    };
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};

    struct Helper {
        root: PathBuf,
        receipt: PathBuf,
        reference: PathBuf,
    }
    impl Helper {
        fn launch(
            &self,
            record: impl FnOnce(&Path) -> broscribe_core::Result<PathBuf>,
        ) -> broscribe_core::Result<PathBuf> {
            let run = self.receipt.parent().unwrap();
            std::fs::create_dir(run)?;
            for name in ["uninstall-after-exit.ps1", "release-inventory.json"] {
                std::fs::write(run.join(name), "synthetic helper file")?;
            }
            std::fs::write(
                &self.receipt,
                "Waiting up to 15 minutes for BroScribe processes to exit.",
            )?;
            assert!(broscribe_platform::same_path(
                &record(&self.receipt)?,
                &self.reference
            ));
            let saved: PathBuf = serde_json::from_slice(&std::fs::read(&self.reference)?).unwrap();
            assert!(broscribe_platform::same_path(&saved, &self.receipt));
            Ok(self.receipt.clone())
        }
    }
    impl DesktopUninstall for Helper {
        fn layout(&self, _: &Path) -> broscribe_core::Result<Layout> {
            Ok(Layout {
                installation: Some(self.root.clone()),
                ..Layout::default()
            })
        }
        fn secret_store(&self, _: &Path) -> Arc<dyn broscribe_platform::SecretStore> {
            Arc::new(FakeSecretStore::default())
        }
        fn remove_credentials(
            &self,
            _: &[CredentialProfile],
            _: bool,
        ) -> broscribe_core::Result<usize> {
            Ok(0)
        }
        fn remove_installation(
            &self,
            _: &Path,
            _: &Path,
            check: &ReleaseRemovalCheck<'_>,
        ) -> broscribe_core::Result<Option<PathBuf>> {
            self.launch(check.windows_before_launch).map(Some)
        }
        fn defers_settings_cleanup(&self) -> bool {
            true
        }
        fn remove_after_exit(
            &self,
            _: &Path,
            root: Option<&Path>,
            _: Option<&ReleaseRemovalCheck<'_>>,
            profile: &DeferredProfile,
            before_launch: &mut dyn FnMut(&Path) -> broscribe_core::Result<PathBuf>,
        ) -> broscribe_core::Result<PathBuf> {
            assert!(root.is_none());
            assert!(!profile.remove_data);
            self.launch(before_launch)
        }
    }

    for from_settings in [false, true] {
        let world = World::new();
        let data = world.data_dir();
        Config::default().save(&data).unwrap();
        let mut id = DefaultHasher::new();
        data.hash(&mut id);
        let run = broscribe_platform::stable_path(&std::env::temp_dir())
            .join(format!("broscribe-uninstall-{:032x}", id.finish()));
        let receipt = run.join("receipt.txt");
        let reference = data.with_extension("uninstall-helper.json");
        let mut deps = world.deps(false);
        deps.agents.uninstall = Arc::new(Helper {
            root: broscribe_platform::stable_path(&world.root),
            receipt: receipt.clone(),
            reference: reference.clone(),
        });
        let flags: &[_] = if from_settings {
            &["uninstall", "--yes", "--from-settings", "--keep-app"]
        } else {
            &["uninstall", "--yes", "--remove-data"]
        };
        let (code, report) = world.run_with(&deps, flags);
        assert_eq!(code, 0, "{report}");
        assert_eq!(data.exists(), from_settings, "{report}");
        assert!(reference.is_file() && run.is_dir());
        assert!(
            report["steps"].as_array().unwrap().iter().any(|step| {
                step["action"] == "Clear after-exit helper reference"
                    && step["status"] == "pending"
                    && step["path"].as_str() == reference.to_str()
            }),
            "{report}"
        );
        let (code, report) = world.run_with(&deps, &["uninstall", "--keep-app"]);
        assert_eq!(code, 2, "{report}");
        assert!(report.to_string().contains("still running"));

        std::fs::File::options()
            .write(true)
            .open(&receipt)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(17 * 60))
            .unwrap();
        let (code, report) = world.run_with(&deps, &["uninstall", "--keep-app"]);
        assert_eq!(code, 0, "{report}");
        assert!(reference.is_file() && run.is_dir());

        // Even an applying retry must leave the old run available when preflight fails.
        std::fs::create_dir_all(&data).unwrap();
        let journal = data.join("journal/synthetic-session");
        std::fs::create_dir_all(&journal).unwrap();
        std::fs::write(journal.join("session.json"), b"synthetic manifest").unwrap();
        let retry = &["uninstall", "--yes", "--keep-app"];
        let (code, report) = world.run_with(&deps, retry);
        assert_eq!(code, 1, "{report}");
        assert!(
            report
                .to_string()
                .contains("unsaved or processing recordings")
        );
        assert!(reference.is_file() && run.join("uninstall-after-exit.ps1").is_file());
        std::fs::remove_dir_all(journal).unwrap();
        let (code, report) = world.run_with(&deps, retry);
        assert_eq!(code, 0, "{report}");
        assert!(!reference.exists() && !run.exists());

        let foreign_run = world
            .tmp
            .path()
            .join("other-temp")
            .join(run.file_name().unwrap());
        let foreign_receipt = foreign_run.join("receipt.txt");
        std::fs::create_dir_all(&foreign_run).unwrap();
        std::fs::write(&foreign_receipt, b"Waiting in a different TEMP.").unwrap();
        let saved = serde_json::to_vec(&foreign_receipt).unwrap();
        std::fs::write(&reference, &saved).unwrap();
        for args in [&["uninstall", "--keep-app"][..], &retry[..]] {
            let (code, report) = world.run_with(&deps, args);
            assert_eq!(
                code,
                if args.contains(&"--yes") { 1 } else { 2 },
                "{report}"
            );
            let detail = helper_inspection_detail(&report);
            assert!(
                detail.contains("outside this user's temporary folder"),
                "{report}"
            );
            assert!(
                detail.contains(&foreign_run.display().to_string()),
                "{report}"
            );
            assert!(
                detail.contains(&format!("delete {} and retry", reference.display())),
                "{report}"
            );
            assert_eq!(std::fs::read(&reference).unwrap(), saved);
            assert!(foreign_receipt.is_file());
        }
        std::fs::remove_dir_all(&foreign_run).unwrap();
        let (code, report) = world.run_with(&deps, &["uninstall", "--keep-app"]);
        assert_eq!(code, 0, "{report}");
        assert!(
            report.to_string().contains("clears the obsolete reference"),
            "{report}"
        );
        assert_eq!(std::fs::read(&reference).unwrap(), saved);
        let (code, report) = world.run_with(&deps, retry);
        assert_eq!(code, 0, "{report}");
        assert!(
            report["steps"].as_array().unwrap().iter().any(|step| {
                step["action"] == "Clear obsolete after-exit helper reference"
                    && step["status"] == "done"
            }),
            "{report}"
        );
        assert!(!reference.exists());

        std::fs::create_dir(&reference).unwrap();
        let (code, report) = world.run_with(&deps, retry);
        assert_eq!(code, 1, "{report}");
        assert!(
            helper_inspection_detail(&report)
                .contains(&format!("delete {} and retry", reference.display())),
            "{report}"
        );
        assert!(reference.is_dir());
    }

    fn helper_inspection_detail(report: &Value) -> &str {
        report["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["action"] == "Inspect previous after-exit helper")
            .unwrap()["detail"]
            .as_str()
            .unwrap()
    }
}

#[test]
fn handed_off_update_restarts_with_the_admitted_library_and_requires_it_for_confirmation() {
    let mut world = World::after_exit();
    let fence = world.handed_off_install();
    let library = world.agent().model().library.clone();
    let (report, service) = world.run_runner(&FakeProcessProbe::new(RUNNER_PID));
    assert!(matches!(report.outcome, RunnerOutcome::Installed { .. }));
    assert_eq!(
        *service.restart_library.lock().unwrap(),
        Some(library.clone())
    );
    world.agent().model.lock().unwrap().library = world.tmp.path().join("wrong-library");
    let (status, value) = world.run(&["update", "confirm"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), super::AGENT_UNPROVEN);
    assert!(world.staging().is_dir());
    world.assert_held(fence);

    world.agent().model.lock().unwrap().library = library;
    let (status, value) = world.run(&["update", "confirm"]);
    assert_eq!(status, 0, "{value}");
}

#[test]
fn install_is_refused_while_recording_and_without_the_agent() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    world.agent().model().recording = true;
    let (status, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "recording_active");
    assert_eq!(world.installed(), identity(10));
    assert_eq!(world.agent().hold(), None);

    world.agent = None;
    let (status, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "daemon_unavailable");
    assert_eq!(world.installed(), identity(10));
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "staged", "the download is kept");
}

#[test]
fn confirmation_waits_for_a_restarted_agent() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    let mut failed_restart = world.deps(true);
    failed_restart.autostart = Box::new(AgentService {
        model: Arc::clone(&world.agent().model),
        registered: true,
        start_fails: true,
        job_without_manager: None,
        platform: world.platform.clone(),
        root: world.root.clone(),
    });
    let (status, value) = world.run_with(&failed_restart, &["update", "install", "--yes"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["agent"]["proven"], false);
    assert_eq!(value["agent"]["code"], "update_agent_unproven");

    let (status, value) = world.run_with(&failed_restart, &["update", "confirm"]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "update_agent_unproven");
    assert!(world.agent().hold().is_some_and(|(_, durable)| durable));

    // An agent that cannot tell its build proves nothing.
    world.agent().model().restart(None);
    let (status, value) = world.run_with(&failed_restart, &["update", "confirm"]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "update_agent_unproven");
    // Same version, previous build: still not the installed one.
    world.agent().model().restart(Some(10));
    let (status, value) = world.run_with(&failed_restart, &["update", "confirm"]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "update_agent_unproven");

    world.agent().model().restart(Some(11));
    let (status, value) = world.run_with(&failed_restart, &["update", "confirm"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(world.agent().hold(), None);
}

#[test]
fn rollback_restores_the_previous_build_on_the_same_admission() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    let (_, value) = world.run(&["update", "install", "--yes"]);
    let fence = value["fence"].as_u64().unwrap();
    let acquired = world.agent().model().acquires();

    let (status, value) = world.run(&["update", "rollback", "--yes"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "rolled_back");
    assert_eq!(value["restored"]["build"], 10);
    assert_eq!(world.installed(), identity(10));
    assert_eq!(
        world.agent().model().acquires(),
        acquired,
        "the held admission {fence} was reattached, never released and acquired again"
    );
    assert_eq!(
        world.agent().hold(),
        None,
        "released after the agent's proof"
    );
}

/// A rollback to a build of an older protocol: this newer command cannot
/// read the restored agent, so it says so at once instead of waiting out
/// the restart deadline, keeps the admission (nothing records) and leaves
/// the proof to the restored installation's own CLI. The restore ended the
/// update, but its admission still holds recording off, so the update
/// settings it runs under stay until recovery proved the restored agent.
#[test]
fn rollback_protects_a_new_live_library_inside_the_installed_tree() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    assert_eq!(world.run(&["update", "install", "--yes"]).0, 0);
    let library = world.root.join("new-library");
    std::fs::create_dir(&library).unwrap();
    let meeting = library.join("synthetic.md");
    std::fs::write(&meeting, "keep").unwrap();
    world.agent().model().library = library;

    let (status, value) = world.run(&["update", "rollback", "--yes"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "update_location_unsupported");
    assert_eq!(world.installed(), identity(11));
    assert_eq!(std::fs::read_to_string(meeting).unwrap(), "keep");
    assert!(world.staging().exists());
}

#[test]
fn handed_off_rollback_checks_current_config_and_both_protection_lists() {
    for missing_list in [Some(true), Some(false), None] {
        let mut world = World::after_exit();
        let fence = world.handed_off_install();
        let probe = FakeProcessProbe::new(RUNNER_PID);
        assert!(matches!(
            world.run_runner(&probe).0.outcome,
            RunnerOutcome::Installed { .. }
        ));
        assert_eq!(world.run(&["update", "rollback", "--yes"]).0, 0);
        let mut handoff = Handoff::load(&world.data_dir()).unwrap().unwrap();
        assert!(handoff.protected_paths.contains(&world.data_dir()));
        assert!(
            handoff
                .contained_paths
                .contains(&world.tmp.path().join("home"))
        );
        let library = world.staging().join("new-library");
        let meeting = library.join("synthetic.md");
        if let Some(protected) = missing_list {
            if protected {
                handoff.protected_paths.clear();
            } else {
                handoff.contained_paths.clear();
            }
            handoff.save().unwrap();
        } else {
            std::fs::create_dir(&library).unwrap();
            std::fs::write(&meeting, "keep").unwrap();
            broscribe_core::config::Config::update(&world.data_dir(), |config| {
                config.library = Some(library);
                Ok(())
            })
            .unwrap();
        }
        let (report, runner) = world.run_runner(&probe);
        assert_eq!(runner_failure(&report), "update_location_unsupported");
        assert_eq!(runner.stops.load(Ordering::SeqCst), 0);
        assert_eq!(world.installed(), identity(11));
        if missing_list.is_none() {
            assert_eq!(std::fs::read_to_string(meeting).unwrap(), "keep");
        }
        world.assert_held(fence);
    }
}

#[test]
fn install_runner_refuses_a_handoff_without_home_protection() {
    let mut world = World::after_exit();
    world.handed_off_install();
    let mut handoff = Handoff::load(&world.data_dir()).unwrap().unwrap();
    handoff.contained_paths.clear();
    handoff.save().unwrap();
    let (report, runner) = world.run_runner(&FakeProcessProbe::new(RUNNER_PID));
    assert_eq!(runner_failure(&report), "update_location_unsupported");
    assert_eq!(runner.stops.load(Ordering::SeqCst), 0);
    assert_eq!(world.installed(), identity(10));
}

#[test]
fn a_rollback_to_another_protocol_keeps_the_admission_for_the_restored_cli() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    let (status, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(status, 0, "{value}");
    let fence = value["fence"].as_u64().unwrap();
    // Build 10, restored next, speaks the previous protocol.
    world.agent().model().restarts_speaking = Some(broscribe_ipc::PROTOCOL_VERSION - 1);

    let (status, value) = world.run(&["update", "rollback", "--yes"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "update_agent_unproven");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("restored installation's own `broscribe update recover`"),
        "{message}"
    );
    assert_eq!(
        world.installed(),
        identity(10),
        "the previous build is back"
    );
    assert_eq!(
        world.agent().model().refused,
        1,
        "the restored agent was asked once, not polled until the deadline"
    );
    assert_eq!(world.agent().hold(), Some((fence, true)), "still held");
    assert_eq!(
        world.agent().model().start_capture(),
        Err(IpcError::INSTALLING)
    );
    let (revision, _) = world.shown_settings();
    let off = UpdateSettings {
        enabled: false,
        ..world.settings()
    };
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "idle", "the restore ended it");
    world.assert_settings_kept(
        std::slice::from_ref(&off),
        revision,
        "update_pending",
        fence,
    );

    // Nor can this command end the admission: it cannot read that agent.
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "helper_outdated");
    assert_eq!(world.agent().hold(), Some((fence, true)));
    world.assert_settings_kept(
        std::slice::from_ref(&off),
        revision,
        "update_pending",
        fence,
    );

    // A command of the restored agent's protocol (here: this agent speaking
    // this command's again) proves the restored build as a new process and
    // only then ends the admission.
    world.agent().model().speaks = broscribe_ipc::PROTOCOL_VERSION;
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "clean");
    assert_eq!(value["admission"]["released"], fence);
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());

    let (status, value) = set_settings(&world.deps(true), &off, Some(revision));
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["revision"], revision + 1);
    assert_eq!(value["updates"], settings_value(&off));
}

#[test]
fn a_crash_mid_replacement_keeps_capture_off_until_recovered() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    world
        .platform
        .set_fault(Some(FakeFault::CrashBeforeChange(0)));
    let deps = world.deps(true);
    let crashed = catch_unwind(AssertUnwindSafe(|| {
        world.run_with(&deps, &["update", "install", "--yes"])
    }));
    assert!(crashed.is_err(), "the simulated crash unwinds");
    world.platform.set_fault(None);

    let (fence, durable) = world
        .agent()
        .hold()
        .expect("the admission outlives the crash");
    assert!(durable);
    assert_eq!(
        world.agent().model().start_capture(),
        Err(IpcError::INSTALLING)
    );
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "interrupted");
    let acquired = world.agent().model().acquires();

    let library = world.staging().join("new-library");
    std::fs::create_dir(&library).unwrap();
    let meeting = library.join("synthetic.md");
    std::fs::write(&meeting, "keep").unwrap();
    let original_library = std::mem::replace(&mut world.agent().model().library, library);
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "update_location_unsupported");
    assert_eq!(std::fs::read_to_string(&meeting).unwrap(), "keep");
    world.assert_held(fence);
    world.agent().model().library = original_library;
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), broscribe_updates::codes::RELEASE_TREE_UNSAFE);
    assert!(meeting.is_file());
    std::fs::rename(
        meeting.parent().unwrap(),
        world.tmp.path().join("kept-library"),
    )
    .unwrap();

    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "rolled_back");
    assert_eq!(world.installed(), identity(10));
    assert_eq!(
        world.agent().model().acquires(),
        acquired,
        "fence {fence} reattached"
    );
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());
}

#[test]
fn interrupted_after_exit_recovery_hands_off_profile_and_home_protection() {
    const CHILD: &str = "BROSCRIBE_TEST_RECOVER_CALLER_CWD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env(CHILD, "1")
            .args([
                "--exact",
                "cmd::update::tests::interrupted_after_exit_recovery_hands_off_profile_and_home_protection",
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

    let mut world = World::after_exit();
    let fence = world.handed_off_install();
    let probe = FakeProcessProbe::new(RUNNER_PID);
    world
        .platform
        .set_fault(Some(FakeFault::CrashBeforeChange(0)));
    let crashed = catch_unwind(AssertUnwindSafe(|| world.run_runner(&probe)));
    assert!(crashed.is_err());
    world.platform.set_fault(None);

    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(&world.root).unwrap();
    let (status, value) = world.run(&["update", "recover"]);
    std::env::set_current_dir(original_dir).unwrap();
    assert_eq!(status, 1, "{value}");
    assert_working_directory_refusal(
        &value,
        &world.data_dir(),
        &world.deps(true).paths.default_library_dir(),
    );
    assert!(Handoff::load(&world.data_dir()).unwrap().is_none());
    world.assert_held(fence);

    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "handed_off");
    let handoff = Handoff::load(&world.data_dir()).unwrap().unwrap();
    assert!(handoff.protected_paths.contains(&world.data_dir()));
    assert!(
        handoff
            .contained_paths
            .contains(&world.tmp.path().join("home"))
    );
    let library = world.staging().join("new-library");
    std::fs::create_dir(&library).unwrap();
    let meeting = library.join("synthetic.md");
    std::fs::write(&meeting, "keep").unwrap();
    broscribe_core::config::Config::update(&world.data_dir(), |config| {
        config.library = Some(library);
        Ok(())
    })
    .unwrap();

    let (report, _) = world.run_runner(&probe);
    assert_eq!(runner_failure(&report), "update_location_unsupported");
    assert_eq!(world.installed(), identity(10));
    assert_eq!(std::fs::read_to_string(meeting).unwrap(), "keep");
    world.assert_held(fence);
}

#[test]
fn recovery_ends_an_admission_left_after_a_finished_step() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    let (_, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(value["state"], "awaiting_confirmation");
    // Awaiting confirmation: recovery keeps the admission.
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "awaiting_confirmation");
    assert!(world.agent().hold().is_some());
    let (status, _) = world.run(&["update", "confirm"]);
    assert_eq!(status, 0);
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "clean");
}

/// A handed-off install whose runner stopped the agent and then gave up
/// waiting for the app still running from the installation: nothing was
/// replaced and the agent runs build 10 again under the durable admission.
/// Recovery with the same data folder discards the stage and ends exactly
/// that admission once the restarted agent proved build 10.
#[test]
fn a_handoff_that_gave_up_on_the_exit_barrier_recovers_onto_the_unchanged_build() {
    let mut world = World::after_exit();
    let fence = world.handed_off_install();
    world.assert_held(fence);

    // While the runner may still run, recovery refuses and changes nothing.
    let mut deps = world.deps(true);
    let probe = FakeProcessProbe::new(std::process::id());
    probe.set_alive(RUNNER_PID, true);
    deps.process = Box::new(probe);
    let (status, value) = world.run_with(&deps, &["update", "recover"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), broscribe_updates::codes::BUSY);
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "staged");
    assert_eq!(value["handed_off"]["kind"], "install");
    world.assert_held(fence);

    world.platform.set_processes(vec![InstallationProcess {
        pid: 4_100,
        image: world.root.join("bin").join("broscribe"),
    }]);
    let (report, runner) = world.run_runner(&FakeProcessProbe::new(RUNNER_PID));
    assert_eq!(runner_failure(&report), "update_runner_timeout");
    assert_eq!(runner.stops.load(Ordering::SeqCst), 1, "stopped first");
    assert_eq!(world.installed(), identity(10), "nothing was replaced");
    let restarted = world.agent().model().pid;
    assert_ne!(restarted, FIRST_AGENT_PID, "the agent runs again");
    assert_eq!(world.agent().model().build, Some(10));
    world.assert_held(fence);

    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "discarded_staged");
    assert_eq!(value["admission"]["released"], fence);
    assert_eq!(world.installed(), identity(10));
    assert!(!world.staging().exists(), "the stage is removed");
    assert_eq!(
        world.agent().model().pid,
        restarted,
        "the runner's restart already proved a new process"
    );
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "idle");
    assert!(value["runner"].is_null(), "{value}");
}

/// After the same failed handoff, the admission stays held while the agent
/// holds another fence, cannot tell its build, or the installation is
/// neither the build the step started from nor the one it was to install;
/// discarding the stage on the way loses nothing recovery needs. The update
/// settings recovery runs under stay until it ended the admission, even
/// with the stage gone, and an unreadable step record keeps them too.
#[test]
fn a_failed_handoff_keeps_the_admission_until_its_own_fence_and_build_are_proven() {
    let mut world = World::after_exit();
    let fence = world.handed_off_install();
    let (revision, _) = world.shown_settings();
    let changes = [
        UpdateSettings {
            enabled: false,
            ..world.settings()
        },
        UpdateSettings {
            signer: Some("TEAMID9999".into()),
            ..world.settings()
        },
    ];
    world.platform.set_processes(vec![InstallationProcess {
        pid: 4_100,
        image: world.root.join("bin").join("broscribe"),
    }]);
    let (report, _) = world.run_runner(&FakeProcessProbe::new(RUNNER_PID));
    assert_eq!(runner_failure(&report), "update_runner_timeout");
    world.platform.set_processes(Vec::new());

    // Another owner's fence: never ended here.
    world.agent().model().held = Some((fence + 1, true, 0));
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "busy");
    world.assert_held(fence + 1);
    world.agent().model().held = Some((fence, true, 0));

    // An agent that cannot tell its build proves nothing.
    world.agent().model().build = None;
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "update_agent_unproven");
    world.assert_held(fence);
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "idle", "the stage was discarded");
    world.assert_settings_kept(&changes, revision, "update_pending", fence);

    // A step record that cannot be read proves nothing either way.
    let record = state_dir(&world.data_dir()).join("install-record.json");
    let kept = std::fs::read(&record).unwrap();
    std::fs::write(&record, b"{\"schema\":").unwrap();
    world.assert_settings_kept(&changes, revision, "schema", fence);
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "schema");
    world.assert_held(fence);
    std::fs::write(&record, kept).unwrap();
    world.agent().model().build = Some(10);

    // Neither build 10 nor build 11 is installed.
    FakeUpdatePlatform::write_payload(&world.root, &identity(12), SIGNER).unwrap();
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "update_agent_unproven");
    world.assert_held(fence);
    FakeUpdatePlatform::write_payload(&world.root, &identity(10), SIGNER).unwrap();

    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "idle", "the stage was discarded");
    world.assert_settings_kept(&changes, revision, "update_pending", fence);
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "clean");
    assert_eq!(value["admission"]["released"], fence);
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());

    let (status, value) = set_settings(&world.deps(true), &changes[0], Some(revision));
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["revision"], revision + 1);
    assert_eq!(value["updates"], settings_value(&changes[0]));
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "off");
}

/// The command never exited, so the runner gave up before stopping the
/// agent: the agent that ran before the step still runs. Recovery restarts
/// it under the held admission and ends the
/// admission once the new process proved build 10.
#[test]
fn a_handoff_that_failed_before_stopping_the_agent_recovers_after_a_restart() {
    let mut world = World::after_exit();
    let fence = world.handed_off_install();
    let probe = FakeProcessProbe::new(RUNNER_PID);
    probe.set_alive(std::process::id(), true);
    let (report, runner) = world.run_runner(&probe);
    assert_eq!(runner_failure(&report), "update_runner_timeout");
    assert_eq!(runner.stops.load(Ordering::SeqCst), 0);
    assert_eq!(world.agent().model().pid, FIRST_AGENT_PID);
    world.assert_held(fence);

    let (status, value) = world.run_with(&world.deps(false), &["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "discarded_staged");
    assert_eq!(value["admission"]["released"], fence);
    assert_ne!(world.agent().model().pid, FIRST_AGENT_PID);
    assert_eq!(world.installed(), identity(10));
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());
}

fn recovery_stops_an_agent_without_a_manager(job: broscribe_platform::LoginJob) {
    let mut world = World::after_exit();
    let fence = world.handed_off_install();
    let process = FakeProcessProbe::new(RUNNER_PID);
    process.set_alive(std::process::id(), true);
    let (report, _) = world.run_runner(&process);
    assert_eq!(runner_failure(&report), "update_runner_timeout");
    let mut deps = world.deps(true);
    deps.autostart = Box::new(AgentService {
        model: Arc::clone(&world.agent().model),
        registered: !matches!(job, broscribe_platform::LoginJob::Absent),
        start_fails: false,
        job_without_manager: Some(job),
        platform: world.platform.clone(),
        root: world.root.clone(),
    });

    let (status, value) = world.run_with(&deps, &["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["admission"]["released"], fence);
    let model = world.agent().model();
    assert_ne!(model.pid, FIRST_AGENT_PID);
    assert!(model.calls.contains(&Call::Shutdown));
    assert!(model.start_capture().is_ok());
}

#[test]
fn recovery_stops_the_admitted_endpoint_for_a_windows_run_entry() {
    recovery_stops_an_agent_without_a_manager(broscribe_platform::LoginJob::Registered {
        serves_profile: Some(true),
    });
}

#[test]
fn recovery_stops_only_the_secondary_windows_profiles_admitted_endpoint() {
    recovery_stops_an_agent_without_a_manager(broscribe_platform::LoginJob::Registered {
        serves_profile: Some(false),
    });
}

#[test]
fn recovery_stops_the_admitted_endpoint_when_the_login_job_is_absent() {
    recovery_stops_an_agent_without_a_manager(broscribe_platform::LoginJob::Absent);
}

#[test]
fn recovery_stops_the_admitted_endpoint_when_the_login_job_is_unsupported() {
    recovery_stops_an_agent_without_a_manager(broscribe_platform::LoginJob::Unsupported);
}

/// The handed-off install failed after its first change (the slot for the
/// replaced build is taken) and the updater put build 10 back and cleared
/// its journal: recovery has no update state left and still proves and
/// releases exactly the step's admission.
#[test]
fn a_handed_off_install_the_updater_undid_recovers_onto_the_restored_build() {
    let mut world = World::after_exit();
    let fence = world.handed_off_install();
    FakeUpdatePlatform::write_payload(
        &world.staging().join("previous/BroScribe.app"),
        &identity(10),
        SIGNER,
    )
    .unwrap();
    let (report, _) = world.run_runner(&FakeProcessProbe::new(RUNNER_PID));
    assert_eq!(runner_failure(&report), "already_exists");
    assert_eq!(world.installed(), identity(10));
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "idle");
    assert_ne!(world.agent().model().pid, FIRST_AGENT_PID);
    world.assert_held(fence);

    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "clean");
    assert_eq!(value["admission"]["released"], fence);
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());
}

#[test]
fn discard_removes_the_download_and_release_keys_show_public_halves_only() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.stage(11);
    let (status, value) = world.run(&["update", "discard"]);
    assert_eq!(status, 0, "{value}");
    assert!(!world.staging().exists());

    let (status, value) = world.run(&["update", "release-key", "show", "--label", "test-release"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["public_key"], world.public_key);
    let stored: HashMap<String, String> = world.secrets.secrets.lock().unwrap().clone();
    for secret in stored.values() {
        assert!(
            !value.to_string().contains(secret.as_str()),
            "no private key material in the output"
        );
    }
    let (status, value) = world.run(&[
        "update",
        "release-key",
        "generate",
        "--label",
        "test-release",
    ]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "update_signing_key_exists");
}

/// The manual upgrade's admission: durable before the command exits,
/// never ended by `update recover`, and released only once the agent runs
/// exactly the installation in place, as a new process when it changed.
#[test]
fn a_manual_upgrade_admission_holds_until_the_replaced_agent_proves_itself() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.agent().model().recording = true;
    let (status, value) = world.run(&["update", "admission", "acquire"]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "recording_active");
    assert_eq!(world.agent().hold(), None);
    world.agent().model().recording = false;

    let (status, value) = world.run(&["update", "admission", "acquire"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "held");
    assert_eq!(value["durable"], true);
    assert_eq!(
        value["agent"],
        json!({ "pid": 4242, "version": VERSION, "build": 10, "target": TARGET })
    );
    assert_eq!(value["installed"], json!(identity(10)));
    let fence = value["fence"].as_u64().unwrap();
    assert_eq!(world.agent().hold(), Some((fence, true)), "durable at exit");
    assert_eq!(
        world.agent().model().start_capture(),
        Err(IpcError::INSTALLING)
    );
    let (status, value) = world.run(&["update", "admission", "acquire"]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "busy", "never reattached");
    let (status, value) = world.run(&["update", "recover"]);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "busy", "{value}");
    assert_eq!(world.agent().hold(), Some((fence, true)));

    // The bundle is replaced while the agent is out of service.
    std::fs::remove_dir_all(&world.root).unwrap();
    FakeUpdatePlatform::write_payload(&world.root, &identity(11), SIGNER).unwrap();
    let fence_arg = fence.to_string();
    let release = [
        "update",
        "admission",
        "release",
        "--fence",
        fence_arg.as_str(),
    ];
    // The old process still runs the old build: nothing is proven.
    let (status, value) = world.run(&release);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "update_agent_unproven");
    // A new process that still runs the old build.
    world.agent().model().restart(Some(10));
    let (status, value) = world.run(&release);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "update_agent_unproven");
    assert_eq!(world.agent().hold(), Some((fence, true)));

    world.agent().model().restart(Some(11));
    let pid = world.agent().model().pid;
    let (status, value) = world.run(&release);
    assert_eq!(status, 0, "{value}");
    assert_eq!(
        value,
        json!({ "state": "released", "fence": fence,
                "agent": { "pid": pid, "version": VERSION, "build": 11, "target": TARGET } })
    );
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());
    let (status, value) = world.run(&release);
    assert_eq!(status, 1);
    assert_eq!(code(&value), "update_admission_lost", "released once");
}

/// The app's toggle turns updates off (naming the revision it showed)
/// while a download and an install race it after it found nothing
/// pending: both are refused until the change is saved, so nothing is
/// fetched, staged, admitted or installed under the settings being turned
/// off, and no update is left pending behind settings that are off.
#[test]
fn turning_updates_off_holds_the_update_state_until_it_is_saved() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.publish(11);
    let (revision, _) = world.shown_settings();
    let off = UpdateSettings {
        enabled: false,
        ..world.settings()
    };

    let deps = world.deps(true);
    let answers = race_at(Pause::SettingsChecked, move || {
        vec![
            run_cli(&deps, &["update", "download"]),
            run_cli(&deps, &["update", "install", "--yes"]),
        ]
    });
    let (status, value) = set_settings(&world.deps(true), &off, Some(revision));
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["revision"], revision + 1);
    assert_eq!(value["updates"], settings_value(&off));

    let answers = answers.borrow();
    assert_eq!(answers.len(), 2, "both raced the settings change");
    for (status, value) in answers.iter() {
        assert_eq!(*status, 1, "{value}");
        assert_eq!(code(value), "update_busy", "{value}");
    }
    assert!(world.transport.requests().is_empty(), "nothing was fetched");
    assert!(!world.staging().exists(), "nothing was staged");
    assert_eq!(world.installed(), identity(10));
    assert_eq!(world.agent().hold(), None, "no admission was taken");
    assert!(world.agent().model().start_capture().is_ok());
    let (_, value) = world.run(&["update", "status"]);
    assert_eq!(value["update"]["state"], "off");
}

/// A download that read the enabled settings before another process
/// turned them off, or pinned another signer, gets the updater's state
/// lock only after that change was saved: it refuses with `config_stale`
/// before fetching anything, so nothing is staged under the settings that
/// were replaced, and the new settings stay saved.
#[test]
fn a_download_that_read_replaced_settings_stages_nothing() {
    let replacements: [fn(&World) -> UpdateSettings; 2] = [
        |world| UpdateSettings {
            enabled: false,
            ..world.settings()
        },
        |world| UpdateSettings {
            signer: Some("TEAMID9999".into()),
            ..world.settings()
        },
    ];
    for replace in replacements {
        let mut world = World::new();
        world.enable();
        world.start_agent();
        world.publish(11);
        let (revision, _) = world.shown_settings();
        let replacement = replace(&world);

        let deps = world.deps(true);
        let competing = replacement.clone();
        let answers = race_at(Pause::BeforeLock, move || {
            vec![set_settings(&deps, &competing, Some(revision))]
        });
        let (status, value) = world.run(&["update", "download"]);
        let answers = answers.borrow();
        assert_eq!(answers.len(), 1, "the settings change raced the download");
        assert_eq!(answers[0].0, 0, "{}", answers[0].1);
        assert_eq!(status, 1, "{replacement:?}: {value}");
        assert_eq!(code(&value), "config_stale", "{value}");
        assert!(
            world.transport.requests().is_empty(),
            "nothing was fetched under the replaced settings"
        );
        assert!(!world.staging().exists(), "nothing was staged");
        assert_eq!(
            world.shown_settings(),
            (revision + 1, settings_value(&replacement))
        );
    }
}

/// Settings changes racing a download that holds the updater's state are
/// refused (`update_busy`), and while its update is staged or awaits
/// confirmation every change is refused (`update_pending`): the update
/// keeps exactly the settings it was checked with, its install and
/// confirmation run under them and capture resumes; afterwards the change
/// is saved at the revision the app showed.
#[test]
fn a_pending_update_keeps_its_settings_until_it_is_confirmed() {
    let mut world = World::new();
    world.enable();
    world.start_agent();
    world.publish(11);
    let original = world.settings();
    let (revision, shown) = world.shown_settings();
    assert_eq!(shown, settings_value(&original));
    let changes = [
        UpdateSettings {
            enabled: false,
            ..original.clone()
        },
        UpdateSettings {
            signer: Some("TEAMID9999".into()),
            ..original.clone()
        },
        UpdateSettings {
            feed_url: Some("https://updates.example/other.json".into()),
            ..original.clone()
        },
    ];

    let deps = world.deps(true);
    let competing = changes.clone();
    let answers = race_at(Pause::Locked, move || {
        competing
            .iter()
            .map(|change| set_settings(&deps, change, Some(revision)))
            .collect()
    });
    let (status, value) = world.run(&["update", "download"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "staged");
    let answers = answers.borrow();
    assert_eq!(
        answers.len(),
        changes.len(),
        "every change raced the download"
    );
    for (status, value) in answers.iter() {
        assert_eq!(*status, 1, "{value}");
        assert_eq!(code(value), "update_busy", "{value}");
    }

    let refused = |world: &World| {
        for change in &changes {
            let (status, value) = set_settings(&world.deps(true), change, Some(revision));
            assert_eq!(status, 1, "{change:?}: {value}");
            assert_eq!(code(&value), "update_pending", "{value}");
        }
        assert_eq!(
            world.shown_settings(),
            (revision, settings_value(&original))
        );
    };
    refused(&world);
    let (status, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "awaiting_confirmation");
    let fence = value["fence"].as_u64().unwrap();
    world.assert_held(fence);
    refused(&world);

    let (status, value) = world.run(&["update", "confirm"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "confirmed");
    assert_eq!(world.installed(), identity(11));
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());

    let (status, value) = set_settings(&world.deps(true), &changes[0], Some(revision));
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["revision"], revision + 1);
    assert_eq!(value["updates"], settings_value(&changes[0]));
}

/// A settings change that read updates off is refused when it would save
/// over enabled settings another process saved meanwhile, under which a
/// download then staged: the staged update keeps the settings it was
/// checked with and installs and confirms under them.
#[test]
fn a_settings_change_read_before_another_is_refused_at_its_save() {
    let mut world = World::new();
    world.start_agent();
    world.publish(11);
    let (status, value) = world.run(&["update", "settings", "set", "--enabled", "false"]);
    assert_eq!(status, 0, "{value}");
    let original = world.settings();
    let (revision, _) = world.shown_settings();
    let other = UpdateSettings {
        signer: Some("TEAMID9999".into()),
        ..original.clone()
    };

    let deps = world.deps(true);
    let enable = original.clone();
    let answers = race_at(Pause::SettingsChecked, move || {
        vec![
            set_settings(&deps, &enable, None),
            run_cli(&deps, &["update", "download"]),
        ]
    });
    let (status, value) = set_settings(&world.deps(true), &other, None);
    {
        let answers = answers.borrow();
        assert_eq!(answers.len(), 2, "both raced the settings change");
        assert_eq!(answers[0].0, 0, "{}", answers[0].1);
        assert_eq!(answers[1].0, 0, "{}", answers[1].1);
        assert_eq!(answers[1].1["state"], "staged");
    }
    assert_eq!(status, 1, "{value}");
    assert_eq!(code(&value), "config_stale", "{value}");
    assert_eq!(
        world.shown_settings(),
        (revision + 1, settings_value(&original))
    );

    let (status, value) = world.run(&["update", "install", "--yes"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "awaiting_confirmation");
    let (status, value) = world.run(&["update", "confirm"]);
    assert_eq!(status, 0, "{value}");
    assert_eq!(value["state"], "confirmed");
    assert_eq!(world.installed(), identity(11));
    assert_eq!(world.agent().hold(), None);
    assert!(world.agent().model().start_capture().is_ok());
}

#[cfg(unix)]
#[test]
fn linux_stages_a_file_only_tarball_under_umask_002() {
    use broscribe_platform::HostPlatform;
    use broscribe_platform_native::NativePlatform;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    if NativePlatform::host_platform() != HostPlatform::Linux {
        return;
    }
    const CHILD: &str = "BROSCRIBE_TEST_UPDATER_UMASK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Give the umask to a dedicated test process, so there is nothing to
        // restore in the parent and concurrent tests keep their usual modes.
        let output = Command::new("sh")
            .args(["-c", "umask 002; exec \"$@\"", "sh"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cmd::update::tests::linux_stages_a_file_only_tarball_under_umask_002",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}"
        );
        return;
    }
    let world = World::new();
    // The native updater excludes the OS temporary folder as an installation location.
    let fixture = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let root = fixture.path().join("installed");
    let incoming = fixture.path().join("BroScribe");
    let target = format!("{}-unknown-linux-gnu", std::env::consts::ARCH);
    let info = |build: u64| {
        json!({"schema": 1, "product": "BroScribe", "version": VERSION,
               "build": build, "target": target})
        .to_string()
    };
    // The Linux check wants an ELF for this machine: borrow a small system one.
    let executable = ["/bin/true", "/usr/bin/true"]
        .iter()
        .find_map(|path| std::fs::read(path).ok())
        .expect("a native `true` executable");
    for (path, build) in [(&root, 10), (&incoming, 11)] {
        std::fs::create_dir_all(path.join("bin")).unwrap();
        std::fs::write(path.join("build-info.json"), info(build)).unwrap();
        let cli = path.join("bin/broscribe");
        std::fs::write(&cli, &executable).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let platform = NativePlatform::updater_for(root.join("bin/broscribe"));
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&root), 0o775);
    let error = platform.installation().unwrap_err();
    assert_eq!(error.code(), "update_location_unsupported");
    assert!(error.to_string().contains("chmod -R go-w <folder>"));
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(root.join("bin"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let archive = Command::new("tar")
        .current_dir(fixture.path())
        .args([
            "--format=ustar",
            "-czf",
            "-",
            "BroScribe/build-info.json",
            "BroScribe/bin/broscribe",
        ])
        .output()
        .unwrap();
    assert!(archive.status.success(), "{:?}", archive.stderr);
    let bytes = archive.stdout;
    let now = Utc::now();
    let manifest = json!({
        "format": MANIFEST_FORMAT, "schema": 1, "product": "BroScribe", "channel": "stable",
        "version": VERSION, "build": 11,
        "published_at": (now - Span::hours(1)).to_rfc3339(),
        "expires_at": (now + Span::days(30)).to_rfc3339(),
        "artifacts": [{
            "target": target, "app_id": "BroScribe", "url": ARTIFACT.replace(".zip", ".tar.gz"),
            "size": bytes.len(),
            "sha256": Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "archive": "tar.gz", "root": "BroScribe", "entries": 2,
            "installed_size": info(11).len() + executable.len(),
        }],
    });
    let signed = sign_manifest(
        world.secrets.as_ref(),
        &[ReleaseKeyLabel::parse("test-release").unwrap()],
        &manifest.to_string(),
        now,
        &Cancel::new(),
    )
    .unwrap();
    world.transport.serve(FEED, signed.envelope.into_bytes());
    world
        .transport
        .serve(&ARTIFACT.replace(".zip", ".tar.gz"), bytes);
    let settings = UpdateSettings {
        enabled: true,
        feed_url: Some(FEED.into()),
        trusted_keys: vec![world.public_key.clone()],
        ..Default::default()
    };
    let mut updater = broscribe_updates::Updater::open(
        &fixture.path().join("state"),
        &settings,
        platform.as_ref(),
        &world.transport,
    )
    .unwrap();
    let broscribe_updates::CheckOutcome::Available(offer) =
        updater.check(now, &Cancel::new()).unwrap()
    else {
        panic!("expected a Linux update offer");
    };
    updater
        .download(&offer, &Cancel::new(), &mut |_| {})
        .unwrap();
    let staged = fixture.path().join(".installed.update/incoming/BroScribe");
    assert_eq!(mode(&staged), 0o755);
    assert_eq!(mode(&staged.join("bin")), 0o755);
}

#[cfg(unix)]
#[test]
fn shadowed_environment_library_blocks_an_update_even_with_a_flag() {
    const CHILD: &str = "BROSCRIBE_TEST_SHADOWED_UPDATE";
    if std::env::var_os(CHILD).is_none() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "cmd::update::tests::shadowed_environment_library_blocks_an_update_even_with_a_flag"])
            .env(CHILD, root.path().join("alias"))
            .env("BROSCRIBE_LIBRARY", root.path().join("alias"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let world = World::new();
    let alias = std::env::var_os(CHILD).unwrap();
    std::os::unix::fs::symlink(&world.root, alias).unwrap();
    world.enable();
    world.publish(11);
    let library = world.data_dir().with_file_name("flag-library");
    let (code, report) = world.run(&["--library", library.to_str().unwrap(), "update", "download"]);
    assert_ne!(code, 0, "{report}");
    assert_eq!(
        report["error"]["code"],
        broscribe_updates::codes::LOCATION_UNSUPPORTED
    );
}

#[test]
fn after_exit_runner_resolves_protected_paths_from_the_cli_working_directory() {
    let mut world = World::after_exit();
    let fence = world.handed_off_install();
    let mut handoff = Handoff::load(&world.data_dir()).unwrap().unwrap();
    handoff.config_base = world.tmp.path().to_path_buf();
    handoff.protected_paths.push(PathBuf::from("Applications"));
    handoff.save().unwrap();
    let (report, runner) = world.run_runner(&FakeProcessProbe::new(RUNNER_PID));
    assert_eq!(
        runner_failure(&report),
        broscribe_updates::codes::LOCATION_UNSUPPORTED
    );
    assert_eq!(runner.stops.load(Ordering::SeqCst), 0);
    assert_eq!(world.installed(), identity(10));
    world.assert_held(fence);
}

#[cfg(unix)]
#[test]
fn copying_the_runner_replaces_the_leaf_without_overwriting_its_target() {
    use super::copy_runner;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("bundled-runner");
    let sentinel = temp.path().join("outside-runner");
    let runner = temp.path().join("update-runner/broscribe-update-runner");
    std::fs::write(&source, b"new runner").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&sentinel, b"keep me").unwrap();
    broscribe_updates::ensure_update_directory(runner.parent().unwrap()).unwrap();
    symlink(&sentinel, &runner).unwrap();
    let staging = runner.with_file_name(".broscribe-update-runner.123.tmp");
    let foreign = runner.with_file_name(".broscribe-update-runner.notes.tmp");
    let above_pid_range = runner.with_file_name(".broscribe-update-runner.4294967296.tmp");
    std::fs::write(&staging, b"stale partial copy").unwrap();
    std::fs::write(&foreign, b"keep this too").unwrap();
    std::fs::write(&above_pid_range, b"keep this PID-like file").unwrap();

    copy_runner(&source, &runner).unwrap();
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep me");
    let meta = std::fs::symlink_metadata(&runner).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.permissions().mode() & 0o777, 0o755);
    assert_eq!(std::fs::read(&runner).unwrap(), b"new runner");
    assert!(!staging.exists());
    assert_eq!(std::fs::read(&foreign).unwrap(), b"keep this too");
    assert_eq!(
        std::fs::read(&above_pid_range).unwrap(),
        b"keep this PID-like file"
    );

    std::fs::remove_file(&runner).unwrap();
    std::fs::create_dir(&runner).unwrap();
    assert!(copy_runner(&source, &runner).is_err());
    assert!(runner.is_dir());
    assert_eq!(
        std::fs::read_dir(runner.parent().unwrap()).unwrap().count(),
        3
    );
}

#[test]
fn orphan_journal_blocks_uninstall_and_blocked_preview_still_emits_its_report() {
    let world = World::new();
    let deps = world.deps(false);
    let track = world.data_dir().join("journal/synthetic-session/mic");
    std::fs::create_dir_all(&track).unwrap();
    let notes = track.join("notes.f32");
    std::fs::write(&notes, b"synthetic foreign file").unwrap();
    let chunk = track.join("000000.f32");
    std::fs::write(&chunk, 0.25_f32.to_le_bytes()).unwrap();

    for (flags, expected) in [(&["--dry-run"][..], 2), (&[][..], 2), (&["--yes"][..], 1)] {
        let mut args = vec!["uninstall", "--keep-app", "--remove-data"];
        args.extend(flags);
        let (code, report) = world.run_with(&deps, &args);
        assert_eq!(code, expected, "{report}");
        assert_eq!(report["uninstall_report"], true);
        assert_eq!(report["dry_run"], expected == 2);
        assert_eq!(report["ready"], false);
        assert!(
            report
                .to_string()
                .contains("Unsaved or processing recordings")
        );
        assert!(report.to_string().contains("synthetic-session"));
        assert!(report.to_string().contains("Move this folder away"));
        assert!(chunk.is_file());
    }
    let (code, report) = world.run_with(
        &deps,
        &[
            "uninstall",
            "--keep-app",
            "--remove-data",
            "--discard-unsaved",
            "--dry-run",
        ],
    );
    assert_eq!(code, 0, "{report}");
    assert_eq!(report["ready"], true);
    assert!(chunk.is_file());
    let (code, report) = world.run_with(&deps, &["uninstall", "--keep-app", "--yes"]);
    assert_eq!(code, 0, "{report}");
    assert!(chunk.is_file());
    std::fs::remove_file(chunk).unwrap();
    let (code, report) = world.run_with(
        &deps,
        &["uninstall", "--keep-app", "--remove-data", "--yes"],
    );
    assert_eq!(code, 0, "{report}");
    assert!(report["steps"].as_array().unwrap().iter().any(|step| {
        step["action"] == "Keep unrecognized entry"
            && step["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("notes.f32"))
    }));
    assert_eq!(std::fs::read(notes).unwrap(), b"synthetic foreign file");
}

#[cfg(unix)]
#[test]
fn linked_journal_or_session_blocks_uninstall_without_following_the_link() {
    use std::os::unix::fs::symlink;

    let world = World::new();
    let deps = world.deps(false);
    let outside = world.tmp.path().join("outside-journal");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), b"keep").unwrap();
    let journal = world.data_dir().join("journal");
    for link in [journal.clone(), journal.join("linked-session")] {
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&outside, &link).unwrap();
        for flags in [&[][..], &["--remove-data", "--discard-unsaved"][..]] {
            let mut args = vec!["uninstall", "--keep-app"];
            args.extend(flags);
            let (code, report) = world.run_with(&deps, &args);
            assert_eq!(code, 2, "{report}");
            assert!(report.to_string().contains("Cannot verify journals"));
            assert!(report.to_string().contains(link.to_str().unwrap()));
        }
        std::fs::remove_file(link).unwrap();
    }
    assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"keep");
}

use broscribe_platform::uninstall::{
    CredentialProfile, DesktopUninstall, Layout, ReleaseRemovalCheck,
};

struct LinuxFixture {
    root: PathBuf,
    launcher: PathBuf,
}
impl DesktopUninstall for LinuxFixture {
    fn layout(&self, _: &Path) -> broscribe_core::Result<Layout> {
        if self.launcher.is_dir() {
            return Err(broscribe_core::Error::io_at(
                &self.launcher,
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            ));
        }
        Ok(Layout {
            installation: Some(self.root.clone()),
            shell_retry: true,
            desktop_paths: vec![self.launcher.clone()],
            ..Default::default()
        })
    }
    fn validate_installation(&self, root: &Path, _: bool) -> broscribe_core::Result<()> {
        broscribe_platform::release_layout::ReleaseLayout::Linux.validate(root, false)
    }
    fn remove_desktop_entry(
        &self,
        _: &Path,
        path: &Path,
        root: &Path,
    ) -> broscribe_core::Result<bool> {
        assert_eq!(root, self.root);
        std::fs::remove_file(path)?;
        Ok(true)
    }
    fn remove_credentials(
        &self,
        _: &[CredentialProfile],
        _: bool,
    ) -> broscribe_core::Result<usize> {
        panic!("an inventory refusal must leave credentials alone");
    }
    fn secret_store(&self, _: &Path) -> Arc<dyn broscribe_platform::SecretStore> {
        panic!("an inventory refusal must leave the credential store alone");
    }
    fn remove_installation(
        &self,
        _: &Path,
        _: &Path,
        _: &ReleaseRemovalCheck<'_>,
    ) -> broscribe_core::Result<Option<PathBuf>> {
        panic!("an unsafe installation must be kept");
    }
}

#[test]
fn refused_linux_installation_keeps_the_root_and_removes_verified_launchers() {
    let world = World::new();
    let mut deps = world.deps(false);
    let root = broscribe_platform::stable_path(&world.tmp.path().join("Linux BroScribe"));
    let launcher = world.tmp.path().join("owned-launcher");
    let unrelated = world.tmp.path().join("personal-launcher");
    let keep = root.join("keep.txt");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(&launcher, "synthetic launcher").unwrap();
    std::fs::write(&unrelated, "synthetic personal launcher").unwrap();
    std::fs::write(&keep, "synthetic user file").unwrap();
    let cli = root.join("broscribe");
    std::fs::write(&cli, b"synthetic executable").unwrap();
    deps.agents.uninstall = Arc::new(LinuxFixture {
        root: root.clone(),
        launcher: launcher.clone(),
    });
    let (code, report) = world.run_with(&deps, &["uninstall", "--yes", "--skip-credentials"]);
    assert_eq!(code, 1, "{report}");
    assert!(!launcher.exists() && unrelated.exists());
    assert_eq!(
        std::fs::read_to_string(keep).unwrap(),
        "synthetic user file"
    );
    assert!(
        report["steps"].as_array().unwrap().iter().any(|step| {
            step["action"] == "Keep BroScribe app"
                && step["status"] == "kept"
                && step["detail"].as_str().unwrap().contains("keep.txt")
                && step["detail"].as_str().unwrap().contains(&format!(
                    "'{}' --library ",
                    cli.display()
                ))
                && step["detail"].as_str().unwrap().contains(" uninstall --data-dir ")
                && step["detail"].as_str().unwrap().contains("--skip-credentials to preview, then add --yes to apply after checking the plan")
        }),
        "{report}"
    );
    std::fs::create_dir(&launcher).unwrap();
    let (code, report) = world.run_with(&deps, &["uninstall", "--skip-credentials"]);
    assert_eq!(code, 2, "{report}");
    assert_eq!(report["uninstall_report"], true);
    assert_eq!(report["ready"], false);
    assert!(report["steps"].as_array().unwrap().iter().any(|step| {
        step["action"] == "Inspect installation and launchers"
            && step["status"] == "failed"
            && step["detail"]
                .as_str()
                .unwrap()
                .contains(launcher.to_str().unwrap())
    }));
    assert!(root.is_dir() && launcher.is_dir());
}

#[test]
fn refused_macos_installation_keeps_the_app_without_a_shell_retry_omission() {
    use broscribe_platform::release_layout::ReleaseLayout;
    use broscribe_platform::uninstall::FakeDesktopUninstall;

    let world = World::new();
    let mut deps = world.deps(false);
    let root = world.tmp.path().join("BroScribe.app");
    std::fs::create_dir(&root).unwrap();
    let keep = root.join("keep.txt");
    std::fs::write(&keep, b"synthetic user file").unwrap();
    deps.agents.uninstall = Arc::new(FakeDesktopUninstall {
        installation: Some(root),
        release_layout: Some(ReleaseLayout::Macos),
        ..Default::default()
    });

    let (code, report) = world.run_with(&deps, &["uninstall", "--yes", "--skip-credentials"]);
    assert_eq!(code, 1, "{report}");
    assert!(keep.is_file());
    let detail = report["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|step| {
            (step["action"] == "Keep BroScribe app").then(|| step["detail"].as_str().unwrap())
        })
        .unwrap_or_else(|| panic!("missing retained app in {report}"));
    assert!(detail.contains("keep.txt"), "{detail}");
    assert!(
        detail.ends_with("Kept so you can correct the failed steps and retry."),
        "{detail}"
    );
    assert!(!detail.contains("Retry command omitted"), "{detail}");
}

#[cfg(unix)]
#[test]
fn uninstall_retry_from_another_cwd_keeps_the_effective_library_and_profiles() {
    use crate::cli::{Command as CliCommand, Parsed, parse};
    use broscribe_platform::paths::{AppPaths, ENV_DATA_DIR, ENV_LIBRARY, resolve_paths_from};
    use std::os::unix::fs::PermissionsExt as _;

    struct RetryPaths {
        home: PathBuf,
        data: PathBuf,
        cache: PathBuf,
        config: PathBuf,
        runtime: PathBuf,
    }
    impl AppPaths for RetryPaths {
        fn data_dir(&self) -> PathBuf {
            self.data.join("broscribe")
        }
        fn cache_dir(&self) -> PathBuf {
            self.cache.join("broscribe")
        }
        fn documents_dir(&self) -> PathBuf {
            self.home.join("Documents")
        }
        fn default_library_dir(&self) -> PathBuf {
            self.home.join("BroScribe")
        }
        fn uninstall_retry_env(&self) -> Vec<(&'static str, &Path)> {
            vec![
                ("HOME", &self.home),
                ("XDG_DATA_HOME", &self.data),
                ("XDG_CACHE_HOME", &self.cache),
                ("XDG_CONFIG_HOME", &self.config),
                ("XDG_RUNTIME_DIR", &self.runtime),
            ]
        }
    }

    // Read a synthetic installed unit without consulting the host's service manager.
    struct FixtureRegistration(PathBuf);
    impl Autostart for FixtureRegistration {
        fn is_enabled(&self) -> broscribe_core::Result<bool> {
            Ok(false)
        }
        fn set_enabled(&self, _: bool) -> broscribe_core::Result<()> {
            panic!("preview and release refusal must leave login registrations alone");
        }
        fn registered_profiles(
            &self,
        ) -> broscribe_core::Result<Vec<broscribe_platform::RegisteredProfile>> {
            let unit = match std::fs::read_to_string(self.0.join("systemd/user/broscribed.service"))
            {
                Ok(unit) => unit,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
                Err(error) => return Err(error.into()),
            };
            let profile = unit
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Environment=\"BROSCRIBE_DATA_DIR=")?
                        .strip_suffix('"')
                })
                .expect("synthetic unit names a profile");
            Ok(vec![PathBuf::from(profile).into()])
        }
    }

    let world = World::new();
    let mut deps = world.deps(false);
    let tmp = broscribe_platform::stable_path(world.tmp.path());
    let home = tmp.join("home");
    let data = tmp.join("xdg data");
    let cache = tmp.join("xdg cache");
    let other_data = tmp.join("other data");
    let other_cache = tmp.join("other cache");
    let config = tmp.join("xdg config");
    let other_config = tmp.join("other config");
    let runtime = tmp.join("xdg runtime link");
    let other_runtime = tmp.join("other runtime");
    std::fs::create_dir(&other_runtime).unwrap();
    std::os::unix::fs::symlink(&other_runtime, &runtime).unwrap();
    let unit_profile = tmp.join("unit profile");
    let foreign_unit_profile = tmp.join("foreign unit profile");
    for (config, profile) in [
        (&config, &unit_profile),
        (&other_config, &foreign_unit_profile),
    ] {
        let unit = config.join("systemd/user/broscribed.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(
            unit,
            format!(
                "[Service]\nEnvironment=\"BROSCRIBE_DATA_DIR={}\"\n",
                profile.display()
            ),
        )
        .unwrap();
        Config::default().save(profile).unwrap();
        broscribe_core::maintenance::ModeLease::acquire(profile, Duration::ZERO).unwrap();
    }
    let foreign_profile = other_data.join("broscribe");
    Config::default().save(&foreign_profile).unwrap();
    broscribe_core::maintenance::ModeLease::acquire(&foreign_profile, Duration::ZERO).unwrap();
    deps.paths = Box::new(RetryPaths {
        home: home.clone(),
        data: data.clone(),
        cache: cache.clone(),
        config: config.clone(),
        runtime: runtime.clone(),
    });
    deps.autostart = Box::new(FixtureRegistration(config.clone()));
    let root = broscribe_platform::stable_path(&world.tmp.path().join("Linux BroScribe"));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("keep.txt"), "synthetic user file").unwrap();
    let launcher = world.tmp.path().join("owned-launcher");
    std::fs::write(&launcher, "synthetic launcher").unwrap();
    deps.agents.uninstall = Arc::new(LinuxFixture {
        root: root.clone(),
        launcher: launcher.clone(),
    });
    let library = broscribe_platform::stable_path(&world.tmp.path().join("library with a 'quote'"));
    let selected = broscribe_platform::stable_path(&world.tmp.path().join("selected profile"));
    let additional = tmp.join("additional profile");
    Config::default().save(&selected).unwrap();
    broscribe_core::maintenance::ModeLease::acquire(&selected, Duration::ZERO).unwrap();
    let cli = root.join("broscribe");
    std::fs::write(
        &cli,
        "#!/bin/sh\nprintf '%s\\0' \"$HOME\" \"$XDG_DATA_HOME\" \"$XDG_CACHE_HOME\" \"$XDG_CONFIG_HOME\" \"$BROSCRIBE_DATA_DIR\" \"$BROSCRIBE_LIBRARY\" \"$XDG_RUNTIME_DIR\" \"$0\" \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();

    let run = |deps: &Deps, argv, env| {
        let Parsed::Run(parsed) = parse(argv) else {
            panic!("invalid synthetic retry arguments")
        };
        let CliCommand::Uninstall(args) = parsed.command else {
            panic!("uninstall expected")
        };
        let mut out = Vec::new();
        let code = crate::Ctx::scoped(
            deps,
            true,
            parsed.library,
            env,
            None,
            &mut out,
            &mut Vec::new(),
            |ctx| {
                crate::cmd::uninstall::run(ctx, args).unwrap();
                ctx.exit_code.unwrap_or(0)
            },
        );
        (code, serde_json::from_slice::<Value>(&out).unwrap())
    };
    let paths = resolve_paths_from(deps.paths.as_ref(), |key| match key {
        ENV_DATA_DIR => Some(selected.clone().into_os_string()),
        ENV_LIBRARY => Some(library.clone().into_os_string()),
        _ => None,
    })
    .unwrap();
    let (_, original) = run(
        &deps,
        vec![
            cli.clone().into_os_string(),
            "uninstall".into(),
            "--yes".into(),
            "--remove-library".into(),
            "--data-dir".into(),
            additional.clone().into_os_string(),
        ],
        paths,
    );
    let detail = original["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|step| {
            step["detail"]
                .as_str()?
                .split_once("Run ")
                .map(|(_, command)| command)
        })
        .unwrap_or_else(|| panic!("retry command missing from {original}"));
    let command = detail
        .strip_suffix(" to preview, then add --yes to apply after checking the plan.")
        .unwrap();
    assert!(!command.contains("--yes"));
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", command])
        .env_clear()
        .env("XDG_DATA_HOME", &other_data)
        .env("XDG_CACHE_HOME", &other_cache)
        .env("XDG_CONFIG_HOME", &other_config)
        .env("XDG_RUNTIME_DIR", &other_runtime)
        .current_dir(world.tmp.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let words = std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim_end_matches('\0')
        .split('\0')
        .collect::<Vec<_>>();
    assert_eq!(
        words,
        [
            home.to_str().unwrap(),
            data.to_str().unwrap(),
            cache.to_str().unwrap(),
            config.to_str().unwrap(),
            selected.to_str().unwrap(),
            library.to_str().unwrap(),
            runtime.to_str().unwrap(),
            cli.to_str().unwrap(),
            "--library",
            library.to_str().unwrap(),
            "uninstall",
            "--data-dir",
            selected.to_str().unwrap(),
            "--data-dir",
            additional.to_str().unwrap(),
            "--remove-library",
        ]
    );
    let paths_from_shell = |paths: &dyn AppPaths| {
        resolve_paths_from(paths, |key| match key {
            ENV_DATA_DIR => Some(words[4].into()),
            ENV_LIBRARY => Some(words[5].into()),
            _ => None,
        })
        .unwrap()
    };
    let replay_args = || words[7..].iter().map(|word| (*word).into()).collect();
    deps.paths = Box::new(RetryPaths {
        home: words[0].into(),
        data: words[1].into(),
        cache: words[2].into(),
        config: words[3].into(),
        runtime: words[6].into(),
    });
    deps.autostart = Box::new(FixtureRegistration(words[3].into()));
    let (_, replay) = run(&deps, replay_args(), paths_from_shell(deps.paths.as_ref()));
    assert_eq!(replay["dry_run"], true);
    for (action, path) in [
        ("Remove library", &library),
        ("Remove data folder", &selected),
        ("Remove data folder", &additional),
        ("Remove data folder", &unit_profile),
    ] {
        assert!(
            replay["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| { s["action"] == action && s["path"].as_str() == path.to_str() }),
            "{replay}"
        );
    }
    for profile in [&foreign_profile, &foreign_unit_profile] {
        assert!(
            !replay.to_string().contains(profile.to_str().unwrap()),
            "{replay}"
        );
    }
    let unresolved_config = tmp.join("missing/../xdg config");
    assert!(unresolved_config.canonicalize().is_err());
    assert_eq!(broscribe_platform::stable_path(&unresolved_config), config);
    deps.paths = Box::new(RetryPaths {
        home: home.clone(),
        data: data.clone(),
        cache: cache.clone(),
        config: unresolved_config.clone(),
        runtime: runtime.clone(),
    });
    deps.autostart = Box::new(FixtureRegistration(unresolved_config.clone()));
    let (_, unresolved) = run(&deps, replay_args(), paths_from_shell(deps.paths.as_ref()));
    assert!(
        !unresolved
            .to_string()
            .contains(unit_profile.to_str().unwrap())
    );
    // Force release refusal again so the applied operation offers a retry.
    std::fs::write(&launcher, "synthetic launcher").unwrap();
    let mut apply = replay_args();
    apply.push("--yes".into());
    let (_, unresolved) = run(&deps, apply, paths_from_shell(deps.paths.as_ref()));
    let config_env = format!(
        "XDG_CONFIG_HOME={} ",
        crate::cmd::agents::shell_quote(unresolved_config.to_str().unwrap())
    );
    assert!(
        unresolved["steps"].as_array().unwrap().iter().any(|step| {
            step["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains(&config_env))
        }),
        "{unresolved}"
    );
    deps.paths = Box::new(RetryPaths {
        home,
        data: other_data,
        cache: other_cache,
        config: other_config.clone(),
        runtime: other_runtime,
    });
    deps.autostart = Box::new(FixtureRegistration(other_config));
    let (_, unpinned) = run(&deps, replay_args(), paths_from_shell(deps.paths.as_ref()));
    for profile in [&foreign_profile, &foreign_unit_profile] {
        assert!(
            unpinned.to_string().contains(profile.to_str().unwrap()),
            "{unpinned}"
        );
    }

    std::fs::write(&launcher, "synthetic launcher").unwrap();
    let keeping_data = vec![
        cli.clone().into_os_string(),
        "uninstall".into(),
        "--yes".into(),
    ];
    let default_paths = || resolve_paths_from(deps.paths.as_ref(), |_| None).unwrap();
    let (_, keeping) = run(&deps, keeping_data.clone(), default_paths());
    let default_library = deps.paths.default_library_dir();
    let default_flag = crate::cmd::agents::shell_quote(default_library.to_str().unwrap());
    assert!(
        keeping
            .to_string()
            .contains(&format!("--library {default_flag}")),
        "{keeping}"
    );

    let broken = tmp.join("uninspected-profile");
    std::fs::write(&broken, "not a profile directory").unwrap();
    let mut with_secondary = keeping_data;
    with_secondary.extend(["--data-dir".into(), broken.clone().into_os_string()]);
    let (_, blocked) = run(&deps, with_secondary.clone(), default_paths());
    assert!(
        !blocked.to_string().contains(" to preview, then add --yes"),
        "{blocked}"
    );
    assert!(
        blocked.to_string().contains(&format!(
            "the profile at {} could not be inspected",
            broken.display()
        )),
        "{blocked}"
    );

    std::fs::remove_file(&broken).unwrap();
    let mut config = Config::default();
    config.library = Some("relative-library".into());
    config.save(&broken).unwrap();
    let (_, relative) = run(&deps, with_secondary, default_paths());
    assert!(
        !relative.to_string().contains(" to preview, then add --yes"),
        "{relative}"
    );
    assert!(
        relative.to_string().contains(&format!(
            "the profile at {} has a relative library path",
            broken.display()
        )),
        "{relative}"
    );
}

/// Exercise the uninstall command against a profile populated through the
/// feature writers. Files owned by another executable are represented at its
/// disk boundary; the Windows helper sees the identical pre-uninstall tree.
#[test]
fn uninstall_allowlist_covers_profile_writers_and_preserves_only_unknown_entries() {
    use broscribe_capture::journal::{JournalWriter, SessionMeta, Track};
    use broscribe_core::{Library, fsutil, lifecycle, maintenance, schema, templates};
    use broscribe_integrations::{CalendarCache, IntegrationId, Integrations, IntegrationsConfig};
    use broscribe_models::{
        ModelStore, Registry,
        pull::{PullOptions, pull},
    };
    use broscribe_pipeline::{bench, worker};
    use broscribe_platform::uninstall::FakeDesktopUninstall;
    use broscribe_updates::{
        CheckOutcome, FakeInstallAdmission, InstallAdmission, UpdateExpectation,
    };

    for with_unknown in [false, true] {
        let world = World::new();
        let mut deps = world.deps(false);
        deps.agents.uninstall = Arc::new(FakeDesktopUninstall {
            installation: Some(world.root.clone()),
            ..Default::default()
        });
        let data = world.data_dir();
        let updates = state_dir(&data);
        let cache = deps.paths.cache_dir();
        let library = deps.paths.default_library_dir();
        let lib = Library::open(&library).unwrap();
        let mut config = Config::default();
        config.updates = world.settings();
        config.save(&data).unwrap();
        let (code, registration) = world.run_with(&deps, &["mcp", "disable"]);
        assert_eq!(code, 0, "{registration}");
        assert!(data.join("mcp-registrations.json").is_file());
        assert!(maintenance::profile_id(&data).unwrap().is_some());

        let session = "synthetic-session";
        let journal = lifecycle::journal_root(&data);
        let mut writer = JournalWriter::create(
            &journal,
            session,
            SessionMeta {
                library_root: Some(broscribe_platform::stable_path(&library)),
                ..Default::default()
            },
            1.0,
            std::process::id(),
        )
        .unwrap();
        for track in [Track::Mic, Track::System] {
            writer.begin_segment(track, 1, None, 16_000, 0).unwrap();
            writer.append(track, 0, &[0.1; 160]).unwrap();
        }
        writer.finish(10_000_000, "synthetic fixture").unwrap();
        lifecycle::set_session_title(&lib, &journal, session, "Synthetic session", || Ok(()))
            .unwrap();
        let session_path = journal.join(session);
        lifecycle::write_discard_intent(&journal, "interrupted-discard").unwrap();
        // Operator config and app-owned artifacts are represented at their disk boundary.
        std::fs::create_dir_all(data.join("state")).unwrap();
        for (name, bytes) in [
            ("detect.toml", "version = 1\n"),
            (
                "setup_rules.toml",
                "version = 1\nfallback = 'speakers_mic'\n",
            ),
            ("models.toml", "models = []\n"),
            ("state/tray-tip-shown", ""),
            ("state/caption-tip-shown", ""),
        ] {
            fsutil::write_atomic(&data.join(name), bytes.as_bytes()).unwrap();
        }
        fsutil::write_json_atomic(
            &session_path.join("roster.json"),
            &json!({"observed_s":0.1,"members":[{"name":"Sample Attendee","is_self":false}]}),
        )
        .unwrap();
        fsutil::write_atomic(
            &session_path.join("captions.jsonl"),
            b"{\"type\":\"synthetic caption\"}\n",
        )
        .unwrap();
        CalendarCache::default()
            .save_private(&session_path.join("calendar.json"))
            .unwrap();
        fsutil::write_json_atomic(
            &data.join("ui-state.json"),
            &json!({"settings_page":"general"}),
        )
        .unwrap();

        let items = Registry::builtin().unwrap().models;
        for item in &items {
            let store = ModelStore::new(&data);
            std::fs::create_dir_all(store.model_dir(&item.id).unwrap().join(".partial")).unwrap();
            std::fs::File::create(store.lock_path(&item.id).unwrap()).unwrap();
        }
        let mut model = items
            .into_iter()
            .find(|item| item.id == "parakeet-tdt-0.6b-v3")
            .unwrap();
        let bytes = b"synthetic offline model";
        model.files = vec![broscribe_models::registry::ModelFile {
            url: "https://models.example/synthetic.onnx".into(),
            sha256: schema::sha256_hex(bytes),
            bytes: bytes.len() as u64,
            archive: false,
            path_in_archive_root: None,
        }];
        let mirror = world.tmp.path().join("fixture-mirror");
        std::fs::create_dir(&mirror).unwrap();
        std::fs::write(mirror.join("synthetic.onnx"), bytes).unwrap();
        pull(
            &model,
            &data,
            &PullOptions {
                mirror: Some(mirror),
                ..Default::default()
            },
            &mut |_, _| {},
            &deps.execution,
        )
        .unwrap();
        let model_dir = data.join("models").join(&model.id);
        let partial = model_dir.join(".partial");
        std::fs::create_dir(&partial).unwrap();
        std::fs::copy(
            model_dir.join("synthetic.onnx"),
            partial.join("synthetic.onnx"),
        )
        .unwrap();

        templates::save_user(
            &data,
            "synthetic-notes",
            "Synthetic",
            "Fixture",
            "Write a synthetic note.",
        )
        .unwrap();
        let id = broscribe_core::MeetingId::parse("m-000001").unwrap();
        let mut meeting = schema::Meeting::new(id.clone(), schema::Source::Import, schema::now());
        meeting.started_at =
            chrono::DateTime::parse_from_rfc3339("2026-11-26T10:00:00+00:00").unwrap();
        meeting.duration_s = 4.0;
        meeting.recording = Some(
            serde_json::from_value(json!({
                "state":"available", "handling":"copy", "path":"audio.wav",
                "sha256": "a".repeat(64), "bytes":364
            }))
            .unwrap(),
        );
        meeting.template_results.push(serde_json::from_value(json!({
            "template_id":"synthetic-notes", "template_name":"Synthetic", "instructions_sha256":"a".repeat(64),
            "provider":"synthetic", "created_at":meeting.started_at, "markdown":"Synthetic generated note"
        })).unwrap());
        meeting.segments.push(schema::Segment {
            speech_overlap: Vec::new(),
            id: 0,
            track: "t0".into(),
            speaker: None,
            start: 0.0,
            end: 2.0,
            text: "Synthetic transcript excerpt.".into(),
            words: Vec::new(),
        });
        let person = schema::Person::new(
            broscribe_core::PersonId::parse("p-123abc").unwrap(),
            "Synthetic Person",
        );
        let mut index = broscribe_core::index::Index::open(lib.root()).unwrap();
        let meeting_dir = lib
            .write(|tx| {
                let dir = tx.create_meeting_dir(&meeting)?;
                worker::write_wav_16k(&dir.join("audio.wav"), &[0.1; 160])?;
                tx.save_person(&person)?;
                tx.save_meeting(&meeting)?;
                index.upsert_meeting(tx, &meeting)?;
                broscribe_core::queue::Queue::open(tx.root())?.enqueue(
                    tx,
                    broscribe_core::queue::VECTORS_KIND,
                    meeting.id.as_str(),
                    json!({"meeting_id": meeting.id}),
                )?;
                Ok(dir)
            })
            .unwrap();
        drop(index);
        drop(broscribe_core::index::VectorCache::open(lib.root()).unwrap());
        let mut markdown_only = meeting.clone();
        markdown_only.id = broscribe_core::MeetingId::parse("m-000002").unwrap();
        let md_dir = lib
            .write(|tx| {
                let dir = tx.create_meeting_dir(&markdown_only)?;
                tx.save_meeting(&markdown_only)?;
                Ok(dir)
            })
            .unwrap();
        std::fs::remove_file(md_dir.join("meeting.json")).unwrap();
        let (code, added) = world.run_with(
            &deps,
            &[
                "bench",
                "excerpt",
                "add",
                id.as_str(),
                "0",
                "2",
                "--set",
                "synthetic-set",
            ],
        );
        assert_eq!(code, 0, "{added}");
        let results = bench::BenchResults {
            schema_version: 1,
            set: "synthetic-set".into(),
            created_at: schema::now(),
            collar_s: 0.25,
            excerpts: 1,
            manifest_hints: false,
            candidates: vec![],
        };
        let result = bench::save_results(&data, &results).unwrap();
        bench::save_results(&data, &results).unwrap();

        let integrations = Integrations::new(
            &data,
            IntegrationsConfig::default(),
            deps.secrets.clone(),
            deps.browser.clone(),
            Arc::new(|_| {}),
        )
        .unwrap();
        integrations
            .cancel_login(
                IntegrationId::Google,
                &broscribe_integrations::LoginAttemptId::parse("synthetic-login").unwrap(),
            )
            .unwrap();
        for provider in IntegrationId::ALL {
            let account = broscribe_integrations::account_dir(&data, provider);
            if provider == IntegrationId::MicrosoftGraph {
                integrations.list_accounts(Some(provider)).unwrap();
            } else {
                integrations.logout(provider).unwrap();
            }
            CalendarCache::default()
                .save_private(&account.join("calendar.json"))
                .unwrap();
            let photos = account.join("photos/1");
            std::fs::create_dir_all(&photos).unwrap();
            fsutil::write_atomic_with_mode(
                &photos.join("p-123abc.img"),
                b"synthetic photo",
                Some(0o600),
            )
            .unwrap();
        }

        // Keep a real installed inventory and a newer staged journal. A
        // timed-out fake handoff writes the report without touching a service.
        for build in [11, 12] {
            world.publish(build);
            let mut updater = broscribe_updates::Updater::open(
                &updates,
                &world.settings(),
                world.platform.as_ref(),
                &world.transport,
            )
            .unwrap();
            let cancel = Cancel::new();
            let CheckOutcome::Available(offer) = updater.check(Utc::now(), &cancel).unwrap() else {
                panic!("expected a synthetic update");
            };
            let staged = updater.download(&offer, &cancel, &mut |_| {}).unwrap();
            if build == 11 {
                let admission = FakeInstallAdmission::default();
                let lease = admission.acquire().unwrap();
                updater
                    .install(&UpdateExpectation::from(&staged), &*lease, &cancel)
                    .unwrap();
                updater.confirm().unwrap();
            }
        }
        let record = broscribe_update_runner::InstallRecord {
            schema: broscribe_update_runner::InstallRecord::SCHEMA,
            fence: 1,
            agent_pid: 42,
            agent_library: None,
            expected: identity(12),
            unchanged: identity(11),
            runner_pid: None,
        };
        record.save(&data).unwrap();
        let handoff = Handoff {
            schema: Handoff::SCHEMA,
            data_dir: data.clone(),
            executable: deps.agents.cli.clone(),
            installation_root: world.root.clone(),
            protected_paths: vec![data.clone(), library.clone()],
            profile_dirs: vec![data.clone()],
            config_base: world.tmp.path().to_path_buf(),
            agent_library: None,
            contained_paths: vec![world.tmp.path().join("home")],
            wait_pids: vec![std::process::id()],
            fence: 1,
            action: broscribe_update_runner::HandoffAction::Recover,
        };
        handoff.save().unwrap();
        let runner = run_handoff(
            &handoff,
            &RunnerDeps {
                platform: world.platform.as_ref(),
                autostart: deps.autostart.as_ref(),
                probe: deps.process.as_ref(),
                agent_listening: &|_| Ok(false),
                timeout: Duration::ZERO,
                poll: Duration::ZERO,
            },
        )
        .unwrap();
        assert!(matches!(runner.outcome, RunnerOutcome::Failed { .. }));
        handoff.save().unwrap();
        super::AdmissionRecord {
            schema: super::AdmissionRecord::SCHEMA,
            fence: 2,
            agent_pid: 42,
            installed: identity(11),
        }
        .save(&data)
        .unwrap();
        let run_dir = data.join("run");
        broscribe_platform_native::ipc::ensure_private_dir(&run_dir).unwrap();
        let admission = broscribe_ipc::install_admission::Persisted {
            last_fence: 2,
            held: None,
        };
        fsutil::write_json_atomic(
            &broscribe_ipc::install_admission::Persisted::path(&data),
            &admission,
        )
        .unwrap();
        std::fs::File::create(run_dir.join("app.lock")).unwrap();

        let playback = broscribe_core::ops::playback_cache_path(&cache, &"1".repeat(64));
        let capture = cache.join("helper-capture/sample-synthetic/sample.wav");
        let private_input =
            cache.join("setup-capture-cli/42/broscribe-voice-synthetic/passage-input.wav");
        let setup = cache.join("setup-capture/synthetic");
        for path in [
            playback,
            capture,
            private_input,
            setup.join("sample.wav"),
            setup.join("passage.wav"),
            setup.join("confirmation.wav"),
            setup.join("try.wav"),
            setup.join("broscribe-voice-synthetic/try-input.aiff"),
            cache.join("audio-check-A0b1C2.wav"),
        ] {
            fsutil::create_dir_durable(path.parent().unwrap()).unwrap();
            worker::write_wav_16k(&path, &[0.1; 160]).unwrap();
        }
        fsutil::write_atomic(&setup.join("words.txt"), b"synthetic passage").unwrap();
        for target in [
            data.join(".broscribe-profile"),
            data.join("ui-state.json"),
            data.join("mcp-registrations.json"),
            session_path.join("calendar.json"),
            session_path.join("roster.json"),
            result,
            updates.join("install-record.json"),
            updates.join("runner-handoff.json"),
            updates.join("runner-report.json"),
            updates.join("manual-admission.json"),
            run_dir.join("install-admission.json"),
        ] {
            let orphan = target.with_file_name(format!(
                ".{}.tmp-42-deadbeef",
                target.file_name().unwrap().to_str().unwrap()
            ));
            std::fs::write(orphan, "synthetic interrupted atomic write").unwrap();
        }
        maintenance::ModeChange::acquire(&data)
            .unwrap()
            .set(maintenance::Mode::Maintenance)
            .unwrap();

        let unknown = if with_unknown {
            std::fs::create_dir_all(library.join("2026/11/planning")).unwrap();
            vec![
                library.join("2026/11/planning/meeting.md"),
                library.join("2026/11/planning/audio.wav"),
                library.join("personal.txt"),
                meeting_dir.join("personal.md"),
                meeting_dir.join("audio.txt"),
                md_dir.join("audio.wav"),
                library.join(".queue/batch-personal.json"),
                data.join("integrations/google/photos/1/p-family.img"),
                session_path.join("mic/notes.f32"),
                cache.join("audio-check-personal.wav"),
                data.join("personal.txt"),
                data.join("bench/results/notes.md"),
                session_path.join(".roster.json.tmp-42-not-ours"),
                partial.join("personal.onnx"),
                cache.join("helper-capture/sample-synthetic/keep.txt"),
            ]
        } else {
            Vec::new()
        };
        for path in &unknown {
            std::fs::write(path, "synthetic user file").unwrap();
        }
        let preview = broscribe_platform::profile_cleanup::clean(
            &data,
            "data",
            false,
            &["run", "updates"],
            cfg!(windows),
        )
        .unwrap();
        for kept in &preview.kept {
            assert!(
                unknown
                    .iter()
                    .any(|path| broscribe_platform::same_path(path, kept)),
                "unexpected writer output {}",
                kept.display()
            );
        }
        #[cfg(windows)]
        assert_windows_allowlist(
            &data,
            &cache,
            &unknown
                .iter()
                .filter(|p| p.starts_with(&data) || p.starts_with(&cache))
                .cloned()
                .collect::<Vec<_>>(),
        );

        let (code, report) = world.run_with(
            &deps,
            &[
                "uninstall",
                "--remove-data",
                "--remove-library",
                "--discard-unsaved",
                "--yes",
            ],
        );
        assert_eq!(code, 0, "{report}");
        let kept: std::collections::BTreeSet<_> = report["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|step| step["status"] == "kept")
            .map(|step| broscribe_platform::stable_path(Path::new(step["path"].as_str().unwrap())))
            .collect();
        let expected: std::collections::BTreeSet<_> = unknown
            .iter()
            .flat_map(|path| {
                path.ancestors().take_while(|parent| {
                    parent.starts_with(&data)
                        || parent.starts_with(&cache)
                        || parent.starts_with(&library)
                })
            })
            .map(broscribe_platform::stable_path)
            .collect();
        assert_eq!(kept, expected, "{report}");
        assert_eq!(data.exists(), with_unknown);
        assert_eq!(cache.exists(), with_unknown);
        assert_eq!(library.exists(), with_unknown);
        assert!(!world.root.exists() && !world.staging().exists());
        assert!(
            world
                .root
                .with_file_name(".BroScribe.app.update.lock")
                .is_file()
        );
        for root in [&data, &cache, &library] {
            let mut dirs = if root.exists() {
                vec![root.clone()]
            } else {
                vec![]
            };
            while let Some(dir) = dirs.pop() {
                for item in std::fs::read_dir(&dir).unwrap() {
                    let item = item.unwrap();
                    assert!(
                        expected.contains(&broscribe_platform::stable_path(&item.path())),
                        "unexpected leftover {}",
                        item.path().display()
                    );
                    if item.file_type().unwrap().is_dir() {
                        dirs.push(item.path());
                    }
                }
            }
        }
        for path in unknown {
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                "synthetic user file"
            );
        }
    }
}

#[cfg(windows)]
fn assert_windows_allowlist(data: &Path, cache: &Path, unknown: &[PathBuf]) {
    fn copy(src: &Path, dst: &Path) {
        std::fs::create_dir(dst).unwrap();
        for item in std::fs::read_dir(src).unwrap() {
            let item = item.unwrap();
            let path = dst.join(item.file_name());
            if item.file_type().unwrap().is_dir() {
                copy(&item.path(), &path);
            } else {
                std::fs::copy(item.path(), path).unwrap();
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let temp_path =
        broscribe_platform::plain_verbatim(&broscribe_platform::stable_path(temp.path()));
    let data_copy = temp_path.join("data");
    let cache_copy = temp_path.join("cache");
    copy(data, &data_copy);
    copy(cache, &cache_copy);
    let helper = temp_path.join("uninstall-after-exit.ps1");
    std::fs::write(
        &helper,
        include_str!("../../../../broscribe-platform-windows/src/uninstall-after-exit.ps1"),
    )
    .unwrap();
    let script = r#"
        $ErrorActionPreference = 'Stop'
        . $env:BROSCRIBE_TEST_HELPER
        $shapes = $env:BROSCRIBE_UNINSTALL_PROFILE_SHAPES | ConvertFrom-Json
        $kept = [Collections.Generic.List[string]]::new()
        $keptParents = [Collections.Generic.List[string]]::new()
        $data = Full-Path $env:BROSCRIBE_TEST_DATA
        foreach ($kind in @('run', 'updates')) { Remove-Owned (Join-Path $data $kind) $kind }
        Remove-Owned $data 'data' @('run', 'cache', 'updates', 'update-runner', 'app-backups')
        Remove-Owned (Full-Path $env:BROSCRIBE_TEST_CACHE) 'cache'
        $kept | ForEach-Object { Write-Output $_ }
    "#;
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let output = std::process::Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("BROSCRIBE_TEST_HELPER", &helper)
        .env("BROSCRIBE_TEST_DATA", &data_copy)
        .env("BROSCRIBE_TEST_CACHE", &cache_copy)
        .env(
            "BROSCRIBE_UNINSTALL_PROFILE_SHAPES",
            broscribe_platform::profile_cleanup::ALLOWLIST,
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let kept: std::collections::BTreeSet<_> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(PathBuf::from)
        .collect();
    let expected: std::collections::BTreeSet<_> = unknown
        .iter()
        .map(|path| {
            if let Ok(rel) = path.strip_prefix(data) {
                data_copy.join(rel)
            } else {
                cache_copy.join(path.strip_prefix(cache).unwrap())
            }
        })
        .collect();
    assert_eq!(kept, expected);
    assert_eq!(data_copy.exists(), !unknown.is_empty());
    assert_eq!(cache_copy.exists(), !unknown.is_empty());
}

#[cfg(windows)]
#[test]
fn detached_update_helper_releases_captured_cli_stdout_before_it_exits() {
    use std::io::Read as _;
    use std::sync::mpsc;

    const FIXTURE: &str = "BROSCRIBE_TEST_DETACHED_HELPER";
    if let Some(directory) = std::env::var_os(FIXTURE) {
        let path = PathBuf::from(directory);
        let system =
            broscribe_platform_native::NativePlatform::update_runner_working_directory(&path)
                .unwrap();
        let mut helper =
            std::process::Command::new(system.join("WindowsPowerShell/v1.0/powershell.exe"));
        helper
            .current_dir(system)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(path.join("wait.ps1"))
            .env(FIXTURE, path);
        broscribe_platform_native::NativePlatform::spawn_update_runner(&mut helper).unwrap();
        return;
    }

    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path();
    std::fs::write(
        path.join("wait.ps1"),
        r#"
$ErrorActionPreference = 'Stop'
$dir = $env:BROSCRIBE_TEST_DETACHED_HELPER
[IO.File]::WriteAllText((Join-Path $dir 'ready'), '')
$until = [DateTime]::UtcNow.AddSeconds(30)
while (![IO.File]::Exists((Join-Path $dir 'finish'))) {
  if ([DateTime]::UtcNow -ge $until) { exit 1 }
  Start-Sleep -Milliseconds 20
}
[IO.File]::WriteAllText((Join-Path $dir 'finished'), '')
"#,
    )
    .unwrap();
    let mut cli = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cmd::update::tests::detached_update_helper_releases_captured_cli_stdout_before_it_exits"])
        .env(FIXTURE, path)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = cli.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.read_to_end(&mut bytes).map(|_| bytes);
        let _ = send.send(result);
    });
    let wait_for = |name: &str| {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !path.join(name).exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        path.join(name).exists()
    };
    let ready = wait_for("ready");
    let captured = receive.recv_timeout(Duration::from_secs(5));
    let still_running = !path.join("finished").exists();
    std::fs::write(path.join("finish"), "").unwrap();
    let finished = wait_for("finished");
    assert!(cli.wait().unwrap().success());
    reader.join().unwrap();
    assert!(
        ready && still_running && finished,
        "grandchild did not wait for its finish signal"
    );
    let output = captured
        .expect("CLI stdout kept open by its long-lived helper")
        .unwrap();
    assert!(String::from_utf8_lossy(&output).contains("1 passed"));
}
