use broscribe_platform::RegisteredProfile;
use broscribe_platform::system::{Notification, NotificationAction, NotificationDelivery};
use futures::channel::oneshot;
use futures::{FutureExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc, LazyLock, Mutex, MutexGuard,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use crate::machine::config_dir;
use broscribe_platform::bounded::{LOGIN_MANAGER_TIMEOUT, login_manager_output};
use broscribe_platform::secret::TIMEOUT_CODE;
use broscribe_platform::{
    AgentServiceStatus, AppPaths, Autostart, BrowserLauncher, Error, HostPlatform, LoginJob,
    LoginRegistrationGuard, Notifier, Result, SecretStore, SettingsPane, SystemSettingsOpener,
};
use broscribe_platform_unix::dirs::open_login_registration_lock;

#[derive(Default)]
pub struct LinuxBrowserLauncher;
impl BrowserLauncher for LinuxBrowserLauncher {
    fn open(&self, url: &str) -> Result<()> {
        broscribe_platform::system::check_web_url(url)?;
        launch_detached(Command::new("xdg-open").arg(url), OPENER_WAIT)
    }
}

pub struct LinuxSystemSettings;

impl SystemSettingsOpener for LinuxSystemSettings {
    fn host_platform(&self) -> HostPlatform {
        HostPlatform::Linux
    }

    fn supports(&self, _: SettingsPane) -> bool {
        false
    }

    fn open(&self, _: SettingsPane) -> Result<()> {
        Err(Error::Unsupported(
            "there is no common system settings pane across Linux desktops".into(),
        ))
    }
}

#[cfg(test)]
#[test]
fn system_settings_panes_are_unsupported_on_linux() {
    assert_eq!(LinuxSystemSettings.host_platform(), HostPlatform::Linux);
    for pane in [
        SettingsPane::Microphone,
        SettingsPane::SystemAudio,
        SettingsPane::Accessibility,
        SettingsPane::Notifications,
        SettingsPane::LoginItems,
        SettingsPane::Taskbar,
    ] {
        assert!(!LinuxSystemSettings.supports(pane));
        assert!(matches!(
            LinuxSystemSettings.open(pane),
            Err(Error::Unsupported(_))
        ));
    }
}

/// How long [`launch_detached`] waits for the opener's own exit status.
const OPENER_WAIT: Duration = Duration::from_secs(2);

/// Starts `command` with null stdio, so a browser it launches never holds a
/// pipe of ours, and reaps it on a background thread. An opener that fails
/// within `wait` is an error; one still running then (it may be the browser
/// itself) is left to the reaper.
fn launch_detached(command: &mut Command, wait: Duration) -> Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let (done, status) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("broscribe-opener-reaper".into())
        .spawn(move || {
            let _ = done.send(child.wait());
        })?;
    match status.recv_timeout(wait) {
        Ok(Ok(status)) if !status.success() => Err(crate::platform_error(
            "opening the browser",
            format!("xdg-open {status}"),
        )),
        Ok(Err(e)) => Err(e.into()),
        Ok(Ok(_)) | Err(_) => Ok(()),
    }
}

/// The default data folder's Secret Service service (keyring's `service`
/// attribute); other profiles append `.profile-<hash>`.
pub const SECRET_SERVICE_BASE: &str = "BroScribe";

#[derive(Clone, Debug)]
pub struct LinuxSecretStore {
    service: String,
    uninstalling: bool,
}
impl LinuxSecretStore {
    /// The store of one data folder's namespace (`secret_namespace`).
    pub fn with_service(service: &str) -> Self {
        Self {
            service: service.to_owned(),
            uninstalling: false,
        }
    }

    pub fn for_uninstall(service: &str) -> Self {
        Self {
            uninstalling: true,
            ..Self::with_service(service)
        }
    }

    fn tolerate_unavailable<T>(&self, result: Result<T>, empty: T) -> Result<T> {
        if result.is_err()
            && self.uninstalling
            && crate::uninstall::secret_service_missing(&self.service)
        {
            return Ok(empty);
        }
        result
    }
}
fn entry(service: &str, key: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service, key).map_err(|_| {
        crate::platform_error(
            "opening Secret Service credential",
            "credential store unavailable",
        )
    })
}
impl SecretStore for LinuxSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        let (service, key) = (self.service.clone(), key.to_owned());
        let result = SECRET_SERVICE.run(Access::Read, move || {
            match entry(&service, &key)?.get_password() {
                Ok(value) => Ok(Some(value)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(_) => Err(crate::platform_error(
                    "reading Secret Service credential",
                    "credential store refused access",
                )),
            }
        });
        self.tolerate_unavailable(result, None)
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        let (service, key, value) = (self.service.clone(), key.to_owned(), value.to_owned());
        SECRET_SERVICE.run(Access::Change, move || {
            entry(&service, &key)?.set_password(&value).map_err(|_| {
                crate::platform_error(
                    "writing Secret Service credential",
                    "credential store refused access",
                )
            })
        })
    }
    fn delete(&self, key: &str) -> Result<()> {
        let (service, key) = (self.service.clone(), key.to_owned());
        let result = SECRET_SERVICE.run(Access::Change, move || {
            match entry(&service, &key)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(_) => Err(crate::platform_error(
                    "deleting Secret Service credential",
                    "credential store refused access",
                )),
            }
        });
        self.tolerate_unavailable(result, ())
    }
}

/// How long one Secret Service call may take. A locked keyring holds a call
/// on its unlock prompt with no deadline of its own. This stays below the
/// agent's longest requests that read secrets (`IntegrationsSync` 120 s,
/// `SystemCalendar::Connect` 150 s); a caller with a shorter IPC deadline
/// still gives up first.
const SECRET_STORE_TIMEOUT: Duration = Duration::from_secs(45);
static SECRET_SERVICE: LazyLock<SecretGate> =
    LazyLock::new(|| SecretGate::new(SECRET_STORE_TIMEOUT));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Read,
    /// A write or delete, which may still apply after its caller gave up.
    Change,
}

/// Bounds Secret Service calls. Each call, including `keyring::Entry::new`
/// (which connects to the session bus), runs on its own worker thread, so
/// concurrent calls proceed independently. A call unanswered at the deadline
/// is abandoned: its caller gets [`TIMEOUT_CODE`], its late result is dropped
/// (never handed to another call), and a late write may still apply once the
/// keyring is unlocked. While any abandoned worker still waits, most likely
/// on the unlock prompt, new calls fail at once with the same code instead of
/// stacking more prompts.
struct SecretGate {
    timeout: Duration,
    state: Arc<Mutex<GateState>>,
}
/// A call's slot is Pending while its worker holds the result sender,
/// Abandoned while its id is in `abandoned`, and Done once its [`Finish`]
/// guard dropped the sender; every transition happens under this one lock.
#[derive(Default)]
struct GateState {
    next: u64,
    abandoned: HashSet<u64>,
}
fn lock(state: &Mutex<GateState>) -> MutexGuard<'_, GateState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}
/// The caller's side of one started call.
struct Call<T> {
    id: u64,
    result: mpsc::Receiver<Result<T>>,
}
/// The worker's side: it sends the result, then (also on panic) marks the
/// slot Done, reopening the gate if its caller had abandoned it.
struct Finish<T> {
    state: Arc<Mutex<GateState>>,
    id: u64,
    sender: Option<mpsc::SyncSender<Result<T>>>,
}
impl<T> Finish<T> {
    fn deliver(self, result: Result<T>) {
        if let Some(sender) = &self.sender {
            // The caller may be gone (abandoned): the result is dropped.
            let _ = sender.send(result);
        }
    }
}
impl<T> Drop for Finish<T> {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        // Disconnecting under the lock: a caller settling its deadline sees
        // either the result or a finished worker, never a stale Pending.
        self.sender.take();
        state.abandoned.remove(&self.id);
    }
}

impl SecretGate {
    fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            state: Arc::default(),
        }
    }
    fn run<T: Send + 'static>(
        &self,
        access: Access,
        operation: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        // The worker is detached; an abandoned one ends on its own.
        let (call, _worker) = self.start(operation)?;
        match call.result.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(unanswered()),
            Err(mpsc::RecvTimeoutError::Timeout) => self.settle(call, access),
        }
    }
    /// Starts `operation` on its own worker, unless an abandoned call is
    /// still waiting.
    fn start<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<(Call<T>, std::thread::JoinHandle<()>)> {
        let id = {
            let mut state = lock(&self.state);
            if !state.abandoned.is_empty() {
                return Err(Error::Service {
                    code: TIMEOUT_CODE,
                    message: "an earlier Secret Service request is still waiting (likely an \
                              unlock prompt); answer it and retry."
                        .into(),
                });
            }
            state.next += 1;
            state.next
        };
        let (sender, result) = mpsc::sync_channel(1);
        let finish = Finish {
            state: self.state.clone(),
            id,
            sender: Some(sender),
        };
        let worker = std::thread::Builder::new()
            .name("broscribe-secret-service".into())
            .spawn(move || finish.deliver(operation()))?;
        Ok((Call { id, result }, worker))
    }
    /// The deadline passed. Under the gate's lock a result sent meanwhile is
    /// still returned; only a worker that has not answered is abandoned.
    fn settle<T>(&self, call: Call<T>, access: Access) -> Result<T> {
        let mut state = lock(&self.state);
        match call.result.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Disconnected) => Err(unanswered()),
            Err(mpsc::TryRecvError::Empty) => {
                state.abandoned.insert(call.id);
                let mut message = format!(
                    "Secret Service did not answer within {} s; the keyring may be locked. \
                     Unlock it and retry.",
                    self.timeout.as_secs()
                );
                if access == Access::Change {
                    message
                        .push_str(" The change may still apply if the keyring is unlocked later.");
                }
                Err(Error::Service {
                    code: TIMEOUT_CODE,
                    message,
                })
            }
        }
    }
}
/// The worker ended without a result (it panicked).
fn unanswered() -> Error {
    crate::platform_error(
        "calling Secret Service",
        "the request ended without an answer",
    )
}

type ActionCallback = Arc<dyn Fn(NotificationAction) + Send + Sync>;

/// Asynchronous outcomes reported through the action callback after
/// `notify_actionable` returned `Pending` (the convention the app shares
/// with macOS): the server accepted `Notify`, or the prompt cannot be shown.
const DELIVERED: &str = "__delivered";
const DENIED: &str = "__denied";
/// How long an action listener lives; the server is asked to expire the
/// prompt itself sooner ([`EXPIRY_MS`]).
const LISTENER_LIFETIME: Duration = Duration::from_secs(180);
const EXPIRY_MS: i32 = 120_000;
const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// Freedesktop notifications. Every D-Bus step (connect, capabilities,
/// `Notify`, `CloseNotification`) runs on a per-prompt I/O thread, so the
/// GPUI loop that calls this never waits for the session bus.
#[derive(Default)]
pub struct LinuxNotifier;

struct Active {
    generation: u64,
    /// Dropping it (withdrawal, replacement) tells the prompt's I/O thread
    /// to close the prompt and stop listening.
    _cancel: oneshot::Sender<()>,
}
/// The live prompt per caller id. Callbacks only run for the generation
/// still registered here, claimed with [`Registry::take`].
#[derive(Default)]
struct Registry {
    active: Mutex<HashMap<String, Active>>,
    next: AtomicU64,
}
impl Registry {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Registers a new prompt for `id`, cancelling the one it replaces.
    fn begin(&self, id: &str) -> (u64, oneshot::Receiver<()>) {
        let generation = self.next.fetch_add(1, Ordering::Relaxed);
        let (cancel, cancelled) = oneshot::channel();
        self.lock().insert(
            id.to_owned(),
            Active {
                generation,
                _cancel: cancel,
            },
        );
        (generation, cancelled)
    }
    fn is_current(&self, id: &str, generation: u64) -> bool {
        self.lock()
            .get(id)
            .is_some_and(|active| active.generation == generation)
    }
    /// Atomically claims `generation` of `id` (an action, a terminal
    /// outcome or expiry) against concurrent withdrawal or replacement.
    fn take(&self, id: &str, generation: u64) -> bool {
        let mut active = self.lock();
        if active.get(id).is_some_and(|a| a.generation == generation) {
            active.remove(id);
            true
        } else {
            false
        }
    }
    /// Removes the callback at once; the I/O thread closes the prompt.
    fn withdraw(&self, id: &str) {
        self.lock().remove(id);
    }
}
static NOTIFICATIONS: LazyLock<Arc<Registry>> = LazyLock::new(Default::default);
static NOTICES: AtomicU64 = AtomicU64::new(1);

enum ServerSignal {
    Action(u32, String),
    Closed(u32),
}
/// The notification server as seen by one prompt's I/O thread.
trait NotificationServer {
    async fn capabilities(&mut self) -> Result<Vec<String>>;
    async fn supports_default_action(&mut self) -> bool;
    async fn notify(&mut self, notification: &Notification) -> Result<u32>;
    async fn close(&mut self, remote: u32) -> Result<()>;
    /// The next signal about any prompt; `None` once the bus is gone.
    async fn next_signal(&mut self) -> Option<ServerSignal>;
}

struct ZbusServer {
    proxy: zbus::Proxy<'static>,
    signals: zbus::MessageStream,
}
impl ZbusServer {
    async fn connect() -> Result<Self> {
        let connection = zbus::connection::Builder::session()
            .map_err(|e| crate::platform_error("opening notification session", e))?
            .method_timeout(Duration::from_secs(3))
            .build()
            .await
            .map_err(|e| crate::platform_error("connecting notification session", e))?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(NOTIFICATIONS_NAME)
            .map_err(|e| crate::platform_error("matching notification sender", e))?
            .path(NOTIFICATIONS_PATH)
            .map_err(|e| crate::platform_error("matching notification path", e))?
            .interface(NOTIFICATIONS_NAME)
            .map_err(|e| crate::platform_error("matching notification interface", e))?
            .build();
        let signals = zbus::MessageStream::for_match_rule(rule, &connection, Some(16))
            .await
            .map_err(|e| crate::platform_error("subscribing notification actions", e))?;
        let proxy = zbus::Proxy::new(
            &connection,
            NOTIFICATIONS_NAME,
            NOTIFICATIONS_PATH,
            NOTIFICATIONS_NAME,
        )
        .await
        .map_err(|e| crate::platform_error("opening notification service", e))?;
        Ok(Self { proxy, signals })
    }
}
/// Plain text, not markup; no action can invoke an arbitrary command.
fn plain_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
impl NotificationServer for ZbusServer {
    async fn capabilities(&mut self) -> Result<Vec<String>> {
        self.proxy
            .call("GetCapabilities", &())
            .await
            .map_err(|e| crate::platform_error("reading notification capabilities", e))
    }
    async fn supports_default_action(&mut self) -> bool {
        let info: zbus::Result<(String, String, String, String)> =
            self.proxy.call("GetServerInformation", &()).await;
        // Xfce advertises actions but dismisses the notification on a body
        // click and draws the default action as an ordinary button.
        !info.is_ok_and(|(name, _, _, _)| name == "Xfce Notify Daemon")
    }
    async fn notify(&mut self, notification: &Notification) -> Result<u32> {
        let actions: Vec<&str> = notification
            .opens_on_click
            .then_some(["default", ""])
            .into_iter()
            .chain(
                notification
                    .actions
                    .iter()
                    .map(|a| [a.id.as_str(), a.label.as_str()]),
            )
            .flatten()
            .collect();
        let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        self.proxy
            .call(
                "Notify",
                &(
                    "BroScribe",
                    0u32,
                    crate::ICON_NAME,
                    plain_text(&notification.title),
                    plain_text(&notification.body),
                    actions,
                    hints,
                    EXPIRY_MS,
                ),
            )
            .await
            .map_err(|e| crate::platform_error("displaying notification", e))
    }
    async fn close(&mut self, remote: u32) -> Result<()> {
        self.proxy
            .call("CloseNotification", &(remote,))
            .await
            .map_err(|e| crate::platform_error("withdrawing notification", e))
    }
    async fn next_signal(&mut self) -> Option<ServerSignal> {
        loop {
            let message = self.signals.next().await?.ok()?;
            let header = message.header();
            match header.member().map(|m| m.as_str()) {
                Some("ActionInvoked") => {
                    if let Ok((remote, action)) = message.body().deserialize::<(u32, String)>() {
                        return Some(ServerSignal::Action(remote, action));
                    }
                }
                Some("NotificationClosed") => {
                    if let Ok((remote, _)) = message.body().deserialize::<(u32, u32)>() {
                        return Some(ServerSignal::Closed(remote));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Starts one prompt's I/O thread; the caller learns the outcome through
/// `callback` ([`DELIVERED`], [`DENIED`] or an action).
fn spawn_delivery<S, C, F>(
    registry: &Arc<Registry>,
    notification: Notification,
    callback: ActionCallback,
    connect: C,
    lifetime: Duration,
) -> Result<std::thread::JoinHandle<()>>
where
    S: NotificationServer,
    C: FnOnce() -> F + Send + 'static,
    F: Future<Output = Result<S>>,
{
    let id = notification.id.clone();
    let (generation, cancelled) = registry.begin(&id);
    let thread_registry = registry.clone();
    std::thread::Builder::new()
        .name("broscribe-notification-io".into())
        .spawn(move || {
            futures::executor::block_on(deliver(
                connect(),
                &thread_registry,
                notification,
                callback,
                generation,
                cancelled,
                lifetime,
            ));
        })
        .map_err(|e| {
            registry.take(&id, generation);
            Error::from(e)
        })
}

enum Wake {
    Cancelled,
    Expired,
    Signal(Option<ServerSignal>),
}
async fn deliver<S: NotificationServer>(
    connect: impl Future<Output = Result<S>>,
    registry: &Registry,
    mut notification: Notification,
    callback: ActionCallback,
    generation: u64,
    cancelled: oneshot::Receiver<()>,
    lifetime: Duration,
) {
    let id = notification.id.clone();
    let report = |action: &str| {
        callback(NotificationAction {
            id: id.clone(),
            action: action.into(),
        })
    };
    let deny = || {
        if registry.take(&id, generation) {
            report(DENIED);
        }
    };
    let Ok(mut server) = connect.await else {
        return deny();
    };
    let Ok(capabilities) = server.capabilities().await else {
        return deny();
    };
    let supports_actions = capabilities.iter().any(|c| c == "actions");
    if !supports_actions && (notification.opens_on_click || !notification.actions.is_empty()) {
        return deny();
    }
    let open_in_app = notification.opens_on_click && !server.supports_default_action().await;
    if open_in_app {
        notification.opens_on_click = false;
    }
    // Withdrawn or replaced while connecting: nothing is shown.
    if !registry.is_current(&id, generation) {
        return;
    }
    let Ok(remote) = server.notify(&notification).await else {
        return deny();
    };
    if registry.is_current(&id, generation) {
        // Keep the native notice informational. If it also had no explicit
        // buttons, offer its missing Open action through the in-app fallback.
        report(if open_in_app && notification.actions.is_empty() {
            DENIED
        } else {
            DELIVERED
        });
    }
    // A bare `oneshot::Receiver` reports itself terminated once its sender
    // is dropped, so `select!` would never poll it and the withdrawal or
    // replacement would go unseen until expiry. `Fuse` is terminated only
    // after it has yielded, so the cancellation is observed.
    let mut cancelled = cancelled.fuse();
    let mut expired = FutureExt::fuse(async_io::Timer::after(lifetime));
    loop {
        let wake = {
            let next = server.next_signal().fuse();
            futures::pin_mut!(next);
            futures::select! {
                _ = cancelled => Wake::Cancelled,
                _ = expired => Wake::Expired,
                signal = next => Wake::Signal(signal),
            }
        };
        match wake {
            // Withdrawn or replaced (possibly while `Notify` was in flight).
            Wake::Cancelled => {
                let _ = server.close(remote).await;
                return;
            }
            Wake::Expired | Wake::Signal(None) => break,
            Wake::Signal(Some(ServerSignal::Action(received, action)))
                if received == remote
                    && ((notification.opens_on_click && action == "default")
                        || notification.actions.iter().any(|a| a.id == action)) =>
            {
                if registry.take(&id, generation) {
                    let _ = server.close(remote).await;
                    report(if notification.opens_on_click && action == "default" {
                        "open"
                    } else {
                        action.as_str()
                    });
                }
                return;
            }
            Wake::Signal(Some(ServerSignal::Closed(received))) if received == remote => break,
            Wake::Signal(Some(_)) => {}
        }
    }
    if registry.take(&id, generation) {
        let _ = server.close(remote).await;
    }
}

impl Notifier for LinuxNotifier {
    /// An action-less prompt with its own id; its outcome is not observed.
    fn notify(&self, title: &str, body: &str) -> Result<()> {
        let notification = Notification {
            preview: Default::default(),
            opens_on_click: false,
            id: format!("__notice-{}", NOTICES.fetch_add(1, Ordering::Relaxed)),
            title: title.into(),
            body: body.into(),
            actions: Vec::new(),
        };
        self.notify_actionable(notification, Arc::new(|_| {}))
            .map(|_| ())
    }
    /// Returns `Pending` at once; the outcome reaches `callback` as
    /// [`DELIVERED`] or [`DENIED`], then any chosen action.
    fn notify_actionable(
        &self,
        notification: Notification,
        callback: Arc<dyn Fn(NotificationAction) + Send + Sync>,
    ) -> Result<NotificationDelivery> {
        spawn_delivery(
            &NOTIFICATIONS,
            notification,
            callback,
            ZbusServer::connect,
            LISTENER_LIFETIME,
        )?;
        Ok(NotificationDelivery::Pending)
    }
    /// Drops the prompt's callback immediately; its I/O thread closes it.
    fn withdraw(&self, id: &str) -> Result<()> {
        NOTIFICATIONS.withdraw(id);
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct LinuxAutostart {
    executable: PathBuf,
}
impl LinuxAutostart {
    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }
    fn path(&self) -> PathBuf {
        config_dir().join("autostart/broscribe.desktop")
    }
    fn daemon(&self) -> Result<LinuxDaemon> {
        let executable = if self
            .executable
            .file_name()
            .is_some_and(|name| name == "broscribed")
        {
            self.executable.clone()
        } else {
            let sibling = self.executable.with_file_name("broscribed");
            if is_executable(&sibling) {
                sibling
            } else {
                resolve_child_executable("broscribed")?
            }
        };
        Ok(LinuxDaemon::with_executable(executable))
    }
    fn unloaded_job(&self, job: LoginJob, daemon: &LinuxDaemon, data_dir: &Path) -> LoginJob {
        if job != LoginJob::Absent {
            return job;
        }
        match daemon.installed_profile() {
            Ok(Some(profile)) => LoginJob::Registered {
                serves_profile: Some(same_login_profile(&profile, data_dir)),
            },
            Ok(None) => match known_desktop_entry(&self.path()) {
                Ok(false) => LoginJob::Absent,
                Ok(true) => LoginJob::Registered {
                    serves_profile: Some(same_login_profile(
                        &crate::LinuxPaths::new().data_dir(),
                        data_dir,
                    )),
                },
                Err(_) => LoginJob::Unknown,
            },
            Err(_) => LoginJob::Registered {
                serves_profile: None,
            },
        }
    }
    fn contents(&self) -> &'static str {
        DESKTOP_ENTRY
    }
}

/// XDG graphical login imports the session's display environment into the
/// systemd user manager, then requests the unit (the graphical-session target
/// may request it too; systemd coalesces the starts). A failed import never
/// prevents the start, and no user-controlled path is interpolated into Exec.
macro_rules! autostart_entry {
    ($name:literal, $comment:literal, $deadline:literal) => {
        concat!(
            "[Desktop Entry]\nType=Application\nVersion=1.0\nName=", $name, "\nComment=", $comment, "\n",
            "Exec=sh -c \"", $deadline, "systemctl --user import-environment DISPLAY WAYLAND_DISPLAY XDG_SESSION_TYPE DBUS_SESSION_BUS_ADDRESS; exec ",
            $deadline, "systemctl --user start broscribed.service\"\n",
            "Terminal=false\nX-GNOME-Autostart-enabled=true\n"
        )
    };
}
const DESKTOP_ENTRY: &str = autostart_entry!(
    "BroScribe Agent",
    "Records meetings in the background",
    "timeout -s KILL 10s "
);
const PREVIOUS_DESKTOP_ENTRY: &str =
    autostart_entry!("BroScribe Agent", "Records meetings in the background", "");
const LEGACY_DESKTOP_ENTRY: &str = autostart_entry!("BroScribe", "Local meeting recorder", "");
/// Folder overrides the daemon must share with the CLI and app that
/// installed it, or they would disagree on its data folder and socket.
const CARRIED_ENVIRONMENT: [&str; 5] = [
    broscribe_platform::paths::ENV_DATA_DIR,
    broscribe_platform::paths::ENV_LIBRARY,
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
];

/// The user unit: bound to the graphical session (started with it, stopped
/// with it, so it always has the display environment), carrying the folder
/// overrides in `env` (name, absolute UTF-8 value).
fn unit_contents(executable: &str, env: &[(&str, String)]) -> Result<String> {
    let mut environment = String::new();
    for (name, value) in env {
        // Environment= expands specifiers (`%`) but not variables (`$`).
        environment.push_str(&format!(
            "Environment={}\n",
            unit_quoted(&format!("{name}={value}"), false)?
        ));
    }
    Ok(format!(
        "[Unit]\nDescription=BroScribe Agent (records meetings in the background)\nPartOf=graphical-session.target\nAfter=graphical-session.target pipewire.service\n\n[Service]\nType=simple\nExecStart={executable}\n{environment}Restart=on-failure\nRestartSec=2\nUMask=0077\n\n[Install]\nWantedBy=graphical-session.target\n"
    ))
}

/// [`CARRIED_ENVIRONMENT`] variables set in this process, made absolute.
fn carried_environment() -> Result<Vec<(&'static str, String)>> {
    let mut env = Vec::new();
    for name in CARRIED_ENVIRONMENT {
        let Some(value) = std::env::var_os(name).filter(|v| !v.is_empty()) else {
            continue;
        };
        let path = PathBuf::from(value);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        let value = path.into_os_string().into_string().map_err(|_| {
            Error::InvalidInput(format!(
                "{name} must be UTF-8 to be carried into the systemd unit"
            ))
        })?;
        env.push((name, value));
    }
    Ok(env)
}
fn known_desktop_entry(path: &Path) -> Result<bool> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(matches!(
            text.as_str(),
            DESKTOP_ENTRY | PREVIOUS_DESKTOP_ENTRY | LEGACY_DESKTOP_ENTRY
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io_at(path, error)),
    }
}

impl Autostart for LinuxAutostart {
    fn agent_status(&self) -> Result<AgentServiceStatus> {
        LinuxDaemon::with_executable(self.executable.clone()).agent_status()
    }
    fn is_enabled(&self) -> Result<bool> {
        match fs::read_to_string(self.path()) {
            Ok(text) => Ok(matches!(
                text.as_str(),
                DESKTOP_ENTRY | PREVIOUS_DESKTOP_ENTRY | LEGACY_DESKTOP_ENTRY
            ) && LinuxDaemon::with_executable(self.executable.clone())
                .unit_path
                .is_file()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    fn set_enabled(&self, enabled: bool) -> Result<()> {
        let registration = self.lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
        self.set_enabled_with_registration(enabled, &registration)
    }
    fn set_enabled_with_registration(
        &self,
        enabled: bool,
        registration: &LoginRegistrationGuard,
    ) -> Result<()> {
        if enabled {
            self.daemon()?.install(registration)?;
            write_config(&self.path(), self.contents().as_bytes())
        } else {
            let daemon = LinuxDaemon::with_executable(self.executable.clone());
            if daemon.unit_path.is_file() {
                daemon.uninstall(registration)?;
            }
            if known_desktop_entry(&self.path())? {
                remove_if_present(&self.path())?;
            }
            Ok(())
        }
    }
    fn start(&self) -> Result<()> {
        let registration = self.lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
        self.start_with_registration(&registration)
    }
    fn check_update_restart(&self) -> Result<()> {
        self.daemon()?.check_update_restart()
    }
    fn update_restart_library(&self, configured: Option<&Path>, default: &Path) -> Result<PathBuf> {
        let daemon = self.daemon()?;
        let installed = daemon.unit_path.try_exists()?;
        let deadline = Instant::now() + LOGIN_MANAGER_TIMEOUT;
        let settings = futures::executor::block_on(async {
            use futures::future::{Either, select};
            let query = systemd_service_environment(installed);
            futures::pin_mut!(query);
            match select(async_io::Timer::at(deadline), query).await {
                Either::Left(_) => None,
                Either::Right((settings, _)) => settings,
            }
        })
        .ok_or_else(|| Error::Service {
            code: "update_agent_unproven",
            message: "Could not verify the library used by the systemd agent restart.".into(),
        })?;
        let environment = if installed {
            settings.environment
        } else {
            // start() creates a unit with these exact folder overrides.
            carried_environment()?
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect()
        };
        restart_library(
            &environment,
            &settings.inherited,
            settings.working_dir.as_deref(),
            configured,
            default,
        )
    }
    fn start_with_registration(&self, registration: &LoginRegistrationGuard) -> Result<()> {
        self.daemon()?.start(registration)
    }
    fn stop(&self) -> Result<()> {
        let daemon = LinuxDaemon::with_executable(self.executable.clone());
        if !daemon.unit_path.try_exists()? {
            return Ok(());
        }
        let registration = self.lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
        daemon.stop(&registration)
    }
    fn lock_login_registration(&self, wait: Duration) -> Result<LoginRegistrationGuard> {
        // XDG_CONFIG_HOME can differ between profiles, even when both callers
        // address the same user manager and unit name.
        let (path, file) = open_login_registration_lock()
            .map_err(|e| Error::io("opening the login registration lock", e))?;
        LoginRegistrationGuard::acquire_open_file(&path, file, wait)
    }
    fn registered_profiles(&self) -> Result<Vec<RegisteredProfile>> {
        let daemon = LinuxDaemon::with_executable(self.executable.clone());
        let live = systemd_registered_profile_until(Instant::now() + LOGIN_MANAGER_TIMEOUT);
        let installed = daemon.installed_registration();
        let mut profiles: Vec<_> = live.into_iter().collect();
        match installed {
            Ok(profile) => profiles.extend(profile),
            // A customized unit can only be interpreted by the live manager.
            Err(error) if profiles.is_empty() => return Err(error),
            Err(_) => {}
        }
        if !profiles.is_empty() || !self.path().try_exists()? {
            return Ok(profiles);
        }
        if known_desktop_entry(&self.path())? {
            // An orphaned graphical entry starts a unit by name, with no
            // profile arguments of its own.
            return Ok(vec![crate::LinuxPaths::new().data_dir().into()]);
        }
        Ok(Vec::new())
    }

    fn login_job(&self, data_dir: &Path) -> LoginJob {
        let daemon = LinuxDaemon::with_executable(self.executable.clone());
        let job = daemon.login_job(data_dir, || {
            systemd_login_data_dir_until(Instant::now() + LOGIN_MANAGER_TIMEOUT)
        });
        self.unloaded_job(job, &daemon, data_dir)
    }
    fn login_job_matches(&self, data_dir: &Path) -> Option<bool> {
        futures::executor::block_on(systemd_login_data_dir())
            .map(|registered| same_login_profile(&registered, data_dir))
    }
    fn login_job_until(&self, data_dir: &Path, deadline: Instant) -> LoginJob {
        let mut daemon = LinuxDaemon::with_executable(self.executable.clone());
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return LoginJob::Unknown;
        }
        daemon.systemctl = user_systemctl(
            PathBuf::from("systemctl"),
            remaining.min(LOGIN_MANAGER_TIMEOUT),
        );
        let job = daemon.login_job(data_dir, || systemd_login_data_dir_until(deadline));
        self.unloaded_job(job, &daemon, data_dir)
    }
    fn login_job_matches_until(&self, data_dir: &Path, deadline: Instant) -> Option<bool> {
        systemd_login_data_dir_until(deadline)
            .map(|registered| same_login_profile(&registered, data_dir))
    }
    fn stop_login_job(&self, _: &LoginRegistrationGuard) -> Result<()> {
        // The lookup used the user manager's loaded unit, which need not be
        // under this process's XDG_CONFIG_HOME.
        LinuxDaemon::with_executable(self.executable.clone())
            .systemctl(&["stop", "broscribed.service"])
    }
}

fn same_login_profile(registered: &Path, requested: &Path) -> bool {
    crate::profile_identity(registered) == crate::profile_identity(requested)
}

fn load_login_unit<T>(call: impl FnOnce(&'static str, &'static str) -> T) -> T {
    // LoadUnit reads the definition without starting it, even if an idle
    // unit was collected after systemctl show.
    call("LoadUnit", "broscribed.service")
}

/// Inspect the service as the user manager sees it, including the manager
/// environment inherited by user services.
async fn systemd_login_data_dir() -> Option<PathBuf> {
    Some(systemd_registered_profile().await?.data_dir)
}

async fn systemd_registered_profile() -> Option<RegisteredProfile> {
    let settings = systemd_service_environment(true).await?;
    registered_profile(
        &settings.environment,
        &settings.inherited,
        settings.working_dir.as_deref(),
    )
}

struct ServiceEnvironment {
    environment: Vec<String>,
    inherited: Vec<String>,
    working_dir: Option<String>,
}

async fn systemd_service_environment(installed: bool) -> Option<ServiceEnvironment> {
    use zbus::{Proxy, connection::Builder};
    const BUS: &str = "org.freedesktop.systemd1";
    let connection = Builder::session()
        .ok()?
        .method_timeout(Duration::from_secs(3))
        .build()
        .await
        .ok()?;
    let manager = Proxy::new(
        &connection,
        BUS,
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await
    .ok()?;
    let inherited = manager
        .get_property::<Vec<String>>("Environment")
        .await
        .ok()?;
    if !installed {
        return Some(ServiceEnvironment {
            environment: Vec::new(),
            inherited,
            working_dir: None,
        });
    }
    let unit: zbus::zvariant::OwnedObjectPath = load_login_unit(|method, name| {
        let manager = &manager;
        async move { manager.call(method, &(name,)).await.ok() }
    })
    .await?;
    let service = Proxy::new(&connection, BUS, unit, "org.freedesktop.systemd1.Service")
        .await
        .ok()?;
    type ExecStart = (String, Vec<String>, bool, u64, u64, u64, u64, u32, i32, i32);
    let commands: Vec<ExecStart> = service.get_property("ExecStart").await.ok()?;
    let [command] = commands.as_slice() else {
        return None;
    };
    // BroScribe's unit uses a bare daemon executable and Environment=.
    // Leave a customized unit alone when it may override the profile.
    if !matches!(command.1.as_slice(), [exe] if exe == &command.0
        && Path::new(exe).file_name().is_some_and(|name| name == "broscribed"))
        || !service
            .get_property::<Vec<(String, bool)>>("EnvironmentFiles")
            .await
            .ok()?
            .is_empty()
        || !service
            .get_property::<Vec<String>>("UnsetEnvironment")
            .await
            .ok()?
            .is_empty()
    {
        return None;
    }
    Some(ServiceEnvironment {
        environment: service.get_property("Environment").await.ok()?,
        inherited,
        working_dir: service
            .get_property::<String>("WorkingDirectory")
            .await
            .ok(),
    })
}

fn systemd_login_data_dir_until(deadline: Instant) -> Option<PathBuf> {
    Some(systemd_registered_profile_until(deadline)?.data_dir)
}

fn systemd_registered_profile_until(deadline: Instant) -> Option<RegisteredProfile> {
    if Instant::now() >= deadline {
        return None;
    }
    futures::executor::block_on(async {
        use futures::future::{Either, select};
        let query = systemd_registered_profile();
        futures::pin_mut!(query);
        match select(async_io::Timer::at(deadline), query).await {
            Either::Left(_) => None,
            Either::Right((profile, _)) => profile,
        }
    })
}

fn service_var<'a>(
    environment: &'a [String],
    inherited: &'a [String],
    name: &str,
) -> Option<&'a str> {
    environment
        .iter()
        .rev()
        .chain(inherited.iter().rev())
        .find_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            (key == name).then_some(value)
        })
}

fn restart_library(
    environment: &[String],
    inherited: &[String],
    working_dir: Option<&str>,
    configured: Option<&Path>,
    default: &Path,
) -> Result<PathBuf> {
    let home = service_var(environment, inherited, "HOME")
        .map(Path::new)
        .filter(|path| path.is_absolute())
        .map(Path::to_path_buf)
        .or_else(broscribe_platform_unix::dirs::account_home)
        .ok_or_else(|| Error::InvalidInput("The systemd user's home folder is unknown.".into()))?;
    let default = default
        .file_name()
        .map_or_else(|| default.to_path_buf(), |name| home.join(name));
    let library = service_var(
        environment,
        inherited,
        broscribe_platform::paths::ENV_LIBRARY,
    )
    .filter(|path| !path.is_empty())
    .map(Path::new)
    .or(configured)
    .unwrap_or(&default);
    let cwd = match working_dir {
        None | Some("" | "~") => home.as_path(),
        Some(path) if Path::new(path).is_absolute() => Path::new(path),
        _ => {
            return Err(Error::InvalidInput(
                "The systemd working directory is unknown.".into(),
            ));
        }
    };
    Ok(cwd.join(library))
}

fn login_data_dir(environment: &[String], inherited: &[String]) -> Option<PathBuf> {
    let var = |name| service_var(environment, inherited, name);
    if let Some(path) = var(broscribe_platform::paths::ENV_DATA_DIR).filter(|path| !path.is_empty())
    {
        let path = PathBuf::from(path);
        return path.is_absolute().then_some(path);
    }
    if let Some(path) = var("XDG_DATA_HOME")
        .map(Path::new)
        .filter(|path| path.is_absolute())
    {
        return Some(path.join("broscribe"));
    }
    Some(Path::new(var("HOME")?).join(".local/share/broscribe")).filter(|path| path.is_absolute())
}

fn registered_profile(
    environment: &[String],
    inherited: &[String],
    working_dir: Option<&str>,
) -> Option<RegisteredProfile> {
    let data_dir = login_data_dir(environment, inherited)?;
    let library = match service_var(
        environment,
        inherited,
        broscribe_platform::paths::ENV_LIBRARY,
    )
    .filter(|path| !path.is_empty())
    .map(PathBuf::from)
    {
        Some(path) if path.is_relative() => {
            Some(restart_library(environment, inherited, working_dir, None, Path::new("")).ok()?)
        }
        library => library,
    };
    Some(RegisteredProfile { data_dir, library })
}

/// Parse only a unit written by BroScribe. A customized unit needs the live
/// manager to establish its effective environment.
fn installed_unit_profile(text: &str, inherited: &[String]) -> Option<RegisteredProfile> {
    let executable = text
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))?;
    let program = unquote_unit(executable, true)?;
    if !Path::new(&program).is_absolute() || Path::new(&program).file_name()? != "broscribed" {
        return None;
    }
    let entries = text
        .lines()
        .filter_map(|line| line.strip_prefix("Environment="))
        .map(|value| unquote_unit(value, false))
        .collect::<Option<Vec<_>>>()?;
    let environment = entries
        .iter()
        .map(|entry| {
            entry
                .split_once('=')
                .map(|(key, value)| (key, value.to_owned()))
        })
        .collect::<Option<Vec<_>>>()?;
    if environment
        .iter()
        .any(|(key, _)| !CARRIED_ENVIRONMENT.contains(key))
        || unit_contents(executable, &environment).ok()?.as_str() != text
    {
        return None;
    }
    registered_profile(&entries, inherited, None)
}

fn unquote_unit(quoted: &str, dollar: bool) -> Option<String> {
    const QUOTE: char = '\x22';
    let mut chars = quoted.strip_prefix(QUOTE)?.strip_suffix(QUOTE)?.chars();
    let mut value = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\\' => value.push(chars.next().filter(|c| matches!(*c, '\\' | QUOTE))?),
            '%' => value.push(chars.next().filter(|c| *c == '%')?),
            '$' if dollar => value.push(chars.next().filter(|c| *c == '$')?),
            c => value.push(c),
        }
    }
    Some(value)
}

/// Runs `systemctl --user` with the given arguments.
type Systemctl = Arc<dyn Fn(&[&str]) -> Result<Output> + Send + Sync>;
fn user_systemctl(executable: PathBuf, timeout: Duration) -> Systemctl {
    Arc::new(move |args: &[&str]| {
        login_manager_output(
            Command::new(&executable)
                .arg("--user")
                .args(args)
                .env("LC_ALL", "C"),
            timeout,
        )
        .map_err(|error| match error {
            Error::Io { source, .. } => Error::io("running systemctl --user", source),
            error => error,
        })
    })
}

fn systemctl_result(output: Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    Err(crate::platform_error(
        "running systemctl --user",
        format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ),
    ))
}

/// Match only errors that say no user bus or manager socket was reachable.
/// A bus permission error or an unrecognized response is still ambiguous.
fn user_manager_absent(stderr: &[u8]) -> bool {
    const CONNECT_ERRORS: &[&str] = &[
        "Failed to connect to bus: ",
        "Failed to connect to user bus: ",
        "Failed to connect to user scope bus via local transport: ",
    ];
    String::from_utf8_lossy(stderr).lines().any(|line| {
        line == "Failed to get properties: The name org.freedesktop.systemd1 was not provided by any .service files"
            || CONNECT_ERRORS.iter().find_map(|&prefix| line.strip_prefix(prefix)).is_some_and(|reason| {
                matches!(reason, "No medium found" | "No such file or directory" | "Connection refused")
                    || reason.starts_with("$DBUS_SESSION_BUS_ADDRESS and $XDG_RUNTIME_DIR not defined")
            })
    })
}

/// The daemon is an ordinary systemd *user* process, never a root service.
///
/// Only [`install`](Self::install) writes the unit's executable and folder
/// overrides (the installer's selection). `start` and `restart` keep an
/// installed unit byte for byte, whatever environment their caller has, so a
/// CLI run without `BROSCRIBE_DATA_DIR` never moves the daemon to another
/// data folder; they create the unit only when none is installed yet.
#[derive(Clone)]
pub struct LinuxDaemon {
    executable: PathBuf,
    unit_path: PathBuf,
    systemctl: Systemctl,
}
impl LinuxDaemon {
    pub fn with_executable(executable: PathBuf) -> Self {
        Self {
            executable,
            unit_path: config_dir().join("systemd/user/broscribed.service"),
            systemctl: user_systemctl(PathBuf::from("systemctl"), LOGIN_MANAGER_TIMEOUT),
        }
    }
    fn installed_profile(&self) -> Result<Option<PathBuf>> {
        Ok(self
            .installed_registration()?
            .map(|profile| profile.data_dir))
    }

    fn installed_registration(&self) -> Result<Option<RegisteredProfile>> {
        let text = match fs::read_to_string(&self.unit_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Error::io_at(&self.unit_path, error)),
        };
        let default = broscribe_platform_unix::dirs::account_home()
            .map(|home| format!("HOME={}", home.display()));
        installed_unit_profile(&text, default.as_slice())
            .map(Some)
            .ok_or_else(|| Error::Service {
                code: "login_job_unverified",
                message: "The installed BroScribe systemd unit's profile could not be verified"
                    .into(),
            })
    }

    fn login_job(
        &self,
        data_dir: &Path,
        registered_data_dir: impl FnOnce() -> Option<PathBuf>,
    ) -> LoginJob {
        let output = match (self.systemctl)(&[
            "show",
            "--property=LoadState",
            "--property=MainPID",
            "broscribed.service",
        ]) {
            Ok(output) => output,
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return LoginJob::Absent;
            }
            Err(_) => return LoginJob::Unknown,
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let property = |key| stdout.lines().find_map(|line| line.strip_prefix(key));
        // systemctl can return nonzero for an unknown unit even when it
        // emits the unit's not-found load state.
        match (
            output.status.success(),
            property("LoadState="),
            property("MainPID="),
        ) {
            (_, Some("not-found"), None | Some("0")) => LoginJob::Absent,
            (false, None, None)
                if stdout.trim().is_empty() && user_manager_absent(&output.stderr) =>
            {
                LoginJob::Absent
            }
            (true, Some("loaded"), Some("0")) => LoginJob::Idle {
                serves_profile: registered_data_dir()
                    .map(|registered| same_login_profile(&registered, data_dir)),
            },
            (true, _, Some(pid)) => pid.parse().map_or(LoginJob::Unknown, LoginJob::Running),
            _ => LoginJob::Unknown,
        }
    }

    fn systemctl(&self, args: &[&str]) -> Result<()> {
        systemctl_result((self.systemctl)(args)?)
    }
    /// Writes the unit for this executable and the caller's folder overrides.
    fn write_unit(&self) -> Result<()> {
        let executable = quoted_executable(&self.executable)?;
        let contents = unit_contents(&executable, &carried_environment()?)?;
        match fs::read_to_string(&self.unit_path) {
            Ok(existing) if existing == contents => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        write_config(&self.unit_path, contents.as_bytes())?;
        self.systemctl(&["daemon-reload"])
    }
    /// Creates the unit when none is installed; an installed one is kept.
    fn ensure_unit(&self) -> Result<()> {
        if self.unit_path.try_exists()? {
            return Ok(());
        }
        self.write_unit()
    }
    fn install(&self, _: &LoginRegistrationGuard) -> Result<()> {
        self.write_unit()?;
        // `reenable` drops a link left by an older [Install] section
        // (default.target) before linking the current one.
        self.systemctl(&["reenable", "broscribed.service"])?;
        self.systemctl(&["start", "broscribed.service"])
    }
    fn check_update_restart(&self) -> Result<()> {
        let unavailable = || {
            Error::Service {
            code: "update_agent_unproven",
            message: "The systemd user manager is unavailable, so the updater cannot restart BroScribe Agent automatically. Use a clean reinstall for an upgrade on a system without a user manager.".into(),
        }
        };
        match (self.systemctl)(&["show", "--property=Version"]) {
            Ok(output) if !output.status.success() && user_manager_absent(&output.stderr) => {
                Err(unavailable())
            }
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(unavailable())
            }
            result => result.and_then(systemctl_result).map_err(|error| Error::Service {
                code: "update_agent_unproven",
                message: format!(
                    "Could not query the systemd user manager to restart BroScribe Agent. Run the update from your desktop session. {error}"
                ),
            }),
        }
    }
    fn start(&self, _: &LoginRegistrationGuard) -> Result<()> {
        self.ensure_unit()?;
        self.systemctl(&["start", "broscribed.service"])
    }
    fn stop(&self, _: &LoginRegistrationGuard) -> Result<()> {
        if !self.unit_path.try_exists()? {
            return Ok(());
        }
        self.systemctl(&["stop", "broscribed.service"])
    }
    #[cfg(test)]
    fn restart(&self, _: &LoginRegistrationGuard) -> Result<()> {
        self.ensure_unit()?;
        self.systemctl(&["restart", "broscribed.service"])
    }
    fn agent_status(&self) -> Result<AgentServiceStatus> {
        Ok(if self.is_running()? {
            AgentServiceStatus::Running
        } else if self.unit_path.is_file() {
            AgentServiceStatus::Stopped
        } else {
            AgentServiceStatus::NotInstalled
        })
    }
    fn is_running(&self) -> Result<bool> {
        // No systemctl, or no user manager to ask: no unit can be running.
        // A timeout or any other failure stays an error.
        let output = match (self.systemctl)(&["is-active", "broscribed.service"]) {
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            result => result?,
        };
        let state = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        match (output.status.code(), state.trim()) {
            (Some(0), _) => Ok(true),
            (_, "") if !output.status.success() && user_manager_absent(&output.stderr) => Ok(false),
            (Some(3), "inactive" | "failed") => Ok(false),
            // Status 4 with a state and no diagnostic means the manager
            // could be queried but the unit could not be found.
            (Some(4), "inactive" | "unknown") if stderr.trim().is_empty() => Ok(false),
            _ => Err(crate::platform_error(
                "querying systemd user daemon",
                format!(
                    "{}: {}",
                    output.status,
                    if stderr.trim().is_empty() {
                        state.trim()
                    } else {
                        stderr.trim()
                    }
                ),
            )),
        }
    }
    fn uninstall(&self, _: &LoginRegistrationGuard) -> Result<()> {
        let args = ["disable", "--now", "broscribed.service"];
        let offline = match (self.systemctl)(&args) {
            Ok(output) if output.status.success() => false,
            Ok(output) if user_manager_absent(&output.stderr) => true,
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => true,
            Ok(output) => return systemctl_result(output),
            Err(error) => return Err(error),
        };
        if offline {
            let link = self
                .unit_path
                .with_file_name("graphical-session.target.wants")
                .join("broscribed.service");
            if std::fs::read_link(&link).is_ok_and(|target| {
                let target = link.parent().expect("unit wants directory").join(target);
                broscribe_platform::stable_path(&target)
                    == broscribe_platform::stable_path(&self.unit_path)
            }) {
                remove_if_present(&link)?;
            }
        }
        remove_if_present(&self.unit_path)?;
        if !offline {
            self.systemctl(&["daemon-reload"])?;
        }
        Ok(())
    }
}

/// Resolve only BroScribe's known helpers. Prefer the installed sibling over
/// PATH, avoiding a shell and preserving non-UTF8 directory names.
fn resolve_child_executable(name: &str) -> Result<PathBuf> {
    if !matches!(
        name,
        "broscribed" | "broscribe-worker" | "broscribe-app" | "broscribe"
    ) {
        return Err(Error::InvalidInput(
            "unknown BroScribe child executable".into(),
        ));
    }
    let current = std::env::current_exe()?;
    if let Some(parent) = current.parent() {
        let sibling = parent.join(name);
        if is_executable(&sibling) {
            return Ok(sibling);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&paths).filter(|p| p.is_absolute()) {
            let candidate = directory.join(name);
            if is_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(Error::NotFound(format!(
        "{name} executable beside the current binary or in PATH"
    )))
}
fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
fn quoted_executable(path: &Path) -> Result<String> {
    if !path.is_absolute() || !is_executable(path) {
        return Err(Error::InvalidInput(
            "autostart executable must be an absolute executable file".into(),
        ));
    }
    let path = path
        .to_str()
        .ok_or_else(|| Error::InvalidInput("autostart executable path must be UTF-8".into()))?;
    unit_quoted(path, true)
}
/// A double-quoted systemd unit value: specifiers (`%`) always escaped, `$`
/// only where the setting expands variables (`ExecStart=`).
fn unit_quoted(value: &str, escape_dollar: bool) -> Result<String> {
    if value.chars().any(char::is_control) {
        return Err(Error::InvalidInput(
            "systemd unit value contains controls".into(),
        ));
    }
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '%' => quoted.push_str("%%"),
            '$' if escape_dollar => quoted.push_str("$$"),
            '\\' | '"' => {
                quoted.push('\\');
                quoted.push(character);
            }
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}
fn write_config(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::InvalidInput("config path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".broscribe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| crate::platform_error("reading clock", e))?
            .as_nanos()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::system::NotificationActionSpec;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::time::Instant;

    const WAIT: Duration = Duration::from_secs(5);

    #[derive(Clone, Default)]
    struct ServerLog(Arc<Mutex<Vec<String>>>);
    impl ServerLog {
        fn push(&self, entry: String) {
            self.0.lock().unwrap().push(entry);
        }
        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }
    /// A synthetic notification server: no session bus is involved.
    struct FakeServer {
        log: ServerLog,
        remote: u32,
        actions: bool,
        default_action: bool,
        signals: futures::channel::mpsc::UnboundedReceiver<ServerSignal>,
    }
    impl NotificationServer for FakeServer {
        async fn capabilities(&mut self) -> Result<Vec<String>> {
            let mut capabilities = vec!["body".to_owned()];
            if self.actions {
                capabilities.push("actions".into());
            }
            Ok(capabilities)
        }
        async fn supports_default_action(&mut self) -> bool {
            self.default_action
        }
        async fn notify(&mut self, notification: &Notification) -> Result<u32> {
            assert!(self.default_action || !notification.opens_on_click);
            self.log.push(format!("notify {}", notification.id));
            Ok(self.remote)
        }
        async fn close(&mut self, remote: u32) -> Result<()> {
            self.log.push(format!("close {remote}"));
            Ok(())
        }
        async fn next_signal(&mut self) -> Option<ServerSignal> {
            self.signals.next().await
        }
    }
    fn server(
        log: &ServerLog,
        remote: u32,
    ) -> (
        FakeServer,
        futures::channel::mpsc::UnboundedSender<ServerSignal>,
    ) {
        let (signal, signals) = futures::channel::mpsc::unbounded();
        let server = FakeServer {
            log: log.clone(),
            remote,
            actions: true,
            default_action: true,
            signals,
        };
        (server, signal)
    }
    fn prompt(id: &str) -> Notification {
        Notification {
            preview: Default::default(),
            opens_on_click: false,
            id: id.into(),
            title: "Synthetic meeting".into(),
            body: "Record it?".into(),
            actions: vec![NotificationActionSpec {
                id: "record".into(),
                label: "Record".into(),
            }],
        }
    }
    fn recorder() -> (ActionCallback, mpsc::Receiver<NotificationAction>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let callback: ActionCallback = Arc::new(move |action| {
            let _ = tx.lock().unwrap().send(action);
        });
        (callback, rx)
    }

    /// The caller gets control back while the bus is still connecting; the
    /// server's acceptance arrives later as `__delivered`, then the action.
    #[test]
    fn actionable_prompts_report_delivery_after_returning() {
        let registry = Arc::new(Registry::default());
        let log = ServerLog::default();
        let (fake, signal) = server(&log, 7);
        let (open, gate) = oneshot::channel::<()>();
        let (callback, outcomes) = recorder();
        let thread = spawn_delivery(
            &registry,
            prompt("ask"),
            callback,
            move || async move {
                let _ = gate.await;
                Ok(fake)
            },
            WAIT * 12,
        )
        .unwrap();
        assert!(outcomes.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(log.entries().is_empty());
        open.send(()).unwrap();
        assert_eq!(outcomes.recv_timeout(WAIT).unwrap().action, DELIVERED);
        // Another prompt's action and an undeclared action are ignored.
        signal
            .unbounded_send(ServerSignal::Action(8, "record".into()))
            .unwrap();
        signal
            .unbounded_send(ServerSignal::Action(7, "unknown".into()))
            .unwrap();
        signal
            .unbounded_send(ServerSignal::Action(7, "record".into()))
            .unwrap();
        let action = outcomes.recv_timeout(WAIT).unwrap();
        assert_eq!(
            (action.id.as_str(), action.action.as_str()),
            ("ask", "record")
        );
        thread.join().unwrap();
        assert_eq!(log.entries(), ["notify ask", "close 7"]);
        assert!(outcomes.try_recv().is_err());
    }

    #[test]
    fn an_informational_default_click_reports_open_without_a_button() {
        let registry = Arc::new(Registry::default());
        let log = ServerLog::default();
        let (fake, signal) = server(&log, 7);
        let (callback, outcomes) = recorder();
        let notification = Notification {
            preview: Default::default(),
            id: "window-closed".into(),
            title: "Synthetic notice".into(),
            body: "Synthetic body".into(),
            opens_on_click: true,
            actions: Vec::new(),
        };
        let thread = spawn_delivery(
            &registry,
            notification,
            callback,
            move || async move { Ok(fake) },
            WAIT * 12,
        )
        .unwrap();
        assert_eq!(outcomes.recv_timeout(WAIT).unwrap().action, DELIVERED);
        signal
            .unbounded_send(ServerSignal::Action(7, "default".into()))
            .unwrap();
        assert_eq!(outcomes.recv_timeout(WAIT).unwrap().action, "open");
        thread.join().unwrap();
        assert_eq!(log.entries(), ["notify window-closed", "close 7"]);
    }

    #[test]
    fn a_server_without_body_activation_posts_an_actionless_notice_and_keeps_fallback() {
        let registry = Arc::new(Registry::default());
        let log = ServerLog::default();
        let (mut fake, signal) = server(&log, 7);
        fake.default_action = false;
        let (callback, outcomes) = recorder();
        let notification = Notification {
            opens_on_click: true,
            actions: Vec::new(),
            ..prompt("final-m-000123")
        };
        let thread = spawn_delivery(
            &registry,
            notification,
            callback,
            move || async move { Ok(fake) },
            WAIT * 12,
        )
        .unwrap();
        assert_eq!(outcomes.recv_timeout(WAIT).unwrap().action, DENIED);
        signal
            .unbounded_send(ServerSignal::Action(7, "default".into()))
            .unwrap();
        signal.unbounded_send(ServerSignal::Closed(7)).unwrap();
        thread.join().unwrap();
        assert_eq!(log.entries(), ["notify final-m-000123", "close 7"]);
        assert!(outcomes.try_recv().is_err());
    }

    #[test]
    fn withdrawing_while_connecting_shows_nothing() {
        let registry = Arc::new(Registry::default());
        let log = ServerLog::default();
        let (fake, _signal) = server(&log, 7);
        let (open, gate) = oneshot::channel::<()>();
        let (callback, outcomes) = recorder();
        let thread = spawn_delivery(
            &registry,
            prompt("ask"),
            callback,
            move || async move {
                let _ = gate.await;
                Ok(fake)
            },
            WAIT * 12,
        )
        .unwrap();
        registry.withdraw("ask");
        open.send(()).unwrap();
        thread.join().unwrap();
        assert!(log.entries().is_empty());
        assert!(outcomes.try_recv().is_err());
    }

    #[test]
    fn withdrawing_a_shown_prompt_closes_it_and_drops_its_callback() {
        let registry = Arc::new(Registry::default());
        let log = ServerLog::default();
        let (fake, signal) = server(&log, 7);
        let (callback, outcomes) = recorder();
        let thread = spawn_delivery(
            &registry,
            prompt("ask"),
            callback,
            move || async move { Ok(fake) },
            WAIT * 12,
        )
        .unwrap();
        assert_eq!(outcomes.recv_timeout(WAIT).unwrap().action, DELIVERED);
        let started = Instant::now();
        registry.withdraw("ask");
        thread.join().unwrap();
        assert!(started.elapsed() < WAIT);
        let _ = signal.unbounded_send(ServerSignal::Action(7, "record".into()));
        assert_eq!(log.entries(), ["notify ask", "close 7"]);
        assert!(outcomes.try_recv().is_err());
    }

    #[test]
    fn a_replacement_closes_the_previous_prompt_of_the_same_id() {
        let registry = Arc::new(Registry::default());
        let log = ServerLog::default();
        let (first, _first_signal) = server(&log, 7);
        let (first_callback, first_outcomes) = recorder();
        let first_thread = spawn_delivery(
            &registry,
            prompt("ask"),
            first_callback,
            move || async move { Ok(first) },
            WAIT * 12,
        )
        .unwrap();
        assert_eq!(first_outcomes.recv_timeout(WAIT).unwrap().action, DELIVERED);
        let (second, second_signal) = server(&log, 9);
        let (second_callback, second_outcomes) = recorder();
        let second_thread = spawn_delivery(
            &registry,
            prompt("ask"),
            second_callback,
            move || async move { Ok(second) },
            WAIT * 12,
        )
        .unwrap();
        first_thread.join().unwrap();
        assert_eq!(
            second_outcomes.recv_timeout(WAIT).unwrap().action,
            DELIVERED
        );
        second_signal
            .unbounded_send(ServerSignal::Action(9, "record".into()))
            .unwrap();
        assert_eq!(second_outcomes.recv_timeout(WAIT).unwrap().action, "record");
        second_thread.join().unwrap();
        let entries = log.entries();
        assert!(entries.contains(&"close 7".to_owned()), "{entries:?}");
        assert_eq!(
            entries.iter().filter(|e| e.starts_with("notify")).count(),
            2
        );
        assert!(first_outcomes.try_recv().is_err());
    }

    #[test]
    fn unsupported_actions_or_an_unreachable_bus_are_reported_denied() {
        let registry = Arc::new(Registry::default());
        let log = ServerLog::default();
        let (mut fake, _signal) = server(&log, 7);
        fake.actions = false;
        let (callback, outcomes) = recorder();
        spawn_delivery(
            &registry,
            prompt("ask"),
            callback,
            move || async move { Ok(fake) },
            WAIT * 12,
        )
        .unwrap()
        .join()
        .unwrap();
        assert_eq!(outcomes.recv_timeout(WAIT).unwrap().action, DENIED);
        let (callback, outcomes) = recorder();
        spawn_delivery(
            &registry,
            prompt("ask"),
            callback,
            || async { Err::<FakeServer, _>(Error::Unsupported("no session bus".into())) },
            WAIT * 12,
        )
        .unwrap()
        .join()
        .unwrap();
        assert_eq!(outcomes.recv_timeout(WAIT).unwrap().action, DENIED);
        assert!(log.entries().is_empty());
    }

    #[test]
    fn an_unanswered_prompt_is_closed_when_its_listener_expires() {
        let registry = Arc::new(Registry::default());
        let log = ServerLog::default();
        let (fake, _signal) = server(&log, 7);
        let (callback, outcomes) = recorder();
        spawn_delivery(
            &registry,
            prompt("ask"),
            callback,
            move || async move { Ok(fake) },
            Duration::from_millis(50),
        )
        .unwrap()
        .join()
        .unwrap();
        assert_eq!(outcomes.recv_timeout(WAIT).unwrap().action, DELIVERED);
        assert!(outcomes.try_recv().is_err());
        assert_eq!(log.entries(), ["notify ask", "close 7"]);
        assert!(!registry.is_current("ask", 0));
    }

    /// A systemd user manager that records its arguments and answers with
    /// `code`, plus an executable daemon file in `dir`.
    fn fake_daemon(dir: &Path, code: i32) -> (LinuxDaemon, ServerLog) {
        let executable = dir.join("bin/broscribed");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, "").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let calls = ServerLog::default();
        let log = calls.clone();
        let daemon = LinuxDaemon {
            executable,
            unit_path: dir.join("config/systemd/user/broscribed.service"),
            systemctl: Arc::new(move |args: &[&str]| {
                log.push(args.join(" "));
                Ok(Output {
                    status: ExitStatus::from_raw(code << 8),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }),
        };
        (daemon, calls)
    }

    fn test_registration(dir: &Path) -> LoginRegistrationGuard {
        LoginRegistrationGuard::acquire(&dir.join("login-registration.lock"), Duration::ZERO)
            .unwrap()
    }

    /// The unit `install` selected (here: a data folder the starting
    /// process does not have) survives `start` and `restart` byte for byte.
    #[test]
    fn offline_unit_profile_accepts_generated_config_and_rejects_wrappers() {
        let exe = unit_quoted("/example/agent folder/broscribed", true).unwrap();
        let unit = unit_contents(
            &exe,
            &[
                ("BROSCRIBE_DATA_DIR", "/example/% synthetic data".into()),
                (
                    "BROSCRIBE_LIBRARY",
                    "/example/% synthetic data/library".into(),
                ),
            ],
        )
        .unwrap();
        let profile = installed_unit_profile(&unit, &[]).unwrap();
        assert_eq!(profile.data_dir, PathBuf::from("/example/% synthetic data"));
        assert_eq!(profile.library, Some(profile.data_dir.join("library")));
        assert!(installed_unit_profile(&unit.replace("broscribed", "wrapper"), &[]).is_none());
        assert!(
            installed_unit_profile(&unit.replace("Restart=on-failure", "Restart=always"), &[])
                .is_none()
        );
    }

    #[test]
    fn update_restart_checks_manager_availability_without_changing_autostart() {
        for (exit, available) in [(0, true), (1, false)] {
            let tmp = tempfile::tempdir().unwrap();
            let (daemon, calls) = fake_daemon(tmp.path(), exit);
            let result = daemon.check_update_restart();
            assert_eq!(result.is_ok(), available);
            if !available {
                assert_eq!(result.unwrap_err().code(), "update_agent_unproven");
            }
            assert_eq!(calls.entries(), ["show --property=Version"]);
            assert!(!daemon.unit_path.exists());
        }
    }

    #[test]
    fn update_restart_on_a_system_without_a_manager_requires_clean_reinstall() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut daemon, _) = fake_daemon(tmp.path(), 1);
        daemon.systemctl = Arc::new(|_| {
            Ok(Output {
                status: ExitStatus::from_raw(1 << 8),
                stdout: Vec::new(),
                stderr: b"Failed to connect to bus: No such file or directory\n".to_vec(),
            })
        });
        let message = daemon.check_update_restart().unwrap_err().to_string();
        assert!(message.contains("cannot restart BroScribe Agent automatically"));
        assert!(message.contains("clean reinstall"));
        assert!(!daemon.unit_path.exists());
    }

    #[test]
    fn start_and_restart_keep_the_installed_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let (daemon, calls) = fake_daemon(tmp.path(), 0);
        let executable = quoted_executable(&daemon.executable).unwrap();
        let installed = unit_contents(
            &executable,
            &[("BROSCRIBE_DATA_DIR", "/synthetic/installed data".into())],
        )
        .unwrap();
        write_config(&daemon.unit_path, installed.as_bytes()).unwrap();
        let registration = test_registration(tmp.path());
        daemon.start(&registration).unwrap();
        daemon.restart(&registration).unwrap();
        assert_eq!(fs::read_to_string(&daemon.unit_path).unwrap(), installed);
        assert_eq!(
            calls.entries(),
            ["start broscribed.service", "restart broscribed.service"]
        );
    }

    #[test]
    fn login_job_uses_main_pid_after_the_manager_environment_changes() {
        use std::num::NonZeroU32;

        let tmp = tempfile::tempdir().unwrap();
        let (mut daemon, _) = fake_daemon(tmp.path(), 0);
        let manager_environment = Arc::new(Mutex::new("BROSCRIBE_DATA_DIR=/synthetic/started"));
        let current = manager_environment.clone();
        daemon.systemctl = Arc::new(move |args| {
            let stdout = match args {
                ["show-environment"] => current.lock().unwrap().as_bytes().to_vec(),
                [
                    "show",
                    "--property=LoadState",
                    "--property=MainPID",
                    "broscribed.service",
                ] => b"MainPID=4242\nLoadState=loaded\n".to_vec(),
                _ => panic!("unexpected systemctl call: {args:?}"),
            };
            Ok(Output {
                status: ExitStatus::from_raw(0),
                stdout,
                stderr: Vec::new(),
            })
        });
        let running = LoginJob::Running(NonZeroU32::new(4242).unwrap());
        assert_eq!(
            (daemon.systemctl)(&["show-environment"]).unwrap().stdout,
            b"BROSCRIBE_DATA_DIR=/synthetic/started"
        );
        assert_eq!(
            daemon.login_job(Path::new("/synthetic/started"), || unreachable!()),
            running
        );
        *manager_environment.lock().unwrap() = "BROSCRIBE_DATA_DIR=/synthetic/changed";
        assert_eq!(
            (daemon.systemctl)(&["show-environment"]).unwrap().stdout,
            b"BROSCRIBE_DATA_DIR=/synthetic/changed"
        );
        assert_eq!(
            daemon.login_job(Path::new("/synthetic/started"), || unreachable!()),
            running
        );
    }

    #[test]
    fn systemd_stop_distinguishes_idle_absent_and_unverified_units() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut daemon, _) = fake_daemon(tmp.path(), 0);
        for (status, stdout, job) in [
            (
                0,
                "MainPID=0\nLoadState=loaded\n",
                LoginJob::Idle {
                    serves_profile: None,
                },
            ),
            (1, "MainPID=0\nLoadState=not-found\n", LoginJob::Absent),
            (1, "", LoginJob::Unknown),
            (0, "MainPID=unknown\nLoadState=loaded\n", LoginJob::Unknown),
        ] {
            daemon.systemctl = Arc::new(move |_| {
                Ok(Output {
                    status: ExitStatus::from_raw(status << 8),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: Vec::new(),
                })
            });
            assert_eq!(daemon.login_job(Path::new("/synthetic"), || None), job);
        }
    }

    #[test]
    fn hung_systemctl_reads_are_unknown_and_mutations_error() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut daemon, _) = fake_daemon(tmp.path(), 0);
        let script = tmp.path().join("fake-systemctl");
        fs::write(&script, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        daemon.systemctl = user_systemctl(script, Duration::from_millis(100));
        assert_eq!(
            daemon.login_job(Path::new("/synthetic"), || unreachable!()),
            LoginJob::Unknown
        );
        assert!(matches!(
            daemon.systemctl(&["stop", "broscribed.service"]),
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::TimedOut
        ));
        write_config(&daemon.unit_path, b"[Unit]\n").unwrap();
        assert!(matches!(
            daemon.agent_status(),
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    #[test]
    fn systemd_stop_treats_a_missing_systemctl_or_user_manager_as_absent() {
        use std::io::ErrorKind;

        let tmp = tempfile::tempdir().unwrap();
        let (mut daemon, _) = fake_daemon(tmp.path(), 0);
        let data = Path::new("/synthetic/profile");
        for (kind, job) in [
            (ErrorKind::NotFound, LoginJob::Absent),
            (ErrorKind::PermissionDenied, LoginJob::Unknown),
        ] {
            daemon.systemctl = Arc::new(move |_| Err(std::io::Error::from(kind).into()));
            assert_eq!(daemon.login_job(data, || unreachable!()), job);
        }
        for (stderr, job) in [
            (
                "Failed to connect to bus: No medium found\n",
                LoginJob::Absent,
            ),
            (
                "Failed to connect to bus: No such file or directory\n",
                LoginJob::Absent,
            ),
            (
                "Failed to connect to bus: Connection refused\n",
                LoginJob::Absent,
            ),
            (
                "Failed to get properties: The name org.freedesktop.systemd1 was not provided by any .service files\n",
                LoginJob::Absent,
            ),
            (
                "Failed to connect to user scope bus via local transport: $DBUS_SESSION_BUS_ADDRESS and $XDG_RUNTIME_DIR not defined (consider using --machine=...)\n",
                LoginJob::Absent,
            ),
            (
                "Failed to connect to bus: Permission denied\n",
                LoginJob::Unknown,
            ),
            (
                "Failed to get properties: unrecognized response\n",
                LoginJob::Unknown,
            ),
        ] {
            daemon.systemctl = Arc::new(move |_| {
                Ok(Output {
                    status: ExitStatus::from_raw(1 << 8),
                    stdout: Vec::new(),
                    stderr: stderr.as_bytes().to_vec(),
                })
            });
            assert_eq!(daemon.login_job(data, || unreachable!()), job);
        }
    }

    #[test]
    fn systemd_idle_job_uses_the_loaded_unit_and_manager_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut daemon, _) = fake_daemon(tmp.path(), 0);
        daemon.systemctl = Arc::new(|_| {
            Ok(Output {
                status: ExitStatus::from_raw(0),
                stdout: b"MainPID=0\nLoadState=loaded\n".to_vec(),
                stderr: Vec::new(),
            })
        });
        let manager = [
            "HOME=/synthetic/home".into(),
            "XDG_DATA_HOME=/synthetic/xdg".into(),
        ];
        let profile = Path::new("/synthetic/profile a");
        for (environment, serves_profile) in [
            (
                vec!["BROSCRIBE_DATA_DIR=/synthetic/profile a".into()],
                Some(true),
            ),
            (
                vec!["BROSCRIBE_DATA_DIR=/synthetic/profile b".into()],
                Some(false),
            ),
            (vec!["BROSCRIBE_DATA_DIR=relative".into()], None),
            (vec![], Some(false)),
        ] {
            assert_eq!(
                daemon.login_job(profile, || login_data_dir(&environment, &manager)),
                LoginJob::Idle { serves_profile }
            );
        }
        assert_eq!(
            login_data_dir(&[], &manager),
            Some("/synthetic/xdg/broscribe".into())
        );
        assert_eq!(
            login_data_dir(&[], &manager[..1]),
            Some("/synthetic/home/.local/share/broscribe".into())
        );
    }

    #[test]
    fn systemd_idle_job_loads_a_collected_unit_to_verify_its_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut daemon, _) = fake_daemon(tmp.path(), 0);
        daemon.systemctl = Arc::new(|args| {
            assert_eq!(
                args,
                [
                    "show",
                    "--property=LoadState",
                    "--property=MainPID",
                    "broscribed.service"
                ]
            );
            Ok(Output {
                status: ExitStatus::from_raw(0),
                stdout: b"MainPID=0\nLoadState=loaded\n".to_vec(),
                stderr: Vec::new(),
            })
        });
        let profile = Path::new("/synthetic/profile");
        let job = daemon.login_job(profile, || {
            // After show has exited the inactive unit is gone from memory,
            // but the manager can load its definition from disk.
            let _: zbus::zvariant::OwnedObjectPath = load_login_unit(|method, name| {
                assert_eq!(name, "broscribed.service");
                (method == "LoadUnit").then(|| {
                    "/org/freedesktop/systemd1/unit/broscribed_2eservice"
                        .try_into()
                        .unwrap()
                })
            })?;
            login_data_dir(&["BROSCRIBE_DATA_DIR=/synthetic/profile".into()], &[])
        });
        assert_eq!(
            job,
            LoginJob::Idle {
                serves_profile: Some(true)
            }
        );
    }

    #[test]
    fn install_writes_the_selected_unit_privately() {
        let tmp = tempfile::tempdir().unwrap();
        let (daemon, calls) = fake_daemon(tmp.path(), 0);
        write_config(&daemon.unit_path, b"[Unit]\nDescription=stale\n").unwrap();
        daemon.install(&test_registration(tmp.path())).unwrap();
        let expected = unit_contents(
            &quoted_executable(&daemon.executable).unwrap(),
            &carried_environment().unwrap(),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&daemon.unit_path).unwrap(), expected);
        let mode = fs::metadata(&daemon.unit_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            calls.entries(),
            [
                "daemon-reload",
                "reenable broscribed.service",
                "start broscribed.service"
            ]
        );
    }

    #[test]
    fn starting_without_a_unit_creates_it_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (daemon, calls) = fake_daemon(tmp.path(), 0);
        let registration = test_registration(tmp.path());
        daemon.start(&registration).unwrap();
        let created = fs::read_to_string(&daemon.unit_path).unwrap();
        daemon.start(&registration).unwrap();
        assert_eq!(fs::read_to_string(&daemon.unit_path).unwrap(), created);
        assert_eq!(
            calls.entries(),
            [
                "daemon-reload",
                "start broscribed.service",
                "start broscribed.service"
            ]
        );
    }

    #[test]
    fn systemd_answers_are_reported_not_guessed() {
        let tmp = tempfile::tempdir().unwrap();
        // Nothing installed: stopping is a no-op, not a systemctl call.
        let (stopped, calls) = fake_daemon(tmp.path(), 0);
        let registration = test_registration(tmp.path());
        stopped.stop(&registration).unwrap();
        assert!(calls.entries().is_empty());
        assert!(stopped.is_running().unwrap());
        let (failing, _) = fake_daemon(tmp.path(), 1);
        assert!(failing.is_running().is_err());
        write_config(&failing.unit_path, b"[Unit]\n").unwrap();
        assert!(failing.restart(&registration).is_err());
    }

    #[test]
    fn systemd_status_distinguishes_stopped_from_unverified() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut daemon, _) = fake_daemon(tmp.path(), 0);
        for (code, stdout, stderr, expected) in [
            (0, "active\n", "", Some(AgentServiceStatus::Running)),
            (3, "inactive\n", "", Some(AgentServiceStatus::Stopped)),
            (3, "failed\n", "", Some(AgentServiceStatus::Stopped)),
            (4, "unknown\n", "", Some(AgentServiceStatus::NotInstalled)),
            (3, "activating\n", "", None),
            (4, "", "Failed to connect to bus: Permission denied\n", None),
            (
                1,
                "",
                "Failed to connect to user scope bus via local transport: $DBUS_SESSION_BUS_ADDRESS and $XDG_RUNTIME_DIR not defined\n",
                Some(AgentServiceStatus::NotInstalled),
            ),
            (
                1,
                "",
                "Failed to connect to bus: No such file or directory\n",
                Some(AgentServiceStatus::NotInstalled),
            ),
        ] {
            daemon.systemctl = Arc::new(move |args| {
                assert_eq!(args, ["is-active", "broscribed.service"]);
                Ok(Output {
                    status: ExitStatus::from_raw(code << 8),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: stderr.as_bytes().to_vec(),
                })
            });
            if expected == Some(AgentServiceStatus::Stopped) {
                write_config(&daemon.unit_path, b"[Unit]\n").unwrap();
            } else {
                remove_if_present(&daemon.unit_path).unwrap();
            }
            assert_eq!(daemon.agent_status().ok(), expected);
        }
        // No systemctl at all: nothing can be running.
        daemon.systemctl = Arc::new(|_| {
            Err(Error::io(
                "running systemctl",
                std::io::Error::from(std::io::ErrorKind::NotFound),
            ))
        });
        remove_if_present(&daemon.unit_path).unwrap();
        assert_eq!(
            daemon.agent_status().ok(),
            Some(AgentServiceStatus::NotInstalled)
        );
    }

    #[test]
    fn opener_that_keeps_stdout_open_does_not_block() {
        // Like a browser started by xdg-open: a child holding stdout/stderr
        // open for long after the opener itself exits.
        // Execute immutable /bin/sh rather than a just-written executable:
        // a concurrent fork can temporarily inherit its writable descriptor.
        let mut opener = Command::new("/bin/sh");
        opener.args(["-c", "(sleep 30; echo late) &\necho started\nexit 0"]);
        let started = std::time::Instant::now();
        launch_detached(
            opener.arg("https://example.invalid/"),
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn opener_failure_is_reported_and_a_lingering_opener_is_not_waited_for() {
        assert!(
            launch_detached(
                Command::new("/bin/sh").args(["-c", "exit 3"]),
                Duration::from_secs(10),
            )
            .is_err()
        );
        let mut lingering = Command::new("/bin/sh");
        lingering.args(["-c", "exec sleep 30"]);
        let started = std::time::Instant::now();
        launch_detached(&mut lingering, Duration::from_millis(200)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn systemd_restart_library_uses_the_effective_service_environment() {
        let manager = vec![
            "HOME=/synthetic/home".into(),
            "BROSCRIBE_LIBRARY=/synthetic/manager".into(),
        ];
        let unit = vec!["BROSCRIBE_LIBRARY=service-library".into()];
        let default = Path::new("/other-home/BroScribe");
        let config = Path::new("configured");
        assert_eq!(
            restart_library(
                &unit,
                &manager,
                Some("/synthetic/working"),
                Some(config),
                default
            )
            .unwrap(),
            Path::new("/synthetic/working/service-library")
        );
        assert_eq!(
            restart_library(&[], &manager, None, Some(config), default).unwrap(),
            Path::new("/synthetic/manager")
        );
        assert_eq!(
            restart_library(&[], &manager[..1], None, Some(config), default).unwrap(),
            Path::new("/synthetic/home/configured")
        );
    }

    #[test]
    fn unit_follows_the_graphical_session_and_carries_folder_overrides() {
        let unit = unit_contents(
            "\"/opt/bro scribe/broscribed\"",
            &[
                ("BROSCRIBE_DATA_DIR", "/data/100%$HOME \"x\"".into()),
                ("XDG_DATA_HOME", "/home/a/.local/share".into()),
            ],
        )
        .unwrap();
        assert!(unit.contains("\nPartOf=graphical-session.target\n"));
        assert!(unit.contains("\nAfter=graphical-session.target pipewire.service\n"));
        assert!(unit.contains("\nWantedBy=graphical-session.target\n"));
        assert!(!unit.contains("default.target"));
        assert!(unit.contains("\nExecStart=\"/opt/bro scribe/broscribed\"\n"));
        assert!(unit.contains("\nEnvironment=\"BROSCRIBE_DATA_DIR=/data/100%%$HOME \\\"x\\\"\"\n"));
        assert!(unit.contains("\nEnvironment=\"XDG_DATA_HOME=/home/a/.local/share\"\n"));
        assert!(unit_contents("\"/b\"", &[("XDG_DATA_HOME", "/a\nb".into())]).is_err());
        assert!(
            !unit_contents("\"/b\"", &[])
                .unwrap()
                .contains("Environment=")
        );
    }

    #[test]
    fn autostart_imports_the_display_environment_before_starting() {
        let exec = DESKTOP_ENTRY
            .lines()
            .find_map(|l| l.strip_prefix("Exec="))
            .unwrap();
        assert_eq!(
            exec,
            "sh -c \"timeout -s KILL 10s systemctl --user import-environment DISPLAY WAYLAND_DISPLAY XDG_SESSION_TYPE DBUS_SESSION_BUS_ADDRESS; exec timeout -s KILL 10s systemctl --user start broscribed.service\""
        );
        let import = exec.find("import-environment").unwrap();
        assert!(import < exec.find("start broscribed.service").unwrap());
        assert!(DESKTOP_ENTRY.contains("\nName=BroScribe Agent\n"));
        assert!(DESKTOP_ENTRY.contains("\nComment=Records meetings in the background\n"));
        let previous_exec = exec.replace("timeout -s KILL 10s ", "");
        assert!(PREVIOUS_DESKTOP_ENTRY.contains(&previous_exec));
        assert!(LEGACY_DESKTOP_ENTRY.contains(&previous_exec));
        assert!(
            unit_contents("\"/opt/broscribed\"", &[])
                .unwrap()
                .contains("\nDescription=BroScribe Agent (records meetings in the background)\n")
        );
    }

    /// Secret Service gates under test: their own instances, millisecond
    /// deadlines, synthetic operations. No session bus is involved.
    const SHORT: Duration = Duration::from_millis(50);

    fn is_open(gate: &SecretGate) -> bool {
        lock(&gate.state).abandoned.is_empty()
    }
    /// An operation blocked until `release` sends or is dropped.
    fn blocked(release: mpsc::Receiver<()>, value: u32) -> impl FnOnce() -> Result<u32> + Send {
        move || {
            let _ = release.recv();
            Ok(value)
        }
    }

    /// A call with no answer by the deadline returns the stable timeout
    /// code; only a change warns that it may still apply later.
    #[test]
    fn an_unanswered_secret_call_times_out_with_its_code() {
        for (access, late_write) in [(Access::Read, false), (Access::Change, true)] {
            let gate = SecretGate::new(SHORT);
            let (release, held) = mpsc::channel();
            let error = gate.run(access, blocked(held, 1)).unwrap_err();
            assert_eq!(error.code(), TIMEOUT_CODE);
            let message = error.to_string();
            assert!(message.contains("keyring may be locked"), "{message}");
            assert_eq!(message.contains("may still apply"), late_write, "{message}");
            assert!(!is_open(&gate));
            drop(release);
        }
    }

    /// While an abandoned worker waits, a new call fails at once with the
    /// same code and never starts; once that worker finishes, calls run
    /// again and its late result reaches nobody.
    #[test]
    fn an_abandoned_secret_call_fails_others_fast_until_it_finishes() {
        let gate = SecretGate::new(WAIT);
        let (release, held) = mpsc::channel();
        let (call, worker) = gate.start(blocked(held, 1)).unwrap();
        let error = gate.settle(call, Access::Change).unwrap_err();
        assert_eq!(error.code(), TIMEOUT_CODE);

        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = started.clone();
        let error = gate
            .run(Access::Read, move || {
                flag.store(true, Ordering::SeqCst);
                Ok(2)
            })
            .unwrap_err();
        assert_eq!(error.code(), TIMEOUT_CODE);
        assert!(error.to_string().contains("still waiting"), "{error}");
        assert!(!started.load(Ordering::SeqCst));

        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(is_open(&gate));
        assert_eq!(gate.run(Access::Read, || Ok(3)).unwrap(), 3);
    }

    /// A result sent after the caller's wait expired but before it settled
    /// is returned, and the finished worker leaves the gate open.
    #[test]
    fn a_secret_result_at_the_deadline_is_returned_and_keeps_the_gate_open() {
        let gate = SecretGate::new(Duration::ZERO);
        let (call, worker) = gate.start(|| Ok(7)).unwrap();
        worker.join().unwrap();
        assert_eq!(gate.settle(call, Access::Change).unwrap(), 7);
        assert!(is_open(&gate));
        assert!(gate.start(|| Ok(8)).is_ok());
    }

    /// Concurrent calls run at the same time (they meet at a barrier) and
    /// each gets its own result.
    #[test]
    fn concurrent_secret_calls_both_succeed() {
        let gate = Arc::new(SecretGate::new(WAIT));
        let meet = Arc::new(std::sync::Barrier::new(2));
        let callers: Vec<_> = [10u32, 20]
            .into_iter()
            .map(|value| {
                let (gate, meet) = (gate.clone(), meet.clone());
                std::thread::spawn(move || {
                    gate.run(Access::Change, move || {
                        meet.wait();
                        Ok(value)
                    })
                })
            })
            .collect();
        let results: Vec<u32> = callers
            .into_iter()
            .map(|caller| caller.join().unwrap().unwrap())
            .collect();
        assert_eq!(results, [10, 20]);
        assert!(is_open(&gate));
    }

    /// Only the abandoned call reopens the gate: another call finishing
    /// meanwhile leaves it closed.
    #[test]
    fn only_the_abandoned_secret_call_reopens_the_gate() {
        let gate = SecretGate::new(WAIT);
        let (release_kept, held_kept) = mpsc::channel();
        let (release_lost, held_lost) = mpsc::channel();
        let (kept, kept_worker) = gate.start(blocked(held_kept, 1)).unwrap();
        let (lost, lost_worker) = gate.start(blocked(held_lost, 2)).unwrap();
        assert_eq!(
            gate.settle(lost, Access::Read).unwrap_err().code(),
            TIMEOUT_CODE
        );

        release_kept.send(()).unwrap();
        kept_worker.join().unwrap();
        assert_eq!(kept.result.recv().unwrap().unwrap(), 1);
        assert!(!is_open(&gate));

        release_lost.send(()).unwrap();
        lost_worker.join().unwrap();
        assert!(is_open(&gate));
    }

    /// A worker that panics ends its call with an error, not the timeout
    /// code, and leaves the gate open.
    #[test]
    fn a_panicking_secret_worker_fails_its_call_and_keeps_the_gate_open() {
        let gate = SecretGate::new(WAIT);
        let error = gate
            .run(Access::Read, || -> Result<u32> { panic!("synthetic") })
            .unwrap_err();
        assert_ne!(error.code(), TIMEOUT_CODE);
        assert!(is_open(&gate));
    }
}
