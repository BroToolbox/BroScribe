//! Platform traits (design §3.2). Implementations live in the
//! `broscribe-platform-*` crates; every trait has a fake in [`fakes`].

pub mod about;
pub mod bounded;
pub mod calendar;
pub mod capture;
pub mod devices;
pub mod encoder;
pub mod fakes;
pub mod ffmpeg_decoder;
pub mod ffmpeg_encoder;
pub mod ffmpeg_player;
pub mod media;
pub mod overlay;
pub mod paths;
pub mod player;
pub mod priority;
pub mod process_tree;
pub mod profile;
pub mod profile_cleanup;
pub mod release_layout;
pub mod shell;
pub mod symphonia_decoder;
pub mod system;
pub mod trust;
pub mod ui;
pub mod uninstall;
pub mod update;
pub mod verbatim;
pub mod watch;

pub use broscribe_core::process::ProcessProbe;
pub use broscribe_core::secret;
pub use broscribe_core::{Error, Result};
pub use calendar::{
    CalendarAuthority, CalendarAuthorization, CalendarProvider, CalendarQuery,
    FakeCalendarAuthority, FakeSystemCalendar, SystemCalendar, SystemCalendarAttendee,
    SystemCalendarEvent, SystemCalendarInfo, UnsupportedCalendarAuthority,
    UnsupportedSystemCalendar,
};
pub use capture::{
    AudioDevice, CaptureSession, MicCapture, MicChunk, MicSink, MicUsageMonitor, MicUser,
    SystemAudioCapture,
};
pub use devices::{DeviceDetails, DeviceInfo, DeviceTransport};
pub use encoder::{AudioEncoder, EncodeSettings, EncodedTrack, EncoderTrack, PcmSource};
pub use media::{DecoderChain, MediaDecoder, MediaInfo, PcmSink, TrackInfo};
pub use paths::{AppPaths, ResolvedPaths, resolve_paths};
pub use player::{AudioPlayer, PlayerState};
pub use priority::{PriorityClass, ProcessPriority};
pub use process_tree::{ProcessTreeControl, ProcessTreeHandle, TreeExit, wait_tree};
pub use profile::{paths_overlap, profile_key, same_path, secret_namespace, stable_path};
pub use shell::{
    ActivationPolicy, AppActivation, Handoff, HandoffListener, HandoffReceiver, HandoffRequest,
    HandoffSender, InstanceChannel, StatusIconPlace, handoff_channel,
};
pub use system::{
    AgentInstanceProbe, AgentServiceStatus, Autostart, BrowserLauncher, HostPlatform, LoginJob,
    LoginRegistrationGuard, Notifier, ProcessUsage, RegisteredProfile, ResourceUsage, SecretStore,
    SettingsPane, SystemInfo, SystemSettingsOpener,
};
pub use trust::{TlsTrust, TrustStore};
pub use ui::{
    AX_ROLES, AX_TRANSIENT, AnnouncementPriority, Announcer, AppUiReader, AxChange,
    AxChangeCallback, AxIncomplete, AxLimits, AxNode, AxObservation, AxScope, AxScopePolicy,
    AxTree, DialogButtonOrder, DisplayAccessibility, DisplayPreferences, DocumentContext,
    MenuItemSpec, NoDisplayAccessibility, NoopAnnouncer, StatusClickSender, StatusIcon, StatusItem,
    TaskbarState, WindowInfo, WindowInspector, ax_transient, is_ax_transient,
};
pub use verbatim::plain_verbatim;
pub use watch::{ChangeCallback, LibraryWatcher, WatchHandle};

/// A setup microphone check on the capture's peak, without keeping audio.
pub fn mic_check(samples: &[f32]) -> (&'static str, Option<f32>) {
    if samples.is_empty() {
        return ("no_audio", None);
    }
    let peak = samples
        .iter()
        .fold(0.0f32, |max, sample| max.max(sample.abs()));
    let dbfs = 20.0 * peak.max(1e-9).log10();
    let level = if dbfs < -70.0 {
        "silence"
    } else if dbfs < -35.0 {
        "quiet"
    } else {
        "good_level"
    };
    (level, Some(dbfs))
}

/// Whether there is insufficient space for a model download and its margin.
pub fn model_download_low_disk(needed: u64, free: Option<u64>) -> bool {
    needed > 0 && free.is_some_and(|free| free < needed.saturating_add(needed / 10))
}

/// Whether memory is below the desktop model recommendation.
pub fn model_download_low_memory(memory: Option<u64>) -> bool {
    memory.is_some_and(|bytes| bytes > 0 && bytes < 8 * 1024 * 1024 * 1024)
}

/// Open a regular leaf without traversing a symlink or Windows reparse point.
pub fn open_regular_nofollow(
    options: &mut std::fs::OpenOptions,
    path: &std::path::Path,
) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Opening a special file such as a FIFO must not block before the type check.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || release_layout::is_reparse(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Expected a regular file",
        ));
    }
    Ok(file)
}
