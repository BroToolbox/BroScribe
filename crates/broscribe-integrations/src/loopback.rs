//! Loopback redirect listener for the authorization code flow (RFC 8252 §7.3).
//!
//! - Bound to `127.0.0.1` only, on a random port.
//! - The `state` is compared in constant time before anything else is used.
//! - Requests with a wrong path, method or `state` get an error page and do
//!   not consume the callback: the listener keeps waiting for the valid one.
//! - The first callback with the right `state` ends the wait (success or the
//!   provider's error), and the listener is closed.
//! - The caller's `still_current` check runs at least every [`CANCEL_POLL`]; once it
//!   fails the wait ends with its error and the listener is closed, so a cancelled or
//!   expired sign-in never accepts a later callback.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crate::encode::parse_query;
use crate::pkce::ct_eq;
use crate::{IntegrationError, Result};

/// How long the browser has to come back.
pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
/// How often a waiting sign-in asks whether it is still wanted.
pub const CANCEL_POLL: Duration = Duration::from_millis(200);

const POLL: Duration = Duration::from_millis(25);
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_REQUEST: usize = 16 * 1024;

pub struct LoopbackListener {
    listener: TcpListener,
    port: u16,
}

/// What the valid callback carried.
enum Callback {
    Code(String),
    Refused(IntegrationError),
}

/// Classifies a provider's callback refusal from its registered `error` code and the
/// numeric Microsoft `AADSTS` codes in `error_description`. Neither raw value is logged or
/// returned: only a fixed message, plus the error code when it is a plain OAuth token.
fn refusal(error: &str, description: Option<&str>) -> IntegrationError {
    let aadsts = |wanted: &[&str]| {
        description.is_some_and(|d| {
            d.split(|c: char| !c.is_ascii_alphanumeric()).any(|token| {
                token
                    .strip_prefix("AADSTS")
                    .is_some_and(|code| wanted.contains(&code))
            })
        })
    };
    if aadsts(&["53003"]) {
        return IntegrationError::ConditionalAccessBlocked;
    }
    if error == "admin_policy_enforced" || aadsts(&["90094", "90095"]) {
        return crate::service::service_error(
            "admin_consent_required",
            "an administrator must approve BroScribe's read-only permissions for this organisation; BroScribe will not retry with another client or scope",
        );
    }
    if error == "access_denied" {
        return IntegrationError::Denied("sign-in was cancelled or refused".into());
    }
    let plain = !error.is_empty()
        && error.len() <= 64
        && error.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    IntegrationError::Denied(if plain {
        format!("the provider returned {error}")
    } else {
        "the provider returned an error".into()
    })
}

impl LoopbackListener {
    pub fn bind() -> Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| IntegrationError::Transient(format!("loopback listener: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| IntegrationError::Transient(format!("loopback listener: {e}")))?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|e| IntegrationError::Transient(format!("loopback listener: {e}")))?;
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>/`, registered as `http://127.0.0.1` (any
    /// port) with the provider.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    /// Waits for the callback carrying `state` and returns its `code`. Ends early with
    /// the error of `still_current` once that check fails (cancellation or expiry).
    pub fn wait_for_code(
        self,
        state: &str,
        timeout: Duration,
        still_current: &dyn Fn() -> Result<()>,
    ) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let mut next_check = Instant::now();
        loop {
            let now = Instant::now();
            if now >= next_check {
                still_current()?;
                next_check = now + CANCEL_POLL;
            }
            if now >= deadline {
                return Err(IntegrationError::Timeout);
            }
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if !peer.ip().is_loopback() {
                        continue;
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    match handle(stream, state, READ_TIMEOUT.min(remaining)) {
                        Some(Callback::Code(code)) => return Ok(code),
                        Some(Callback::Refused(e)) => return Err(e),
                        None => {}
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(POLL.min(deadline - now).min(next_check - now));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    return Err(IntegrationError::Transient(format!(
                        "loopback listener: {e}"
                    )));
                }
            }
        }
        // `self` (and the listening socket) is dropped on return.
    }
}

/// Serves one connection; `Some` only for the request carrying `state`.
fn handle(mut stream: TcpStream, state: &str, timeout: Duration) -> Option<Callback> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(timeout.max(Duration::from_millis(1))));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let Some(target) = read_request_target(&mut stream, Instant::now() + timeout) else {
        respond(&mut stream, 400, "Bad request.");
        return None;
    };
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if path != "/" {
        respond(&mut stream, 404, "Not found.");
        return None;
    }
    let params = parse_query(query);
    if ["state", "code", "error"]
        .iter()
        .any(|key| params.iter().filter(|(k, _)| k == key).count() > 1)
    {
        respond(&mut stream, 400, "Ambiguous callback parameters.");
        return None;
    }
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let state_ok = get("state").is_some_and(|s| ct_eq(s.as_bytes(), state.as_bytes()));
    if !state_ok {
        respond(
            &mut stream,
            400,
            "This sign-in response does not belong to the sign-in BroScribe is waiting for.",
        );
        return None;
    }
    if let Some(error) = get("error") {
        respond(
            &mut stream,
            200,
            "Sign-in was not completed. You can close this tab and return to BroScribe.",
        );
        return Some(Callback::Refused(refusal(error, get("error_description"))));
    }
    match get("code").filter(|c| !c.is_empty()) {
        Some(code) => {
            respond(
                &mut stream,
                200,
                "Signed in. You can close this tab and return to BroScribe.",
            );
            Some(Callback::Code(code.to_string()))
        }
        None => {
            respond(
                &mut stream,
                400,
                "The sign-in response has no authorization code.",
            );
            None
        }
    }
}

/// Reads the request head and returns the target of a `GET` request line.
fn read_request_target(stream: &mut TcpStream, deadline: Instant) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() >= MAX_REQUEST || Instant::now() >= deadline {
            return None;
        }
        stream
            .set_read_timeout(Some(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ))
            .ok()?;
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = std::str::from_utf8(&buf).ok()?;
    let line = head.lines().next()?;
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    (method == "GET" && parts.next().is_some_and(|v| v.starts_with("HTTP/")))
        .then(|| target.to_string())
}

fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>BroScribe</title></head>\
         <body><p>{message}</p></body></html>"
    );
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead as _;

    fn get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        write!(s, "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
        let mut line = String::new();
        std::io::BufReader::new(s).read_line(&mut line).unwrap();
        line
    }

    fn current() -> Result<()> {
        Ok(())
    }

    #[test]
    fn wrong_requests_do_not_consume_the_callback() {
        let l = LoopbackListener::bind().unwrap();
        let port = l.port();
        assert!(l.redirect_uri().starts_with("http://127.0.0.1:"));
        let waiter = std::thread::spawn(move || {
            l.wait_for_code("good-state", Duration::from_secs(10), &current)
        });
        assert!(get(port, "/favicon.ico").contains(" 404 "));
        assert!(get(port, "/?code=c1&state=bad-state").contains(" 400 "));
        assert!(get(port, "/?code=c1").contains(" 400 "));
        assert!(get(port, "/?state=good-state").contains(" 400 "));
        assert!(get(port, "/?code=the%20code&state=good-state").contains(" 200 "));
        assert_eq!(waiter.join().unwrap().unwrap(), "the code");
        // Closed after the one valid callback.
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
    }

    #[test]
    fn provider_error_ends_the_wait() {
        let l = LoopbackListener::bind().unwrap();
        let port = l.port();
        let waiter =
            std::thread::spawn(move || l.wait_for_code("s", Duration::from_secs(10), &current));
        assert!(get(port, "/?error=access_denied&state=s").contains(" 200 "));
        let err = waiter.join().unwrap().unwrap_err();
        assert_eq!(err.code(), "denied");
    }

    #[test]
    fn callback_refusals_classify_admin_consent_and_conditional_access_without_raw_text() {
        let cases = [
            (
                "/?error=access_denied&error_description=AADSTS90094%3A+synthetic-secret-detail&state=s",
                "admin_consent_required",
            ),
            (
                "/?error=admin_policy_enforced&state=s",
                "admin_consent_required",
            ),
            (
                "/?error=access_denied&error_description=AADSTS53003%3A+synthetic-secret-detail&state=s",
                "conditional_access_blocked",
            ),
            ("/?error=access_denied&state=s", "denied"),
            ("/?error=Synthetic%20secret%20detail&state=s", "denied"),
        ];
        for (target, code) in cases {
            let l = LoopbackListener::bind().unwrap();
            let port = l.port();
            let waiter =
                std::thread::spawn(move || l.wait_for_code("s", Duration::from_secs(10), &current));
            assert!(get(port, target).contains(" 200 "));
            let err = waiter.join().unwrap().unwrap_err();
            assert_eq!(err.code(), code, "{target}");
            let text = err.to_string();
            assert!(!text.contains("secret"), "{text}");
            assert!(!text.contains("AADSTS"), "{text}");
        }
    }

    #[test]
    fn times_out() {
        let l = LoopbackListener::bind().unwrap();
        let err = l
            .wait_for_code("s", Duration::from_millis(100), &current)
            .unwrap_err();
        assert_eq!(err.code(), "timeout");
    }

    #[test]
    fn a_failed_currency_check_ends_the_wait_and_closes_the_listener() {
        let l = LoopbackListener::bind().unwrap();
        let port = l.port();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cancelled.clone();
        let started = Instant::now();
        let waiter = std::thread::spawn(move || {
            l.wait_for_code("s", Duration::from_secs(30), &|| {
                if flag.load(std::sync::atomic::Ordering::SeqCst) {
                    Err(IntegrationError::Denied("synthetic cancellation".into()))
                } else {
                    Ok(())
                }
            })
        });
        std::thread::sleep(Duration::from_millis(100));
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        let err = waiter.join().unwrap().unwrap_err();
        assert_eq!(err.code(), "denied");
        assert!(started.elapsed() < Duration::from_secs(5));
        // A late callback finds nothing listening.
        assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err());
    }
}
