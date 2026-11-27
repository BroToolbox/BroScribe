//! Unix building blocks of the daemon transport (plan M4 §2, §12.8). The
//! OS-neutral protocol client and server live in
//! `broscribe_platform_native::ipc`; this crate is their real Unix half:
//!
//! - [`dirs`]: protected per-user folders and socket path length checks;
//!   socket folders are owned by the user (mode 0700), sockets are 0600.
//! - [`instance`]: single instance through a locked pid file, then the
//!   listening socket.
//! - [`peer`]: the same-user check on every connection, behind
//!   [`PeerCredentials`] (each OS crate implements it).
//! - [`pipe`]: nonblocking child pipes with bounded readiness waits.
//! - [`process_group`]: process-group containment of helper children.
//! - [`signals`]: service termination signals.
//! - [`stream`]: client connections bounded by a deadline.
#![cfg(unix)]

pub mod dirs;
pub mod instance;
pub mod peer;
pub mod pipe;
pub mod process_group;
pub mod signals;
pub mod stream;

pub use dirs::{SocketLocation, check_private_dir, current_uid, ensure_private_dir};
pub use instance::{Instance, InstanceError, PidFile};
pub use peer::{FixedPeerUid, PeerCredentials, verify_same_user};

/// Whether an `accept` failure is descriptor exhaustion, which only a pause
/// (not an immediate retry) can relieve.
pub fn is_resource_exhaustion(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_exhaustion_is_recognised() {
        assert!(is_resource_exhaustion(&std::io::Error::from_raw_os_error(
            libc::EMFILE
        )));
        assert!(is_resource_exhaustion(&std::io::Error::from_raw_os_error(
            libc::ENFILE
        )));
        assert!(!is_resource_exhaustion(&std::io::Error::from(
            std::io::ErrorKind::ConnectionAborted
        )));
    }
}
