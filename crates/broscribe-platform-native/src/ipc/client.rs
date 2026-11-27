//! Blocking client for the CLI, MCP and the app (on a background thread).
//!
//! Every call has a total deadline covering its write and its response
//! (one-shot mutating calls also cover the connection). Mutating calls go
//! through [`Client::call_mutating`]: when the connection breaks after the
//! request was sent, it asks the daemon for the request's outcome by ID
//! (`request.status`) instead of guessing, and reports
//! [`CallError::Uncertain`] when the daemon cannot say. Every question
//! (connect, write, response) ends with the reconcile window and stops at
//! the caller's cancellation. Only [`CallError::NotSent`] allows the caller
//! to run the operation itself.

use std::fmt;
use std::io::{self, ErrorKind};
use std::time::{Duration, Instant};

use broscribe_ipc::{
    Call, Event, IpcError, MAX_LINE_BYTES, Request, RequestId, RequestState, Response,
    ServerMessage,
};
use serde_json::Value;

use super::framing::{Lines, ShortWrite, next_slice, write_all};
use super::{CancelToken, Endpoint, platform};

/// Total time of one request (write and response).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Connecting and the reply of a call the agent answers at once (a ping or
/// status). Also the budget of a probe that may meet an agent that has just
/// started: its endpoint accepts connections before it serves them, so a
/// shorter budget turns a busy machine's start into a false failure.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
/// How long [`Client::call_mutating`] keeps asking about a request that is
/// still running after the connection broke; [`Client::call_mutating_within`]
/// takes a longer budget for calls that may wait on the user.
pub const RECONCILE_WINDOW: Duration = Duration::from_secs(120);
const RECONCILE_PAUSE: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub enum CallError {
    /// Nothing reached the daemon (no daemon, connection refused, cancelled
    /// before sending): the caller may run the operation itself.
    NotSent(io::Error),
    /// The daemon answered with an error.
    Remote(IpcError),
    /// The request may or may not have run: the connection broke after it
    /// was sent and the daemon could not tell its outcome. Never run it
    /// again locally; report it.
    Uncertain {
        request_id: RequestId,
        detail: String,
    },
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::NotSent(e) => write!(f, "daemon not reached: {e}"),
            CallError::Remote(e) => write!(f, "{e}"),
            CallError::Uncertain { request_id, detail } => write!(
                f,
                "the daemon may or may not have run request {request_id} ({detail}); check before retrying"
            ),
        }
    }
}

impl std::error::Error for CallError {}

pub struct Client {
    lines: Lines<platform::Wire>,
    timeout: Duration,
}

impl Client {
    /// Connects to the daemon of `endpoint` within `timeout`; later calls
    /// each get `timeout` too. `None` when no daemon listens. A Unix socket
    /// folder that is not private to this user is `PermissionDenied`.
    pub fn connect_daemon(endpoint: &Endpoint, timeout: Duration) -> io::Result<Option<Self>> {
        let deadline = Instant::now() + timeout;
        match Self::connect_by(endpoint, deadline, None) {
            Ok(lines) => Ok(Some(Self { lines, timeout })),
            Err(e) if platform::is_absent(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Connects before `deadline`. With `cancel`, connection attempts are
    /// sliced so cancellation is noticed (`Interrupted`); retrying an
    /// attempt cut short is safe because nothing is written before it
    /// completes.
    fn connect_by(
        endpoint: &Endpoint,
        deadline: Instant,
        cancel: Option<&CancelToken>,
    ) -> io::Result<Lines<platform::Wire>> {
        loop {
            let Some(slice) = next_slice(deadline, cancel) else {
                return Err(cancelled_before_sending());
            };
            match platform::connect(endpoint, slice) {
                Err(e) if e.kind() == ErrorKind::TimedOut && slice < deadline => {}
                result => return result.map(Lines::new),
            }
        }
    }

    /// A read-only call (or one the caller does not need to reconcile);
    /// transport failures are [`CallError::NotSent`].
    pub fn call(&mut self, call: Call) -> Result<Value, CallError> {
        let deadline = Instant::now() + self.timeout;
        let response = self
            .call_by(call, deadline, None)
            .map_err(CallError::NotSent)?;
        response.outcome.map_err(CallError::Remote)
    }

    /// Sends `call` and waits for its response, all before `deadline`.
    fn call_by(
        &mut self,
        call: Call,
        deadline: Instant,
        cancel: Option<&CancelToken>,
    ) -> io::Result<Response> {
        let request = Request::new(call);
        self.send(&request, deadline, cancel)
            .map_err(|short| short.error)?;
        self.read_response(&request.id, deadline, cancel)
    }

    /// Runs a mutating call on the daemon of `endpoint`, reconciling by
    /// request ID when the connection breaks after sending. `timeout` bounds
    /// connecting, sending and the response together.
    pub fn call_mutating(
        endpoint: &Endpoint,
        call: Call,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        Self::call_mutating_within(endpoint, call, timeout, RECONCILE_WINDOW)
    }

    /// [`Self::call_mutating`] that keeps asking about the same request ID
    /// for `reconcile_window` after the connection breaks. It never resends
    /// the call under a new ID.
    pub fn call_mutating_within(
        endpoint: &Endpoint,
        call: Call,
        timeout: Duration,
        reconcile_window: Duration,
    ) -> Result<Value, CallError> {
        Self::call_mutating_cancellable(
            endpoint,
            call,
            timeout,
            reconcile_window,
            &CancelToken::new(),
        )
    }

    /// [`Self::call_mutating_within`] that `cancel` interrupts: before any
    /// byte of the request is written it is [`CallError::NotSent`]
    /// (`Interrupted`); after, writing, waiting and reconciliation stop with
    /// [`CallError::Uncertain`].
    pub fn call_mutating_cancellable(
        endpoint: &Endpoint,
        call: Call,
        timeout: Duration,
        reconcile_window: Duration,
        cancel: &CancelToken,
    ) -> Result<Value, CallError> {
        let deadline = Instant::now() + timeout;
        let lines =
            Self::connect_by(endpoint, deadline, Some(cancel)).map_err(CallError::NotSent)?;
        Self { lines, timeout }.mutate_by(endpoint, call, deadline, reconcile_window, cancel)
    }

    /// Sends a mutation to the peer already verified on this connection.
    /// Reconnects only to reconcile its request ID, never to send the mutation.
    pub fn call_connected_mutating(
        self,
        endpoint: &Endpoint,
        call: Call,
        cancel: &CancelToken,
    ) -> Result<Value, CallError> {
        let deadline = Instant::now() + self.timeout;
        self.mutate_by(endpoint, call, deadline, RECONCILE_WINDOW, cancel)
    }

    fn mutate_by(
        mut self,
        endpoint: &Endpoint,
        call: Call,
        deadline: Instant,
        reconcile_window: Duration,
        cancel: &CancelToken,
    ) -> Result<Value, CallError> {
        let request = Request::new(call);
        let outcome = match self.send(&request, deadline, Some(cancel)) {
            // Not one byte reached the daemon: nothing can have run.
            Err(short) if short.written == 0 => return Err(CallError::NotSent(short.error)),
            Err(short) => Err(short.error),
            Ok(()) => self.read_response(&request.id, deadline, Some(cancel)),
        };
        match outcome {
            Ok(response) => response.outcome.map_err(CallError::Remote),
            Err(e) => reconcile(
                endpoint,
                &request.id,
                self.timeout,
                reconcile_window,
                cancel,
                e.to_string(),
            ),
        }
    }

    /// Subscribes to events; the connection then only carries events.
    pub fn subscribe(mut self) -> Result<Subscription, CallError> {
        self.call(Call::Subscribe)?;
        Ok(Subscription { lines: self.lines })
    }

    /// Writes `request` before `deadline`; a short write reports how much of
    /// it the daemon may have received.
    fn send(
        &mut self,
        request: &Request,
        deadline: Instant,
        cancel: Option<&CancelToken>,
    ) -> Result<(), ShortWrite> {
        let mut line = request.to_line().map_err(|e| ShortWrite {
            written: 0,
            error: io::Error::other(e),
        })?;
        line.push('\n');
        write_all(self.lines.wire_mut(), line.as_bytes(), deadline, cancel)
    }

    fn read_response(
        &mut self,
        id: &RequestId,
        deadline: Instant,
        cancel: Option<&CancelToken>,
    ) -> io::Result<Response> {
        loop {
            let Some(slice) = next_slice(deadline, cancel) else {
                return Err(io::Error::new(
                    ErrorKind::Interrupted,
                    "cancelled while waiting for the daemon",
                ));
            };
            let line = match self.lines.read_line(MAX_LINE_BYTES, Some(slice)) {
                Ok(Some(line)) => line,
                Ok(None) => {
                    return Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "the daemon closed the connection",
                    ));
                }
                Err(e) if e.kind() == ErrorKind::TimedOut && slice < deadline => continue,
                Err(e) => return Err(e),
            };
            match ServerMessage::parse(&line)
                .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?
            {
                ServerMessage::Response(resp) if resp.id == *id => return Ok(resp),
                ServerMessage::Response(_) | ServerMessage::Event(_) => {}
            }
        }
    }
}

fn cancelled_before_sending() -> io::Error {
    io::Error::new(ErrorKind::Interrupted, "cancelled before sending")
}

/// Asks about `id` until the daemon knows its outcome, it says it never saw
/// it, `window` passes or `cancel` is set. Each question (connect, write,
/// response) gets at most `timeout` and never outlasts the window's end.
fn reconcile(
    endpoint: &Endpoint,
    id: &RequestId,
    timeout: Duration,
    window: Duration,
    cancel: &CancelToken,
    detail: String,
) -> Result<Value, CallError> {
    let window_end = Instant::now() + window;
    let uncertain = |detail: String| CallError::Uncertain {
        request_id: id.clone(),
        detail,
    };
    while Instant::now() < window_end {
        if cancel.is_cancelled() {
            return Err(uncertain(format!("{detail}; cancelled")));
        }
        // An endpoint that stopped being private may belong to another
        // user: never send it the request ID; a missing one may come back.
        if let Err(e) = platform::precheck(endpoint) {
            return Err(uncertain(format!("{detail}; {e}")));
        }
        let deadline = window_end.min(Instant::now() + timeout);
        match ask(endpoint, id, deadline, cancel) {
            Some(RequestState::Done { result }) => return Ok(result),
            Some(RequestState::Failed { error }) => return Err(CallError::Remote(error)),
            Some(RequestState::Unknown) => {
                return Err(uncertain(format!(
                    "{detail}; the daemon has no record of it"
                )));
            }
            Some(RequestState::Running) | None => {}
        }
        let pause_end = window_end.min(Instant::now() + RECONCILE_PAUSE);
        while let Some(slice) = next_slice(pause_end, Some(cancel)) {
            let left = slice.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left);
        }
    }
    if cancel.is_cancelled() {
        return Err(uncertain(format!("{detail}; cancelled")));
    }
    Err(uncertain(format!("{detail}; still unresolved")))
}

/// What the daemon knows about `id`, asked before `deadline`; `None` when it
/// could not tell (unreachable, silent, cancelled or an invalid answer).
fn ask(
    endpoint: &Endpoint,
    id: &RequestId,
    deadline: Instant,
    cancel: &CancelToken,
) -> Option<RequestState> {
    let lines = Client::connect_by(endpoint, deadline, Some(cancel)).ok()?;
    let mut client = Client {
        lines,
        timeout: deadline.saturating_duration_since(Instant::now()),
    };
    let call = Call::RequestStatus {
        request_id: id.clone(),
    };
    let response = client.call_by(call, deadline, Some(cancel)).ok()?;
    serde_json::from_value(response.outcome.ok()?).ok()
}

/// An event stream (after `subscribe`).
pub struct Subscription {
    lines: Lines<platform::Wire>,
}

impl Subscription {
    /// The next event; `None` when the daemon closed the stream.
    pub fn next_event(&mut self) -> io::Result<Option<Event>> {
        self.next_event_by(None)
    }

    /// The next event before the deadline. A timeout preserves partial lines.
    pub fn next_event_before(&mut self, deadline: Instant) -> io::Result<Option<Event>> {
        self.next_event_by(Some(deadline))
    }

    fn next_event_by(&mut self, deadline: Option<Instant>) -> io::Result<Option<Event>> {
        loop {
            let Some(line) = self.lines.read_line(MAX_LINE_BYTES, deadline)? else {
                return Ok(None);
            };
            match ServerMessage::parse(&line)
                .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?
            {
                ServerMessage::Event(e) => return Ok(Some(e)),
                ServerMessage::Response(_) => {}
            }
        }
    }
}
