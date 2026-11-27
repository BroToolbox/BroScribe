//! File locks: the single-writer library lock on `<library>/.lock` (design
//! §3.1 "Before M4"), the queue lock, and the desktop app's instance lock.
//!
//! A lock is an OS file lock on one open handle, so it must be taken at most
//! once per process at a time: use [`crate::Library::write`], which holds the
//! library lock for one write transaction only.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::{Error, Result, paths};

pub struct LibraryLock;

impl LibraryLock {
    pub const DEFAULT_WAIT: Duration = Duration::from_secs(5);

    /// Takes the exclusive lock, polling for up to `wait`, else `Error::LibraryBusy`.
    pub fn acquire(root: &Path, wait: Duration) -> Result<LockGuard> {
        acquire_file(&root.join(paths::LOCK_FILE), root, wait)
    }
}

/// Lease bookkeeping lock on `<library>/.queue/.lock`, separate from the
/// library lock so heartbeats and fresh claims can run during library writes.
///
/// Lock order is always library → queue: code holding the queue lock must
/// never take the library lock.
pub struct QueueLock;

impl QueueLock {
    /// Default wait: lease bookkeeping holds this lock only for one small
    /// file write.
    pub const DEFAULT_WAIT: Duration = Duration::from_secs(2);

    /// Takes the exclusive queue lock of the library at `root`, polling for up
    /// to `wait`, else `Error::LibraryBusy`.
    pub fn acquire(root: &Path, wait: Duration) -> Result<LockGuard> {
        let dir = root.join(paths::QUEUE_DIR);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io_at(&dir, e))?;
        acquire_file(&dir.join(paths::LOCK_FILE), &dir, wait)
    }

    /// [`QueueLock::acquire`] that creates nothing (for reads such as a dry
    /// run): `None` when the library has no queue lock file, in which case
    /// no job was ever queued (enqueuing takes this lock).
    pub fn acquire_existing(root: &Path, wait: Duration) -> Result<Option<LockGuard>> {
        let dir = root.join(paths::QUEUE_DIR);
        let path = dir.join(paths::LOCK_FILE);
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io_at(&path, e)),
        };
        lock_file(file, &path, &dir, wait).map(Some)
    }
}

/// Folder of run-time files inside the data folder; the agent's socket and
/// pid file live there too (`broscribe_platform_unix::RUN_DIR`).
const RUN_DIR: &str = "run";
const APP_LOCK_FILE: &str = "app.lock";

/// One desktop app per data folder: held by the running app for its whole
/// life; the OS releases it when the process ends, however it ends.
pub struct AppInstanceLock;

impl AppInstanceLock {
    /// Takes `<data_dir>/run/app.lock` without waiting; `Ok(None)` when
    /// another process holds it.
    pub fn try_acquire(data_dir: &Path) -> Result<Option<LockGuard>> {
        let dir = data_dir.join(RUN_DIR);
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(&dir).map_err(|e| Error::io_at(&dir, e))?;
        let path = dir.join(APP_LOCK_FILE);
        let file = open_lock_file(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(LockGuard { file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(Error::io_at(&path, e)),
        }
    }
}

/// Serializes binding an agent endpoint with stopping the profile's login
/// job. The agent releases it after binding, while `daemon stop` holds it
/// from before its first connection through unload. The inode stays in place
/// and the OS releases the reservation even if its owner exits unexpectedly.
pub struct DaemonStartupLock;

impl DaemonStartupLock {
    pub fn try_acquire(data_dir: &Path) -> Result<Option<LockGuard>> {
        let path = data_dir.join("daemon-start.lock");
        let file = open_lock_file(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(LockGuard { file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(Error::io_at(&path, e)),
        }
    }
}

const POLL: Duration = Duration::from_millis(50);

pub(crate) fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| Error::io_at(path, e))
}

fn acquire_file(path: &Path, busy: &Path, wait: Duration) -> Result<LockGuard> {
    lock_file(open_lock_file(path)?, path, busy, wait)
}

/// Locks `file` (at `path`), polling for up to `wait`, else `library_busy`
/// naming `busy`.
fn lock_file(file: File, path: &Path, busy: &Path, wait: Duration) -> Result<LockGuard> {
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(LockGuard { file }),
            Err(TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(Error::LibraryBusy(busy.display().to_string()));
                }
                std::thread::sleep(POLL.min(deadline - now));
            }
            Err(TryLockError::Error(e)) => return Err(Error::io_at(path, e)),
        }
    }
}

/// Proof that a lock (library, queue or app instance) is held; releases it on drop.
#[derive(Debug)]
pub struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_after_wait_then_free_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let guard = LibraryLock::acquire(dir.path(), Duration::ZERO).unwrap();
        let start = Instant::now();
        let err = LibraryLock::acquire(dir.path(), Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.code(), "library_busy");
        assert!(start.elapsed() >= Duration::from_millis(200));
        drop(guard);
        LibraryLock::acquire(dir.path(), Duration::ZERO).unwrap();
    }

    #[test]
    fn queue_lock_is_independent_of_library_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lib = LibraryLock::acquire(dir.path(), Duration::ZERO).unwrap();
        let q = QueueLock::acquire(dir.path(), Duration::ZERO).unwrap();
        assert!(dir.path().join(".queue/.lock").is_file());
        let err = QueueLock::acquire(dir.path(), Duration::ZERO).unwrap_err();
        assert_eq!(err.code(), "library_busy");
        drop((lib, q));
    }

    #[test]
    fn second_app_instance_is_refused_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let first = AppInstanceLock::try_acquire(&data).unwrap().unwrap();
        assert!(data.join("run/app.lock").is_file());
        let start = Instant::now();
        assert!(AppInstanceLock::try_acquire(&data).unwrap().is_none());
        assert!(start.elapsed() < Duration::from_secs(1), "zero wait");
        drop(first);
        assert!(AppInstanceLock::try_acquire(&data).unwrap().is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(data.join("run"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}
