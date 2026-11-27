//! OS-neutral transport of the daemon protocol: the blocking [`Client`]
//! (CLI, MCP, app background threads) and the async [`Listener`] (daemon),
//! over authenticated Unix sockets or Windows named pipes.
//!
//! Every endpoint is private to the current user: Unix sockets live in a
//! 0700 folder owned by the user and every accepted peer's UID is checked;
//! Windows pipes carry a current-user-only DACL, refuse remote clients and
//! check the peer's token SID on both ends. Callers never see the platform.

mod client;
mod framing;
mod server;
mod stdin;
#[cfg(test)]
pub(crate) mod tests;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod win;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use win as platform;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use client::{CallError, Client, DEFAULT_TIMEOUT, RECONCILE_WINDOW, Subscription};
pub use framing::read_line;
pub use server::{Connection, ConnectionReader, ConnectionWriter, ListenError, Listener};
pub use stdin::NonBlockingStdin;

pub struct NativeAgentInstanceProbe;

impl broscribe_platform::AgentInstanceProbe for NativeAgentInstanceProbe {
    fn is_held(&self, data_dir: &Path) -> std::io::Result<bool> {
        platform::agent_instance_held(data_dir)
    }
}

/// The daemon endpoint of one data folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    data_dir: PathBuf,
    #[cfg(unix)]
    socket: Result<broscribe_platform_unix::SocketLocation, (std::io::ErrorKind, String)>,
}

impl Endpoint {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            #[cfg(unix)]
            socket: crate::unix_socket_location(data_dir).map_err(|e| (e.kind(), e.to_string())),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The Unix socket and its private folder, or the path resolution error.
    #[cfg(unix)]
    pub fn socket_location(&self) -> std::io::Result<&broscribe_platform_unix::SocketLocation> {
        self.socket
            .as_ref()
            .map_err(|(kind, message)| std::io::Error::new(*kind, message.clone()))
    }
}

/// Cooperative cancellation of a blocking call; clones share one flag.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    /// Shares cancellation with a caller that also passes the flag to
    /// operations outside IPC.
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Termination request for the service (SIGTERM/Ctrl-C on Unix; console
/// Ctrl-C, close, logoff and shutdown on Windows).
pub async fn termination() -> std::io::Result<()> {
    platform::termination().await
}

/// Whether an accept failure is resource exhaustion, which only a pause
/// (not an immediate retry) can relieve.
pub fn is_resource_exhaustion(error: &std::io::Error) -> bool {
    platform::is_resource_exhaustion(error)
}

/// Creates `dir` if needed and checks it is a real folder private to this
/// user (Unix: mode 0700, owned, not a symlink; Windows: not a reparse
/// point, inside the per-user profile ACLs).
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    platform::ensure_private_dir(dir)
}
