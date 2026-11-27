//! macOS implementations of the platform traits. Empty on other systems.
#![cfg(target_os = "macos")]

mod about;
mod announcer;
mod autostart;
mod ax;
mod browser;
mod calendar;
mod decoder;
mod devices;
mod display;
mod encoder;
mod keychain;
mod mic;
mod mic_usage;
mod notifier;
mod overlay;
mod peer;
mod player;
mod priority;
mod resource;
mod status_item;
mod system_info;
mod tap;
pub mod uninstall;
mod update;
mod watcher;
mod windows;

pub use about::MacAboutReader;
pub use announcer::MacAnnouncer;
pub use autostart::{AgentStatus, MacAutostart};
pub use notifier::MacNotifier;

use std::path::PathBuf;

use broscribe_platform::ffmpeg_decoder::FfmpegDecoder;
use broscribe_platform::symphonia_decoder::SymphoniaDecoder;
use broscribe_platform::{AppPaths, DecoderChain, MediaDecoder, ProcessProbe};

pub use ax::MacAppUiReader;
pub use browser::{MacBrowserLauncher, MacSystemSettings};
pub use calendar::{
    CalendarKind, EventStoreAccess, MAX_WINDOW_S, MacSystemCalendar, PROMPT_TIMEOUT,
    ParticipantKind, RawCalendar, RawCalendarEvent, RawParticipant, SourceKind, eligible_calendars,
    selected_events,
};
pub use decoder::MacMediaDecoder;
pub use devices::MacDeviceInfo;
pub use display::MacDisplayAccessibility;
pub use encoder::MacAudioEncoder;
pub use keychain::{KEYCHAIN_SERVICE, MacKeychain, profile_identity};
pub use mic::MacMicCapture;
pub use mic_usage::MacMicUsageMonitor;
pub use overlay::MacRecordingOverlay;
pub use peer::{MacPeerCredentials, socket_location};
pub use player::{MacAudioPlayer, NATIVE_EXTENSIONS};
pub use priority::MacProcessPriority;
pub use resource::MacResourceUsage;
pub use status_item::MacStatusItem;
pub use system_info::MacSystemInfo;
pub use tap::MacSystemAudioCapture;
pub use update::MacUpdatePlatform;
pub use watcher::MacLibraryWatcher;
pub use windows::MacWindowInspector;

/// Confirmation dialogs place Cancel before the action on macOS (SYS-151).
pub fn dialog_button_order() -> broscribe_platform::DialogButtonOrder {
    broscribe_platform::DialogButtonOrder::CancelFirst
}

/// The status icon lives in the menu bar on macOS (SHL-14).
pub fn status_icon_place() -> broscribe_platform::StatusIconPlace {
    broscribe_platform::StatusIconPlace::MenuBar
}

/// `~/Library/Application Support/BroScribe`, `~/Library/Caches/BroScribe`, `~/BroScribe`.
pub struct MacPaths {
    home: PathBuf,
}

impl MacPaths {
    pub fn new() -> Self {
        Self {
            home: std::env::home_dir().unwrap_or_else(|| PathBuf::from("/")),
        }
    }
}

impl Default for MacPaths {
    fn default() -> Self {
        Self::new()
    }
}

impl AppPaths for MacPaths {
    fn data_dir(&self) -> PathBuf {
        self.home.join("Library/Application Support/BroScribe")
    }
    fn default_data_dir_source(&self) -> &'static str {
        "HOME"
    }
    fn cache_dir(&self) -> PathBuf {
        self.home.join("Library/Caches/BroScribe")
    }
    fn documents_dir(&self) -> PathBuf {
        self.home.join("Documents")
    }
    fn default_library_dir(&self) -> PathBuf {
        self.home.join("BroScribe")
    }
}

/// Liveness via `kill(pid, 0)`; `EPERM` (another user's process) counts as alive.
pub struct MacProcessProbe;

impl ProcessProbe for MacProcessProbe {
    fn is_alive(&self, pid: u32) -> bool {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 performs only the existence and permission checks.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn current_pid(&self) -> u32 {
        std::process::id()
    }
}

/// Decoders in order: AVFoundation (native containers and codecs, one track
/// at a time), symphonia (formats AVFoundation cannot open, such as Ogg),
/// then ffmpeg when `ffmpeg` and `ffprobe` are installed. A decoder that
/// fails before emitting audio passes the file to the next one.
pub fn decoder_chain() -> DecoderChain {
    let mut decoders: Vec<Box<dyn MediaDecoder>> =
        vec![Box::new(MacMediaDecoder), Box::new(SymphoniaDecoder::new())];
    let ffmpeg = FfmpegDecoder::locate();
    if ffmpeg.is_available() {
        decoders.push(Box::new(ffmpeg));
    }
    DecoderChain::new(decoders)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive() {
        let p = MacProcessProbe;
        assert!(p.is_alive(p.current_pid()));
        assert!(!p.is_alive(0));
        assert!(!p.is_alive(u32::MAX));
    }

    #[test]
    fn paths_under_home() {
        let p = MacPaths {
            home: PathBuf::from("/Users/example"),
        };
        assert_eq!(
            p.data_dir(),
            PathBuf::from("/Users/example/Library/Application Support/BroScribe")
        );
        assert_eq!(
            p.default_library_dir(),
            PathBuf::from("/Users/example/BroScribe")
        );
    }

    #[test]
    fn chain_is_avfoundation_then_symphonia_then_optional_ffmpeg() {
        let chain = decoder_chain();
        let names = chain.names();
        assert_eq!(names[..2], [MacMediaDecoder::NAME, SymphoniaDecoder::NAME]);
        let ffmpeg = FfmpegDecoder::locate().is_available();
        assert_eq!(names.len(), 2 + usize::from(ffmpeg));
        assert_eq!(names.get(2) == Some(&"ffmpeg"), ffmpeg);
    }
}
