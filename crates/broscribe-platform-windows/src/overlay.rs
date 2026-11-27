//! Native policy for the app's own decorated recording controls.

use broscribe_platform::overlay::{
    CaptureExclusion, HasWindowHandle, OverlayCapability, OverlayPlacement, OverlayPresence,
    RawWindowHandle, RecordingOverlay,
};
use broscribe_platform::{Error, Result};
use windows::Win32::{
    Foundation::HWND,
    System::SystemInformation::OSVERSIONINFOW,
    UI::WindowsAndMessaging::{
        HWND_TOPMOST, SET_WINDOW_POS_FLAGS, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SetWindowDisplayAffinity, SetWindowPos, ShowWindow, WDA_EXCLUDEFROMCAPTURE,
        WDA_NONE,
    },
};

pub struct WinRecordingOverlay;

struct WindowsPresence;
impl OverlayPresence for WindowsPresence {}

pub(crate) fn hwnd(window: &dyn HasWindowHandle) -> Result<HWND> {
    match window.window_handle().map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(window)) => Ok(HWND(window.hwnd.get() as *mut _)),
        _ => Err(Error::Unsupported("the window has no Win32 handle".into())),
    }
}

const TOPMOST_FLAGS: SET_WINDOW_POS_FLAGS =
    SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0 | SWP_NOMOVE.0 | SWP_NOSIZE.0);

fn supports_capture_exclusion() -> bool {
    // GetVersionEx can report a compatibility version. Use the actual build to
    // avoid silently asking older Windows for WDA_MONITOR instead of exclusion.
    windows::core::link!("ntdll.dll" "system" fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32);
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    let status = unsafe { RtlGetVersion(&mut info) };
    status >= 0 && (info.dwMajorVersion, info.dwBuildNumber) >= (10, 19041)
}

impl RecordingOverlay for WinRecordingOverlay {
    fn capability(&self) -> OverlayCapability {
        OverlayCapability::DecoratedTopmost
    }

    fn present_new(
        &self,
        window: &dyn HasWindowHandle,
        _placement: OverlayPlacement,
    ) -> Result<Box<dyn OverlayPresence>> {
        self.present_existing(window)?;
        Ok(Box::new(WindowsPresence))
    }

    fn present_existing(&self, window: &dyn HasWindowHandle) -> Result<()> {
        let hwnd = hwnd(window)?;
        // SAFETY: the handle is borrowed from the app's own live window on
        // the UI thread. The flags preserve its frame and current focus.
        unsafe {
            SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, TOPMOST_FLAGS)
                .map_err(|error| crate::error("setting recording controls topmost", error))?;
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(())
    }

    fn capture_exclusion_supported(&self) -> bool {
        supports_capture_exclusion()
    }

    fn set_capture_excluded(
        &self,
        window: &dyn HasWindowHandle,
        excluded: bool,
    ) -> Result<CaptureExclusion> {
        if !self.capture_exclusion_supported() {
            return Ok(CaptureExclusion::Unsupported);
        }
        let affinity = if excluded {
            WDA_EXCLUDEFROMCAPTURE
        } else {
            WDA_NONE
        };
        // SAFETY: the call receives the handle of the app's live top-level window.
        unsafe { SetWindowDisplayAffinity(hwnd(window)?, affinity) }.map_err(|error| {
            crate::error("changing recording controls capture exclusion", error)
        })?;
        Ok(CaptureExclusion::Applied)
    }
}

#[test]
fn decorated_topmost_preserves_focus_and_frame() {
    assert_eq!(TOPMOST_FLAGS, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
    assert_eq!(
        WinRecordingOverlay.capability(),
        OverlayCapability::DecoratedTopmost
    );
}
