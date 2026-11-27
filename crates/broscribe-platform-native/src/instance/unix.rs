//! Unix half: a socket beside the agent's, in its private folder.

use std::fs::{self, Permissions};
use std::io::{self, ErrorKind, Read, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use broscribe_platform::shell::{HANDOFF_MAX_LINE, answer_request};
use broscribe_platform::{HandoffListener, HandoffSender};
use broscribe_platform_unix::{
    PeerCredentials, SocketLocation, check_private_dir, ensure_private_dir,
    stream::{wait_readable, wait_writable},
    verify_same_user,
};

use super::{REQUEST_WAIT, remaining};

/// How long checking an existing socket, or waking the listener on drop,
/// may take.
const PROBE_WAIT: Duration = Duration::from_millis(200);
/// Pause after an accept failed for lack of descriptors.
const EXHAUSTED_PAUSE: Duration = Duration::from_millis(100);

/// The agent's socket folder with the file renamed `app…` (shorter, so it
/// fits `sun_path` wherever the agent's does).
pub(super) fn location(data_dir: &Path) -> io::Result<SocketLocation> {
    beside_agent_socket(crate::unix_socket_location(data_dir)?)
}

pub(super) fn beside_agent_socket(daemon: SocketLocation) -> io::Result<SocketLocation> {
    let name = daemon
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("broscribed.sock")
        .replacen("broscribed", "app", 1);
    SocketLocation::in_dir(daemon.dir, &name, daemon.fallback)
}

pub(super) fn listen(
    data_dir: &Path,
    sender: HandoffSender,
    handle_wait: Duration,
) -> io::Result<Box<dyn HandoffListener>> {
    let socket = location(data_dir)?;
    ensure_private_dir(&socket.dir)?;
    // The caller holds the app instance lock, so a socket left here is a
    // crashed app's; still verify a socket is no longer accepting connections.
    match fs::symlink_metadata(&socket.path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if broscribe_platform_unix::stream::connect_within(
                &socket.path,
                Instant::now() + PROBE_WAIT,
            )
            .is_ok()
            {
                return Err(io::Error::new(
                    ErrorKind::AddrInUse,
                    format!("another BroScribe listens on {}", socket.path.display()),
                ));
            }
            fs::remove_file(&socket.path)?;
        }
        Ok(_) => {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", socket.path.display()),
            ));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(&socket.path)?;
    fs::set_permissions(&socket.path, Permissions::from_mode(0o600))?;
    let stopping = Arc::new(AtomicBool::new(false));
    let credentials = crate::peer_credentials();
    let thread_stopping = stopping.clone();
    std::thread::Builder::new()
        .name("broscribe-app-handoff".into())
        .spawn(move || serve(listener, credentials, sender, handle_wait, thread_stopping))?;
    Ok(Box::new(UnixHandoffListener {
        socket: socket.path,
        stopping,
    }))
}

/// Answers one connection at a time until the listener is dropped.
fn serve(
    listener: UnixListener,
    credentials: Box<dyn PeerCredentials>,
    sender: HandoffSender,
    handle_wait: Duration,
    stopping: Arc<AtomicBool>,
) {
    for stream in listener.incoming() {
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) if broscribe_platform_unix::is_resource_exhaustion(&e) => {
                std::thread::sleep(EXHAUSTED_PAUSE);
                continue;
            }
            Err(_) => continue,
        };
        // Another user's connection is closed unread.
        if verify_same_user(credentials.as_ref(), stream.as_fd()).is_err() {
            continue;
        }
        let _ = answer(stream, &sender, handle_wait);
    }
}

fn answer(mut stream: UnixStream, sender: &HandoffSender, handle_wait: Duration) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    let line = read_line(&mut stream, Instant::now() + REQUEST_WAIT)?;
    let answer = answer_request(&line, sender, handle_wait);
    write_line(&mut stream, answer, Instant::now() + REQUEST_WAIT)
}

/// Sends `word` to the running app and reads its answer, all before
/// `deadline`.
pub(super) fn exchange(data_dir: &Path, word: &str, deadline: Instant) -> io::Result<String> {
    let socket = location(data_dir)?;
    check_private_dir(&socket.dir)?;
    let mut stream = broscribe_platform_unix::stream::connect_within(&socket.path, deadline)?;
    stream.set_nonblocking(true)?;
    write_line(&mut stream, word, deadline)?;
    read_line(&mut stream, deadline)
}

/// Nobody listens (yet): no socket, or a socket nobody accepts on.
pub(super) fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::NotFound | ErrorKind::ConnectionRefused
    )
}

struct UnixHandoffListener {
    socket: PathBuf,
    stopping: Arc<AtomicBool>,
}

impl HandoffListener for UnixHandoffListener {}

impl Drop for UnixHandoffListener {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Wakes the accept so the thread sees the flag and ends.
        let _ = broscribe_platform_unix::stream::connect_within(
            &self.socket,
            Instant::now() + PROBE_WAIT,
        );
        let _ = fs::remove_file(&self.socket);
    }
}

/// One line of at most [`HANDOFF_MAX_LINE`] bytes, without its newline.
pub(super) fn read_line(stream: &mut UnixStream, deadline: Instant) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        remaining(deadline)?;
        match stream.read(&mut byte) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() + 1 >= HANDOFF_MAX_LINE => {
                return Err(io::Error::new(ErrorKind::InvalidData, "line too long"));
            }
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => wait_readable(stream, deadline)?,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(line).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))
}

fn write_line(stream: &mut UnixStream, line: &str, deadline: Instant) -> io::Result<()> {
    let line = format!("{line}\n");
    let mut bytes = line.as_bytes();
    while !bytes.is_empty() {
        remaining(deadline)?;
        match stream.write(bytes) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(written) => bytes = &bytes[written..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => wait_writable(stream, deadline)?,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
