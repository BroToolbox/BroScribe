//! The app shell's platform behaviour (U22):
//!
//! - [`InstanceChannel`]: the desktop app's own single-instance channel, one
//!   per data folder (SHL-14). A second launch hands its request to the
//!   running app through it, so bringing the window forward never depends on
//!   the recording agent. The real channel is a Unix socket in a folder
//!   private to the user (peer user checked on every connection) or a
//!   Windows named pipe with a current-user ACL (peer SID checked on both
//!   ends), in `broscribe-platform-native`.
//! - [`AppActivation`]: the activation policy that gives the app a Dock icon
//!   and a ⌘-Tab entry while a regular window is open (SHL-15). It only has
//!   an effect on macOS; the app applies it through its UI toolkit.
//! - [`StatusIconPlace`]: where the app's status icon lives, for text that
//!   sends the user there.
//!
//! The wire format is one request line answered by one line:
//! [`HANDOFF_DONE`] once the running app has handled the request, anything
//! else (or nothing) when it couldn't.

use std::sync::mpsc;
use std::time::Duration;

use crate::{Error, Result};

/// What a second launch asks the running app to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffRequest {
    /// Bring the main window forward, opening it when it is closed.
    Show,
    NotificationStatus,
    RequestNotifications,
    AccessibilityStatus,
    RequestAccessibility,
    LibrarySwitchStatus,
    LibraryApplied,
    ReloadLibrary,
    Dock(DockCommand),
}

impl HandoffRequest {
    /// The request's line on the wire, without its newline.
    pub fn word(self) -> &'static str {
        match self {
            Self::Show => "show",
            Self::NotificationStatus => "notification-status",
            Self::RequestNotifications => "request-notifications",
            Self::AccessibilityStatus => "accessibility-status",
            Self::RequestAccessibility => "request-accessibility",
            Self::LibrarySwitchStatus => "library-switch-status",
            Self::LibraryApplied => "library-applied",
            Self::ReloadLibrary => "reload-library",
            Self::Dock(command) => command.word(),
        }
    }

    /// The request a line names; `None` for anything else.
    pub fn parse(line: &str) -> Option<Self> {
        match line.trim_end_matches(['\r', '\n']) {
            "show" => Some(Self::Show),
            "notification-status" => Some(Self::NotificationStatus),
            "request-notifications" => Some(Self::RequestNotifications),
            "accessibility-status" => Some(Self::AccessibilityStatus),
            "request-accessibility" => Some(Self::RequestAccessibility),
            "library-switch-status" => Some(Self::LibrarySwitchStatus),
            "library-applied" => Some(Self::LibraryApplied),
            "reload-library" => Some(Self::ReloadLibrary),
            word => DockCommand::parse(word).map(Self::Dock),
        }
    }
}

/// A Dock or jump-list action. Words stay stable as the menu changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockCommand {
    StartRecording,
    StopRecording,
    ShowRecordingControls,
    ReviewQueue,
}

impl DockCommand {
    pub fn word(self) -> &'static str {
        match self {
            Self::StartRecording => "record-start",
            Self::StopRecording => "record-stop",
            Self::ShowRecordingControls => "recording-controls",
            Self::ReviewQueue => "review-queue",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "record-start" => Self::StartRecording,
            "record-stop" => Self::StopRecording,
            "recording-controls" => Self::ShowRecordingControls,
            "review-queue" => Self::ReviewQueue,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DockItem {
    pub command: DockCommand,
    pub label: &'static str,
}

/// Publishes Windows taskbar tasks through the native shell.
pub trait JumpList: Send + Sync {
    fn replace(&self, items: &[DockItem]) -> Result<()>;
}

/// The running app's answer once it handled a request.
pub const HANDOFF_DONE: &str = "done";
/// Its answer when it couldn't handle the request in time or didn't know it.
pub const HANDOFF_REFUSED: &str = "refused";
/// Longest request or answer line, newline included.
pub const HANDOFF_MAX_LINE: usize = 64;
/// How long the running app's channel waits for the app to handle a request
/// before it answers [`HANDOFF_REFUSED`].
pub const HANDOFF_HANDLE_WAIT: Duration = Duration::from_secs(5);
/// How long a second launch waits in all: for the running app's channel to
/// accept (it may still be starting), for the request to be handled and for
/// the answer.
pub const HANDOFF_TIMEOUT: Duration = Duration::from_secs(8);

/// One request delivered to the running app. [`Handoff::done`] answers the
/// second launch; dropping it unanswered refuses.
#[derive(Debug)]
pub struct Handoff {
    pub request: HandoffRequest,
    answered: mpsc::SyncSender<&'static str>,
}

impl Handoff {
    /// A request and the receiver its answer arrives on.
    pub fn new(request: HandoffRequest) -> (Self, mpsc::Receiver<&'static str>) {
        let (answered, receiver) = mpsc::sync_channel(1);
        (Self { request, answered }, receiver)
    }

    /// The app handled the request.
    pub fn done(self) {
        self.reply(HANDOFF_DONE);
    }

    /// Return a fixed protocol value to the caller.
    pub fn reply(self, value: &'static str) {
        let _ = self.answered.try_send(value);
    }
}

/// Where the running app's channel delivers second launches' requests.
pub type HandoffSender = futures::channel::mpsc::UnboundedSender<Handoff>;
/// Where the running app receives them, drained on its UI loop.
pub type HandoffReceiver = futures::channel::mpsc::UnboundedReceiver<Handoff>;

/// A new pair: the sender goes to [`InstanceChannel::listen`].
pub fn handoff_channel() -> (HandoffSender, HandoffReceiver) {
    futures::channel::mpsc::unbounded()
}

/// The next request, waiting for it on this thread; `None` once every
/// sender is gone. For a thread that does nothing else (tests): the app
/// drains its receiver on the UI loop.
pub fn next_handoff_blocking(requests: &mut HandoffReceiver) -> Option<Handoff> {
    loop {
        match requests.try_recv() {
            Ok(handoff) => return Some(handoff),
            Err(e) if e.is_closed() => return None,
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// Hands the request `line` names to the app through `sender` and waits up
/// to `wait` for it to be handled; returns the answer line (no newline).
/// Every [`InstanceChannel`] answers through this.
pub fn answer_request(line: &str, sender: &HandoffSender, wait: Duration) -> &'static str {
    let Some(request) = HandoffRequest::parse(line) else {
        return HANDOFF_REFUSED;
    };
    let (handoff, answered) = Handoff::new(request);
    if sender.unbounded_send(handoff).is_err() {
        return HANDOFF_REFUSED;
    }
    match answered.recv_timeout(wait) {
        Ok(value) => value,
        Err(_) => HANDOFF_REFUSED,
    }
}

/// `Ok` when the running app answered that it handled the request.
pub fn check_answer(line: &str) -> Result<()> {
    if line.trim_end_matches(['\r', '\n']) == HANDOFF_DONE {
        Ok(())
    } else {
        Err(Error::Unsupported(
            "the running BroScribe didn't handle the request".into(),
        ))
    }
}

/// Kept by the running app while it listens; dropping it stops listening.
pub trait HandoffListener: Send {}

/// The desktop app's single-instance channel for one data folder (SHL-14).
/// Only the process holding the app instance lock listens.
pub trait InstanceChannel: Send + Sync {
    /// Starts accepting second launches' requests, delivered through
    /// `sender`. Fails when another process already listens here.
    fn listen(&self, sender: HandoffSender) -> Result<Box<dyn HandoffListener>>;

    /// Delivers `request` to the running app and waits, at most `timeout`,
    /// until it answers that it handled it. Retries while the running app
    /// isn't listening yet.
    fn send(&self, request: HandoffRequest, timeout: Duration) -> Result<()> {
        check_answer(&self.request(request, timeout)?)
    }

    /// Asks for a short typed reply from the same authenticated endpoint.
    fn request(&self, request: HandoffRequest, timeout: Duration) -> Result<String>;
}

/// Where the app's status icon lives, named in text that sends the user
/// there (3.19 decision 3: each platform's own place name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusIconPlace {
    /// macOS: the menu bar.
    MenuBar,
    /// Windows: the notification area, often under ^ on the taskbar.
    NotificationArea,
    /// Linux with a StatusNotifierItem host.
    SystemTray,
    /// Linux without a StatusNotifierItem host: the icon shows nowhere.
    NoTrayHost,
}

/// The app's activation policy: whether it has a Dock icon, a ⌘-Tab entry
/// and its menu bar (macOS; other systems list each window on its own).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationPolicy {
    Regular,
    Accessory,
}

/// Sets the activation policy and activates the app, on the UI loop that
/// owns the windows. `Cx` is that loop's context (GPUI's `App` in the app),
/// so this crate needs no UI toolkit; [`crate::fakes::FakeActivation`]
/// records the calls in tests.
pub trait AppActivation<Cx: ?Sized> {
    fn set_activation_policy(&self, policy: ActivationPolicy, cx: &mut Cx);
    /// Brings the app to the front (a deliberate open only).
    fn activate(&self, cx: &mut Cx);
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn requests_and_answers_have_one_line_each() {
        assert_eq!(HandoffRequest::Show.word(), "show");
        assert_eq!(HandoffRequest::parse("show"), Some(HandoffRequest::Show));
        assert_eq!(HandoffRequest::parse("show\n"), Some(HandoffRequest::Show));
        assert_eq!(
            HandoffRequest::parse("show\r\n"),
            Some(HandoffRequest::Show)
        );
        assert_eq!(HandoffRequest::parse("Show"), None);
        assert_eq!(HandoffRequest::parse("show more"), None);
        assert_eq!(HandoffRequest::parse(""), None);
        assert!(check_answer("done\n").is_ok());
        assert!(check_answer(HANDOFF_REFUSED).is_err());
        assert!(check_answer("").is_err());
        assert!(HANDOFF_HANDLE_WAIT < HANDOFF_TIMEOUT);
    }

    #[test]
    fn a_handled_request_is_answered_done() {
        let (sender, mut requests) = handoff_channel();
        let app = std::thread::spawn(move || {
            let handoff = next_handoff_blocking(&mut requests).unwrap();
            assert_eq!(handoff.request, HandoffRequest::Show);
            handoff.done();
        });
        assert_eq!(
            answer_request("show\n", &sender, Duration::from_secs(5)),
            HANDOFF_DONE
        );
        app.join().unwrap();
    }

    #[test]
    fn unknown_unhandled_or_undeliverable_requests_are_refused() {
        let (sender, mut requests) = handoff_channel();
        // Not a request: never delivered.
        assert_eq!(
            answer_request("open", &sender, Duration::from_secs(5)),
            HANDOFF_REFUSED
        );
        assert!(requests.try_recv().is_err(), "nothing was delivered");
        // Delivered but dropped without an answer.
        let app = std::thread::spawn(move || {
            let handoff = next_handoff_blocking(&mut requests).unwrap();
            drop(handoff);
            requests
        });
        assert_eq!(
            answer_request("show", &sender, Duration::from_secs(5)),
            HANDOFF_REFUSED
        );
        let requests = app.join().unwrap();
        // Delivered but never handled: refused once the wait is over.
        let started = Instant::now();
        assert_eq!(
            answer_request("show", &sender, Duration::from_millis(50)),
            HANDOFF_REFUSED
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        // Nobody receives any more.
        drop(requests);
        assert_eq!(
            answer_request("show", &sender, Duration::from_secs(5)),
            HANDOFF_REFUSED
        );
    }
}
