//! Independent executable admission, using synthetic profiles and no models.
use broscribe_core::maintenance::{Mode, ModeChange, ModeLease};
use std::{
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

#[test]
fn every_worker_command_refuses_before_config_library_models_or_replay_files() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    ModeChange::acquire(&data)
        .unwrap()
        .set(Mode::Maintenance)
        .unwrap();
    std::fs::write(data.join("config.toml"), b"invalid = [config").unwrap();
    let library = tmp.path().join("must-not-open");
    for args in [
        vec!["run-one", "synthetic-job"],
        vec!["live", "synthetic-session"],
        vec!["live-speakers", "synthetic-session"],
        vec!["live-speakers-replay", "--pcm", "must-not-read.pcm"],
        vec!["retention"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_broscribe-worker"))
            .arg("--library")
            .arg(&library)
            .args(args)
            .env("BROSCRIBE_DATA_DIR", &data)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("(maintenance)"),
            "worker must report the maintenance error code, not a config/model failure"
        );
        assert!(
            output.stdout.is_empty(),
            "no misleading live model/loading event in maintenance"
        );
        assert!(!library.exists());
        assert!(!data.join("cache").exists());
    }
    assert_eq!(
        std::fs::read(data.join("config.toml")).unwrap(),
        b"invalid = [config"
    );
}

#[test]
fn independently_admitted_live_child_keeps_profile_fenced_after_parent_crashes() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let library = tmp.path().join("library");
    // The parent owns the same profile, but its crash must not un-fence its worker.
    // The worker inherits this pipe directly, so killing the parent does not end stdin.
    let mut child = ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "live_parent_subprocess",
                "--ignored",
                "--nocapture",
            ])
            .env("WORKER_TEST_DATA", &data)
            .env("WORKER_TEST_LIBRARY", &library)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !library.join(".queue").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "live child exited before parking"
        );
        assert!(
            Instant::now() < deadline,
            "worker did not open its synthetic library"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // Child::wait closes its stored stdin. Keep this writer outside Child
    // so only the explicit EOF below, not reaping the parent, ends the worker.
    let keep_input_open = child.0.stdin.take().unwrap();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(
        ModeChange::acquire(&data).is_err(),
        "surviving child must own admission independently"
    );
    // EOF releases the parked live worker without loading models or invoking a provider.
    drop(keep_input_open);
    loop {
        if let Ok(change) = ModeChange::acquire(&data) {
            change.set(Mode::Maintenance).unwrap();
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not release admission after EOF"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "subprocess parent for the real orphan-worker ownership regression"]
fn live_parent_subprocess() {
    let data = std::path::PathBuf::from(std::env::var_os("WORKER_TEST_DATA").unwrap());
    let library = std::env::var_os("WORKER_TEST_LIBRARY").unwrap();
    let _admission = ModeLease::acquire(&data, ModeLease::DEFAULT_WAIT).unwrap();
    let mut worker = Command::new(env!("CARGO_BIN_EXE_broscribe-worker"))
        .arg("--library")
        .arg(library)
        .args(["live-speakers", "synthetic-session"])
        .env("BROSCRIBE_DATA_DIR", data)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The supervising process is deliberately killed by the outer test.
    // If the worker unexpectedly exits first, make that failure observable.
    loop {
        assert!(
            worker.try_wait().unwrap().is_none(),
            "worker exited before parent crash"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
