//! Windows half: a per-user named pipe with a current-user DACL; the
//! peer's token SID is checked on both ends.

use std::io::{self, ErrorKind};
use std::os::windows::io::AsRawHandle as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use broscribe_platform::shell::{HANDOFF_MAX_LINE, answer_request};
use broscribe_platform::{HandoffListener, HandoffSender};
use broscribe_platform_windows::{PipeSecurity, WinPipeStream, app_pipe_name, verify_pipe_client};
use tokio::io::{AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};

use super::{REQUEST_WAIT, remaining};

/// `ERROR_FILE_NOT_FOUND` / `ERROR_PATH_NOT_FOUND`: nobody listens.
const NOT_FOUND: [i32; 2] = [2, 3];
/// How long waking the listener on drop may take.
const WAKE_WAIT: Duration = Duration::from_millis(200);
const CREATE_RETRY: Duration = Duration::from_millis(25);
const MAX_CREATE_RETRY: Duration = Duration::from_secs(1);

pub(super) fn listen(
    data_dir: &Path,
    sender: HandoffSender,
    handle_wait: Duration,
) -> io::Result<Box<dyn HandoffListener>> {
    let name = app_pipe_name(data_dir)?;
    let mut security = PipeSecurity::current_user()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    // Claim the name here, so a name another process holds is reported to
    // the caller rather than lost on the thread.
    let first = {
        let _context = runtime.enter();
        create(&name, &mut security, true)?
    };
    let stopping = Arc::new(AtomicBool::new(false));
    let thread_stopping = stopping.clone();
    let thread_name = name.clone();
    std::thread::Builder::new()
        .name("broscribe-app-handoff".into())
        .spawn(move || {
            runtime.block_on(serve(
                thread_name,
                security,
                first,
                sender,
                handle_wait,
                thread_stopping,
            ))
        })?;
    Ok(Box::new(WinHandoffListener { name, stopping }))
}

/// Answers one connection at a time until the listener is dropped.
async fn serve(
    name: String,
    mut security: PipeSecurity,
    mut next: NamedPipeServer,
    sender: HandoffSender,
    handle_wait: Duration,
    stopping: Arc<AtomicBool>,
) {
    loop {
        let connected = next.connect().await;
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        // Hand the connected instance over only once its successor exists,
        // keeping the name owned throughout.
        let Some(fresh) = create_successor(&stopping, || create(&name, &mut security, false)).await
        else {
            return;
        };
        let server = std::mem::replace(&mut next, fresh);
        if connected.is_err() {
            continue;
        }
        // Another user's connection is closed unread.
        if verify_pipe_client(server.as_raw_handle()).is_err() {
            continue;
        }
        let _ = answer(server, &sender, handle_wait).await;
    }
}

async fn create_successor<T>(
    stopping: &AtomicBool,
    mut create: impl FnMut() -> io::Result<T>,
) -> Option<T> {
    let mut pause = CREATE_RETRY;
    while !stopping.load(Ordering::SeqCst) {
        if let Ok(fresh) = create() {
            return Some(fresh);
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(MAX_CREATE_RETRY);
    }
    None
}

async fn answer(
    mut server: NamedPipeServer,
    sender: &HandoffSender,
    handle_wait: Duration,
) -> io::Result<()> {
    let line = tokio::time::timeout(REQUEST_WAIT, read_line(&mut server))
        .await
        .map_err(|_| io::Error::from(ErrorKind::TimedOut))??;
    // This thread serves nothing else meanwhile.
    let answer = answer_request(&line, sender, handle_wait);
    write_reply(&mut server, format!("{answer}\n").as_bytes(), REQUEST_WAIT).await?;
    // Keep the instance until the peer has read the answer and closed.
    let mut rest = [0u8; 1];
    let _ = tokio::time::timeout(REQUEST_WAIT, server.read(&mut rest)).await;
    Ok(())
}

async fn write_reply(
    server: &mut (impl AsyncWrite + Unpin),
    reply: &[u8],
    wait: Duration,
) -> io::Result<()> {
    tokio::time::timeout(wait, async {
        server.write_all(reply).await?;
        server.flush().await
    })
    .await
    .map_err(|_| io::Error::from(ErrorKind::TimedOut))?
}

async fn read_line(server: &mut NamedPipeServer) -> io::Result<String> {
    let mut line = Vec::new();
    loop {
        let byte = server.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        if line.len() + 1 >= HANDOFF_MAX_LINE {
            return Err(io::Error::new(ErrorKind::InvalidData, "line too long"));
        }
        line.push(byte);
    }
    String::from_utf8(line).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))
}

fn create(name: &str, security: &mut PipeSecurity, first: bool) -> io::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .access_inbound(true)
        .access_outbound(true)
        .pipe_mode(PipeMode::Byte);
    // SAFETY: `security` holds a valid SECURITY_ATTRIBUTES (current-user
    // DACL) that outlives this call.
    unsafe { options.create_with_security_attributes_raw(name, security.as_ptr()) }
}

/// Sends `word` to the running app and reads its answer, all before
/// `deadline`.
pub(super) fn exchange(data_dir: &Path, word: &str, deadline: Instant) -> io::Result<String> {
    let name = app_pipe_name(data_dir)?;
    let mut stream = WinPipeStream::connect_named(&name, deadline)?;
    stream.write_all_within(format!("{word}\n").as_bytes(), deadline)?;
    stream.read_line(remaining(deadline)?, HANDOFF_MAX_LINE)
}

/// Nobody listens (yet).
pub(super) fn is_absent(error: &io::Error) -> bool {
    error.kind() == ErrorKind::NotFound
        || error
            .raw_os_error()
            .is_some_and(|code| NOT_FOUND.contains(&code))
}

struct WinHandoffListener {
    name: String,
    stopping: Arc<AtomicBool>,
}

impl HandoffListener for WinHandoffListener {}

impl Drop for WinHandoffListener {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Wakes the pending connect so the thread sees the flag and ends,
        // releasing the name.
        let _ = WinPipeStream::connect_named(&self.name, Instant::now() + WAKE_WAIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn successor_creation_recovers_after_transient_errors() {
        let stopping = AtomicBool::new(false);
        let mut attempts = 0;
        let successor = create_successor(&stopping, || {
            attempts += 1;
            if attempts < 3 {
                Err(io::Error::other("synthetic pipe creation failure"))
            } else {
                Ok("successor")
            }
        })
        .await;
        assert_eq!(successor, Some("successor"));
        assert_eq!(attempts, 3);
        stopping.store(true, Ordering::SeqCst);
        let stopped = create_successor(&stopping, || Ok("unexpected")).await;
        assert_eq!(stopped, None);
    }

    #[tokio::test]
    async fn blocked_reply_expires() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let result = write_reply(&mut writer, b"done\n", Duration::from_millis(20)).await;
        assert_eq!(result.unwrap_err().kind(), ErrorKind::TimedOut);
    }
}
