//! Peer credentials of daemon connections through `getpeereid`.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};

use broscribe_platform_unix::dirs::{RUN_DIR, SOCKET_NAME, darwin_user_dir};
use broscribe_platform_unix::{PeerCredentials, SocketLocation};

pub struct MacPeerCredentials;

impl PeerCredentials for MacPeerCredentials {
    fn peer_uid(&self, fd: BorrowedFd<'_>) -> io::Result<u32> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: `fd` is open for the borrow's lifetime; both out-pointers
        // point to live locals.
        let rc = unsafe { libc::getpeereid(fd.as_raw_fd(), &mut uid, &mut gid) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

/// Keep the usual short socket path. A data folder whose path cannot fit in
/// sockaddr_un gets its own fallback, so it cannot address another profile.
pub fn socket_location(data_dir: &Path) -> io::Result<SocketLocation> {
    socket_location_for(data_dir, darwin_user_dir)
}

fn socket_location_for(
    data_dir: &Path,
    user_dir: impl FnOnce() -> io::Result<PathBuf>,
) -> io::Result<SocketLocation> {
    let data_dir = broscribe_platform::stable_path(data_dir);
    if let Ok(socket) = SocketLocation::in_dir(data_dir.join(RUN_DIR), SOCKET_NAME, false) {
        return Ok(socket);
    }
    let key = broscribe_platform::profile_key(data_dir.as_os_str().as_encoded_bytes());
    SocketLocation::in_dir(
        user_dir()?.join("broscribe"),
        &format!("broscribed-{key}.sock"),
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    use broscribe_platform_unix::{current_uid, verify_same_user};

    use super::*;

    #[test]
    fn peer_of_a_socket_pair_is_this_user() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(
            MacPeerCredentials.peer_uid(a.as_fd()).unwrap(),
            current_uid()
        );
        verify_same_user(&MacPeerCredentials, a.as_fd()).unwrap();
    }

    #[test]
    fn long_profiles_cannot_address_each_others_fallback_socket() {
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let user = tmp.path().join("0");
        let locate = |dir: &Path| socket_location_for(dir, || Ok(user.clone())).unwrap();
        let parent = tmp.path().join("profile".repeat(20));
        let a = parent.join("a");
        let b = parent.join("b");
        let a_socket = locate(&a);
        let b_socket = locate(&b);
        assert!(a_socket.fallback && b_socket.fallback);
        assert_eq!(a_socket.dir, user.join("broscribe"));
        let key = broscribe_platform::profile_key(
            broscribe_platform::stable_path(&a)
                .as_os_str()
                .as_encoded_bytes(),
        );
        assert_eq!(
            a_socket.path,
            a_socket.dir.join(format!("broscribed-{key}.sock"))
        );
        assert_ne!(a_socket.path, b_socket.path);
        assert_eq!(a_socket, locate(&a.join(".")));
        assert!(broscribe_platform_unix::dirs::fits_sun_path(&a_socket.path));
    }

    #[test]
    fn oversized_fallback_socket_reports_an_error() {
        let root = Path::new("/synthetic").join("x".repeat(104));
        let error = socket_location_for(&root.join("data"), || Ok(root.clone())).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("too long for sun_path"));
    }

    #[test]
    fn tmp_aliases_select_one_socket_even_at_the_length_limit() {
        use broscribe_platform_unix::dirs::{SUN_PATH_BYTES, fits_sun_path};

        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let canonical = tmp.path().canonicalize().unwrap();
        let short = tmp.path().join("data");
        let short_socket =
            socket_location_for(&short, || panic!("short paths need no runtime directory"))
                .unwrap();
        assert!(!short_socket.fallback);
        assert_eq!(
            short_socket.path,
            canonical.join("data/run/broscribed.sock")
        );

        let len = SUN_PATH_BYTES - canonical.join("run").join(SOCKET_NAME).as_os_str().len() - 1;
        let name = "x".repeat(len);
        let boundary = tmp.path().join(&name);
        assert!(
            fits_sun_path(&boundary.join("run").join(SOCKET_NAME)),
            "the alias alone fits in sun_path"
        );
        let locate =
            |dir: &Path| socket_location_for(dir, || Ok(tmp.path().to_path_buf())).unwrap();
        let fallback = locate(&boundary);
        assert!(fallback.fallback);
        assert_eq!(fallback, locate(&canonical.join(name)));
        std::fs::create_dir(&boundary).unwrap();
        assert_eq!(fallback, locate(&boundary));
    }
}
