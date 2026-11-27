//! Windows half: per-user named pipes with a current-user DACL, peer SID
//! checks on both ends, console control events.

use std::io::{self, ErrorKind};
use std::os::windows::io::AsRawHandle;
use std::process::ChildStdin;
use std::time::{Duration, Instant};

pub(super) use broscribe_platform_windows::agent_instance_held;
use broscribe_platform_windows::{PipeSecurity, WinPipeStream};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};

use super::framing;
use super::{Endpoint, ListenError};

/// `ERROR_ACCESS_DENIED`: a first instance of this pipe name already exists.
const ACCESS_DENIED: i32 = 5;
/// `ERROR_FILE_NOT_FOUND` / `ERROR_PATH_NOT_FOUND`: no daemon pipe.
const NOT_FOUND: [i32; 2] = [2, 3];
/// `ERROR_NO_DATA`: the client connected and left before `connect` returned.
const NO_DATA: i32 = 232;
/// Longest pause while waiting for a nonblocking pipe to accept writes.
const MAX_STDIN_PAUSE: Duration = Duration::from_millis(5);

pub(super) struct Wire(WinPipeStream);

impl framing::Wire for Wire {
    fn read_some(&mut self, buf: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
        self.0.read_within(buf, deadline)
    }

    fn write_some_by(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        self.0.write_some_within(bytes, deadline)
    }
}

/// Connects and authenticates the server's user before anything is sent.
pub(super) fn connect(endpoint: &Endpoint, deadline: Instant) -> io::Result<Wire> {
    WinPipeStream::connect(endpoint.data_dir(), deadline).map(Wire)
}

pub(super) fn is_absent(error: &io::Error) -> bool {
    error.kind() == ErrorKind::NotFound
        || error
            .raw_os_error()
            .is_some_and(|code| NOT_FOUND.contains(&code))
}

/// The pipe's ACL and server SID check are verified on every connection.
pub(super) fn precheck(_endpoint: &Endpoint) -> io::Result<()> {
    Ok(())
}

pub(super) struct ServerListener {
    name: String,
    security: PipeSecurity,
    /// The instance the next client connects to; always one exists, so the
    /// first-instance claim on the name is never released while listening.
    next: NamedPipeServer,
}

impl ServerListener {
    pub(super) fn bind(endpoint: &Endpoint) -> Result<Self, ListenError> {
        let name = broscribe_platform_windows::pipe_name(endpoint.data_dir())?;
        let mut security = PipeSecurity::current_user()?;
        let next = create(&name, &mut security, true).map_err(|e| {
            if e.raw_os_error() == Some(ACCESS_DENIED) {
                ListenError::AlreadyRunning { pid: None }
            } else {
                ListenError::Io(e)
            }
        })?;
        Ok(Self {
            name,
            security,
            next,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<ServerStream> {
        loop {
            let connected = self.next.connect().await;
            // Hand the connected instance over only once its successor
            // exists, keeping the name owned throughout.
            let fresh = create(&self.name, &mut self.security, false)?;
            let server = std::mem::replace(&mut self.next, fresh);
            match connected {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(NO_DATA) => continue,
                Err(e) => return Err(e),
            }
            if broscribe_platform_windows::verify_pipe_client(server.as_raw_handle()).is_ok() {
                return Ok(server);
            }
        }
    }
}

fn create(name: &str, security: &mut PipeSecurity, first: bool) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .access_inbound(true)
        .access_outbound(true)
        .pipe_mode(PipeMode::Byte);
    // SAFETY: `security` holds a valid SECURITY_ATTRIBUTES (current-user
    // DACL) that outlives this call.
    unsafe { options.create_with_security_attributes_raw(name, security.as_ptr()) }
}

pub(super) type ServerStream = NamedPipeServer;
pub(super) type ReadHalf = tokio::io::ReadHalf<NamedPipeServer>;
pub(super) type WriteHalf = tokio::io::WriteHalf<NamedPipeServer>;

pub(super) fn split(stream: ServerStream) -> (ReadHalf, WriteHalf) {
    tokio::io::split(stream)
}

pub(super) async fn termination() -> io::Result<()> {
    use tokio::signal::windows;
    let mut interrupt = windows::ctrl_c()?;
    let mut close = windows::ctrl_close()?;
    let mut logoff = windows::ctrl_logoff()?;
    let mut shutdown = windows::ctrl_shutdown()?;
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = close.recv() => {}
        _ = logoff.recv() => {}
        _ = shutdown.recv() => {}
    }
    Ok(())
}

pub(super) fn is_resource_exhaustion(error: &io::Error) -> bool {
    broscribe_platform_windows::is_resource_exhaustion(error)
}

/// The per-user profile folders already carry user-only ACLs; refuse a
/// reparse point (symlink/junction) that could redirect into another tree.
pub(super) fn ensure_private_dir(dir: &std::path::Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is not a plain folder", dir.display()),
        ));
    }
    Ok(())
}

pub(super) fn set_stdin_nonblocking(stdin: &ChildStdin) -> io::Result<()> {
    broscribe_platform_windows::set_pipe_nonblocking(stdin.as_raw_handle())
}

pub(super) fn write_stdin(stdin: &mut ChildStdin, bytes: &[u8]) -> io::Result<usize> {
    broscribe_platform_windows::write_nonblocking(stdin.as_raw_handle(), bytes)
}

/// Anonymous pipes offer no readiness wait: a short pause, then the caller
/// retries the write (`true` means "try again", not a guarantee).
pub(super) fn wait_stdin_writable(_stdin: &ChildStdin, timeout: Duration) -> io::Result<bool> {
    std::thread::sleep(timeout.min(MAX_STDIN_PAUSE));
    Ok(true)
}
