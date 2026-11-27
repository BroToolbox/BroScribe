//! Worker command builders must preserve the selected profile across child cwd changes.
use broscribed::WorkerCommand;
use std::{path::Path, process::Command};

#[path = "fixtures/compile_live_timing_worker.rs"]
mod timing_fixture;

#[test]
fn both_child_builders_write_only_the_explicit_profile_or_native_default() {
    let tmp = tempfile::tempdir().unwrap();
    let program = tmp
        .path()
        .join(format!("profile-fixture{}", std::env::consts::EXE_SUFFIX));
    timing_fixture::compile_live_timing_worker(&program);
    let cwd = tmp.path().join("unrelated-cwd");
    std::fs::create_dir(&cwd).unwrap();
    for asynchronous in [false, true] {
        for overridden in [false, true] {
            let root = tmp.path().join(format!("case-{asynchronous}-{overridden}"));
            let native = root.join("native");
            let explicit = root.join("explicit");
            let library = root.join("library");
            let inherited = root.join("wrong-inherited-profile");
            // A subprocess supplies inherited A without mutating the test runner's environment.
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "profile_builder_subprocess",
                    "--ignored",
                    "--nocapture",
                ])
                .env("BROSCRIBE_DATA_DIR", &inherited)
                .env("PROFILE_FIXTURE", &program)
                .env("PROFILE_NATIVE", &native)
                .env("PROFILE_EXPLICIT", &explicit)
                .env("PROFILE_LIBRARY", &library)
                .env("PROFILE_CWD", &cwd)
                .env("PROFILE_OVERRIDE", if overridden { "1" } else { "0" })
                .env("PROFILE_ASYNC", if asynchronous { "1" } else { "0" })
                .status()
                .unwrap();
            assert!(status.success());
            let (selected, untouched): (&Path, &Path) = if overridden {
                (&explicit, &native)
            } else {
                (&native, &explicit)
            };
            assert_eq!(
                std::fs::read(selected.join("child-effect")).unwrap(),
                b"synthetic profile write"
            );
            assert_eq!(
                std::fs::read(library.join("child-effect")).unwrap(),
                b"synthetic library write"
            );
            assert!(!untouched.exists());
            assert!(!inherited.exists());
        }
    }
    assert_eq!(std::fs::read_dir(cwd).unwrap().count(), 0);
}

#[test]
#[ignore = "subprocess entry point for inherited-profile regression"]
fn profile_builder_subprocess() {
    let path =
        |key| std::path::PathBuf::from(std::env::var_os(key).expect("subprocess fixture setting"));
    let worker = WorkerCommand {
        program: path("PROFILE_FIXTURE"),
        prefix: vec![
            "profile-effect".into(),
            path("PROFILE_NATIVE").into_os_string(),
        ],
        data_dir_override: (std::env::var("PROFILE_OVERRIDE").unwrap() == "1")
            .then(|| path("PROFILE_EXPLICIT")),
    };
    let library = path("PROFILE_LIBRARY");
    let cwd = path("PROFILE_CWD");
    let status = if std::env::var("PROFILE_ASYNC").unwrap() == "1" {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                worker
                    .tokio_command(&library, &[])
                    .current_dir(&cwd)
                    .status()
                    .await
                    .unwrap()
            })
    } else {
        worker
            .std_command(&library, &[])
            .current_dir(&cwd)
            .status()
            .unwrap()
    };
    assert!(status.success());
}
