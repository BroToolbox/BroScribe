//! Same-user check on accepted connections. Reading a socket peer's
//! credentials is OS-specific (`getpeereid` on macOS, `SO_PEERCRED` on
//! Linux), so each OS crate implements [`PeerCredentials`].

use std::io::{self, ErrorKind};
use std::os::fd::BorrowedFd;

use crate::dirs::current_uid;

pub trait PeerCredentials: Send + Sync {
    /// Effective user ID of the process at the other end of the connected
    /// Unix stream socket `fd`.
    fn peer_uid(&self, fd: BorrowedFd<'_>) -> io::Result<u32>;
}

/// Fake for tests: every peer has this user ID.
#[derive(Clone, Copy, Debug)]
pub struct FixedPeerUid(pub u32);

impl PeerCredentials for FixedPeerUid {
    fn peer_uid(&self, _fd: BorrowedFd<'_>) -> io::Result<u32> {
        Ok(self.0)
    }
}

/// `PermissionDenied` unless the peer of `fd` runs as this process's user.
pub fn verify_same_user(creds: &dyn PeerCredentials, fd: BorrowedFd<'_>) -> io::Result<()> {
    let peer = creds.peer_uid(fd)?;
    let me = current_uid();
    if peer == me {
        Ok(())
    } else {
        Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("connection from uid {peer} refused (daemon runs as uid {me})"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn other_users_are_refused() {
        let (a, _b) = UnixStream::pair().unwrap();
        verify_same_user(&FixedPeerUid(current_uid()), a.as_fd()).unwrap();
        let other = FixedPeerUid(current_uid().wrapping_add(1));
        assert_eq!(
            verify_same_user(&other, a.as_fd()).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
    }
}
