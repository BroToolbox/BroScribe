//! Unix-socket client connections bounded by a deadline. A plain blocking
//! `connect` can wait without limit on Linux when the listener's backlog is
//! full; this one never outlives the caller's deadline.

use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// Longest pause between attempts against a saturated listener.
const MAX_PAUSE: Duration = Duration::from_millis(20);

/// Connects to the stream socket at `path` before `deadline`; the returned
/// stream is in blocking mode. A missing socket is `NotFound`, no listener
/// `ConnectionRefused`, an expired deadline `TimedOut`.
pub fn connect_within(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let address = address(path)?;
    let mut pause = Duration::from_millis(1);
    loop {
        remaining(deadline)?;
        let socket = socket()?;
        // SAFETY: `address` is a fully initialised sockaddr_un of the given length.
        let result = unsafe {
            libc::connect(
                socket.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        let error = (result != 0).then(io::Error::last_os_error);
        match error.as_ref().and_then(io::Error::raw_os_error) {
            None => return finish(socket),
            Some(libc::EINPROGRESS) => {
                wait_connected(&socket, deadline)?;
                return finish(socket);
            }
            // A saturated listener: retry with a fresh socket until the deadline.
            Some(libc::EAGAIN) => {
                let left = remaining(deadline)?;
                std::thread::sleep(pause.min(left));
                pause = (pause * 2).min(MAX_PAUSE);
            }
            Some(libc::EINTR) => {}
            _ => return Err(error.expect("connect failed")),
        }
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(io::Error::new(ErrorKind::TimedOut, "connecting timed out"))
    } else {
        Ok(left)
    }
}

fn address(path: &Path) -> io::Result<libc::sockaddr_un> {
    // SAFETY: an all-zero sockaddr_un is valid and NUL-terminates sun_path.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "socket path does not fit sockaddr_un",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
    }
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    Ok(address)
}

/// A nonblocking, close-on-exec stream socket.
fn socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket creation; the descriptor is owned right away.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor nobody else owns.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    set_flag(&socket, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)?;
    set_flag(&socket, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)?;
    #[cfg(target_os = "macos")]
    {
        let on: libc::c_int = 1;
        // SAFETY: valid option pointer and length; avoids SIGPIPE on writes.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&on as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
    }
    Ok(socket)
}

fn set_flag(
    socket: &OwnedFd,
    get: libc::c_int,
    set: libc::c_int,
    flag: libc::c_int,
) -> io::Result<()> {
    // SAFETY: fcntl get/set of descriptor flags on an owned descriptor.
    let flags = unsafe { libc::fcntl(socket.as_raw_fd(), get) };
    if flags < 0 || unsafe { libc::fcntl(socket.as_raw_fd(), set, flags | flag) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wait_connected(socket: &OwnedFd, deadline: Instant) -> io::Result<()> {
    wait_ready(socket, libc::POLLOUT, deadline)?;
    let mut code: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: valid out-pointers for SO_ERROR.
    if unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&mut code as *mut libc::c_int).cast(),
            &mut len,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

/// Waits for a read (including EOF) on a nonblocking stream.
pub fn wait_readable(stream: &UnixStream, deadline: Instant) -> io::Result<()> {
    wait_ready(stream, libc::POLLIN, deadline)
}

/// Waits for a write on a nonblocking stream.
pub fn wait_writable(stream: &UnixStream, deadline: Instant) -> io::Result<()> {
    wait_ready(stream, libc::POLLOUT, deadline)
}

fn wait_ready(socket: &impl AsRawFd, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let left = remaining(deadline)?;
        let mut descriptor = libc::pollfd {
            fd: socket.as_raw_fd(),
            events,
            revents: 0,
        };
        let millis = left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        return Ok(());
    }
}

fn finish(socket: OwnedFd) -> io::Result<UnixStream> {
    let stream = UnixStream::from(socket);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    #[test]
    fn connects_reports_absence_and_respects_deadlines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.sock");
        let deadline = || Instant::now() + Duration::from_secs(2);
        assert_eq!(
            connect_within(&path, deadline()).unwrap_err().kind(),
            ErrorKind::NotFound
        );
        let listener = UnixListener::bind(&path).unwrap();
        assert_eq!(
            connect_within(&path, Instant::now()).unwrap_err().kind(),
            ErrorKind::TimedOut,
        );
        let mut client = connect_within(&path, deadline()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        client.write_all(b"x").unwrap();
        let mut byte = [0u8; 1];
        server.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
        drop(client);
        drop(server);
        drop(listener);
        // Do not rely on a dropped listener becoming unreachable immediately:
        // a child inheriting it is one possible cause. This socket never listens.
        let refused_path = tmp.path().join("refused.sock");
        let refused_socket = socket().unwrap();
        let refused_address = address(&refused_path).unwrap();
        // SAFETY: the address is initialised and valid for the supplied length.
        assert_eq!(
            unsafe {
                libc::bind(
                    refused_socket.as_raw_fd(),
                    (&refused_address as *const libc::sockaddr_un).cast(),
                    std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
                )
            },
            0
        );
        assert_eq!(
            connect_within(&refused_path, deadline())
                .unwrap_err()
                .kind(),
            ErrorKind::ConnectionRefused
        );
        assert_eq!(
            connect_within(&path, Instant::now()).unwrap_err().kind(),
            ErrorKind::TimedOut,
            "an expired deadline is refused before attempting a connection"
        );
        let long = tmp.path().join("x".repeat(200));
        assert_eq!(
            connect_within(&long, deadline()).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
}
