//! Preallocated SPSC capture blocks. The producer never allocates or locks;
//! sample timestamps and complete blocks become visible in one release store.
use broscribe_platform::Result;
use std::{
    sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};
const BLOCK: usize = 8192;
const SLOTS: usize = 64;
struct Block {
    samples: Box<[AtomicU32]>,
    len: AtomicUsize,
    host_100ns: AtomicU64,
}
pub(crate) struct TimedRing {
    blocks: Box<[Block]>,
    write: AtomicUsize,
    read: AtomicUsize,
    dropped: AtomicU64,
}
impl TimedRing {
    pub(crate) fn new() -> Self {
        Self {
            blocks: (0..SLOTS)
                .map(|_| Block {
                    samples: (0..BLOCK).map(|_| AtomicU32::new(0)).collect(),
                    len: AtomicUsize::new(0),
                    host_100ns: AtomicU64::new(0),
                })
                .collect(),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
        }
    }
    pub(crate) fn push(
        &self,
        frames: usize,
        host_100ns: u64,
        rate: u32,
        mut sample: impl FnMut(usize) -> f32,
    ) {
        let mut offset = 0;
        while offset < frames {
            let len = (frames - offset).min(BLOCK);
            let write = self.write.load(Ordering::Relaxed);
            let read = self.read.load(Ordering::Acquire);
            if write.wrapping_sub(read) >= SLOTS {
                self.dropped
                    .fetch_add((frames - offset) as u64, Ordering::Relaxed);
                return;
            }
            let block = &self.blocks[write % SLOTS];
            for i in 0..len {
                block.samples[i].store(sample(offset + i).to_bits(), Ordering::Relaxed);
            }
            block.len.store(len, Ordering::Relaxed);
            block.host_100ns.store(
                host_100ns.saturating_add((offset as u64 * 10_000_000) / u64::from(rate)),
                Ordering::Relaxed,
            );
            self.write.store(write.wrapping_add(1), Ordering::Release);
            offset += len;
        }
    }
    pub(crate) fn pop(&self, samples: &mut Vec<f32>) -> Option<u64> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.write.load(Ordering::Acquire) {
            return None;
        }
        let block = &self.blocks[read % SLOTS];
        let len = block.len.load(Ordering::Relaxed);
        samples.clear();
        samples.extend(
            block.samples[..len]
                .iter()
                .map(|v| f32::from_bits(v.load(Ordering::Relaxed))),
        );
        let time = block.host_100ns.load(Ordering::Relaxed);
        self.read.store(read.wrapping_add(1), Ordering::Release);
        Some(time)
    }
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
/// CPAL's WASAPI capture timestamps and WASAPI QPCPosition both use the
/// system QPC epoch. Calibrate once, outside the real-time callback.
pub(crate) struct HostClock {
    instant: Instant,
    qpc_100ns: u64,
}
impl HostClock {
    pub(crate) fn new() -> Result<Self> {
        use windows::Win32::System::Performance::{
            QueryPerformanceCounter, QueryPerformanceFrequency,
        };
        let mut frequency = 0;
        let mut count = 0;
        unsafe { QueryPerformanceFrequency(&mut frequency) }
            .map_err(|e| crate::error("querying host clock frequency", e))?;
        let before = Instant::now();
        unsafe { QueryPerformanceCounter(&mut count) }
            .map_err(|e| crate::error("querying host capture clock", e))?;
        let after = Instant::now();
        Ok(Self {
            instant: before + after.duration_since(before) / 2,
            qpc_100ns: (count as u128 * 10_000_000 / frequency as u128) as u64,
        })
    }
    pub(crate) fn instant(&self, qpc_100ns: u64) -> Option<Instant> {
        let delta = Duration::from_nanos(self.qpc_100ns.abs_diff(qpc_100ns).saturating_mul(100));
        if qpc_100ns >= self.qpc_100ns {
            self.instant.checked_add(delta)
        } else {
            self.instant.checked_sub(delta)
        }
    }
}
