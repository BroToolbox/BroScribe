//! Actual cold agent entry point, private synthetic profiles only.
use broscribe_core::maintenance::{Mode, ModeChange};
use broscribe_ipc::{AgentPing, AgentReadiness, Call, Request};
use broscribe_platform_native::ipc::{CallError, Client, Endpoint};
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn snapshot(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn visit(root: &Path, dir: &Path, out: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                out.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    visit(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn cold_maintenance_rejects_fast_paths_without_opening_or_repairing_data() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let library = tmp.path().join("library");
    std::fs::create_dir_all(library.join(".queue")).unwrap();
    std::fs::create_dir_all(data.join("journals/orphan")).unwrap();
    std::fs::write(data.join("config.toml"), b"invalid = [config").unwrap();
    std::fs::write(
        data.join("journals/orphan/journal.jsonl"),
        b"synthetic unfinished journal\n",
    )
    .unwrap();
    std::fs::write(library.join(".queue/pending.json"), b"synthetic queued job").unwrap();
    std::fs::write(library.join("media.wav"), b"synthetic media bytes").unwrap();
    let before = snapshot(&library);
    ModeChange::acquire(&data)
        .unwrap()
        .set(Mode::Maintenance)
        .unwrap();
    let journal = std::fs::read(data.join("journals/orphan/journal.jsonl")).unwrap();
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_broscribed"))
            .arg("--data-dir")
            .arg("data")
            .arg("--library")
            .arg("library")
            .arg("--worker")
            .arg(tmp.path().join("must-not-be-launched"))
            .env(
                "BROSCRIBE_DATA_DIR",
                tmp.path().join("wrong-inherited-profile"),
            )
            .current_dir(tmp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let endpoint = Endpoint::new(&data);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut client = loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "agent exited before serving control"
        );
        // The connect timeout also configures every RPC on this connection.
        // Give the child time to be scheduled during parallel workspace tests.
        if let Ok(Some(client)) = Client::connect_daemon(&endpoint, Duration::from_secs(2)) {
            break client;
        }
        assert!(Instant::now() < deadline, "control agent did not start");
        std::thread::sleep(Duration::from_millis(10));
    };
    let ping = AgentPing::from_value(client.call(Call::Ping).unwrap()).unwrap();
    assert_eq!(ping.mode, Mode::Maintenance);
    assert_eq!(ping.readiness, AgentReadiness::MaintenanceControlReady);
    assert_eq!(ping.pid, child.0.id());
    assert!(
        ModeChange::acquire(&data).is_err(),
        "serving agent must retain admission"
    );
    for call in [
        Call::Status,
        Call::Subscribe,
        Call::RequestStatus {
            request_id: Request::new(Call::Ping).id,
        },
        Call::AppShow,
        Call::InstallAcquire,
        Call::InstallConfirm { fence: 1 },
        Call::InstallRelease { fence: 1 },
        Call::RecordStart(Default::default()),
        Call::PermissionsRequest {
            permission: "microphone".into(),
        },
        Call::KickQueue,
    ] {
        match client.call(call) {
            Err(CallError::Remote(error)) => assert_eq!(error.code, "maintenance"),
            other => panic!("expected typed maintenance, got {other:?}"),
        }
        // Subscribe must not turn this connection into an event stream.
        AgentPing::from_value(client.call(Call::Ping).unwrap()).unwrap();
    }
    assert_eq!(client.call(Call::Shutdown).unwrap()["stopping"], true);
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "control shutdown did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(snapshot(&library), before);
    assert_eq!(
        std::fs::read(data.join("journals/orphan/journal.jsonl")).unwrap(),
        journal
    );
    assert!(!data.join("cache").exists());
    assert!(!data.join("accounts").exists());
    assert_eq!(
        std::fs::read(data.join("config.toml")).unwrap(),
        b"invalid = [config"
    );
    assert!(!tmp.path().join("wrong-inherited-profile").exists());
    assert!(!library.join(".index").exists());
    assert!(!library.join("people").exists());
    assert!(!data.join("run/install-admission.json").exists());
    ModeChange::acquire(&data)
        .unwrap()
        .set(Mode::Normal)
        .unwrap();
}

#[test]
fn runtime_teardown_keeps_admission_until_blocking_writes_finish_on_success_and_error() {
    for fail in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let output = tmp.path().join("finished");
        let (parked_tx, parked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (returned_tx, returned_rx) = std::sync::mpsc::channel();
        let running_data = data.clone();
        let running_output = output.clone();
        let owner = std::thread::spawn(move || {
            let result = broscribed::server::run_admitted(&running_data, |_| async move {
                tokio::task::spawn_blocking(move || {
                    parked_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    std::fs::write(running_output, b"synthetic completed recovery").unwrap();
                });
                if fail {
                    Err(broscribe_core::Error::Internal(
                        "synthetic startup failure".into(),
                    ))
                } else {
                    Ok(())
                }
            });
            returned_tx.send(result.is_err()).unwrap();
        });
        parked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let returned_early = returned_rx.try_recv().is_ok();
        let refused = ModeChange::acquire(&data).is_err();
        let output_missing = !output.exists();
        release_tx.send(()).unwrap();
        assert_eq!(
            returned_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            fail
        );
        owner.join().unwrap();
        assert!(!returned_early, "runtime returned with a parked writer");
        assert!(refused, "teardown released admission before its writer");
        assert!(output_missing);
        assert_eq!(
            std::fs::read(output).unwrap(),
            b"synthetic completed recovery"
        );
        ModeChange::acquire(&data)
            .unwrap()
            .set(Mode::Maintenance)
            .unwrap();
    }
}
