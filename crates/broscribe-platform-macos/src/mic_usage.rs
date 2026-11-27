//! Which processes hold a microphone: Core Audio process objects
//! (`kAudioHardwarePropertyProcessObjectList`, macOS 14.2+), filtered by
//! `kAudioProcessPropertyIsRunningInput`, with each helper mapped to the
//! process responsible for it.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::LazyLock;

use broscribe_platform::{Error, MicUsageMonitor, MicUser, Result};
use objc2::rc::autoreleasepool;
use objc2_app_kit::NSRunningApplication;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertySelector,
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningInput, kAudioProcessPropertyPID,
};
use objc2_core_foundation::{CFRetained, CFString};

pub struct MacMicUsageMonitor;

impl MicUsageMonitor for MacMicUsageMonitor {
    fn mic_users(&self) -> Result<Vec<MicUser>> {
        let system = kAudioObjectSystemObject as AudioObjectID;
        let objects: Vec<AudioObjectID> =
            read_array(system, kAudioHardwarePropertyProcessObjectList).map_err(|status| {
                Error::Unsupported(format!(
                    "Core Audio process list (needs macOS 14.2+): OSStatus {status}"
                ))
            })?;
        let mut users = Vec::new();
        autoreleasepool(|_| {
            for object in objects {
                // A process that exits between the list and this read just drops out.
                let Ok(running) = read_value::<u32>(object, kAudioProcessPropertyIsRunningInput)
                else {
                    continue;
                };
                if running == 0 {
                    continue;
                }
                let Ok(pid) = read_value::<libc::pid_t>(object, kAudioProcessPropertyPID) else {
                    continue;
                };
                let Ok(pid) = u32::try_from(pid) else {
                    continue;
                };
                let bundle_id = read_cfstring(object, kAudioProcessPropertyBundleID)
                    .or_else(|| bundle_id_for_pid(pid));
                let responsible_pid = responsible_pid(pid).filter(|&r| r != pid);
                let responsible_bundle_id = responsible_pid.and_then(bundle_id_for_pid);
                users.push(MicUser {
                    pid,
                    bundle_id,
                    responsible_pid,
                    responsible_bundle_id,
                });
            }
        });
        Ok(users)
    }
}

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Reads a fixed-size property; `Err(OSStatus)` on failure.
fn read_value<T: Copy + Default>(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> std::result::Result<T, i32> {
    let addr = address(selector);
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: `addr`, `size` and `value` are valid for the call; `size` is the
    // byte size of `value`, so Core Audio writes at most that much.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    if status != 0 {
        return Err(status);
    }
    if size as usize != std::mem::size_of::<T>() {
        return Err(-1);
    }
    Ok(value)
}

/// Reads an array property of `T`s.
fn read_array<T: Copy + Default>(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> std::result::Result<Vec<T>, i32> {
    let addr = address(selector);
    let mut size = 0u32;
    // SAFETY: `addr` and `size` are valid; no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        return Err(status);
    }
    let elem = std::mem::size_of::<T>();
    let mut out = vec![T::default(); size as usize / elem];
    if out.is_empty() {
        return Ok(out);
    }
    let mut size = (out.len() * elem) as u32;
    // SAFETY: `out` holds `size` writable bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(out.as_mut_ptr().cast::<c_void>()).ok_or(-1)?,
        )
    };
    if status != 0 {
        return Err(status);
    }
    // The list may have shrunk between the two calls.
    out.truncate(size as usize / elem);
    Ok(out)
}

/// Reads a `CFStringRef` property (returned retained, per Core Audio's rules).
fn read_cfstring(object: AudioObjectID, selector: AudioObjectPropertySelector) -> Option<String> {
    let raw = read_value::<usize>(object, selector).ok()?;
    let ptr = NonNull::new(raw as *mut CFString)?;
    // SAFETY: Core Audio hands out a +1 `CFStringRef` for string properties.
    let s = unsafe { CFRetained::from_raw(ptr) };
    let s = (*s).to_string();
    (!s.is_empty()).then_some(s)
}

/// The bundle id of a running process registered with Launch Services.
pub(crate) fn bundle_id_for_pid(pid: u32) -> Option<String> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    let id = (*app.bundleIdentifier()?).to_string();
    (!id.is_empty()).then_some(id)
}

type ResponsibleFn = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;

/// `responsibility_get_pid_responsible_for_pid` from libSystem, looked up at
/// run time (not in the public headers); `None` where it does not exist.
static RESPONSIBLE_FN: LazyLock<Option<ResponsibleFn>> = LazyLock::new(|| {
    // SAFETY: `dlsym` with `RTLD_DEFAULT` and a NUL-terminated name.
    let sym = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"responsibility_get_pid_responsible_for_pid".as_ptr(),
        )
    };
    if sym.is_null() {
        return None;
    }
    // SAFETY: the symbol has this C signature (pid in, pid out).
    Some(unsafe { std::mem::transmute::<*mut c_void, ResponsibleFn>(sym) })
});

/// The process responsible for `pid` (the app behind a helper or XPC
/// service); falls back to the parent process, ignoring `launchd`.
pub(crate) fn responsible_pid(pid: u32) -> Option<u32> {
    let raw = libc::pid_t::try_from(pid).ok()?;
    if let Some(f) = *RESPONSIBLE_FN {
        // SAFETY: plain C call on an integer.
        let r = unsafe { f(raw) };
        if r > 1 {
            return u32::try_from(r).ok();
        }
    }
    parent_pid(raw).filter(|&p| p > 1)
}

fn parent_pid(pid: libc::pid_t) -> Option<u32> {
    // SAFETY: an all-zero `proc_bsdinfo` is a valid out-parameter.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is writable and `size` is its size.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast::<c_void>(),
            size,
        )
    };
    (n == size).then_some(info.pbi_ppid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsible_and_parent_lookups_run() {
        // The responsible answer depends on how tests are launched; only the
        // lookups themselves are checked.
        let _ = responsible_pid(std::process::id());
        let own = libc::pid_t::try_from(std::process::id()).unwrap();
        assert!(parent_pid(own).is_some());
    }

    /// Prints counts only (no process names), per the privacy rules.
    #[test]
    #[ignore = "enumerates this Mac's audio processes"]
    fn real_mic_users_enumerate() {
        let users = MacMicUsageMonitor.mic_users().expect("process list");
        let with_bundle = users.iter().filter(|u| u.bundle_id.is_some()).count();
        let with_parent = users.iter().filter(|u| u.responsible_pid.is_some()).count();
        println!(
            "mic users: {}, with bundle id: {with_bundle}, with responsible app: {with_parent}",
            users.len()
        );
    }
}
