use broscribe_platform::{Result, WindowInfo, WindowInspector};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM},
        UI::WindowsAndMessaging::*,
    },
    core::BOOL,
};
#[path = "ax.rs"]
mod ax;
#[path = "status.rs"]
mod status;
pub use ax::WinAppUiReader;
pub use status::WinStatusItem;

pub struct WinWindowInspector;
pub(crate) fn top_windows(pid: Option<u32>) -> Result<Vec<(HWND, u32)>> {
    unsafe extern "system" fn collect(hwnd: HWND, param: LPARAM) -> BOOL {
        let windows = unsafe { &mut *(param.0 as *mut Vec<(HWND, u32)>) };
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
            }
            if pid != 0 {
                windows.push((hwnd, pid));
            }
        }
        true.into()
    }
    let mut windows: Vec<(HWND, u32)> = Vec::new();
    unsafe {
        EnumWindows(
            Some(collect),
            LPARAM((&mut windows as *mut Vec<(HWND, u32)>) as isize),
        )
    }
    .map_err(|e| crate::error("enumerating top-level windows", e))?;
    if let Some(pid) = pid {
        windows.retain(|(_, owner)| *owner == pid);
    }
    Ok(windows)
}
impl WindowInspector for WinWindowInspector {
    fn windows(&self) -> Result<Vec<WindowInfo>> {
        top_windows(None).map(|windows| {
            windows
                .into_iter()
                .map(|(hwnd, pid)| {
                    let path = crate::native::process_path(pid);
                    let app = path
                        .as_ref()
                        .and_then(|p| p.file_stem())
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let len = unsafe { GetWindowTextLengthW(hwnd) }.max(0) as usize;
                    let mut title = vec![0u16; (len + 1).min(32768)];
                    let copied = unsafe { GetWindowTextW(hwnd, &mut title) }.max(0) as usize;
                    WindowInfo {
                        pid,
                        app,
                        bundle_id: path.as_deref().and_then(crate::native::app_identity),
                        title: Some(String::from_utf16_lossy(&title[..copied])),
                    }
                })
                .collect()
        })
    }
}
