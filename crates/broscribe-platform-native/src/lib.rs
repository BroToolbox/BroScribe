//! The one place that picks the platform implementation for the current OS.

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
compile_error!("BroScribe supports macOS, Windows and Linux");

#[cfg(target_os = "linux")]
use broscribe_platform_linux as imp;
#[cfg(target_os = "macos")]
use broscribe_platform_macos as imp;
#[cfg(target_os = "windows")]
use broscribe_platform_windows as imp;
#[cfg(target_os = "linux")]
use imp::LinuxSystemSettings as NativeSystemSettings;
#[cfg(target_os = "macos")]
use imp::MacSystemSettings as NativeSystemSettings;
#[cfg(target_os = "windows")]
use imp::WinSystemSettings as NativeSystemSettings;

mod about;
mod calendar;
mod instance;
pub mod ipc;
#[cfg(unix)]
mod process_tree;
mod trust;
pub mod uninstall;

use broscribe_platform::{
    AppPaths, AppUiReader, AudioPlayer, BrowserLauncher, CalendarAuthority, DecoderChain,
    LibraryWatcher, MicCapture, MicUsageMonitor, ProcessProbe, ResourceUsage, Result, SecretStore,
    StatusClickSender, StatusItem, SystemCalendar, SystemInfo, WindowInspector,
};
use broscribe_platform::{HostPlatform, SystemSettingsOpener};

pub use instance::NativeInstanceChannel;

pub struct NativePlatform;

impl NativePlatform {
    /// Selects the platform's title style for fixed notification copy.
    pub fn notification_title(mac: &str, other: &str) -> String {
        if Self::host_platform() == HostPlatform::MacOs {
            mac
        } else {
            other
        }
        .into()
    }

    pub fn jump_list(
        data_dir: Option<&std::path::Path>,
        library: Option<&std::path::Path>,
    ) -> Result<Option<Box<dyn broscribe_platform::shell::JumpList>>> {
        #[cfg(target_os = "windows")]
        {
            Ok(Some(Box::new(imp::WinJumpList::new(data_dir, library)?)))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (data_dir, library);
            Ok(None)
        }
    }

    pub fn system_settings() -> impl SystemSettingsOpener {
        NativeSystemSettings
    }

    pub fn host_platform() -> HostPlatform {
        Self::system_settings().host_platform()
    }
}

#[cfg(target_os = "macos")]
impl NativePlatform {
    pub fn diagnostic_policy() -> broscribe_platform::about::DiagnosticPolicy {
        broscribe_platform::about::DiagnosticPolicy {
            external_tools_beside_executable: false,
            native_playback: true,
            reader_permission_is_app_specific: true,
        }
    }

    pub fn paths() -> Box<dyn AppPaths> {
        Box::new(imp::MacPaths::new())
    }

    pub fn process_probe() -> Box<dyn ProcessProbe> {
        Box::new(imp::MacProcessProbe)
    }

    pub fn decoder_chain() -> DecoderChain {
        imp::decoder_chain()
    }

    pub fn mic_capture() -> Box<dyn MicCapture> {
        Box::new(imp::MacMicCapture)
    }

    pub fn system_audio_capture() -> std::sync::Arc<dyn broscribe_platform::SystemAudioCapture> {
        std::sync::Arc::new(imp::MacSystemAudioCapture::default())
    }

    pub fn process_priority() -> std::sync::Arc<dyn broscribe_platform::ProcessPriority> {
        std::sync::Arc::new(imp::MacProcessPriority)
    }

    pub fn audio_encoder() -> std::sync::Arc<dyn broscribe_platform::AudioEncoder> {
        std::sync::Arc::new(imp::MacAudioEncoder)
    }

    pub fn device_info() -> Box<dyn broscribe_platform::DeviceInfo> {
        Box::new(imp::MacDeviceInfo)
    }

    /// Processes holding a microphone (meeting detection).
    pub fn mic_usage_monitor() -> Box<dyn MicUsageMonitor> {
        Box::new(imp::MacMicUsageMonitor)
    }

    /// On-screen windows (meeting detection); titles only with Screen Recording.
    pub fn window_inspector() -> Box<dyn WindowInspector> {
        Box::new(imp::MacWindowInspector)
    }

    /// Whether window titles are readable; never prompts for the permission.
    pub fn window_titles_available() -> bool {
        imp::MacWindowInspector::titles_available()
    }

    /// Meeting-app UI through the Accessibility API (readers, design §9).
    /// Observers must be attached on the UI loop's main thread.
    pub fn app_ui_reader() -> Box<dyn AppUiReader> {
        Box::new(imp::MacAppUiReader::default())
    }

    /// Main-thread player (`!Send`).
    pub fn audio_player() -> Box<dyn AudioPlayer> {
        Box::new(imp::MacAudioPlayer::new())
    }

    pub fn library_watcher() -> Box<dyn LibraryWatcher> {
        Box::new(imp::MacLibraryWatcher)
    }

    pub fn system_info() -> Box<dyn SystemInfo> {
        Box::new(imp::MacSystemInfo)
    }

    /// The calling process's peak memory and CPU time (bench workers).
    pub fn resource_usage() -> Box<dyn ResourceUsage> {
        Box::new(imp::MacResourceUsage)
    }

    pub fn browser() -> Box<dyn BrowserLauncher> {
        Box::new(imp::MacBrowserLauncher)
    }

    /// The Keychain store for provider API keys and integration tokens of
    /// the data folder `data_dir` (see [`Self::secret_service`]).
    pub fn secret_store(data_dir: &std::path::Path) -> Box<dyn SecretStore> {
        Box::new(imp::MacKeychain::with_service(&Self::secret_service(
            imp::KEYCHAIN_SERVICE,
            data_dir,
        )))
    }

    pub fn notifier(_profile: Option<&std::path::Path>) -> Box<dyn broscribe_platform::Notifier> {
        Box::new(imp::MacNotifier)
    }

    /// EventKit access. Only the installed agent may use it: its bundle owns
    /// the calendar grant (other processes get `helper_required` on request).
    pub fn system_calendar() -> Box<dyn SystemCalendar> {
        Box::new(imp::MacSystemCalendar)
    }

    /// The agent's live calendar opt-in/grant generations over its
    /// private endpoint; never EventKit in this process.
    pub fn calendar_authority(data_dir: &std::path::Path) -> Box<dyn CalendarAuthority> {
        Box::new(calendar::HelperCalendarAuthority::new(ipc::Endpoint::new(
            data_dir,
        )))
    }

    pub fn autostart(executable: std::path::PathBuf) -> Box<dyn broscribe_platform::Autostart> {
        Box::new(imp::MacAutostart::new(executable))
    }

    /// The menu-bar item. Call on the main thread, inside the UI event loop
    /// (GPUI's launch callback); errors otherwise. Clicks arrive on `clicks`
    /// as menu item ids.
    pub fn status_item(clicks: StatusClickSender) -> Result<Box<dyn StatusItem>> {
        Ok(Box::new(imp::MacStatusItem::new(clicks)?))
    }
    /// The recording panel's nonactivating overlay. Use on the main thread.
    pub fn recording_overlay() -> std::rc::Rc<dyn broscribe_platform::overlay::RecordingOverlay> {
        std::rc::Rc::new(imp::MacRecordingOverlay::default())
    }

    /// Direct assistive-technology announcements on macOS (Q1 Option B).
    pub fn announcer() -> std::sync::Arc<dyn broscribe_platform::Announcer> {
        std::sync::Arc::new(imp::MacAnnouncer)
    }

    /// Increase Contrast and Reduce Transparency (ACC-32).
    pub fn display_accessibility() -> std::sync::Arc<dyn broscribe_platform::DisplayAccessibility> {
        std::sync::Arc::new(imp::MacDisplayAccessibility)
    }
}

#[cfg(target_os = "windows")]
impl NativePlatform {
    pub fn diagnostic_policy() -> broscribe_platform::about::DiagnosticPolicy {
        broscribe_platform::about::DiagnosticPolicy {
            external_tools_beside_executable: true,
            native_playback: false,
            reader_permission_is_app_specific: false,
        }
    }

    pub fn paths() -> Box<dyn AppPaths> {
        Box::new(imp::WinPaths::new())
    }
    pub fn process_probe() -> Box<dyn ProcessProbe> {
        Box::new(imp::WinProcessProbe)
    }
    pub fn decoder_chain() -> DecoderChain {
        imp::decoder_chain()
    }
    pub fn mic_capture() -> Box<dyn MicCapture> {
        Box::new(imp::WinMicCapture)
    }
    pub fn system_audio_capture() -> std::sync::Arc<dyn broscribe_platform::SystemAudioCapture> {
        std::sync::Arc::new(imp::WinSystemAudioCapture::default())
    }
    pub fn process_priority() -> std::sync::Arc<dyn broscribe_platform::ProcessPriority> {
        std::sync::Arc::new(imp::WinProcessPriority)
    }
    pub fn audio_encoder() -> std::sync::Arc<dyn broscribe_platform::AudioEncoder> {
        std::sync::Arc::new(imp::audio_encoder())
    }
    pub fn device_info() -> Box<dyn broscribe_platform::DeviceInfo> {
        Box::new(imp::WinDeviceInfo)
    }
    pub fn mic_usage_monitor() -> Box<dyn MicUsageMonitor> {
        Box::new(imp::WinMicUsageMonitor)
    }
    pub fn window_inspector() -> Box<dyn WindowInspector> {
        Box::new(imp::WinWindowInspector)
    }
    pub fn window_titles_available() -> bool {
        true
    }
    pub fn app_ui_reader() -> Box<dyn AppUiReader> {
        Box::new(imp::WinAppUiReader)
    }
    pub fn audio_player() -> Box<dyn AudioPlayer> {
        Box::new(imp::audio_player())
    }
    pub fn library_watcher() -> Box<dyn LibraryWatcher> {
        Box::new(imp::WinLibraryWatcher)
    }
    pub fn system_info() -> Box<dyn SystemInfo> {
        Box::new(imp::WinSystemInfo)
    }
    pub fn resource_usage() -> Box<dyn ResourceUsage> {
        Box::new(imp::WinResourceUsage)
    }
    pub fn browser() -> Box<dyn BrowserLauncher> {
        Box::new(imp::WinBrowserLauncher)
    }
    /// Credential Manager for the data folder `data_dir` (see [`Self::secret_service`]).
    pub fn secret_store(data_dir: &std::path::Path) -> Box<dyn SecretStore> {
        Box::new(imp::WinSecretStore::with_service(&Self::secret_service(
            imp::SECRET_SERVICE_BASE,
            data_dir,
        )))
    }
    pub fn status_item(clicks: StatusClickSender) -> Result<Box<dyn StatusItem>> {
        Ok(Box::new(imp::WinStatusItem::new(clicks)?))
    }
    pub fn recording_overlay() -> std::rc::Rc<dyn broscribe_platform::overlay::RecordingOverlay> {
        std::rc::Rc::new(imp::WinRecordingOverlay)
    }
    pub fn autostart(executable: std::path::PathBuf) -> Box<dyn broscribe_platform::Autostart> {
        Box::new(imp::WinAutostart::new(executable))
    }
    pub fn notifier(profile: Option<&std::path::Path>) -> Box<dyn broscribe_platform::Notifier> {
        Box::new(imp::WinNotifier::new(profile))
    }
    /// The system calendar is a macOS feature.
    pub fn system_calendar() -> Box<dyn SystemCalendar> {
        Box::new(broscribe_platform::UnsupportedSystemCalendar)
    }
    /// Remote calendar authorities live in the agent on every platform.
    pub fn calendar_authority(data_dir: &std::path::Path) -> Box<dyn CalendarAuthority> {
        Box::new(calendar::HelperCalendarAuthority::new(ipc::Endpoint::new(
            data_dir,
        )))
    }
    pub fn process_tree_control() -> std::sync::Arc<dyn broscribe_platform::ProcessTreeControl> {
        std::sync::Arc::new(imp::WinProcessTreeControl)
    }

    /// Windows announces live regions natively through AccessKit/UIA.
    pub fn announcer() -> std::sync::Arc<dyn broscribe_platform::Announcer> {
        std::sync::Arc::new(broscribe_platform::NoopAnnouncer)
    }

    /// A Windows contrast theme (ACC-32).
    pub fn display_accessibility() -> std::sync::Arc<dyn broscribe_platform::DisplayAccessibility> {
        std::sync::Arc::new(imp::WinDisplayAccessibility)
    }
}

#[cfg(target_os = "linux")]
impl NativePlatform {
    pub fn diagnostic_policy() -> broscribe_platform::about::DiagnosticPolicy {
        broscribe_platform::about::DiagnosticPolicy {
            external_tools_beside_executable: false,
            native_playback: false,
            reader_permission_is_app_specific: false,
        }
    }

    pub fn paths() -> Box<dyn AppPaths> {
        Box::new(imp::LinuxPaths::new())
    }
    pub fn process_probe() -> Box<dyn ProcessProbe> {
        Box::new(imp::LinuxProcessProbe)
    }
    pub fn decoder_chain() -> DecoderChain {
        imp::decoder_chain()
    }
    pub fn mic_capture() -> Box<dyn MicCapture> {
        Box::new(imp::LinuxMicCapture)
    }
    pub fn system_audio_capture() -> std::sync::Arc<dyn broscribe_platform::SystemAudioCapture> {
        std::sync::Arc::new(imp::LinuxSystemAudioCapture::default())
    }
    pub fn process_priority() -> std::sync::Arc<dyn broscribe_platform::ProcessPriority> {
        std::sync::Arc::new(imp::LinuxProcessPriority)
    }
    pub fn audio_encoder() -> std::sync::Arc<dyn broscribe_platform::AudioEncoder> {
        std::sync::Arc::new(broscribe_platform::ffmpeg_encoder::FfmpegEncoder::locate())
    }
    pub fn device_info() -> Box<dyn broscribe_platform::DeviceInfo> {
        Box::new(imp::LinuxDeviceInfo)
    }
    pub fn mic_usage_monitor() -> Box<dyn MicUsageMonitor> {
        Box::new(imp::LinuxMicUsageMonitor)
    }
    pub fn window_inspector() -> Box<dyn WindowInspector> {
        Box::new(imp::LinuxWindowInspector)
    }
    pub fn window_titles_available() -> bool {
        imp::LinuxWindowInspector::titles_available()
    }
    pub fn app_ui_reader() -> Box<dyn AppUiReader> {
        Box::new(imp::LinuxAppUiReader)
    }
    pub fn audio_player() -> Box<dyn AudioPlayer> {
        Box::new(broscribe_platform::ffmpeg_player::FfplayPlayer::locate())
    }
    pub fn library_watcher() -> Box<dyn LibraryWatcher> {
        Box::new(imp::LinuxLibraryWatcher)
    }
    pub fn system_info() -> Box<dyn SystemInfo> {
        Box::new(imp::LinuxSystemInfo)
    }
    pub fn resource_usage() -> Box<dyn ResourceUsage> {
        Box::new(imp::LinuxResourceUsage)
    }
    pub fn browser() -> Box<dyn BrowserLauncher> {
        Box::new(imp::LinuxBrowserLauncher)
    }
    /// Secret Service for the data folder `data_dir` (see [`Self::secret_service`]).
    pub fn secret_store(data_dir: &std::path::Path) -> Box<dyn SecretStore> {
        Box::new(imp::LinuxSecretStore::with_service(&Self::secret_service(
            imp::SECRET_SERVICE_BASE,
            data_dir,
        )))
    }
    pub fn status_item(clicks: StatusClickSender) -> Result<Box<dyn StatusItem>> {
        Ok(Box::new(imp::LinuxStatusItem::new(clicks)?))
    }
    pub fn recording_overlay() -> std::rc::Rc<dyn broscribe_platform::overlay::RecordingOverlay> {
        std::rc::Rc::new(imp::LinuxRecordingOverlay)
    }
    pub fn autostart(executable: std::path::PathBuf) -> Box<dyn broscribe_platform::Autostart> {
        Box::new(imp::LinuxAutostart::new(executable))
    }
    pub fn notifier(_profile: Option<&std::path::Path>) -> Box<dyn broscribe_platform::Notifier> {
        Box::new(imp::LinuxNotifier)
    }
    /// The system calendar is a macOS feature.
    pub fn system_calendar() -> Box<dyn SystemCalendar> {
        Box::new(broscribe_platform::UnsupportedSystemCalendar)
    }
    pub fn calendar_authority(data_dir: &std::path::Path) -> Box<dyn CalendarAuthority> {
        Box::new(calendar::HelperCalendarAuthority::new(ipc::Endpoint::new(
            data_dir,
        )))
    }

    /// Linux announces live regions natively through AccessKit/AT-SPI.
    pub fn announcer() -> std::sync::Arc<dyn broscribe_platform::Announcer> {
        std::sync::Arc::new(broscribe_platform::NoopAnnouncer)
    }

    /// The XDG Settings portal's contrast setting (ACC-32).
    pub fn display_accessibility() -> std::sync::Arc<dyn broscribe_platform::DisplayAccessibility> {
        std::sync::Arc::new(imp::LinuxDisplayAccessibility)
    }
}

impl NativePlatform {
    /// AccessKit's AT-SPI adapter exposes only numeric Value interfaces.
    /// Use the accessible description for a timer's value in words there.
    pub const fn timer_value_uses_description() -> bool {
        cfg!(target_os = "linux")
    }

    /// Order of the safe (Cancel) and action buttons in confirmation dialogs
    /// (SYS-151): `CancelFirst` on macOS and Linux, `ActionFirst` on Windows,
    /// unless a test installed [`broscribe_platform::fakes::FakeDialogButtonOrder`].
    pub fn dialog_button_order() -> broscribe_platform::DialogButtonOrder {
        broscribe_platform::fakes::dialog_button_order_override()
            .unwrap_or_else(imp::dialog_button_order)
    }

    /// The desktop app's own single-instance channel for `data_dir`
    /// (SHL-14): a second launch hands its request to the running app
    /// through it, without the recording agent.
    pub fn instance_channel(
        data_dir: &std::path::Path,
    ) -> std::sync::Arc<dyn broscribe_platform::InstanceChannel> {
        std::sync::Arc::new(NativeInstanceChannel::new(data_dir))
    }

    /// Where the status icon lives (SHL-14): the menu bar on macOS, the
    /// notification area on Windows, the system tray on Linux when a
    /// StatusNotifierWatcher runs and nowhere otherwise; unless a test
    /// installed [`broscribe_platform::fakes::FakeStatusIconPlace`].
    pub fn status_icon_place() -> broscribe_platform::StatusIconPlace {
        broscribe_platform::fakes::status_icon_place_override()
            .unwrap_or_else(imp::status_icon_place)
    }

    /// The secret namespace of `data_dir`: `base` when it is the native
    /// default data folder (also when an override names that folder), so
    /// existing entries keep their names; `<base>.profile-<hash>` for any
    /// other, so profiles never share secrets. Only `BROSCRIBE_DATA_DIR` /
    /// `--data-dir` select a profile: moving the native default itself
    /// (`HOME`, `XDG_DATA_HOME`, `APPDATA`) keeps the base namespace.
    fn secret_service(base: &str, data_dir: &std::path::Path) -> String {
        broscribe_platform::secret_namespace(
            base,
            &imp::profile_identity(data_dir),
            &imp::profile_identity(&Self::paths().data_dir()),
        )
    }

    /// Contains helper children with their descendants (process groups on
    /// Unix, Job Objects on Windows).
    #[cfg(unix)]
    pub fn process_tree_control() -> std::sync::Arc<dyn broscribe_platform::ProcessTreeControl> {
        std::sync::Arc::new(process_tree::GroupTreeControl)
    }

    /// A working directory outside any installation that Windows may remove.
    pub fn update_runner_working_directory(
        _runner_dir: &std::path::Path,
    ) -> Result<std::path::PathBuf> {
        #[cfg(target_os = "windows")]
        return imp::system_directory();
        #[cfg(not(target_os = "windows"))]
        Ok(_runner_dir.to_path_buf())
    }

    /// Start the runner without holding the calling CLI's capture pipes.
    pub fn spawn_update_runner(command: &mut std::process::Command) -> Result<u32> {
        #[cfg(target_os = "windows")]
        return imp::spawn_update_runner(command);
        #[cfg(not(target_os = "windows"))]
        command.spawn().map(|child| child.id()).map_err(|error| {
            broscribe_platform::Error::io_at(command.get_program().as_ref(), error)
        })
    }

    /// The OS side of the signed, opt-in updater for this platform.
    pub fn updater() -> std::sync::Arc<dyn broscribe_platform::update::UpdatePlatform> {
        #[cfg(target_os = "macos")]
        {
            std::sync::Arc::new(imp::MacUpdatePlatform::new())
        }
        #[cfg(target_os = "linux")]
        {
            std::sync::Arc::new(imp::LinuxUpdatePlatform::new())
        }
        #[cfg(target_os = "windows")]
        {
            std::sync::Arc::new(imp::WindowsUpdatePlatform::new())
        }
    }

    /// The updater for the installation of `executable` instead of the
    /// running one: the external update runner, copied outside the
    /// installation it replaces, passes the original executable BroScribe's
    /// own `update install` handed it. Only the path is taken from the
    /// caller (absolute, resolved to a regular file); every native location,
    /// ownership, identity, architecture and signer check of
    /// [`Self::updater`] applies to that installation unchanged, and the
    /// install admission is still required.
    pub fn updater_for(
        executable: std::path::PathBuf,
    ) -> std::sync::Arc<dyn broscribe_platform::update::UpdatePlatform> {
        #[cfg(target_os = "macos")]
        {
            std::sync::Arc::new(imp::MacUpdatePlatform::for_executable(executable))
        }
        #[cfg(target_os = "linux")]
        {
            std::sync::Arc::new(imp::LinuxUpdatePlatform::for_executable(executable))
        }
        #[cfg(target_os = "windows")]
        {
            std::sync::Arc::new(imp::WindowsUpdatePlatform::for_executable(executable))
        }
    }

    /// The platform-selected HTTPS verifier or explicit anchor snapshot.
    /// An unavailable probe never disables verification or substitutes roots.
    pub fn trust_store() -> std::sync::Arc<dyn broscribe_platform::TrustStore> {
        std::sync::Arc::new(trust::NativeTrustStore)
    }

    /// Run once on GPUI's foreground executor. Linux AT-SPI notifications are
    /// marshalled here; macOS and Windows dispatch directly on their native loop.
    pub async fn dispatch_ui_events() {
        #[cfg(target_os = "linux")]
        imp::LinuxAppUiReader::dispatch_events().await;
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        std::future::pending::<()>().await;
    }
}

/// Platform-specific Unix endpoint policy, shared by daemon and clients.
#[cfg(unix)]
pub(crate) fn unix_socket_location(
    data_dir: &std::path::Path,
) -> std::io::Result<broscribe_platform_unix::SocketLocation> {
    imp::socket_location(data_dir)
}

/// Same-user check of accepted daemon socket connections.
#[cfg(unix)]
pub(crate) fn peer_credentials() -> Box<dyn broscribe_platform_unix::PeerCredentials> {
    #[cfg(target_os = "macos")]
    {
        Box::new(imp::MacPeerCredentials)
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(imp::LinuxPeerCredentials)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handed_off_updaters_share_the_platform_capability_and_refuse_bad_paths() {
        let running = NativePlatform::updater();
        let relative = NativePlatform::updater_for(std::path::PathBuf::from("broscribe"));
        assert_eq!(relative.replacement(), running.replacement());
        assert_eq!(relative.installation().unwrap_err().code(), "invalid_input");
        let missing = std::env::current_exe()
            .unwrap()
            .with_file_name("broscribe-missing-handoff");
        assert_eq!(
            NativePlatform::updater_for(missing)
                .installation()
                .unwrap_err()
                .code(),
            "not_found"
        );
    }

    #[test]
    fn only_the_native_default_data_folder_keeps_the_base_secret_namespace() {
        const BASE: &str = "org.broscribe.BroScribe.test";
        let native = NativePlatform::paths().data_dir();
        assert_eq!(NativePlatform::secret_service(BASE, &native), BASE);
        assert_eq!(
            NativePlatform::secret_service(BASE, &native.join(".")),
            BASE
        );
        let tmp = tempfile::tempdir().unwrap();
        let profile = NativePlatform::secret_service(BASE, tmp.path());
        assert!(profile.starts_with("org.broscribe.BroScribe.test.profile-"));
        assert_eq!(
            profile,
            NativePlatform::secret_service(BASE, &tmp.path().join("x").join(".."))
        );
        assert_ne!(
            profile,
            NativePlatform::secret_service(BASE, &tmp.path().join("other"))
        );
    }

    #[test]
    fn status_icon_place_follows_platform_and_respects_fake_override() {
        use broscribe_platform::StatusIconPlace;
        use broscribe_platform::fakes::FakeStatusIconPlace;

        let native = NativePlatform::status_icon_place();
        #[cfg(target_os = "macos")]
        assert_eq!(native, StatusIconPlace::MenuBar);
        #[cfg(target_os = "windows")]
        assert_eq!(native, StatusIconPlace::NotificationArea);
        #[cfg(target_os = "linux")]
        assert!(matches!(
            native,
            StatusIconPlace::SystemTray | StatusIconPlace::NoTrayHost
        ));
        {
            let _fake = FakeStatusIconPlace::new(StatusIconPlace::NoTrayHost);
            assert_eq!(
                NativePlatform::status_icon_place(),
                StatusIconPlace::NoTrayHost
            );
        }
        assert_eq!(NativePlatform::status_icon_place(), native);
    }

    #[test]
    fn dialog_button_order_follows_platform_and_respects_fake_override() {
        use broscribe_platform::DialogButtonOrder;
        use broscribe_platform::fakes::FakeDialogButtonOrder;

        let native = NativePlatform::dialog_button_order();
        #[cfg(target_os = "windows")]
        assert_eq!(native, DialogButtonOrder::ActionFirst);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert_eq!(native, DialogButtonOrder::CancelFirst);

        {
            let _fake = FakeDialogButtonOrder::new(DialogButtonOrder::ActionFirst);
            assert_eq!(
                NativePlatform::dialog_button_order(),
                DialogButtonOrder::ActionFirst
            );
        }
        {
            let _fake = FakeDialogButtonOrder::new(DialogButtonOrder::CancelFirst);
            assert_eq!(
                NativePlatform::dialog_button_order(),
                DialogButtonOrder::CancelFirst
            );
        }
        assert_eq!(NativePlatform::dialog_button_order(), native);
    }
}
