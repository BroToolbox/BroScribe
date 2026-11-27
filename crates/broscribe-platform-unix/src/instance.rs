//! Single instance: a locked pid file in `<data_dir>/run/`, then the
//! listening socket (mode 0600) in its private folder.
//!
//! The pid-file lock (`flock`, released by the OS when the process dies) makes
//! the daemon unique per data directory. Linux also holds a persistent
//! endpoint-file lock before replacing a stale socket.

use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use crate::dirs::{PID_NAME, RUN_DIR, SocketLocation, check_private_dir, ensure_private_dir};

#[derive(Debug)]
pub enum InstanceError {
    /// Another daemon holds the pid file (its pid, when readable).
    AlreadyRunning {
        pid: Option<u32>,
    },
    Io(io::Error),
}

impl fmt::Display for InstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstanceError::AlreadyRunning { pid: Some(pid) } => {
                write!(f, "broscribed is already running (pid {pid})")
            }
            InstanceError::AlreadyRunning { pid: None } => {
                write!(f, "broscribed is already running")
            }
            InstanceError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InstanceError {}

impl From<io::Error> for InstanceError {
    fn from(e: io::Error) -> Self {
        InstanceError::Io(e)
    }
}

/// An exclusively locked pid file holding this process's ID; removed and
/// unlocked on drop.
#[derive(Debug)]
pub struct PidFile {
    file: File,
    path: PathBuf,
}

impl PidFile {
    pub fn acquire(path: &Path) -> Result<Self, InstanceError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut text = String::new();
                let pid = file
                    .read_to_string(&mut text)
                    .ok()
                    .and_then(|_| text.trim().parse().ok());
                return Err(InstanceError::AlreadyRunning { pid });
            }
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }
        // Owned from here on, so a failed write still unlocks (and removes
        // the half-written file) on drop.
        let mut pid = Self {
            file,
            path: path.to_path_buf(),
        };
        pid.file.set_len(0)?;
        pid.file
            .write_all(format!("{}\n", std::process::id()).as_bytes())?;
        pid.file.sync_all()?;
        Ok(pid)
    }

    /// The pid recorded in the pid file at `path`, if any (the process may
    /// be gone).
    pub fn read_pid(path: &Path) -> io::Result<Option<u32>> {
        match fs::read_to_string(path) {
            Ok(text) => Ok(text.trim().parse().ok()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = self.file.unlock();
    }
}

/// The daemon's claim on its data folder: pid file plus listening socket.
/// Dropping removes the socket, then the pid file.
#[derive(Debug)]
pub struct Instance {
    listener: UnixListener,
    socket: SocketLocation,
    _pid_file: PidFile,
    // Never unlink this lock file: a waiter may already hold its inode.
    #[cfg(target_os = "linux")]
    _endpoint_lock: EndpointLock,
}

impl Instance {
    /// Probe the same pid-file lock held by current and older agents. Opening
    /// an existing file and dropping the handle leaves its contents and path alone.
    pub fn is_held(data_dir: &Path) -> io::Result<bool> {
        let run = data_dir.join(RUN_DIR);
        let file = check_private_dir(&run).and_then(|()| {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(run.join(PID_NAME))
        });
        let file = match file {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "agent pid file is not a regular file",
            ));
        }
        match file.try_lock() {
            Ok(()) => Ok(false),
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// Locks `<data_dir>/run/broscribed.pid` and listens on the chosen socket.
    pub fn acquire_at(data_dir: &Path, socket: SocketLocation) -> Result<Self, InstanceError> {
        let run = data_dir.join(RUN_DIR);
        ensure_private_dir(&run)?;
        if socket.dir != run {
            ensure_private_dir(&socket.dir)?;
        }
        let pid_file = PidFile::acquire(&run.join(PID_NAME))?;
        #[cfg(target_os = "linux")]
        let endpoint_lock = lock_endpoint(&socket.path)?;
        remove_stale_socket(&socket.path)?;
        let listener = UnixListener::bind(&socket.path)?;
        fs::set_permissions(&socket.path, Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            socket,
            _pid_file: pid_file,
            #[cfg(target_os = "linux")]
            _endpoint_lock: endpoint_lock,
        })
    }

    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    pub fn socket(&self) -> &SocketLocation {
        &self.socket
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket.path);
    }
}

/// Linux endpoints can be shared by data-folder aliases or explicit socket
/// locations. Claim the endpoint itself before deciding its socket is stale.
#[cfg(target_os = "linux")]
fn lock_endpoint(socket: &Path) -> Result<EndpointLock, InstanceError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(socket.with_extension("lock"))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            let mut text = String::new();
            let pid = file
                .read_to_string(&mut text)
                .ok()
                .and_then(|_| text.trim().parse().ok());
            return Err(InstanceError::AlreadyRunning { pid });
        }
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }
    let mut lock = EndpointLock(file);
    lock.0.set_len(0)?;
    writeln!(lock.0, "{}", std::process::id())?;
    Ok(lock)
}

/// A held endpoint lock. Unlocks explicitly on drop, as [`PidFile`] does:
/// flock locks belong to the open file description, and a child forked by
/// another thread holds a copy of it until its exec closes it
/// (close-on-exec). Closing only this handle would leave the lock held.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct EndpointLock(File);

#[cfg(target_os = "linux")]
impl Drop for EndpointLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Removes a leftover socket file; anything else at that path is an error.
fn remove_stale_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(path),
        Ok(_) => Err(io::Error::new(
            ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket", path.display()),
        )),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;
    use crate::dirs::SOCKET_NAME;

    fn location(dir: &Path) -> SocketLocation {
        SocketLocation::in_dir(dir.join(RUN_DIR), SOCKET_NAME, false).unwrap()
    }

    #[test]
    fn second_instance_is_refused_until_the_first_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Instance::acquire_at(tmp.path(), location(tmp.path())).unwrap();
        let mode = fs::symlink_metadata(&first.socket().path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        match Instance::acquire_at(tmp.path(), location(tmp.path())) {
            Err(InstanceError::AlreadyRunning { pid }) => {
                assert_eq!(pid, Some(std::process::id()));
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        let socket = first.socket().path.clone();
        drop(first);
        assert!(!socket.exists());
        Instance::acquire_at(tmp.path(), location(tmp.path())).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_endpoint_cannot_be_replaced_by_another_data_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let first_dir = temporary.path().join("one");
        let second_dir = temporary.path().join("two");
        let endpoint = location(temporary.path());
        let first = Instance::acquire_at(&first_dir, endpoint.clone()).unwrap();
        assert!(matches!(
            Instance::acquire_at(&second_dir, endpoint.clone()),
            Err(InstanceError::AlreadyRunning { .. })
        ));
        // Refusing the second instance must leave the first listener reachable.
        std::os::unix::net::UnixStream::connect(&endpoint.path).unwrap();
        drop(first);
        Instance::acquire_at(&second_dir, endpoint).unwrap();
    }

    #[test]
    fn stale_socket_is_replaced_but_other_files_are_not() {
        // The crashed daemon is simulated by binding and dropping a listener
        // here. A sibling test that forks between the bind and the drop holds
        // a copy of the descriptor for a moment, so the socket still accepts.
        // Run alone so the dropped listener is really gone.
        const CHILD: &str = "BROSCRIBE_TEST_ISOLATED_STALE_DAEMON_SOCKET";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .env(CHILD, "1")
                .args([
                    "--exact",
                    "instance::tests::stale_socket_is_replaced_but_other_files_are_not",
                    "--test-threads=1",
                ])
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let loc = location(tmp.path());
        ensure_private_dir(&loc.dir).unwrap();
        // A socket left by a crashed daemon.
        drop(UnixListener::bind(&loc.path).unwrap());
        assert!(loc.path.exists());
        drop(Instance::acquire_at(tmp.path(), loc.clone()).unwrap());
        fs::write(&loc.path, b"not a socket").unwrap();
        assert!(Instance::acquire_at(tmp.path(), loc).is_err());
    }
}
