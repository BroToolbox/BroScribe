//! The desktop app's own single-instance channel (SHL-14): the native
//! [`InstanceChannel`] of one data folder.
//!
//! Only the app holding the data folder's app instance lock listens. A
//! second launch connects, sends one request line and waits for the
//! running app's answer line ([`broscribe_platform::shell`]); the recording
//! agent is never involved. The endpoint is private to the current user,
//! like the agent's:
//!
//! - Unix: a socket beside the agent's (`<data_dir>/run/app.sock` on macOS,
//!   or `app-<key>.sock` in the agent's hashed socket folder) in a 0700
//!   folder owned by the user, mode 0600, with every peer's user checked.
//! - Windows: the pipe `\\.\pipe\broscribe-app-<sid>-<key>` with a
//!   current-user DACL, remote clients refused, the peer's token SID
//!   checked on both ends.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use broscribe_platform::shell::HANDOFF_HANDLE_WAIT;
use broscribe_platform::{
    Error, HandoffListener, HandoffRequest, HandoffSender, InstanceChannel, Result,
};

#[cfg(test)]
mod tests;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod win;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use win as platform;

/// Pause between attempts while the running app isn't listening yet.
const RETRY_PAUSE: Duration = Duration::from_millis(100);
/// How long the listening side waits for a connected peer's request line.
const REQUEST_WAIT: Duration = Duration::from_secs(2);

/// The single-instance channel of one data folder.
pub struct NativeInstanceChannel {
    data_dir: PathBuf,
    /// How long the listening side waits for the app to handle a request.
    handle_wait: Duration,
}

impl NativeInstanceChannel {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            handle_wait: HANDOFF_HANDLE_WAIT,
        }
    }

    #[cfg(test)]
    fn with_handle_wait(mut self, wait: Duration) -> Self {
        self.handle_wait = wait;
        self
    }
}

impl InstanceChannel for NativeInstanceChannel {
    fn listen(&self, sender: HandoffSender) -> Result<Box<dyn HandoffListener>> {
        platform::listen(&self.data_dir, sender, self.handle_wait)
            .map_err(|e| Error::io("listening for other BroScribe launches", e))
    }

    fn request(&self, request: HandoffRequest, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let mut absent = None;
        let answer = loop {
            match platform::exchange(&self.data_dir, request.word(), deadline) {
                Ok(answer) => break answer,
                // The running app may still be starting: try again until
                // the deadline.
                Err(e) if platform::is_absent(&e) && Instant::now() + RETRY_PAUSE < deadline => {
                    absent = Some(e);
                    std::thread::sleep(RETRY_PAUSE);
                }
                // A retry pause that overran the deadline still means no app
                // was there: report that, not a timeout that varies with load.
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut && absent.is_some() => {
                    let e = absent.take().expect("checked above");
                    return Err(Error::io("handing over to the running BroScribe", e));
                }
                Err(e) => return Err(Error::io("handing over to the running BroScribe", e)),
            }
        };
        Ok(answer.trim_end_matches(['\r', '\n']).to_owned())
    }
}

/// The time left before `deadline`; `TimedOut` once it passed.
fn remaining(deadline: Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(io::Error::new(
            ErrorKind::TimedOut,
            "the running BroScribe did not answer in time",
        ))
    } else {
        Ok(left)
    }
}
