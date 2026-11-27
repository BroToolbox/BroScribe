use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use broscribe_core::maintenance::{Mode, ModeChange, ModeLease};

const CHILD_ROOT: &str = "BROSCRIBE_TEST_MAINTENANCE_ROOT";
const CHILD_ROLE: &str = "BROSCRIBE_TEST_MAINTENANCE_ROLE";
const TEST: &str =
    "maintenance_excludes_live_processes_persists_after_crash_and_resumes_explicitly";

struct ChildOwner(Child);

impl ChildOwner {
    fn spawn(root: &Path, role: &str) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(CHILD_ROOT, root)
                .env(CHILD_ROLE, role)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        )
    }

    fn wait_ready(&mut self) {
        let mut stdout = self.0.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut byte = [0];
            // The test harness writes its own prefix before the child holds admission.
            let result = loop {
                if let Err(error) = stdout.read_exact(&mut byte) {
                    break Err(error);
                }
                if byte[0] == 0 {
                    break Ok(());
                }
            };
            let _ = tx.send(result);
        });
        let result = rx.recv_timeout(Duration::from_secs(10));
        if result.is_err() {
            let _ = self.0.kill();
        }
        reader.join().unwrap();
        result
            .expect("child admission handshake timed out")
            .unwrap();
    }

    fn wait_success(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "maintenance child failed: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "maintenance child timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ChildOwner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn maintenance_excludes_live_processes_persists_after_crash_and_resumes_explicitly() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = Path::new(&root);
        if std::env::var(CHILD_ROLE).unwrap() == "blocked" {
            assert_eq!(
                ModeLease::acquire(root, Duration::ZERO).unwrap_err().code(),
                "maintenance_busy"
            );
            return;
        }
        let lease = ModeLease::acquire(root, Duration::ZERO).unwrap();
        lease.require_normal().unwrap();
        std::io::stdout().write_all(&[0]).unwrap();
        std::io::stdout().flush().unwrap();
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte).unwrap();
        drop(lease);
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let mut actor = ChildOwner::spawn(root.path(), "hold");
    actor.wait_ready();
    assert_eq!(
        ModeChange::acquire(root.path()).unwrap_err().code(),
        "maintenance_busy"
    );

    // A crash, not Rust unwinding, must release the OS lease.
    actor.0.kill().unwrap();
    actor.0.wait().unwrap();
    let change = ModeChange::acquire(root.path()).unwrap();
    change.set(Mode::Maintenance).unwrap();
    drop(change);

    let maintenance = ModeLease::acquire(root.path(), Duration::ZERO).unwrap();
    assert_eq!(maintenance.mode(), Mode::Maintenance);
    assert_eq!(
        maintenance.require_normal().unwrap_err().code(),
        "maintenance"
    );
    assert_eq!(
        ModeChange::acquire(root.path()).unwrap_err().code(),
        "maintenance_busy"
    );
    drop(maintenance);

    let change = ModeChange::acquire(root.path()).unwrap();
    change.set(Mode::Normal).unwrap();
    // Disabling must not unlink the locked inode and let a new actor bypass it.
    let mut blocked = ChildOwner::spawn(root.path(), "blocked");
    blocked.wait_success();
    drop(change);

    let resumed = ModeLease::acquire(root.path(), Duration::ZERO).unwrap();
    assert_eq!(resumed.mode(), Mode::Normal);
    resumed.require_normal().unwrap();
}

#[test]
fn library_writer_clone_keeps_admission_after_frontend_owner_drops() {
    let profile = tempfile::tempdir().unwrap();
    let content = tempfile::tempdir().unwrap();
    let admission =
        std::sync::Arc::new(ModeLease::acquire(profile.path(), Duration::ZERO).unwrap());
    let library = broscribe_core::Library::open(content.path())
        .unwrap()
        .with_mode_lease(admission.clone())
        .unwrap();
    let pending = library.clone();
    drop(library);
    drop(admission);

    let effect = content.path().join("synthetic-effect");
    let written = effect.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        pending.write(|_| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            broscribe_core::fsutil::write_atomic(&effect, b"synthetic data")
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(
        ModeChange::acquire(profile.path()).unwrap_err().code(),
        "maintenance_busy"
    );
    release_tx.send(()).unwrap();
    writer.join().unwrap().unwrap();
    assert_eq!(std::fs::read(written).unwrap(), b"synthetic data");
    ModeChange::acquire(profile.path())
        .unwrap()
        .set(Mode::Maintenance)
        .unwrap();
}

#[test]
fn invalid_control_paths_never_admit_normal_work_or_remove_unknown_entries() {
    use broscribe_core::maintenance::{LOCK_FILE, REQUEST_FILE};
    let profile = tempfile::tempdir().unwrap();
    let marker = profile.path().join(REQUEST_FILE);
    std::fs::create_dir(&marker).unwrap();
    assert_eq!(
        ModeLease::acquire(profile.path(), Duration::ZERO)
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    let change = ModeChange::acquire(profile.path()).unwrap();
    for requested in [Mode::Normal, Mode::Maintenance] {
        assert_eq!(change.set(requested).unwrap_err().code(), "invalid_input");
    }
    assert!(marker.is_dir());
    drop(change);

    let blocked = tempfile::tempdir().unwrap();
    let lock = blocked.path().join(LOCK_FILE);
    std::fs::create_dir(&lock).unwrap();
    assert_eq!(
        ModeLease::acquire(blocked.path(), Duration::ZERO)
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    assert_eq!(
        ModeChange::acquire(blocked.path()).unwrap_err().code(),
        "invalid_input"
    );
    assert!(lock.is_dir());

    let not_a_folder = profile.path().join("data-is-a-file");
    std::fs::write(&not_a_folder, b"synthetic protected data").unwrap();
    assert_eq!(
        ModeLease::acquire(&not_a_folder, Duration::ZERO)
            .unwrap_err()
            .code(),
        "io"
    );
    assert_eq!(ModeChange::acquire(&not_a_folder).unwrap_err().code(), "io");
    assert_eq!(
        std::fs::read(not_a_folder).unwrap(),
        b"synthetic protected data"
    );
}

#[cfg(unix)]
#[test]
fn control_symlinks_are_not_followed_and_their_targets_are_preserved() {
    use broscribe_core::maintenance::{LOCK_FILE, REQUEST_FILE};
    for name in [REQUEST_FILE, LOCK_FILE] {
        let profile = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("synthetic-target");
        std::fs::write(&target, b"synthetic protected target").unwrap();
        let link = profile.path().join(name);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            ModeLease::acquire(profile.path(), Duration::ZERO)
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        if name == REQUEST_FILE {
            let change = ModeChange::acquire(profile.path()).unwrap();
            assert_eq!(
                change.set(Mode::Normal).unwrap_err().code(),
                "invalid_input"
            );
        } else {
            assert_eq!(
                ModeChange::acquire(profile.path()).unwrap_err().code(),
                "invalid_input"
            );
        }
        assert!(
            std::fs::symlink_metadata(link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(target).unwrap(),
            b"synthetic protected target"
        );
    }
}
