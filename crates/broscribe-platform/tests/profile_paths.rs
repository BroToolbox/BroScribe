use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use broscribe_platform::fakes::FakePaths;
use broscribe_platform::paths::{ENV_DATA_DIR, resolve_paths_from};

const CHILD_TARGET: &str = "BROSCRIBE_TEST_PROFILE_TARGET";
const CONTROLLER: &str = "BROSCRIBE_TEST_PROFILE_WRITABLE_CWD";
const TEST: &str = "relative_override_keeps_effects_in_parent_profile_across_child_cwd";
const EFFECT: &str = "synthetic-profile-effect";

#[test]
fn relative_override_keeps_effects_in_parent_profile_across_child_cwd() {
    if let Some(target) = std::env::var_os(CHILD_TARGET) {
        std::fs::write(PathBuf::from(target).join(EFFECT), b"synthetic data").unwrap();
        return;
    }

    if std::env::var_os(CONTROLLER).is_none() {
        // The source tree may be read-only. Isolate cwd in a subprocess
        // instead of mutating the test suite's process-wide working directory.
        let writable_cwd = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CONTROLLER, "1")
            .current_dir(writable_cwd.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "profile controller failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let cwd = std::env::current_dir().unwrap();
    let profile = tempfile::Builder::new()
        .prefix(".broscribe-profile-test-")
        .tempdir_in(&cwd)
        .unwrap();
    let other_cwd = tempfile::tempdir().unwrap();
    let defaults = tempfile::tempdir().unwrap();
    let relative = profile.path().strip_prefix(&cwd).unwrap();
    let paths = resolve_paths_from(&FakePaths::new(defaults.path()), |key| {
        (key == ENV_DATA_DIR).then(|| OsString::from(relative))
    })
    .unwrap();

    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(CHILD_TARGET, &paths.data_dir)
        .current_dir(other_cwd.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child could not write the selected profile: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(profile.path().join(EFFECT)).unwrap(),
        b"synthetic data"
    );
    assert!(!defaults.path().join("data").join(EFFECT).exists());
}
