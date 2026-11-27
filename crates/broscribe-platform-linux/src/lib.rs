//! Linux implementations of the platform contracts. GPUI owns the UI loop.
#![cfg(target_os = "linux")]

mod about;
mod accessibility;
mod audio_ring;
mod capture;
mod desktop;
mod display;
mod ipc;
mod machine;
mod process_tree;
mod services;
mod status;
pub mod uninstall;
mod update;
mod watcher;

pub use about::LinuxAboutReader;
pub use accessibility::LinuxAppUiReader;
pub use capture::{
    LinuxDeviceInfo, LinuxMicCapture, LinuxMicUsageMonitor, LinuxSystemAudioCapture,
};
pub use desktop::LinuxWindowInspector;
pub use display::LinuxDisplayAccessibility;
pub use ipc::{LinuxPeerCredentials, profile_identity, socket_location};
pub use machine::{
    LinuxPaths, LinuxProcessPriority, LinuxProcessProbe, LinuxResourceUsage, LinuxSystemInfo,
};
pub use services::{
    LinuxAutostart, LinuxBrowserLauncher, LinuxNotifier, LinuxSecretStore, LinuxSystemSettings,
    SECRET_SERVICE_BASE,
};
pub use status::LinuxStatusItem;
pub use update::LinuxUpdatePlatform;
pub use watcher::LinuxLibraryWatcher;

/// Linux keeps a decorated floating window and accounts for the main window
/// when choosing its initial frame.
pub struct LinuxRecordingOverlay;

impl broscribe_platform::overlay::RecordingOverlay for LinuxRecordingOverlay {
    fn capability(&self) -> broscribe_platform::overlay::OverlayCapability {
        broscribe_platform::overlay::OverlayCapability::Standard
    }

    fn default_frame(
        &self,
        visible: broscribe_platform::overlay::OverlayRect,
        default: broscribe_platform::overlay::OverlayRect,
        main: Option<broscribe_platform::overlay::OverlayRect>,
        min_width: f64,
    ) -> broscribe_platform::overlay::OverlayRect {
        broscribe_platform::overlay::beside_main_window(visible, default, main, min_width)
    }

    fn present_standard(
        &self,
        window: &dyn broscribe_platform::overlay::HasWindowHandle,
    ) -> broscribe_platform::Result<()> {
        use broscribe_platform::overlay::RawWindowHandle;
        use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt as _, StackMode};
        let xid = match window.window_handle().map(|handle| handle.as_raw()) {
            Ok(RawWindowHandle::Xlib(handle)) => handle.window as u32,
            Ok(RawWindowHandle::Xcb(handle)) => handle.window.get(),
            // Wayland has no client-side raise that leaves activation alone.
            _ => return Ok(()),
        };
        let (connection, _) = x11rb::connect(None).map_err(|error| {
            platform_error("connecting to X11 to present recording controls", error)
        })?;
        connection
            .configure_window(xid, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))
            .map_err(|error| platform_error("raising recording controls", error))?
            .check()
            .map_err(|error| platform_error("raising recording controls", error))
    }
}

/// Confirmation dialogs place Cancel before the action on Linux (SYS-151).
pub fn dialog_button_order() -> broscribe_platform::DialogButtonOrder {
    broscribe_platform::DialogButtonOrder::CancelFirst
}

/// Where the status icon lives (SHL-14): the system tray when a
/// StatusNotifierWatcher runs on the session bus, nowhere otherwise.
pub fn status_icon_place() -> broscribe_platform::StatusIconPlace {
    if status::status_notifier_watcher_present() {
        broscribe_platform::StatusIconPlace::SystemTray
    } else {
        broscribe_platform::StatusIconPlace::NoTrayHost
    }
}

/// Freedesktop icon name of notifications. Adwaita 48 has only the
/// `-symbolic` microphone, not `audio-input-microphone`. (The tray item
/// sends BroScribe's own waveform pixmaps instead of a themed name.)
pub(crate) const ICON_NAME: &str = "audio-input-microphone-symbolic";

pub fn decoder_chain() -> broscribe_platform::DecoderChain {
    broscribe_platform::DecoderChain::new(vec![
        Box::new(broscribe_platform::symphonia_decoder::SymphoniaDecoder::new()),
        Box::new(broscribe_platform::ffmpeg_decoder::FfmpegDecoder::locate()),
    ])
}

fn platform_error(context: &str, error: impl std::fmt::Display) -> broscribe_platform::Error {
    broscribe_platform::Error::io(context, std::io::Error::other(error.to_string()))
}
