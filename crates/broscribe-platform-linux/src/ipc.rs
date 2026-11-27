use sha2::{Digest, Sha256};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};

use broscribe_platform_unix::dirs::linux_socket_location;
use broscribe_platform_unix::{PeerCredentials, SocketLocation};

#[derive(Default)]
pub struct LinuxPeerCredentials;
impl PeerCredentials for LinuxPeerCredentials {
    fn peer_uid(&self, fd: BorrowedFd<'_>) -> io::Result<u32> {
        let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: fd is borrowed/live, buffer and socklen point to writable
        // correctly-sized storage. Linux fills ucred for AF_UNIX peers.
        if unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if size as usize != std::mem::size_of::<libc::ucred>() {
            return Err(io::Error::other(
                "SO_PEERCRED returned an invalid credential size",
            ));
        }
        // SAFETY: getsockopt succeeded and returned exactly a ucred.
        Ok(unsafe { credentials.assume_init() }.uid)
    }
}

/// A data folder's identity: its [`broscribe_platform::stable_path`] bytes,
/// so symlink aliases and not-yet-created folders share one endpoint and one
/// secret namespace. Linux paths are case-sensitive: no folding.
pub fn profile_identity(data_dir: &Path) -> Vec<u8> {
    broscribe_platform::stable_path(data_dir)
        .into_os_string()
        .into_encoded_bytes()
}

/// Linux socket policy; the shared Unix crate supplies framing, locking,
/// transport, private-directory creation and same-user enforcement.
/// Never accept a relative or foreign-owned XDG runtime directory.
pub fn socket_location(data_dir: &Path) -> io::Result<SocketLocation> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    socket_location_with_runtime(data_dir, runtime.as_deref())
}

fn socket_location_with_runtime(
    data_dir: &Path,
    runtime: Option<&Path>,
) -> io::Result<SocketLocation> {
    let digest = Sha256::digest(profile_identity(data_dir));
    let key = u64::from_be_bytes(digest[..8].try_into().expect("eight digest bytes"));
    linux_socket_location(&format!("broscribed-{key:016x}.sock"), runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn data_directories_have_distinct_stable_endpoints() {
        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let locate =
            |path: &Path| socket_location_with_runtime(path, Some(temporary.path())).unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let one = locate(&first);
        let two = locate(&second);
        assert_ne!(one.path, two.path);
        assert_eq!(one.path, locate(&first.join(".")).path);
        assert!(broscribe_platform_unix::dirs::fits_sun_path(&one.path));
    }

    #[test]
    fn endpoint_is_stable_across_creating_a_data_directory_under_a_symlink() {
        let temporary = tempfile::tempdir_in("/tmp").unwrap();
        let locate =
            |path: &Path| socket_location_with_runtime(path, Some(temporary.path())).unwrap();
        let real = temporary.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = temporary.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let data = alias.join("not-yet").join("data");
        // Daemon first start: the endpoint is derived before the folder exists.
        let first_start = locate(&data);
        assert_eq!(first_start.path, locate(&real.join("not-yet/data")).path);
        assert_eq!(
            first_start.path,
            locate(&alias.join("not-yet/./x/../data")).path
        );
        std::fs::create_dir_all(&data).unwrap();
        // Clients after creation canonicalize the real path.
        assert_eq!(first_start.path, locate(&data).path);
        assert_eq!(first_start.path, locate(&real.join("not-yet/data")).path);
        assert_ne!(first_start.path, locate(&real).path);
    }

    #[test]
    fn authenticated_connection_exchanges_frames() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ipc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(path).unwrap();
            broscribe_platform_unix::verify_same_user(&LinuxPeerCredentials, stream.as_fd())
                .unwrap();
            stream.write_all(b"synthetic request\n").unwrap();
            let mut reply = [0; 3];
            stream.read_exact(&mut reply).unwrap();
            assert_eq!(&reply, b"ok\n");
        });
        let (mut stream, _) = listener.accept().unwrap();
        broscribe_platform_unix::verify_same_user(&LinuxPeerCredentials, stream.as_fd()).unwrap();
        assert_eq!(
            LinuxPeerCredentials.peer_uid(stream.as_fd()).unwrap(),
            broscribe_platform_unix::current_uid()
        );
        let mut request = [0; 18];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"synthetic request\n");
        stream.write_all(b"ok\n").unwrap();
        client.join().unwrap();
    }
}
