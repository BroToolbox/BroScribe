//! The external runner over synthetic parts: a deterministic update
//! platform on real folders that reports `AfterExit` (files in use cannot
//! move), an in-memory feed signed with a synthetic key in a fake secret
//! store, and a synthetic agent service whose endpoint answers until it is
//! stopped. No real installation, agent, key or Windows machine.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use broscribe_core::config::{Config, UpdateSettings};
use broscribe_platform::Autostart;
use broscribe_platform::fakes::{FakeProcessProbe, FakeSecretStore};
use broscribe_platform::update::{
    FakeUpdatePlatform, InstallationProcess, PayloadIdentity, Replacement, UpdatePlatform,
};
use broscribe_update_runner::{
    Handoff, HandoffAction, RunnerDeps, RunnerOutcome, RunnerReport, run_handoff, state_dir,
};
use broscribe_updates::{
    Cancel, CheckOutcome, ExistingKey, FakeTransport, MANIFEST_FORMAT, ReleaseKeyLabel,
    UpdateExpectation, UpdateStatus, Updater, generate_release_key, sign_manifest,
};
use chrono::{Duration as Span, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};

const TARGET: &str = "x86_64-pc-windows-msvc";
const APP_ID: &str = "BroScribe";
const FEED: &str = "https://updates.example/stable.json";
const ARTIFACT: &str = "https://updates.example/BroScribe-11.zip";
const SIGNER: &str = "ab12";
const COMMAND_PID: u32 = 31_337;

type Listening = broscribe_core::Result<bool>;

fn identity(build: u64) -> PayloadIdentity {
    PayloadIdentity {
        product: "BroScribe".into(),
        app_id: APP_ID.into(),
        version: "1.0.0".into(),
        build,
        target: TARGET.into(),
    }
}

/// The agent's service registration: stopping it closes the endpoint,
/// starting it opens it again.
#[derive(Default)]
struct AgentService {
    listening: AtomicBool,
    stops: AtomicUsize,
    starts: AtomicUsize,
}

impl Autostart for AgentService {
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
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.listening.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct World {
    tmp: tempfile::TempDir,
    root: PathBuf,
    platform: FakeUpdatePlatform,
    transport: FakeTransport,
    settings: UpdateSettings,
    agent: Arc<AgentService>,
    probe: FakeProcessProbe,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Programs").join("BroScribe");
        FakeUpdatePlatform::write_payload(&root, &identity(10), SIGNER).unwrap();
        let mut platform = FakeUpdatePlatform::new(root.clone(), TARGET);
        platform.atomic_exchange = false;
        platform.replacement = Replacement::AfterExit;
        let secrets = FakeSecretStore::default();
        let label = ReleaseKeyLabel::parse("test-release").unwrap();
        // A synthetic key in a fake store, never a real release key.
        let key = generate_release_key(&secrets, &label, ExistingKey::Refuse).unwrap();
        let settings = UpdateSettings {
            enabled: true,
            feed_url: Some(FEED.into()),
            channel: "stable".into(),
            trusted_keys: vec![key.key.public_key_hex()],
            signer: Some(SIGNER.into()),
        };
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let mut config = Config::load(&data_dir).unwrap();
        config.updates = settings.clone();
        config.save(&data_dir).unwrap();
        let transport = FakeTransport::new();
        publish(&transport, &secrets, &label, 11);
        let agent = Arc::new(AgentService::default());
        agent.listening.store(true, Ordering::SeqCst);
        Self {
            root,
            platform,
            transport,
            settings,
            agent,
            probe: FakeProcessProbe::new(1),
            tmp,
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.tmp.path().join("data")
    }

    fn updater(&self) -> Updater<'_> {
        Updater::open(
            &state_dir(&self.data_dir()),
            &self.settings,
            &self.platform,
            &self.transport,
        )
        .unwrap()
    }

    fn stage(&self) {
        let mut updater = self.updater();
        let CheckOutcome::Available(offer) = updater.check(Utc::now(), &Cancel::new()).unwrap()
        else {
            panic!("expected an offer");
        };
        updater
            .download(&offer, &Cancel::new(), &mut |_| {})
            .unwrap();
    }

    fn handoff(&self, action: HandoffAction) -> Handoff {
        let handoff = Handoff {
            schema: Handoff::SCHEMA,
            data_dir: self.data_dir(),
            executable: self.root.join("broscribe.exe"),
            installation_root: self.root.clone(),
            protected_paths: vec![self.data_dir()],
            profile_dirs: vec![self.data_dir()],
            config_base: self.data_dir(),
            agent_library: None,
            contained_paths: vec![self.root.with_file_name("home")],
            wait_pids: vec![COMMAND_PID],
            fence: 7,
            action,
        };
        handoff.save().unwrap();
        handoff
    }

    /// What the command confirmed for the staged update.
    fn staged_expectation(&self) -> UpdateExpectation {
        UpdateExpectation::from(&self.updater().staged().unwrap().unwrap())
    }

    /// What the command confirmed for the installed, unconfirmed update.
    fn awaiting_expectation(&self) -> UpdateExpectation {
        match self.updater().status().unwrap() {
            UpdateStatus::AwaitingConfirmation { from, to, signer } => {
                UpdateExpectation { from, to, signer }
            }
            other => panic!("expected an installed update, got {other:?}"),
        }
    }

    fn run(
        &self,
        handoff: &Handoff,
        listening: &(dyn Fn(&Path) -> broscribe_core::Result<bool> + Sync),
    ) -> RunnerReport {
        run_handoff(
            handoff,
            &RunnerDeps {
                platform: &self.platform,
                autostart: self.agent.as_ref(),
                probe: &self.probe,
                agent_listening: listening,
                timeout: Duration::from_millis(300),
                poll: Duration::from_millis(10),
            },
        )
        .unwrap()
    }

    fn installed(&self) -> PayloadIdentity {
        self.platform.inspect(&self.root).unwrap()
    }
}

#[test]
fn maintenance_runner_refuses_before_service_or_report_effects() {
    use broscribe_core::maintenance::{Mode, ModeChange};
    let w = World::new();
    let handoff = w.handoff(HandoffAction::Recover);
    let change = ModeChange::acquire(&w.data_dir()).unwrap();
    change.set(Mode::Maintenance).unwrap();
    drop(change);
    let result = run_handoff(
        &handoff,
        &RunnerDeps {
            platform: &w.platform,
            autostart: w.agent.as_ref(),
            probe: &w.probe,
            agent_listening: &|_| panic!("must reject before checking the agent"),
            timeout: Duration::ZERO,
            poll: Duration::ZERO,
        },
    );
    assert_eq!(result.unwrap_err().code(), "maintenance");
    assert_eq!(w.agent.starts.load(Ordering::SeqCst), 0);
    assert_eq!(w.agent.stops.load(Ordering::SeqCst), 0);
    assert_eq!(Handoff::load(&w.data_dir()).unwrap(), Some(handoff));
    assert!(RunnerReport::load(&w.data_dir()).unwrap().is_none());
}

#[test]
fn handoff_observation_refuses_oversized_or_nonregular_records() {
    let w = World::new();
    let handoff = w.handoff(HandoffAction::Recover);
    let path = state_dir(&w.data_dir()).join("runner-handoff.json");
    let mut bytes = vec![b' '; 256 * 1024];
    bytes.extend(serde_json::to_vec(&handoff).unwrap());
    std::fs::write(&path, bytes).unwrap();
    assert!(Handoff::load(&w.data_dir()).is_err());
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(Handoff::load(&w.data_dir()).is_err());
}

fn publish(
    transport: &FakeTransport,
    secrets: &FakeSecretStore,
    label: &ReleaseKeyLabel,
    build: u64,
) {
    use zip::write::SimpleFileOptions;
    let id = identity(build);
    let info = json!({
        "schema": 1, "product": id.product, "app_id": id.app_id,
        "version": id.version, "build": id.build, "target": id.target,
    })
    .to_string();
    let files: [(&str, &[u8]); 3] = [
        ("BroScribe/build-info.json", info.as_bytes()),
        ("BroScribe/signature", SIGNER.as_bytes()),
        ("BroScribe/broscribe.exe", b"MZ synthetic"),
    ];
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .add_directory("BroScribe/", SimpleFileOptions::default())
        .unwrap();
    let mut installed = 0u64;
    for (name, body) in files {
        writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(body).unwrap();
        installed += body.len() as u64;
    }
    let bytes = writer.finish().unwrap().into_inner();
    let now = Utc::now();
    let manifest = json!({
        "format": MANIFEST_FORMAT,
        "schema": 1,
        "product": "BroScribe",
        "channel": "stable",
        "version": id.version,
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
            "root": "BroScribe",
            "entries": 4,
            "installed_size": installed,
        }],
    });
    let signed = sign_manifest(
        secrets,
        std::slice::from_ref(label),
        &manifest.to_string(),
        now,
        &Cancel::new(),
    )
    .unwrap();
    transport.serve(FEED, signed.envelope.into_bytes());
    transport.serve(ARTIFACT, bytes);
}

#[test]
fn a_handed_off_install_replaces_after_exit_and_restarts_the_agent() {
    let world = World::new();
    world.stage();
    let handoff = world.handoff(HandoffAction::Install {
        expected: world.staged_expectation(),
    });
    let agent = Arc::clone(&world.agent);
    let listening = move |_: &Path| -> Listening { Ok(agent.listening.load(Ordering::SeqCst)) };
    let report = world.run(&handoff, &listening);

    assert_eq!(
        report.outcome,
        RunnerOutcome::Installed {
            from: identity(10),
            to: identity(11)
        }
    );
    assert_eq!(report.fence, 7);
    assert_eq!(report.agent_start_error, None);
    assert_eq!(world.installed(), identity(11));
    assert_eq!(
        world.agent.stops.load(Ordering::SeqCst),
        1,
        "orderly stop first"
    );
    assert_eq!(
        world.agent.starts.load(Ordering::SeqCst),
        1,
        "started on the new build"
    );
    assert_eq!(Handoff::load(&world.data_dir()).unwrap(), None);
    assert_eq!(RunnerReport::load(&world.data_dir()).unwrap(), Some(report));

    // Rolling back is handed over the same way.
    let handoff = world.handoff(HandoffAction::Rollback {
        expected: world.awaiting_expectation(),
    });
    let report = world.run(&handoff, &listening);
    assert_eq!(
        report.outcome,
        RunnerOutcome::RolledBack {
            restored: identity(10)
        }
    );
    assert_eq!(world.installed(), identity(10));
    assert_eq!(world.updater().status().unwrap(), UpdateStatus::Idle);
}

#[test]
fn an_agent_that_answers_again_stops_the_replacement() {
    let world = World::new();
    world.stage();
    let handoff = world.handoff(HandoffAction::Install {
        expected: world.staged_expectation(),
    });
    // Stopped for the wait, back by the time the lease is confirmed.
    let probes = AtomicUsize::new(0);
    let listening = |_: &Path| -> Listening { Ok(probes.fetch_add(1, Ordering::SeqCst) > 0) };
    let report = world.run(&handoff, &listening);
    match &report.outcome {
        RunnerOutcome::Failed { code, .. } => assert_eq!(code, "update_admission_lost"),
        other => panic!("expected a failure, got {other:?}"),
    }
    assert_eq!(world.installed(), identity(10), "nothing was replaced");
    assert!(matches!(
        world.updater().status().unwrap(),
        UpdateStatus::Staged { .. }
    ));
    assert_eq!(
        world.agent.starts.load(Ordering::SeqCst),
        1,
        "the agent runs again"
    );
}

#[test]
fn a_command_that_never_exits_leaves_everything_untouched() {
    let world = World::new();
    world.stage();
    world.probe.set_alive(COMMAND_PID, true);
    let handoff = world.handoff(HandoffAction::Install {
        expected: world.staged_expectation(),
    });
    let listening = |_: &Path| -> Listening { Ok(false) };
    let report = world.run(&handoff, &listening);
    match &report.outcome {
        RunnerOutcome::Failed { code, .. } => assert_eq!(code, "update_runner_timeout"),
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert_eq!(
        world.agent.stops.load(Ordering::SeqCst),
        0,
        "the agent was never stopped"
    );
    assert_eq!(world.installed(), identity(10));
}

#[test]
fn a_handoff_for_another_update_installs_nothing() {
    let world = World::new();
    world.stage();
    let handoff = world.handoff(HandoffAction::Install {
        expected: UpdateExpectation {
            to: identity(12),
            ..world.staged_expectation()
        },
    });
    let agent = Arc::clone(&world.agent);
    let listening = move |_: &Path| -> Listening { Ok(agent.listening.load(Ordering::SeqCst)) };
    let report = world.run(&handoff, &listening);
    match &report.outcome {
        RunnerOutcome::Failed { code, .. } => assert_eq!(code, "update_changed"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(world.installed(), identity(10));
    assert_eq!(
        world.agent.stops.load(Ordering::SeqCst),
        0,
        "checked before the agent is stopped"
    );
    assert!(matches!(
        world.updater().status().unwrap(),
        UpdateStatus::Staged { .. }
    ));
}

#[test]
fn the_runner_refuses_an_installation_that_contains_the_home() {
    let world = World::new();
    world.stage();
    let mut handoff = world.handoff(HandoffAction::Install {
        expected: world.staged_expectation(),
    });
    handoff.contained_paths = vec![world.root.join("home")];
    let report = world.run(&handoff, &|_| Ok(false));
    assert!(matches!(
        report.outcome,
        RunnerOutcome::Failed { code, .. } if code == broscribe_updates::codes::LOCATION_UNSUPPORTED
    ));
    assert_eq!(world.installed(), identity(10));
}

/// The command is gone, but the app it was started from (or any other
/// process running from the installation) still runs: nothing moves until
/// it exits, and the agent runs again meanwhile.
#[test]
fn the_runner_waits_for_every_process_running_from_the_installation() {
    let world = World::new();
    world.stage();
    world.platform.set_processes(vec![InstallationProcess {
        pid: 4_100,
        image: world.root.join("BroScribe.exe"),
    }]);
    let agent = Arc::clone(&world.agent);
    let listening = move |_: &Path| -> Listening { Ok(agent.listening.load(Ordering::SeqCst)) };
    let expected = world.staged_expectation();
    let handoff = world.handoff(HandoffAction::Install {
        expected: expected.clone(),
    });
    let report = world.run(&handoff, &listening);
    match &report.outcome {
        RunnerOutcome::Failed { code, .. } => assert_eq!(code, "update_runner_timeout"),
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert_eq!(world.installed(), identity(10), "nothing was replaced");
    assert!(matches!(
        world.updater().status().unwrap(),
        UpdateStatus::Staged { .. }
    ));
    assert_eq!(report.agent_start_error, None);
    assert!(
        world.agent.listening.load(Ordering::SeqCst),
        "the agent runs again"
    );

    world.platform.set_processes(Vec::new());
    let handoff = world.handoff(HandoffAction::Install { expected });
    let report = world.run(&handoff, &listening);
    assert_eq!(
        report.outcome,
        RunnerOutcome::Installed {
            from: identity(10),
            to: identity(11)
        }
    );
    assert_eq!(world.installed(), identity(11));
}

#[test]
fn detached_runner_persists_secondary_update_state_warnings() {
    let world = World::new();
    world.stage();
    let secondary = world.tmp.path().join("secondary/updates");
    std::fs::create_dir_all(&secondary).unwrap();
    std::fs::write(
        secondary.join("installed-inventory.json"),
        "invalid synthetic state",
    )
    .unwrap();
    let mut handoff = world.handoff(HandoffAction::Install {
        expected: world.staged_expectation(),
    });
    handoff
        .profile_dirs
        .push(secondary.parent().unwrap().to_path_buf());
    handoff.wait_pids.clear();
    handoff.save().unwrap();
    let report = world.run(&handoff, &|_| {
        Ok(world.agent.listening.load(Ordering::SeqCst))
    });
    assert!(
        matches!(report.outcome, RunnerOutcome::Installed { .. }),
        "{report:?}"
    );
    assert_eq!(report.warnings.len(), 1);
    assert!(broscribe_platform::same_path(
        &report.warnings[0].state_dir,
        &secondary
    ));
    assert!(
        report.warnings[0]
            .message
            .contains("installed-inventory.json")
    );
    assert_eq!(RunnerReport::load(&world.data_dir()).unwrap(), Some(report));
}
