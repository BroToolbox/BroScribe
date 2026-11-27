//! Secrets, browser, autostart, notifications and machine information.

use std::{
    fs::{File, OpenOptions, TryLockError},
    num::NonZeroU32,
    path::Path,
    time::{Duration, Instant},
};

use crate::{Error, Result};

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

/// Opens web pages (licence and terms links) in the default browser.
/// Only `http` and `https` URLs are accepted (see [`check_web_url`]).
pub trait BrowserLauncher: Send + Sync {
    fn open(&self, url: &str) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostPlatform {
    MacOs,
    Windows,
    Linux,
}

impl HostPlatform {
    pub fn secret_store_name(self) -> &'static str {
        match self {
            Self::MacOs => "Keychain",
            Self::Windows => "Windows Credential Manager",
            Self::Linux => "system keyring",
        }
    }

    pub fn reveal_label(self) -> &'static str {
        match self {
            Self::MacOs => "Show in Finder",
            Self::Windows => "Show in File Explorer",
            Self::Linux => "Show in file manager",
        }
    }

    /// The reveal command with the object named, for use in contextual UI.
    pub fn reveal_object_label(self, object: &str) -> String {
        self.reveal_label()
            .replacen("Show in ", &format!("Show {object} in "), 1)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsPane {
    Microphone,
    SystemAudio,
    Accessibility,
    Notifications,
    LoginItems,
    Taskbar,
}

/// Opens only known system panes. The platform maps each pane to its native
/// destination and the application never supplies a URL.
pub trait SystemSettingsOpener: Send + Sync {
    fn host_platform(&self) -> HostPlatform;
    fn supports(&self, pane: SettingsPane) -> bool;
    /// Reads the application’s notification authorization without prompting.
    fn notifications_permission(&self) -> Option<&'static str> {
        None
    }
    fn open(&self, pane: SettingsPane) -> Result<()>;
}

/// Accepts only absolute `http://` / `https://` URLs with a host and without
/// whitespace or control characters; everything else (`file:`, custom
/// schemes, `javascript:`…) is refused.
pub fn check_web_url(url: &str) -> Result<()> {
    let refuse = |why: &str| Err(Error::InvalidInput(format!("refusing to open URL ({why})")));
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return refuse("whitespace or control characters");
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return refuse("not an absolute URL");
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return refuse("only http and https are allowed");
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    if host.is_empty() || host.starts_with(':') {
        return refuse("missing host");
    }
    Ok(())
}

/// The operating system's view of a registered agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentServiceStatus {
    NotInstalled,
    Stopped,
    NeedsApproval,
    Running,
}

/// A snapshot of the job the login manager would stop.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoginJob {
    /// One-shot login registration, such as the Windows Run entry.
    #[default]
    Unsupported,
    Absent,
    /// Installed but unloaded, or a one-shot startup registration.
    Registered {
        serves_profile: Option<bool>,
    },
    Idle {
        /// Whether the loaded configuration would start the requested profile.
        serves_profile: Option<bool>,
    },
    Running(NonZeroU32),
    Unknown,
}

/// Serializes changes to the per-user login job across all profiles.
/// Leave its directory entry in place.
#[derive(Debug, Default)]
pub struct LoginRegistrationGuard {
    file: Option<File>,
}

/// Unlocks explicitly: flock locks belong to the open file description, and a
/// child forked by another thread holds a copy of it until its exec closes it
/// (close-on-exec), so closing only this handle could leave the lock held.
impl Drop for LoginRegistrationGuard {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            let _ = file.unlock();
        }
    }
}

impl LoginRegistrationGuard {
    pub const DEFAULT_WAIT: Duration = Duration::from_secs(15);

    /// Opens the lock in a private directory and waits at most `wait` for it.
    pub fn acquire(path: &Path, wait: Duration) -> Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options.open(path).map_err(|e| Error::io_at(path, e))?;
        Self::acquire_open_file(path, file, wait)
    }

    /// Locks the file selected and opened by the platform.
    pub fn acquire_open_file(path: &Path, file: File, wait: Duration) -> Result<Self> {
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file: Some(file) }),
                Err(TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(Error::Service {
                            code: "busy",
                            message: "Another operation is changing BroScribe Agent’s login registration. Wait for it to finish, then try again".into(),
                        });
                    }
                    std::thread::sleep(Duration::from_millis(50).min(remaining));
                }
                Err(TryLockError::Error(e)) => return Err(Error::io_at(path, e)),
            }
        }
    }
}

/// A login registration can override a profile's library independently of config.
pub struct RegisteredProfile {
    pub data_dir: std::path::PathBuf,
    pub library: Option<std::path::PathBuf>,
}

impl From<std::path::PathBuf> for RegisteredProfile {
    fn from(data_dir: std::path::PathBuf) -> Self {
        Self {
            data_dir,
            library: None,
        }
    }
}

pub trait Autostart: Send + Sync {
    fn is_enabled(&self) -> Result<bool>;
    fn set_enabled(&self, enabled: bool) -> Result<()>;
    /// Change registration under a guard already held by the caller.
    /// Adapters with a registration lock must use the supplied guard.
    fn set_enabled_with_registration(
        &self,
        enabled: bool,
        _registration: &LoginRegistrationGuard,
    ) -> Result<()> {
        self.set_enabled(enabled)
    }
    /// Installed profiles, even when their jobs are unloaded.
    fn registered_profiles(&self) -> Result<Vec<RegisteredProfile>> {
        Ok(Vec::new())
    }
    /// Remove the verified registration while the shared reservation is held.
    fn unregister_profile(&self, _data_dir: &Path, guard: &LoginRegistrationGuard) -> Result<()> {
        self.set_enabled_with_registration(false, guard)
    }
    fn agent_install_location_advisory(&self) -> bool {
        false
    }
    fn agent_status(&self) -> Result<AgentServiceStatus> {
        Ok(if self.is_enabled()? {
            AgentServiceStatus::Stopped
        } else {
            AgentServiceStatus::NotInstalled
        })
    }
    fn start(&self) -> Result<()> {
        Err(Error::Unsupported(
            "starting this platform's agent is unavailable".into(),
        ))
    }
    /// Check that an update can restart the agent without changing login preferences.
    /// Called under the shared login registration guard.
    fn check_update_restart(&self) -> Result<()> {
        Ok(())
    }
    /// Resolve the library a service-manager restart will actually use.
    fn update_restart_library(
        &self,
        configured: Option<&Path>,
        default: &Path,
    ) -> Result<std::path::PathBuf> {
        Ok(configured.unwrap_or(default).to_path_buf())
    }
    /// Start under a guard already held by the caller.
    fn start_with_registration(&self, _registration: &LoginRegistrationGuard) -> Result<()> {
        self.start()
    }
    /// Restart the admitted profile without relying on the caller's cwd
    /// or library override. Service managers keep their installed settings.
    fn start_for_update(
        &self,
        registration: &LoginRegistrationGuard,
        _data_dir: &Path,
        _library: &Path,
    ) -> Result<()> {
        self.start_with_registration(registration)
    }
    fn stop(&self) -> Result<()> {
        Err(Error::Unsupported(
            "stopping this platform's agent is unavailable".into(),
        ))
    }
    /// Reserve the shared login registration. One-shot registrations have no
    /// login job to unload and need no reservation.
    fn lock_login_registration(&self, _wait: Duration) -> Result<LoginRegistrationGuard> {
        Ok(LoginRegistrationGuard::default())
    }
    /// Query the live PID, or identify the configured profile if the job is idle.
    fn login_job(&self, _data_dir: &Path) -> LoginJob {
        LoginJob::Unsupported
    }
    /// Query within the caller's settle budget. Adapters with multiple stages
    /// share this absolute deadline across the stages.
    fn login_job_until(&self, data_dir: &Path, _deadline: Instant) -> LoginJob {
        self.login_job(data_dir)
    }
    /// Identify the loaded configuration when a live job cannot be matched by PID.
    fn login_job_matches(&self, _data_dir: &Path) -> Option<bool> {
        None
    }
    fn login_job_matches_until(&self, data_dir: &Path, _deadline: Instant) -> Option<bool> {
        self.login_job_matches(data_dir)
    }
    /// Unload under the same reservation used to verify the idle job's profile
    /// or identify its live process and take a stop admission.
    /// Platforms with a one-shot login registration may have no job to unload.
    fn stop_login_job(&self, _registration: &LoginRegistrationGuard) -> Result<()> {
        self.stop()
    }
}

/// Observes the operating-system claim that keeps the agent unique per profile.
pub trait AgentInstanceProbe: Send + Sync {
    /// Check without waiting for or retaining the claim.
    fn is_held(&self, data_dir: &Path) -> std::io::Result<bool>;
}

/// A platform notification, with stable identifiers returned by its actions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub id: String,
    pub title: String,
    pub body: String,
    pub preview: NotificationPreview,
    /// Report a click on the notification itself as `open`, with no button.
    pub opens_on_click: bool,
    pub actions: Vec<NotificationActionSpec>,
}

/// Kind of generic preview a platform can show when personal details are hidden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotificationPreview {
    #[default]
    App,
    Detection,
    Recording,
    Meeting,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationActionSpec {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationAction {
    pub id: String,
    /// `__denied` / `__delivered` report asynchronous delivery outcomes.
    pub action: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationDelivery {
    Pending,
    Delivered,
    Denied,
}

/// The OS authorization for posting BroScribe notifications.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotificationAuthorization {
    #[default]
    Unsupported,
    Unknown,
    NotDetermined,
    Granted,
    Denied,
}

pub trait Notifier: Send + Sync {
    /// Reads notification access without showing an OS prompt.
    fn authorization(&self) -> NotificationAuthorization {
        NotificationAuthorization::Unsupported
    }

    /// Requests the system's authorization. May wait for the system reply.
    fn request_authorization(&self) -> NotificationAuthorization {
        self.authorization()
    }

    fn notify(&self, title: &str, body: &str) -> Result<()>;
    /// Call on the application's UI loop. The callback may run on another
    /// thread; callers marshal actions back to that loop.
    fn notify_actionable(
        &self,
        _notification: Notification,
        _callback: std::sync::Arc<dyn Fn(NotificationAction) + Send + Sync>,
    ) -> Result<NotificationDelivery> {
        Ok(NotificationDelivery::Denied)
    }
    /// Withdraw an expired or answered notification on the UI loop.
    fn withdraw(&self, _id: &str) -> Result<()> {
        Ok(())
    }
}

/// Machine facts for model profile checks (setup step 3).
pub trait SystemInfo: Send + Sync {
    /// Whether the platform knows any of these registry aliases as an installed application.
    fn application_installed(&self, _identities: &[String]) -> Option<bool> {
        None
    }

    /// CPU / chip name, e.g. `Apple M2 Pro`; empty when unknown.
    fn chip(&self) -> String;
    /// Physical memory in bytes; 0 when unknown.
    fn memory_bytes(&self) -> u64;
    /// Bytes available to the user on the volume holding `path`. `path` may
    /// not exist yet; its nearest existing ancestor is used.
    fn free_disk_bytes(&self, path: &Path) -> Result<u64>;
    /// Logical CPU cores; at least 1.
    fn cpu_cores(&self) -> usize;
}

/// Resource usage of the calling process so far (`getrusage(RUSAGE_SELF)`
/// on Unix), with units normalised across platforms.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcessUsage {
    /// Peak resident set size in bytes.
    pub peak_rss_bytes: u64,
    /// CPU time spent in user mode, in seconds.
    pub user_cpu_s: f64,
    /// CPU time spent in the kernel on the process's behalf, in seconds.
    pub system_cpu_s: f64,
}

/// Measures the calling process (bench workers report their own peak memory).
pub trait ResourceUsage: Send + Sync {
    fn self_usage(&self) -> Result<ProcessUsage>;
}

/// Test double for [`ResourceUsage`]: returns a fixed value, or an error
/// when `usage` is `None` (platform cannot measure).
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeResourceUsage {
    pub usage: Option<ProcessUsage>,
}

impl ResourceUsage for FakeResourceUsage {
    fn self_usage(&self) -> Result<ProcessUsage> {
        self.usage
            .ok_or_else(|| Error::Unsupported("resource usage is not measured here".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_fake_opens_only_supported_panes() {
        let settings = crate::fakes::FakeSystemSettings {
            host: HostPlatform::Windows,
            supported: vec![SettingsPane::Microphone],
            opened: std::sync::Mutex::new(Vec::new()),
            notifications_permission: Some("denied"),
        };
        assert_eq!(settings.host_platform(), HostPlatform::Windows);
        assert_eq!(settings.notifications_permission(), Some("denied"));
        assert!(settings.supports(SettingsPane::Microphone));
        assert!(settings.open(SettingsPane::Microphone).is_ok());
        assert!(settings.open(SettingsPane::SystemAudio).is_err());
        assert_eq!(*settings.opened.lock().unwrap(), [SettingsPane::Microphone]);
    }

    #[test]
    fn web_urls_only() {
        for ok in [
            "https://example.org",
            "HTTP://example.org/licence?x=1#y",
            "https://user@example.org:8443/a",
        ] {
            assert!(check_web_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:someone@example.org",
            "ftp://example.org",
            "https://",
            "https://:80/x",
            "https://example.org/a b",
            "https://example.org/\n",
            "example.org",
            "",
        ] {
            assert!(check_web_url(bad).is_err(), "{bad}");
        }
    }
}
