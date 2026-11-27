//! Native Windows edges. GPUI remains the only owner of the UI message pump.
//!
//! Everything here is Windows-only except the pure policy modules
//! (`secret_chunks`, `process_tree`, `pipe_naming`, `install_location`),
//! which build and are unit-tested on every OS.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

mod about;
mod install_location;
mod pipe_naming;
mod process_tree;
mod secret_chunks;

pub use pipe_naming::{profile_identity, recorded_profile_identity};

/// Confirmation dialogs place the action before Cancel on Windows (SYS-151).
pub fn dialog_button_order() -> broscribe_platform::DialogButtonOrder {
    broscribe_platform::DialogButtonOrder::ActionFirst
}

/// The status icon lives in the notification area on Windows (SHL-14).
pub fn status_icon_place() -> broscribe_platform::StatusIconPlace {
    broscribe_platform::StatusIconPlace::NotificationArea
}

#[cfg(target_os = "windows")]
mod audio;
#[cfg(target_os = "windows")]
mod devices;
#[cfg(target_os = "windows")]
mod display;
#[cfg(target_os = "windows")]
mod ipc;
#[cfg(target_os = "windows")]
mod job;
#[cfg(target_os = "windows")]
mod jump_list;
#[cfg(target_os = "windows")]
mod media;
#[cfg(target_os = "windows")]
mod native;
#[cfg(target_os = "windows")]
mod overlay;
#[cfg(target_os = "windows")]
mod shell;
#[cfg(target_os = "windows")]
mod timed;
#[cfg(target_os = "windows")]
mod toast;
#[cfg(target_os = "windows")]
mod ui;
#[cfg(target_os = "windows")]
pub mod uninstall;
#[cfg(target_os = "windows")]
mod update;
#[cfg(target_os = "windows")]
mod watcher;

#[cfg(target_os = "windows")]
pub use windows_only::*;

#[cfg(target_os = "windows")]
mod windows_only {
    pub use crate::about::WinAboutReader;
    pub use crate::audio::{WinMicCapture, WinMicUsageMonitor, WinSystemAudioCapture};
    pub use crate::devices::WinDeviceInfo;
    pub use crate::display::WinDisplayAccessibility;
    pub use crate::ipc::{
        PipeSecurity, WinPipeStream, agent_instance_held, app_pipe_name, current_user_sid,
        is_resource_exhaustion, pipe_name, set_pipe_nonblocking, verify_pipe_client,
        write_nonblocking,
    };
    pub use crate::job::WinProcessTreeControl;
    pub use crate::jump_list::WinJumpList;
    pub use crate::media::WinMediaDecoder;
    pub use crate::native::system_directory;
    pub use crate::native::{
        WinPaths, WinProcessPriority, WinProcessProbe, WinResourceUsage, WinSystemInfo,
    };
    pub use crate::overlay::WinRecordingOverlay;
    pub use crate::shell::{
        SECRET_SERVICE_BASE, WinAutostart, WinBrowserLauncher, WinNotifier, WinSecretStore,
        WinSystemSettings,
    };
    pub use crate::ui::{WinAppUiReader, WinStatusItem, WinWindowInspector};
    pub use crate::update::WindowsUpdatePlatform;
    pub use crate::watcher::WinLibraryWatcher;
    pub use broscribe_platform::ffmpeg_encoder::FfmpegEncoder;
    pub use broscribe_platform::ffmpeg_player::FfplayPlayer;
    pub type WinAudioEncoder = FfmpegEncoder;
    pub type WinAudioPlayer = FfplayPlayer;

    pub fn audio_encoder() -> WinAudioEncoder {
        FfmpegEncoder::with_binary(crate::native::find_binary("ffmpeg.exe"))
    }

    pub fn audio_player() -> WinAudioPlayer {
        FfplayPlayer::with_binaries(
            crate::native::find_binary("ffplay.exe"),
            crate::native::find_binary("ffprobe.exe"),
        )
    }

    pub fn decoder_chain() -> broscribe_platform::DecoderChain {
        use broscribe_platform::{
            ffmpeg_decoder::FfmpegDecoder, symphonia_decoder::SymphoniaDecoder,
        };
        broscribe_platform::DecoderChain::new(vec![
            Box::new(WinMediaDecoder),
            Box::new(SymphoniaDecoder),
            Box::new(FfmpegDecoder::with_binaries(
                crate::native::find_binary("ffmpeg.exe"),
                crate::native::find_binary("ffprobe.exe"),
            )),
        ])
    }
}

#[cfg(target_os = "windows")]
fn error(context: &str, error: impl std::fmt::Display) -> broscribe_platform::Error {
    broscribe_platform::Error::io(context, std::io::Error::other(error.to_string()))
}
#[cfg(target_os = "windows")]
fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Balances COM initialization, including when GPUI already owns an STA.
#[cfg(target_os = "windows")]
struct Com(bool);
#[cfg(target_os = "windows")]
impl Com {
    fn new() -> broscribe_platform::Result<Self> {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.0 == 0x80010106u32 as i32 {
            return Ok(Self(false));
        }
        hr.ok().map_err(|e| error("initializing COM", e))?;
        Ok(Self(true))
    }
}
#[cfg(target_os = "windows")]
impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

/// Start the update runner without inheriting unrelated caller handles.
#[cfg(target_os = "windows")]
pub fn spawn_update_runner(command: &std::process::Command) -> broscribe_platform::Result<u32> {
    shell::spawn_detached_command(command).map(|(_, pid)| pid)
}
