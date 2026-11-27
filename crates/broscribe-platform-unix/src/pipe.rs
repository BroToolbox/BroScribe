//! Nonblocking descriptors (child pipes, client sockets) with bounded
//! readiness waits, so deadlines never depend on per-call socket options.
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

pub fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: borrowed descriptor stays valid; F_GETFL has no pointer argument.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: updates only descriptor flags, preserving the existing ones.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Waits at most `timeout_ms` (negative: without limit) until `fd` can be
/// written; `false` on timeout. Never used on capture/journal threads.
pub fn wait_writable(fd: BorrowedFd<'_>, timeout_ms: i32) -> io::Result<bool> {
    wait(fd, libc::POLLOUT, timeout_ms)
}

/// Waits at most `timeout_ms` (negative: without limit) until `fd` has data,
/// end of stream or an error to report; `false` on timeout.
pub fn wait_readable(fd: BorrowedFd<'_>, timeout_ms: i32) -> io::Result<bool> {
    wait(fd, libc::POLLIN, timeout_ms)
}

fn wait(fd: BorrowedFd<'_>, events: libc::c_short, timeout_ms: i32) -> io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    // SAFETY: descriptor is valid for the single-element poll array.
    let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // Hang-up and errors count as ready: the next read or write reports them.
    Ok(result > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn readiness_reports_data_end_of_stream_and_timeouts() {
        let (mut a, b) = UnixStream::pair().unwrap();
        set_nonblocking(b.as_fd()).unwrap();
        assert!(!wait_readable(b.as_fd(), 10).unwrap());
        assert!(wait_writable(b.as_fd(), 10).unwrap());
        a.write_all(b"x").unwrap();
        assert!(wait_readable(b.as_fd(), 1000).unwrap());
        let mut byte = [0u8; 1];
        assert_eq!((&b).read(&mut byte).unwrap(), 1);
        assert_eq!(
            (&b).read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(a);
        // The closed peer is ready to read: end of stream, not a hang.
        assert!(wait_readable(b.as_fd(), 1000).unwrap());
        assert_eq!((&b).read(&mut byte).unwrap(), 0);
    }
}
