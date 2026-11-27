//! Application windows from `CGWindowListCopyWindowInfo`, including those on
//! other Spaces or minimized (a Meet tab often sits in a background window).
//!
//! Owners (pid, app name, bundle id) are always available; titles only when
//! the user has granted Screen Recording. BroScribe never requests that
//! permission here: without it every title is `None` and detection relies on
//! mic usage and app identity.

use std::collections::HashMap;

use broscribe_platform::{Result, WindowInfo, WindowInspector};
use objc2::rc::autoreleasepool;
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::{
    CGPreflightScreenCaptureAccess, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGNullWindowID, kCGWindowLayer, kCGWindowName, kCGWindowOwnerName, kCGWindowOwnerPID,
};

use crate::mic_usage::bundle_id_for_pid;

pub struct MacWindowInspector;

impl MacWindowInspector {
    /// Whether window titles are readable (Screen Recording granted). Never
    /// prompts.
    pub fn titles_available() -> bool {
        CGPreflightScreenCaptureAccess()
    }
}

type WindowDict = CFDictionary<CFString, CFType>;

impl WindowInspector for MacWindowInspector {
    fn windows(&self) -> Result<Vec<WindowInfo>> {
        let options = CGWindowListOption::OptionAll | CGWindowListOption::ExcludeDesktopElements;
        let Some(list) = CGWindowListCopyWindowInfo(options, kCGNullWindowID) else {
            return Ok(Vec::new());
        };
        // SAFETY: the window list is documented as an array of dictionaries
        // keyed by CFString.
        let list: CFRetained<CFArray<WindowDict>> = unsafe { CFRetained::cast_unchecked(list) };
        let titles = Self::titles_available();
        // SAFETY: the `kCGWindow*` keys are immutable CFString constants.
        let (k_pid, k_owner, k_name, k_layer) = unsafe {
            (
                kCGWindowOwnerPID,
                kCGWindowOwnerName,
                kCGWindowName,
                kCGWindowLayer,
            )
        };
        let mut bundles: HashMap<u32, Option<String>> = HashMap::new();
        let mut out = Vec::new();
        autoreleasepool(|_| {
            for dict in list.iter() {
                // Normal application windows only (menus, overlays and the
                // menu bar sit on other layers).
                if number(&dict, k_layer).unwrap_or(0) != 0 {
                    continue;
                }
                let Some(pid) = number(&dict, k_pid).and_then(|p| u32::try_from(p).ok()) else {
                    continue;
                };
                let app = string(&dict, k_owner).unwrap_or_default();
                let bundle_id = bundles
                    .entry(pid)
                    .or_insert_with(|| bundle_id_for_pid(pid))
                    .clone();
                let title = if titles {
                    string(&dict, k_name).filter(|t| !t.is_empty())
                } else {
                    None
                };
                out.push(WindowInfo {
                    pid,
                    app,
                    bundle_id,
                    title,
                });
            }
        });
        Ok(out)
    }
}

fn value<'a>(dict: &'a WindowDict, key: &CFString) -> Option<&'a CFType> {
    // SAFETY: the dictionary is immutable (a copy owned by us).
    unsafe { dict.get_unchecked(key) }
}

fn number(dict: &WindowDict, key: &CFString) -> Option<i64> {
    value(dict, key)?.downcast_ref::<CFNumber>()?.as_i64()
}

fn string(dict: &WindowDict, key: &CFString) -> Option<String> {
    Some(value(dict, key)?.downcast_ref::<CFString>()?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prints counts only (no app names or titles), per the privacy rules.
    #[test]
    #[ignore = "reads this Mac's window list"]
    fn real_window_list() {
        let windows = MacWindowInspector.windows().expect("window list");
        let with_bundle = windows.iter().filter(|w| w.bundle_id.is_some()).count();
        let with_title = windows.iter().filter(|w| w.title.is_some()).count();
        println!(
            "windows: {}, with bundle id: {with_bundle}, with title: {with_title}, titles available: {}",
            windows.len(),
            MacWindowInspector::titles_available()
        );
    }
}
