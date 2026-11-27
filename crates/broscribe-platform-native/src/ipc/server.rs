//! The daemon side: one authenticated listener per data folder and
//! line-framed connections. Peers running as another user are dropped
//! before the daemon ever sees them.

use std::fmt;
use std::io;
use std::path::Path;

use broscribe_ipc::MAX_LINE_BYTES;
use tokio::io::{AsyncWriteExt, BufReader};

use super::{Endpoint, framing, platform};

#[derive(Debug)]
pub enum ListenError {
    /// Another daemon owns this data folder's endpoint (its pid, when known).
    AlreadyRunning {
        pid: Option<u32>,
    },
    Io(io::Error),
}

impl fmt::Display for ListenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ListenError::AlreadyRunning { pid: Some(pid) } => {
                write!(f, "broscribed is already running (pid {pid})")
            }
            ListenError::AlreadyRunning { pid: None } => {
                write!(f, "broscribed is already running")
            }
            ListenError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ListenError {}

impl From<io::Error> for ListenError {
    fn from(e: io::Error) -> Self {
        ListenError::Io(e)
    }
}

/// The daemon's single-instance endpoint. Dropping it releases the
/// endpoint (Unix: removes the socket, then the pid file).
pub struct Listener {
    inner: platform::ServerListener,
    endpoint: Endpoint,
}

impl Listener {
    /// Claims `data_dir`'s endpoint. Call inside a Tokio runtime.
    pub fn bind(data_dir: &Path) -> Result<Self, ListenError> {
        let endpoint = Endpoint::new(data_dir);
        let inner = platform::ServerListener::bind(&endpoint)?;
        Ok(Self { inner, endpoint })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The next connection from a process of this user; other users'
    /// connections are closed unseen. An error is the listener's own
    /// (log it and keep accepting; pause when [`super::is_resource_exhaustion`]).
    pub async fn accept(&mut self) -> io::Result<Connection> {
        Ok(Connection(self.inner.accept().await?))
    }
}

/// An authenticated connection.
pub struct Connection(platform::ServerStream);

impl Connection {
    pub fn into_split(self) -> (ConnectionReader, ConnectionWriter) {
        let (read, write) = platform::split(self.0);
        (
            ConnectionReader(BufReader::new(read)),
            ConnectionWriter(write),
        )
    }
}

pub struct ConnectionReader(BufReader<platform::ReadHalf>);

impl ConnectionReader {
    /// The next request line, bounded by [`MAX_LINE_BYTES`]; `None` at end
    /// of stream. A longer line is `InvalidData`: close the connection.
    pub async fn read_line(&mut self) -> io::Result<Option<String>> {
        framing::read_line(&mut self.0, MAX_LINE_BYTES).await
    }
}

pub struct ConnectionWriter(platform::WriteHalf);

impl ConnectionWriter {
    /// Writes `line` and its newline (appended in place, no copy), then flushes.
    pub async fn write_line(&mut self, mut line: String) -> io::Result<()> {
        line.push('\n');
        self.0.write_all(line.as_bytes()).await?;
        self.0.flush().await
    }
}
