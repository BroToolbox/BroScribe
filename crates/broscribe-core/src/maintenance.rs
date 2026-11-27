//! Offline per-profile maintenance admission. Take this lock before any
//! endpoint, configuration, library or subordinate lock. Never unlink its inode.
//! This coordinates cooperating actors, not outside writers replacing control
//! paths/inodes, including a path swap between metadata validation and open.

use std::fs::{self, File, TryLockError};
use std::io::{ErrorKind, Read as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, fsutil};

pub const REQUEST_FILE: &str = "maintenance.request";
pub const LOCK_FILE: &str = "maintenance.lock";
pub const ERROR_CODE: &str = "maintenance";
pub const BUSY_CODE: &str = "maintenance_busy";
const POLL: Duration = Duration::from_millis(50);

pub const PROFILE_MARKER: &str = ".broscribe-profile";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileMarker {
    schema: u32,
    id: String,
}

/// Read a marker without claiming the folder. Unknown, malformed and
/// non-regular markers do not identify a BroScribe profile.
pub fn profile_id(data_dir: &Path) -> Result<Option<String>> {
    const LIMIT: u64 = 256;
    let path = data_dir.join(PROFILE_MARKER);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io_at(&path, error)),
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if meta.file_attributes() & 0x400 != 0 {
            return Ok(None);
        }
    }
    if !meta.is_file() || meta.len() > LIMIT {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    File::open(&path)
        .and_then(|file| file.take(LIMIT + 1).read_to_end(&mut bytes))
        .map_err(|error| Error::io_at(&path, error))?;
    Ok(serde_json::from_slice::<ProfileMarker>(&bytes)
        .ok()
        .filter(|marker| {
            marker.schema == 1
                && marker.id.len() == 32
                && marker
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .map(|marker| marker.id))
}

fn claim_profile(data_dir: &Path) -> Result<()> {
    if profile_id(data_dir)?.is_some() {
        return Ok(());
    }
    let path = data_dir.join(PROFILE_MARKER);
    if regular_or_missing(&path)? {
        return Err(Error::InvalidInput(format!(
            "Invalid BroScribe profile marker at {}",
            path.display()
        )));
    }
    let marker = ProfileMarker {
        schema: 1,
        id: format!("{:032x}", rand::random::<u128>()),
    };
    let bytes = serde_json::to_vec(&marker).map_err(|error| Error::Internal(error.to_string()))?;
    fsutil::write_atomic_with_mode(&path, &bytes, Some(0o600))
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Normal,
    Maintenance,
}

#[derive(Debug)]
struct ModeLock {
    file: File,
    path: PathBuf,
}

impl ModeLock {
    fn open(data_dir: &Path) -> Result<Self> {
        if !data_dir.is_absolute() {
            return Err(Error::InvalidInput(
                "maintenance admission requires an absolute data directory".into(),
            ));
        }
        fsutil::create_dir_durable(data_dir)?;
        let path = data_dir.join(LOCK_FILE);
        regular_or_missing(&path)?;
        let file = crate::lock::open_lock_file(&path)?;
        if !file
            .metadata()
            .map_err(|e| Error::io_at(&path, e))?
            .is_file()
        {
            return Err(not_regular(&path));
        }
        Ok(Self { file, path })
    }

    fn data_dir(&self) -> &Path {
        // The path is always an absolute directory joined with LOCK_FILE.
        self.path.parent().expect("maintenance lock has a parent")
    }
}

/// Unlocks explicitly, as `LockGuard` does: flock locks belong to the open
/// file description, and a child forked by another thread holds a copy of
/// it until its exec closes it (close-on-exec). Closing only this descriptor
/// would leave the lock held through that copy; unlocking releases it.
impl Drop for ModeLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Shared admission for one profile, with its mode latched while locked.
/// Retain through every effect and runtime/thread teardown; children admit
/// independently. Mode changes must wait for this owner to finish.
#[derive(Debug)]
pub struct ModeLease {
    lock: ModeLock,
    mode: Mode,
}

impl ModeLease {
    pub const DEFAULT_WAIT: Duration = Duration::from_secs(5);

    pub fn acquire(data_dir: &Path, wait: Duration) -> Result<Self> {
        let lock = ModeLock::open(data_dir)?;
        let mut deadline = None;
        let mut needs_marker = false;
        loop {
            let result = if needs_marker {
                lock.file.try_lock()
            } else {
                lock.file.try_lock_shared()
            };
            match result {
                Ok(()) if needs_marker => {
                    claim_profile(lock.data_dir())?;
                    lock.file
                        .unlock()
                        .map_err(|e| Error::io_at(&lock.path, e))?;
                    needs_marker = false;
                }
                Ok(()) if profile_id(lock.data_dir())?.is_some() => break,
                Ok(()) => {
                    lock.file
                        .unlock()
                        .map_err(|e| Error::io_at(&lock.path, e))?;
                    needs_marker = true;
                }
                Err(TryLockError::WouldBlock) => {
                    let now = Instant::now();
                    let deadline = *deadline.get_or_insert_with(|| now + wait);
                    if now >= deadline {
                        return Err(busy());
                    }
                    std::thread::sleep(POLL.min(deadline - now));
                    // A concurrent initializer may have written the marker
                    // and subsequently retained a shared admission.
                    if needs_marker && profile_id(lock.data_dir())?.is_some() {
                        needs_marker = false;
                    }
                }
                Err(TryLockError::Error(e)) => return Err(Error::io_at(&lock.path, e)),
            }
        }
        let mode = read_mode(lock.data_dir())?;
        Ok(Self { lock, mode })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn data_dir(&self) -> &Path {
        self.lock.data_dir()
    }

    pub fn require_normal(&self) -> Result<()> {
        match self.mode {
            Mode::Normal => Ok(()),
            Mode::Maintenance => Err(Error::Service {
                code: ERROR_CODE,
                message: "BroScribe is in maintenance mode; close all clients, disable maintenance offline, then explicitly restart normal operation".into(),
            }),
        }
    }
}

/// Exclusive offline mode-control lock. The caller must also prove the legacy
/// endpoint absent and inspect updater/install holds read-only while this is
/// held, BEFORE calling `set`. This module cannot observe platform services.
#[derive(Debug)]
pub struct ModeChange {
    lock: ModeLock,
}

impl ModeChange {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        let lock = ModeLock::open(data_dir)?;
        match lock.file.try_lock() {
            Ok(()) => Ok(Self { lock }),
            Err(TryLockError::WouldBlock) => Err(busy()),
            Err(TryLockError::Error(e)) => Err(Error::io_at(&lock.path, e)),
        }
    }

    pub fn data_dir(&self) -> &Path {
        self.lock.data_dir()
    }

    /// Changes only the persisted request, never starts/stops any actor.
    /// A post-change sync error is ambiguous: inspect status, never roll back.
    pub fn set(&self, requested: Mode) -> Result<()> {
        let data_dir = self.data_dir();
        let current = read_mode(data_dir)?;
        let path = data_dir.join(REQUEST_FILE);
        let result = if current == requested {
            // Also completes a retry after a previous post-change sync error.
            fsutil::sync_dir(data_dir)
        } else {
            match requested {
                Mode::Maintenance => fsutil::write_atomic_with_mode(&path, b"", Some(0o600)),
                Mode::Normal => match fs::remove_file(&path) {
                    Ok(()) => fsutil::sync_dir(data_dir),
                    Err(e) if e.kind() == ErrorKind::NotFound => fsutil::sync_dir(data_dir),
                    Err(e) => Err(Error::io_at(&path, e)),
                },
            }
        };
        result.map_err(|error| Error::Service {
            code: error.code(),
            message: format!(
                "{error}; the maintenance request may already have changed; inspect maintenance status before proceeding"
            ),
        })
    }
}

fn read_mode(data_dir: &Path) -> Result<Mode> {
    Ok(if regular_or_missing(&data_dir.join(REQUEST_FILE))? {
        Mode::Maintenance
    } else {
        Mode::Normal
    })
}

fn regular_or_missing(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(not_regular(path)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

fn not_regular(path: &Path) -> Error {
    Error::InvalidInput(format!(
        "maintenance control path must be a regular file, not a symlink or other file type: {}",
        path.display()
    ))
}

fn busy() -> Error {
    Error::Service {
        code: BUSY_CODE,
        message: "maintenance mode is locked; close all BroScribe clients (including idle MCP servers) and stop the agent and workers before changing it".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_lease_is_released_while_a_copy_of_its_descriptor_is_open() {
        let profile = tempfile::tempdir().unwrap();
        let lease = ModeLease::acquire(profile.path(), Duration::ZERO).unwrap();
        // What a child forked by another thread holds until its exec.
        let copy = lease.lock.file.try_clone().unwrap();
        drop(lease);
        assert!(
            ModeChange::acquire(profile.path()).is_ok(),
            "a forked copy must not keep a dropped lease locked"
        );
        drop(copy);
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn admission_creates_one_private_profile_marker() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profile");
        assert_eq!(profile_id(&path).unwrap(), None);
        let a = ModeLease::acquire(&path, Duration::ZERO).unwrap();
        let id = profile_id(&path).unwrap().unwrap();
        let b = ModeLease::acquire(&path, Duration::ZERO).unwrap();
        assert_eq!(profile_id(a.data_dir()).unwrap(), Some(id.clone()));
        assert_eq!(profile_id(b.data_dir()).unwrap(), Some(id));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(path.join(PROFILE_MARKER))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
