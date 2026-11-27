//! Machine facts from `sysctl`, `statfs` and Foundation volume capacity.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use broscribe_platform::{Error, Result, SystemInfo};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_foundation::{NSNumber, NSURL, NSURLVolumeAvailableCapacityForImportantUsageKey};

pub struct MacSystemInfo;

/// How long one important-usage reading is reused. The query asks the system
/// how much it could purge, too slow to repeat on every disk check of a
/// recording (once a second).
const IMPORTANT_USAGE_TTL: Duration = Duration::from_secs(30);

/// One important-usage reading of a volume and the `statfs` figure taken
/// with it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ImportantUsage {
    /// `None` when the system reported no figure (then `statfs` is used).
    bytes: Option<u64>,
    statfs_bytes: u64,
    at: Instant,
}

/// Latest reading per mount point.
static IMPORTANT_USAGE: LazyLock<Mutex<HashMap<Vec<u8>, ImportantUsage>>> =
    LazyLock::new(Default::default);

/// Bytes available for a user-requested task such as a recording. On APFS,
/// `statfs` leaves out purgeable space (caches, local snapshots) that macOS
/// frees on demand, which stopped recordings early on nearly full disks; the
/// important-usage capacity counts it. Between readings, every byte `statfs`
/// lost since is taken off that figure (space freed meanwhile is not added);
/// without a figure from the system, `statfs` alone is reported. Nothing is
/// estimated beyond what the system reported.
fn available_bytes(statfs_now: u64, reading: &ImportantUsage) -> u64 {
    match reading.bytes {
        Some(bytes) => bytes.saturating_sub(reading.statfs_bytes.saturating_sub(statfs_now)),
        None => statfs_now,
    }
}

/// `NSURLVolumeAvailableCapacityForImportantUsageKey` of the volume holding
/// `path`; `None` when the system does not report a positive figure.
fn important_usage_bytes(path: &Path) -> Option<u64> {
    autoreleasepool(|_| {
        let url = NSURL::from_file_path(path)?;
        let mut value: Option<Retained<AnyObject>> = None;
        // SAFETY: the documented key, whose value is an NSNumber or nil.
        if let Err(error) = unsafe {
            url.getResourceValue_forKey_error(
                &mut value,
                NSURLVolumeAvailableCapacityForImportantUsageKey,
            )
        } {
            tracing::debug!("important-usage capacity unavailable: {error}");
            return None;
        }
        let bytes = value?.downcast::<NSNumber>().ok()?.as_i64();
        u64::try_from(bytes).ok().filter(|&bytes| bytes > 0)
    })
}

/// Reads a sysctl value into `buf`; returns the bytes written.
fn sysctl_raw(name: &CStr, buf: &mut [u8]) -> Option<usize> {
    let mut len = buf.len();
    // SAFETY: `name` is NUL-terminated, `buf`/`len` describe a writable
    // buffer, and no new value is set.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(len)
}

fn sysctl_string(name: &CStr) -> Option<String> {
    let mut buf = [0u8; 256];
    let len = sysctl_raw(name, &mut buf)?;
    let bytes = &buf[..len];
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let s = String::from_utf8_lossy(&bytes[..end]).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

fn sysctl_u64(name: &CStr) -> Option<u64> {
    let mut buf = [0u8; 8];
    match sysctl_raw(name, &mut buf)? {
        8 => Some(u64::from_ne_bytes(buf)),
        4 => Some(u64::from(u32::from_ne_bytes([
            buf[0], buf[1], buf[2], buf[3],
        ]))),
        _ => None,
    }
}

impl SystemInfo for MacSystemInfo {
    fn application_installed(&self, identities: &[String]) -> Option<bool> {
        Some(autoreleasepool(|_| {
            let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
            identities.iter().any(|id| {
                !id.ends_with('*')
                    && workspace
                        .URLForApplicationWithBundleIdentifier(
                            &objc2_foundation::NSString::from_str(id),
                        )
                        .is_some()
            })
        }))
    }

    fn chip(&self) -> String {
        sysctl_string(c"machdep.cpu.brand_string").unwrap_or_default()
    }

    fn memory_bytes(&self) -> u64 {
        sysctl_u64(c"hw.memsize").unwrap_or(0)
    }

    fn free_disk_bytes(&self, path: &Path) -> Result<u64> {
        let existing = path
            .ancestors()
            .find(|p| !p.as_os_str().is_empty() && p.exists())
            .unwrap_or(Path::new("/"));
        let c_path = CString::new(existing.as_os_str().as_bytes())
            .map_err(|_| Error::InvalidInput(format!("path with NUL: {}", path.display())))?;
        // SAFETY: an all-zero `statfs` is a valid out-parameter.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is NUL-terminated and `st` is writable.
        if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
            return Err(Error::Io {
                context: format!("statfs {}", existing.display()),
                source: std::io::Error::last_os_error(),
            });
        }
        let statfs_bytes = st.f_bavail.saturating_mul(u64::from(st.f_bsize));
        let volume: Vec<u8> = st
            .f_mntonname
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        let now = Instant::now();
        let cached = IMPORTANT_USAGE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&volume)
            .copied()
            .filter(|reading| now.duration_since(reading.at) < IMPORTANT_USAGE_TTL);
        let reading = cached.unwrap_or_else(|| {
            let reading = ImportantUsage {
                bytes: important_usage_bytes(existing),
                statfs_bytes,
                at: now,
            };
            IMPORTANT_USAGE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(volume, reading);
            reading
        });
        Ok(available_bytes(statfs_bytes, &reading))
    }

    fn cpu_cores(&self) -> usize {
        sysctl_u64(c"hw.logicalcpu")
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n > 0)
            .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plausible_values() {
        let info = MacSystemInfo;
        let chip = info.chip();
        assert!(!chip.is_empty());
        let mem = info.memory_bytes();
        assert!((1 << 30..=1 << 44).contains(&mem), "{mem}");
        let cores = info.cpu_cores();
        assert!((1..=1024).contains(&cores), "{cores}");
        let dir = tempfile::tempdir().unwrap();
        let free = info.free_disk_bytes(dir.path()).unwrap();
        assert!(free > 0);
        // A path that does not exist yet uses its nearest existing ancestor.
        let later = info
            .free_disk_bytes(&dir.path().join("not/yet/created"))
            .unwrap();
        assert!(later > 0);
    }

    #[test]
    fn important_usage_counts_purgeable_space_and_never_grows_between_readings() {
        const GIB: u64 = 1 << 30;
        let reading = ImportantUsage {
            bytes: Some(40 * GIB),
            statfs_bytes: 10 * GIB,
            at: Instant::now(),
        };
        // Purgeable space the system reported counts as available.
        assert_eq!(available_bytes(10 * GIB, &reading), 40 * GIB);
        // Bytes written since the reading are taken off it...
        assert_eq!(available_bytes(7 * GIB, &reading), 37 * GIB);
        // ...space freed meanwhile is not added until the next reading...
        assert_eq!(available_bytes(12 * GIB, &reading), 40 * GIB);
        // ...and consumption past the figure reports none, not a wrap.
        let full = ImportantUsage {
            bytes: Some(GIB),
            ..reading
        };
        assert_eq!(available_bytes(8 * GIB, &full), 0);
        // Without a system figure, statfs is reported unchanged.
        let unknown = ImportantUsage {
            bytes: None,
            ..reading
        };
        assert_eq!(available_bytes(3 * GIB, &unknown), 3 * GIB);
    }

    #[test]
    fn important_usage_reading_is_positive_or_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(important_usage_bytes(dir.path()).is_none_or(|bytes| bytes > 0));
    }
}
