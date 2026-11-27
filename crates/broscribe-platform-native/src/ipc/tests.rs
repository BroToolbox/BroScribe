//! Transport regressions over the real OS endpoint of a temporary data
//! folder (Unix socket or named pipe) with a synthetic daemon.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use broscribe_ipc::{Call, Event, IpcError, Request, RequestId, RequestState, Response};
use serde_json::json;

use super::*;

#[tokio::test]
async fn agent_instance_probe_observes_the_native_listener_claim() {
    use broscribe_platform::AgentInstanceProbe;

    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("data");
    let probe = NativeAgentInstanceProbe;
    assert!(!probe.is_held(&data).unwrap());
    let listener = Listener::bind(&data).unwrap();
    assert!(probe.is_held(&data).unwrap());
    drop(listener);
    assert!(!probe.is_held(&data).unwrap());
}

/// Permission-changing tests must not chmod the shared per-user runtime folder.
#[cfg(unix)]
pub(crate) fn private_endpoint() -> (tempfile::TempDir, Endpoint) {
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let dir = tmp.path().join("run");
    let endpoint = Endpoint {
        data_dir: tmp.path().join("data"),
        socket: Ok(broscribe_platform_unix::SocketLocation {
            path: dir.join("daemon.sock"),
            dir,
            fallback: false,
        }),
    };
    ensure_private_dir(&endpoint.socket_location().unwrap().dir).unwrap();
    (tmp, endpoint)
}

#[cfg(unix)]
#[test]
fn socket_resolution_failure_reaches_the_client_and_listener() {
    use std::io::ErrorKind;

    let tmp = tempfile::tempdir().unwrap();
    let message = "Unix socket path is too long for sun_path";
    let endpoint = Endpoint {
        data_dir: tmp.path().join("data"),
        socket: Err((ErrorKind::InvalidInput, message.into())),
    };
    let Err(client_error) = Client::connect_daemon(&endpoint, Duration::from_secs(1)) else {
        panic!("expected a socket resolution error");
    };
    assert_eq!(client_error.kind(), ErrorKind::InvalidInput);
    assert_eq!(client_error.to_string(), message);
    assert!(matches!(
        platform::ServerListener::bind(&endpoint),
        Err(ListenError::Io(error)) if error.kind() == ErrorKind::InvalidInput && error.to_string() == message
    ));
    assert!(!endpoint.data_dir().exists());
}

/// What the synthetic daemon does with one request.
pub(crate) enum Reply {
    Respond(Response),
    /// Keep the connection open without answering.
    Hold,
    /// Close the connection without answering (a lost response).
    Close,
    /// Answer, then stream these events.
    Stream(Response, Vec<Event>),
}

pub(crate) struct FakeDaemon {
    pub(crate) endpoint: Endpoint,
    seen: Arc<Mutex<Vec<(RequestId, Call)>>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl FakeDaemon {
    pub(crate) fn start(handler: impl FnMut(&Request) -> Reply + Send + 'static) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let log = Arc::clone(&seen);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                type Handler = Box<dyn FnMut(&Request) -> Reply + Send>;
                let handler: Arc<Mutex<Handler>> = Arc::new(Mutex::new(Box::new(handler)));
                let mut listener = Listener::bind(&data).unwrap();
                ready_tx.send(listener.endpoint().clone()).unwrap();
                // Like the daemon: every connection is served concurrently.
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let connection = tokio::select! {
                        _ = &mut stopped => break,
                        accepted = listener.accept() => accepted.unwrap(),
                    };
                    let handler = Arc::clone(&handler);
                    let log = Arc::clone(&log);
                    connections.spawn(async move {
                        let (mut reader, mut writer) = connection.into_split();
                        while let Ok(Some(line)) = reader.read_line().await {
                            let request = Request::parse(&line).unwrap();
                            log.lock()
                                .unwrap()
                                .push((request.id.clone(), request.call.clone()));
                            let reply = {
                                let mut handler = handler.lock().unwrap();
                                (*handler)(&request)
                            };
                            match reply {
                                Reply::Respond(response) => {
                                    if writer
                                        .write_line(response.to_line().unwrap())
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                // Keeps the connection open until the daemon stops.
                                Reply::Hold => std::future::pending::<()>().await,
                                Reply::Close => return,
                                Reply::Stream(response, events) => {
                                    if writer
                                        .write_line(response.to_line().unwrap())
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                    for event in events {
                                        if writer
                                            .write_line(event.to_line().unwrap())
                                            .await
                                            .is_err()
                                        {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    });
                }
                connections.abort_all();
            });
        });
        let endpoint = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        Self {
            endpoint,
            seen,
            stop: Some(stop),
            thread: Some(thread),
            _dir: dir,
        }
    }

    pub(crate) fn seen(&self) -> Vec<(RequestId, Call)> {
        self.seen.lock().unwrap().clone()
    }

    fn mutations(&self) -> Vec<RequestId> {
        self.seen()
            .into_iter()
            .filter(|(_, call)| !matches!(call, Call::RequestStatus { .. }))
            .map(|(id, _)| id)
            .collect()
    }

    fn asked(&self) -> Vec<RequestId> {
        self.seen()
            .into_iter()
            .filter_map(|(_, call)| match call {
                Call::RequestStatus { request_id } => Some(request_id),
                _ => None,
            })
            .collect()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn stop_call() -> Call {
    Call::RecordStop {
        session_id: "s-1".into(),
    }
}

/// Loses every mutation's response, then answers each `request.status`
/// with `states` in turn (the last repeats).
fn lossy(states: Vec<RequestState>) -> FakeDaemon {
    let mut asked = 0;
    FakeDaemon::start(move |request| match &request.call {
        Call::RequestStatus { .. } => {
            let state = states.get(asked).or(states.last()).unwrap();
            asked += 1;
            Reply::Respond(Response::ok(
                request.id.clone(),
                serde_json::to_value(state).unwrap(),
            ))
        }
        _ => Reply::Close,
    })
}

#[test]
fn calls_round_trip_and_subscriptions_stream_events() {
    let daemon = FakeDaemon::start(|request| match request.call {
        Call::Subscribe => Reply::Stream(
            Response::ok(request.id.clone(), json!(null)),
            vec![Event::Meters {
                me_dbfs: -20.0,
                others_dbfs: -30.0,
            }],
        ),
        _ => Reply::Respond(Response::ok(request.id.clone(), json!({"ok": true}))),
    });
    let mut client = Client::connect_daemon(&daemon.endpoint, Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(client.call(Call::Status).unwrap(), json!({"ok": true}));
    let mut events = Client::connect_daemon(&daemon.endpoint, Duration::from_secs(5))
        .unwrap()
        .unwrap()
        .subscribe()
        .unwrap();
    assert!(matches!(
        events.next_event().unwrap(),
        Some(Event::Meters { .. })
    ));
}

#[test]
fn a_missing_daemon_is_none_and_one_listener_owns_a_folder() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Client::connect_daemon(&Endpoint::new(dir.path()), Duration::from_secs(2))
            .unwrap()
            .is_none()
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _context = runtime.enter();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let first = Listener::bind(&data).unwrap();
    assert!(matches!(
        Listener::bind(&data),
        Err(ListenError::AlreadyRunning { .. })
    ));
    drop(first);
    Listener::bind(&data).unwrap();
}

#[test]
fn a_late_completion_inside_the_window_is_returned_without_resending() {
    let daemon = lossy(vec![
        RequestState::Running,
        RequestState::Running,
        RequestState::Done {
            result: json!({"connected": true}),
        },
    ]);
    let value = Client::call_mutating_within(
        &daemon.endpoint,
        stop_call(),
        Duration::from_secs(5),
        Duration::from_secs(30),
    )
    .unwrap();
    assert_eq!(value, json!({"connected": true}));
    let mutations = daemon.mutations();
    assert_eq!(mutations.len(), 1, "the write was sent again");
    let asked = daemon.asked();
    assert_eq!(asked.len(), 3);
    assert!(asked.iter().all(|id| *id == mutations[0]));
}

#[test]
fn a_request_still_running_or_unknown_stays_uncertain() {
    for (states, window) in [
        (vec![RequestState::Running], Duration::from_millis(1200)),
        (vec![RequestState::Unknown], Duration::from_secs(30)),
    ] {
        let daemon = lossy(states);
        let started = Instant::now();
        let err = Client::call_mutating_within(
            &daemon.endpoint,
            stop_call(),
            Duration::from_secs(5),
            window,
        )
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        let mutations = daemon.mutations();
        assert_eq!(mutations.len(), 1, "the write was sent again");
        let CallError::Uncertain { request_id, .. } = err else {
            panic!("{err}");
        };
        // The caller learns the original ID; no other ID was asked about.
        assert_eq!(request_id, mutations[0]);
        assert!(daemon.asked().iter().all(|id| *id == request_id));
    }
}

#[test]
fn remote_errors_keep_their_code() {
    let daemon = FakeDaemon::start(|request| {
        Reply::Respond(Response::err(
            request.id.clone(),
            IpcError::new("busy", "synthetic"),
        ))
    });
    let err =
        Client::call_mutating(&daemon.endpoint, stop_call(), Duration::from_secs(5)).unwrap_err();
    assert!(
        matches!(&err, CallError::Remote(e) if e.code == "busy"),
        "{err}"
    );
}

#[test]
fn a_silent_daemon_cannot_outlast_the_total_deadline() {
    let daemon = FakeDaemon::start(|_| Reply::Hold);
    let mut client = Client::connect_daemon(&daemon.endpoint, Duration::from_millis(400))
        .unwrap()
        .unwrap();
    let started = Instant::now();
    let err = client.call(Call::Status).unwrap_err();
    assert!(
        matches!(&err, CallError::NotSent(e) if e.kind() == std::io::ErrorKind::TimedOut),
        "{err}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn cancellation_before_sending_is_not_sent_and_after_sending_is_uncertain() {
    let daemon = FakeDaemon::start(|_| Reply::Hold);
    let cancel = CancelToken::new();
    cancel.cancel();
    let err = Client::call_mutating_cancellable(
        &daemon.endpoint,
        stop_call(),
        Duration::from_secs(5),
        Duration::from_secs(30),
        &cancel,
    )
    .unwrap_err();
    assert!(
        matches!(&err, CallError::NotSent(e) if e.kind() == std::io::ErrorKind::Interrupted),
        "{err}"
    );
    assert!(
        daemon.seen().is_empty(),
        "a cancelled call reached the daemon"
    );

    let cancel = CancelToken::new();
    let later = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        later.cancel();
    });
    let started = Instant::now();
    let err = Client::call_mutating_cancellable(
        &daemon.endpoint,
        stop_call(),
        Duration::from_secs(30),
        Duration::from_secs(30),
        &cancel,
    )
    .unwrap_err();
    canceller.join().unwrap();
    assert!(matches!(err, CallError::Uncertain { .. }), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(daemon.mutations().len(), 1, "the write was sent again");
}

/// Loses every mutation's response, then accepts each `request.status`
/// question and never answers it; `asked` hears of each question once the
/// daemon has read it.
fn lossy_then_silent(asked: std::sync::mpsc::Sender<()>) -> FakeDaemon {
    FakeDaemon::start(move |request| match request.call {
        Call::RequestStatus { .. } => {
            let _ = asked.send(());
            Reply::Hold
        }
        _ => Reply::Close,
    })
}

/// The lone mutation was sent once and only its ID was asked about.
fn assert_reconciled_without_resending(daemon: &FakeDaemon, err: CallError) {
    let CallError::Uncertain { request_id, .. } = err else {
        panic!("{err}");
    };
    assert_eq!(daemon.mutations(), vec![request_id.clone()]);
    let asked = daemon.asked();
    assert!(!asked.is_empty());
    assert!(asked.iter().all(|id| *id == request_id));
}

#[test]
fn a_silent_status_answer_ends_with_the_reconcile_window() {
    let (asked, status_seen) = std::sync::mpsc::channel();
    let daemon = lossy_then_silent(asked);
    // A request timeout far beyond the window: the status question must not
    // get a fresh one.
    let timeout = Duration::from_secs(120);
    let started = Instant::now();
    let err = Client::call_mutating_within(
        &daemon.endpoint,
        stop_call(),
        timeout,
        Duration::from_secs(2),
    )
    .unwrap_err();
    let elapsed = started.elapsed();
    status_seen
        .try_recv()
        .expect("the silent status question was never asked");
    assert!(elapsed < timeout / 4, "{elapsed:?}");
    assert_reconciled_without_resending(&daemon, err);
}

#[test]
fn cancellation_interrupts_a_silent_status_answer() {
    let (asked, status_seen) = std::sync::mpsc::channel();
    let daemon = lossy_then_silent(asked);
    let cancel = CancelToken::new();
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            // Only once the status question waits on the silent daemon.
            status_seen
                .recv_timeout(Duration::from_secs(60))
                .expect("the status question was never asked");
            cancel.cancel();
        })
    };
    let timeout = Duration::from_secs(120);
    let started = Instant::now();
    let err =
        Client::call_mutating_cancellable(&daemon.endpoint, stop_call(), timeout, timeout, &cancel)
            .unwrap_err();
    let elapsed = started.elapsed();
    canceller.join().unwrap();
    assert!(elapsed < timeout / 4, "{elapsed:?}");
    assert_reconciled_without_resending(&daemon, err);
}

#[cfg(unix)]
mod unix_folder {
    use std::fs::{self, Permissions};
    use std::io::{BufRead, BufReader, ErrorKind, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn mutating_calls_refuse_a_shared_socket_folder() {
        let (_tmp, endpoint) = private_endpoint();
        let socket = endpoint.socket_location().unwrap();
        let listener = UnixListener::bind(&socket.path).unwrap();
        listener.set_nonblocking(true).unwrap();
        fs::set_permissions(&socket.dir, Permissions::from_mode(0o755)).unwrap();
        let err =
            Client::call_mutating(&endpoint, stop_call(), Duration::from_secs(1)).unwrap_err();
        assert!(
            matches!(&err, CallError::NotSent(e) if e.kind() == ErrorKind::PermissionDenied),
            "{err}"
        );
        assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
        fs::set_permissions(&socket.dir, Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn reconciliation_stops_when_the_folder_turns_shared() {
        let (_tmp, endpoint) = private_endpoint();
        let socket = endpoint.socket_location().unwrap().clone();
        let listener = UnixListener::bind(&socket.path).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            // The folder becomes shared, then the connection breaks.
            fs::set_permissions(&socket.dir, Permissions::from_mode(0o755)).unwrap();
            drop(stream);
            listener.set_nonblocking(true).unwrap();
            std::thread::sleep(Duration::from_millis(1200));
            let reconnected = listener.accept().is_ok();
            fs::set_permissions(&socket.dir, Permissions::from_mode(0o700)).unwrap();
            reconnected
        });
        let err =
            Client::call_mutating(&endpoint, stop_call(), Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, CallError::Uncertain { .. }), "{err}");
        assert!(
            !server.join().unwrap(),
            "reconcile connected to a shared folder"
        );
    }

    #[test]
    fn cancellation_mid_write_stops_without_repeating_bytes() {
        let (_tmp, endpoint) = private_endpoint();
        let listener = UnixListener::bind(&endpoint.socket_location().unwrap().path).unwrap();
        let (partial_tx, partial_rx) = std::sync::mpsc::channel();
        let (returned_tx, returned_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut received = vec![0; 4096];
            stream.read_exact(&mut received).unwrap();
            partial_tx.send(()).unwrap();
            // Reads nothing more until the client has returned.
            returned_rx.recv().unwrap();
            stream.read_to_end(&mut received).unwrap();
            received
        });
        let cancel = CancelToken::new();
        let canceller = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                // Only after the daemon has read part of the request.
                let _ = partial_rx.recv();
                cancel.cancel();
            })
        };
        // A valid request line far larger than the socket buffers.
        let call = Call::RecordStop {
            session_id: "s".repeat(900_000),
        };
        let timeout = Duration::from_secs(120);
        let started = Instant::now();
        let err =
            Client::call_mutating_cancellable(&endpoint, call.clone(), timeout, timeout, &cancel)
                .unwrap_err();
        let elapsed = started.elapsed();
        returned_tx.send(()).unwrap();
        canceller.join().unwrap();
        assert!(elapsed < timeout / 4, "{elapsed:?}");
        let CallError::Uncertain { request_id, .. } = err else {
            panic!("{err}");
        };
        let mut line = Request {
            id: request_id,
            call,
        }
        .to_line()
        .unwrap();
        line.push('\n');
        assert!(line.len() < broscribe_ipc::MAX_LINE_BYTES);
        let received = server.join().unwrap();
        assert!(received.len() < line.len(), "the whole request was written");
        assert!(
            line.as_bytes().starts_with(&received),
            "bytes were lost or repeated"
        );
    }
}
