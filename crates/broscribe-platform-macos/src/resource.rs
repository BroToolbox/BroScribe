//! Process resource usage from `getrusage(RUSAGE_SELF)`.

use broscribe_platform::{Error, ProcessUsage, ResourceUsage, Result};

pub struct MacResourceUsage;

fn seconds(t: libc::timeval) -> f64 {
    t.tv_sec as f64 + f64::from(t.tv_usec) / 1e6
}

impl ResourceUsage for MacResourceUsage {
    fn self_usage(&self) -> Result<ProcessUsage> {
        // SAFETY: an all-zero `rusage` is a valid value of this plain C struct.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `usage` is a valid, writable `rusage`.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return Err(Error::io(
                "reading process resource usage",
                std::io::Error::last_os_error(),
            ));
        }
        Ok(ProcessUsage {
            // macOS reports `ru_maxrss` in bytes (Linux uses kilobytes).
            peak_rss_bytes: u64::try_from(usage.ru_maxrss).unwrap_or(0),
            user_cpu_s: seconds(usage.ru_utime),
            system_cpu_s: seconds(usage.ru_stime),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_rss_is_in_bytes() {
        let usage = MacResourceUsage.self_usage().unwrap();
        // A running test binary needs well over 1 MB: kilobytes would read
        // as about a thousand times less.
        assert!(usage.peak_rss_bytes > 1_000_000, "{usage:?}");
        assert!(usage.user_cpu_s >= 0.0 && usage.system_cpu_s >= 0.0);
    }
}
