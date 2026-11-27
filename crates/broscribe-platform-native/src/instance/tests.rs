use std::sync::mpsc;
use std::time::{Duration, Instant};

use broscribe_platform::shell::{DockCommand, next_handoff_blocking};
use broscribe_platform::{HandoffReceiver, HandoffRequest, handoff_channel};

use super::*;

/// The running app's side: answers `done` to every request it receives,
/// reporting each on `handled`, until the channel closes.
fn answering(
    mut requests: HandoffReceiver,
    handled: mpsc::Sender<HandoffRequest>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while let Some(handoff) = next_handoff_blocking(&mut requests) {
            let _ = handled.send(handoff.request);
            handoff.done();
        }
    })
}

#[test]
fn a_second_launch_hands_show_to_the_running_app() {
    let dir = tempfile::tempdir().unwrap();
    let channel = NativeInstanceChannel::new(dir.path());
    let (sender, requests) = handoff_channel();
    let listener = channel.listen(sender).unwrap();
    let (handled, shown) = mpsc::channel();
    let app = answering(requests, handled);

    // Another process would build its own channel for the same folder.
    NativeInstanceChannel::new(dir.path())
        .send(HandoffRequest::Show, Duration::from_secs(10))
        .unwrap();
    assert_eq!(
        shown.recv_timeout(Duration::from_secs(5)).unwrap(),
        HandoffRequest::Show
    );
    NativeInstanceChannel::new(dir.path())
        .send(HandoffRequest::Show, Duration::from_secs(10))
        .unwrap();
    assert_eq!(
        shown.recv_timeout(Duration::from_secs(5)).unwrap(),
        HandoffRequest::Show
    );

    drop(listener);
    app.join().unwrap();
    // Nobody listens any more: the handoff fails within its timeout.
    let started = Instant::now();
    assert!(
        NativeInstanceChannel::new(dir.path())
            .send(HandoffRequest::Show, Duration::from_millis(400))
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_native_channel_round_trips_stable_jump_list_commands() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, requests) = handoff_channel();
    let listener = NativeInstanceChannel::new(dir.path())
        .listen(sender)
        .unwrap();
    let (handled, received) = mpsc::channel();
    let app = answering(requests, handled);
    let client = NativeInstanceChannel::new(dir.path());
    for command in [
        DockCommand::StartRecording,
        DockCommand::StopRecording,
        DockCommand::ShowRecordingControls,
        DockCommand::ReviewQueue,
    ] {
        let request = HandoffRequest::Dock(command);
        client.send(request, Duration::from_secs(5)).unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            request
        );
    }
    drop(listener);
    app.join().unwrap();
}

#[test]
fn an_unhandled_request_fails_the_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let channel =
        NativeInstanceChannel::new(dir.path()).with_handle_wait(Duration::from_millis(50));
    // The app never drains its requests (its UI loop is stuck).
    let (sender, _requests) = handoff_channel();
    let _listener = channel.listen(sender).unwrap();
    assert!(
        NativeInstanceChannel::new(dir.path())
            .send(HandoffRequest::Show, Duration::from_secs(10))
            .is_err()
    );
}

#[test]
fn nobody_listening_fails_after_retrying_until_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let error = NativeInstanceChannel::new(dir.path())
        .send(HandoffRequest::Show, Duration::from_millis(350))
        .unwrap_err();
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "it retried"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(error.code(), "io");
}

#[test]
fn a_live_listener_is_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, requests) = handoff_channel();
    let first = NativeInstanceChannel::new(dir.path())
        .listen(sender)
        .unwrap();
    let (other, _unused) = handoff_channel();
    assert!(
        NativeInstanceChannel::new(dir.path())
            .listen(other)
            .is_err()
    );
    // The first still answers.
    let (handled, shown) = mpsc::channel();
    let app = answering(requests, handled);
    NativeInstanceChannel::new(dir.path())
        .send(HandoffRequest::Show, Duration::from_secs(10))
        .unwrap();
    assert_eq!(
        shown.recv_timeout(Duration::from_secs(5)).unwrap(),
        HandoffRequest::Show
    );
    drop(first);
    app.join().unwrap();
    // Once it is gone, a new app listens there.
    let (again, _requests) = handoff_channel();
    let _second = NativeInstanceChannel::new(dir.path())
        .listen(again)
        .unwrap();
}

#[cfg(unix)]
#[test]
fn app_fallback_socket_stays_beside_the_agents_and_fits() {
    use broscribe_platform_unix::{SocketLocation, dirs::fits_sun_path};

    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let folder = dir.path().join("0/broscribe");
    let daemon =
        SocketLocation::in_dir(folder.clone(), "broscribed-0123456789abcdef.sock", true).unwrap();
    let app = unix::beside_agent_socket(daemon).unwrap();
    assert_eq!(app.dir, folder);
    assert_eq!(app.path, folder.join("app-0123456789abcdef.sock"));
    assert!(app.fallback);
    assert!(fits_sun_path(&app.path));
}

#[cfg(unix)]
#[test]
fn the_socket_is_private_and_a_crashed_apps_socket_is_replaced() {
    use std::os::unix::fs::MetadataExt as _;

    // The crashed app is simulated by binding and dropping a listener in this
    // process. A concurrent test that forks between the bind and the drop
    // briefly holds a copy of that descriptor, so the socket still accepts
    // and the probe rightly reports it alive. Run alone, with no sibling
    // tests forking, so the dropped listener is really gone.
    const CHILD: &str = "BROSCRIBE_TEST_ISOLATED_STALE_APP_SOCKET";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env(CHILD, "1")
            .args([
                "--exact",
                "instance::tests::the_socket_is_private_and_a_crashed_apps_socket_is_replaced",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let socket = unix::location(dir.path()).unwrap();
    let daemon = crate::unix_socket_location(dir.path()).unwrap();
    assert_eq!(socket.dir, daemon.dir);
    assert_ne!(socket.path, daemon.path, "never the agent's socket");
    // A crashed app left its socket behind: nobody accepts on it.
    broscribe_platform_unix::ensure_private_dir(&socket.dir).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&socket.path).unwrap());
    assert!(socket.path.exists());

    let (sender, _requests) = handoff_channel();
    let listener = NativeInstanceChannel::new(dir.path())
        .listen(sender)
        .unwrap();
    assert_eq!(
        std::fs::symlink_metadata(&socket.path).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::symlink_metadata(&socket.dir).unwrap().mode() & 0o777,
        0o700
    );
    drop(listener);
    assert!(!socket.path.exists(), "dropping the listener removes it");

    // Something that isn't a socket is never removed.
    std::fs::write(&socket.path, b"synthetic").unwrap();
    let (sender, _requests) = handoff_channel();
    assert!(
        NativeInstanceChannel::new(dir.path())
            .listen(sender)
            .is_err()
    );
    assert_eq!(std::fs::read(&socket.path).unwrap(), b"synthetic");
}

#[cfg(unix)]
#[test]
fn lines_are_bounded_and_end_at_the_newline() {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;
    let read = |bytes: &[u8]| {
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.write_all(bytes).unwrap();
        drop(writer);
        unix::read_line(&mut reader, Instant::now() + Duration::from_secs(1))
    };
    assert_eq!(read(b"show\nrest").unwrap(), "show");
    assert_eq!(read(b"show").unwrap_err().kind(), ErrorKind::UnexpectedEof);
    let long = vec![b'x'; broscribe_platform::shell::HANDOFF_MAX_LINE * 2];
    assert_eq!(read(&long).unwrap_err().kind(), ErrorKind::InvalidData);
}

#[cfg(unix)]
#[test]
fn a_partial_line_does_not_extend_the_wire_deadline() {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;

    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    reader.set_nonblocking(true).unwrap();
    writer.write_all(b"sh").unwrap();
    let deadline = Instant::now() + Duration::from_millis(40);
    assert_eq!(
        unix::read_line(&mut reader, deadline).unwrap_err().kind(),
        ErrorKind::TimedOut
    );
    writer.write_all(b"ow\n").unwrap();
    assert_eq!(
        unix::read_line(&mut reader, deadline).unwrap_err().kind(),
        ErrorKind::TimedOut,
        "data available after the original deadline cannot revive the request"
    );
}
