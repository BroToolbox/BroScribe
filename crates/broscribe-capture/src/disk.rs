//! Disk guard (plan M4 §13.3): recording needs room for the raw journal.

/// Bytes per second of journal for the given track rates (raw f32 mono).
pub fn journal_bytes_per_second(rates: &[u32]) -> u64 {
    rates.iter().map(|r| u64::from(*r) * 4).sum()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiskGuard {
    /// Stop below this much free space.
    pub min_free_bytes: u64,
    /// Stop when less than this much journal time fits.
    pub min_headroom_s: f64,
    /// Warn below this much free space, before the stop.
    pub warn_free_bytes: u64,
    /// Warn when less than this much journal time fits.
    pub warn_headroom_s: f64,
}

impl Default for DiskGuard {
    /// Stop at 500 MB or 15 minutes of journal headroom; warn from 1 GB or
    /// 30 minutes.
    fn default() -> Self {
        Self {
            min_free_bytes: broscribe_ipc::recording_words::MIN_FREE_BYTES,
            min_headroom_s: broscribe_ipc::recording_words::MIN_HEADROOM_SECONDS as f64,
            warn_free_bytes: 1024 * 1024 * 1024,
            warn_headroom_s: 30.0 * 60.0,
        }
    }
}

use broscribe_ipc::DiskLevel;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiskStatus {
    pub level: DiskLevel,
    pub free_bytes: u64,
    /// Recording time the free space still holds (infinite when nothing is
    /// being written).
    pub headroom_s: f64,
}

impl DiskStatus {
    pub fn is_low(&self) -> bool {
        self.level == DiskLevel::Low
    }
}

impl DiskGuard {
    /// Checks `free_bytes` against a journal growing at `bytes_per_second`.
    pub fn check(&self, free_bytes: u64, bytes_per_second: u64) -> DiskStatus {
        let headroom_s = if bytes_per_second == 0 {
            f64::INFINITY
        } else {
            free_bytes as f64 / bytes_per_second as f64
        };
        let level = if free_bytes < self.min_free_bytes || headroom_s < self.min_headroom_s {
            DiskLevel::Low
        } else if free_bytes < self.warn_free_bytes || headroom_s < self.warn_headroom_s {
            DiskLevel::Warning
        } else {
            DiskLevel::Ok
        };
        DiskStatus {
            level,
            free_bytes,
            headroom_s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_on_either_limit() {
        let guard = DiskGuard::default();
        // Two 48 kHz tracks: 384 000 B/s, so 15 min needs ~346 MB.
        let rate = journal_bytes_per_second(&[48_000, 48_000]);
        assert_eq!(rate, 384_000);
        let gb = 1024 * 1024 * 1024;
        assert_eq!(guard.check(2 * gb, rate).level, DiskLevel::Ok);
        assert!(
            guard.check(400 * 1024 * 1024, rate).is_low(),
            "below 500 MB"
        );
        // Plenty of bytes but a huge journal rate: headroom decides.
        let huge = journal_bytes_per_second(&[192_000; 8]);
        let status = guard.check(gb, huge);
        assert!(status.is_low());
        assert!(status.headroom_s < 900.0);
        assert_eq!(guard.check(gb, 0).headroom_s, f64::INFINITY);
    }

    #[test]
    fn warns_before_the_protective_stop() {
        let guard = DiskGuard::default();
        let rate = journal_bytes_per_second(&[48_000, 48_000]);
        let mb = 1024 * 1024;
        // 700 MB: above the stop, below the 1 GB warning.
        assert_eq!(guard.check(700 * mb, rate).level, DiskLevel::Warning);
        // 1.5 GB holds over an hour at this rate: no warning.
        assert_eq!(guard.check(1536 * mb, rate).level, DiskLevel::Ok);
        // Enough bytes but only 20 minutes of headroom at a high rate.
        let fast = 2 * 1024 * mb / (20 * 60);
        assert_eq!(guard.check(2 * 1024 * mb, fast).level, DiskLevel::Warning);
    }
}
