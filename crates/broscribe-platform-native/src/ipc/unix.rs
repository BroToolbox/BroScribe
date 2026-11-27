//! Unix half: sockets in a private folder, peer-UID checks, process signals.

use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ChildStdin;
use std::time::{Duration, Instant};

use broscribe_platform_unix::{
    Instance, InstanceError, PeerCredentials, check_private_dir, verify_same_user,
};

use super::framing;
use super::{Endpoint, ListenError};

/// A nonblocking client socket; deadlines are enforced with `poll`, never
/// with per-call socket options (macOS refuses `SO_RCVTIMEO`/`SO_SNDTIMEO`
/// with `EINVAL` once the peer has closed, hiding a response already sent).
pub(super) struct Wire(UnixStream);

fn timed_out() -> io::Error {
    io::Error::new(ErrorKind::TimedOut, "the daemon did not answer in time")
}

/// `poll` milliseconds until `deadline` (at least 1 while time remains,
/// rounded up so a sub-millisecond rest is still waited); -1 without one.
fn poll_millis(deadline: Option<Instant>) -> io::Result<i32> {
    let Some(deadline) = deadline else {
        return Ok(-1);
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(timed_out());
    }
    Ok(left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32)
}

impl framing::Wire for Wire {
    fn read_some(&mut self, buf: &mut [u8], deadline: Option<Instant>) -> io::Result<usize> {
        loop {
            match self.0.read(buf) {
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    let millis = poll_millis(deadline)?;
                    match broscribe_platform_unix::pipe::wait_readable(self.0.as_fd(), millis) {
                        Ok(_) => {}
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
                result => return result,
            }
        }
    }

    fn write_some_by(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<usize> {
        loop {
            match self.0.write(bytes) {
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    let millis = poll_millis(Some(deadline))?;
                    match broscribe_platform_unix::pipe::wait_writable(self.0.as_fd(), millis) {
                        Ok(_) => {}
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
                result => return result,
            }
        }
    }
}

/// Connects after checking the socket folder is private to this user.
pub(super) fn connect(endpoint: &Endpoint, deadline: Instant) -> io::Result<Wire> {
    let socket = endpoint.socket_location()?;
    check_private_dir(&socket.dir)?;
    let stream = broscribe_platform_unix::stream::connect_within(&socket.path, deadline)?;
    stream.set_nonblocking(true)?;
    Ok(Wire(stream))
}

pub(super) fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::NotFound | ErrorKind::ConnectionRefused
    )
}

pub(super) fn agent_instance_held(data_dir: &Path) -> io::Result<bool> {
    Instance::is_held(data_dir)
}

/// Before re-contacting an endpoint: refuse a folder that stopped being
/// private; a missing one may come back.
pub(super) fn precheck(endpoint: &Endpoint) -> io::Result<()> {
    match check_private_dir(&endpoint.socket_location()?.dir) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub(super) struct ServerListener {
    // Dropped before the instance, which then removes the socket file.
    listener: tokio::net::UnixListener,
    credentials: Box<dyn PeerCredentials>,
    _instance: Instance,
}

impl ServerListener {
    pub(super) fn bind(endpoint: &Endpoint) -> Result<Self, ListenError> {
        let socket = endpoint.socket_location()?.clone();
        let instance = Instance::acquire_at(endpoint.data_dir(), socket).map_err(|e| match e {
            InstanceError::AlreadyRunning { pid } => ListenError::AlreadyRunning { pid },
            InstanceError::Io(e) => ListenError::Io(e),
        })?;
        let listener = instance.listener().try_clone()?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener: tokio::net::UnixListener::from_std(listener)?,
            credentials: crate::peer_credentials(),
            _instance: instance,
        })
    }

    pub(super) async fn accept(&mut self) -> io::Result<ServerStream> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            if verify_same_user(self.credentials.as_ref(), stream.as_fd()).is_ok() {
                return Ok(stream);
            }
        }
    }
}

pub(super) type ServerStream = tokio::net::UnixStream;
pub(super) type ReadHalf = tokio::net::unix::OwnedReadHalf;
pub(super) type WriteHalf = tokio::net::unix::OwnedWriteHalf;

pub(super) fn split(stream: ServerStream) -> (ReadHalf, WriteHalf) {
    stream.into_split()
}

pub(super) async fn termination() -> io::Result<()> {
    broscribe_platform_unix::signals::termination().await
}

pub(super) fn is_resource_exhaustion(error: &io::Error) -> bool {
    broscribe_platform_unix::is_resource_exhaustion(error)
}

pub(super) fn ensure_private_dir(dir: &std::path::Path) -> io::Result<()> {
    broscribe_platform_unix::ensure_private_dir(dir)
}

pub(super) fn set_stdin_nonblocking(stdin: &ChildStdin) -> io::Result<()> {
    broscribe_platform_unix::pipe::set_nonblocking(stdin.as_fd())
}

pub(super) fn write_stdin(stdin: &mut ChildStdin, bytes: &[u8]) -> io::Result<usize> {
    stdin.write(bytes)
}

pub(super) fn wait_stdin_writable(stdin: &ChildStdin, timeout: Duration) -> io::Result<bool> {
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    broscribe_platform_unix::pipe::wait_writable(stdin.as_fd(), millis)
}
