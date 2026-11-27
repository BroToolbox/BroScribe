//! Audio capture and microphone usage.
//!
//! `MicCapture` is used by the app (M3) for the voice enrollment passage and
//! the setup "Try it" step; `SystemAudioCapture` and `MicUsageMonitor` arrive
//! with the recorder (M4).

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::Result;

/// Captures the system output (other participants) as one mono track.
///
/// Like [`MicCapture`], chunks reach `sink` on a capture worker thread, never
/// on the real-time audio thread. A session ends on its own when the output
/// route changes (device switch, sample-rate change, tap lost): the sink is
/// dropped and [`CaptureSession::failure`] says why, so the recorder starts a
/// new segment with a fresh session.
pub trait SystemAudioCapture: Send + Sync {
    /// Processes whose audio is left out of the capture (BroScribe's own
    /// helpers, audio-check playback). Running sessions are updated in place
    /// where the platform allows it; otherwise the list applies from the next
    /// [`start`](Self::start).
    fn set_excluded_pids(&self, pids: &[u32]) -> Result<()>;

    /// Starts capturing everything the system plays except the excluded
    /// processes. Chunks reach `sink` until the session stops or fails.
    fn start(&self, sink: MicSink) -> Result<Box<dyn CaptureSession>>;
}

/// An audio input device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioDevice {
    /// Stable identifier to pass back to [`MicCapture::start`].
    pub id: String,
    /// Human-readable name.
    pub name: String,
    pub is_default: bool,
    /// Supported capture rates (Hz), ascending.
    pub sample_rates: Vec<u32>,
}

/// A block of captured audio, downmixed to mono at the device rate, with its levels.
#[derive(Clone, Debug, PartialEq)]
pub struct MicChunk {
    pub samples_mono_f32: Vec<f32>,
    /// The device's capture rate; resampling to 16 kHz happens in the pipeline.
    pub sample_rate: u32,
    /// Host-clock instant of the first captured sample (before buffering).
    /// `None` is supported for simple fakes; the recorder then uses arrival time.
    pub host_time: Option<std::time::Instant>,
    /// Largest `|x|` in the chunk (linear, 0..=1 for in-range audio).
    pub level_peak: f32,
    /// Root mean square of the chunk (linear).
    pub level_rms: f32,
}

impl MicChunk {
    /// Builds a chunk and computes its levels.
    pub fn new(samples_mono_f32: Vec<f32>, sample_rate: u32) -> Self {
        let (level_peak, level_rms) = levels(&samples_mono_f32);
        Self {
            samples_mono_f32,
            sample_rate,
            host_time: None,
            level_peak,
            level_rms,
        }
    }
}

/// Peak `|x|` and RMS of `samples`; `(0, 0)` when empty.
pub fn levels(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = 0.0f32;
    let mut sum_sq = 0.0f64;
    for &s in samples {
        peak = peak.max(s.abs());
        sum_sq += f64::from(s) * f64::from(s);
    }
    (peak, (sum_sq / samples.len() as f64).sqrt() as f32)
}

/// Receives captured chunks on a capture worker thread (never on the
/// real-time audio thread).
pub type MicSink = Box<dyn FnMut(MicChunk) + Send>;

/// Microphone capture.
pub trait MicCapture: Send + Sync {
    /// Input devices; may be empty.
    fn devices(&self) -> Result<Vec<AudioDevice>>;

    /// Starts capturing from `device` (an [`AudioDevice::id`]) or the default
    /// input. Chunks reach `sink` on a worker thread until the session is
    /// stopped. If the device fails, capture ends and `sink` is dropped, so a
    /// channel sender inside it disconnects; [`CaptureSession::failure`] then
    /// says why.
    fn start(&self, device: Option<&str>, sink: MicSink) -> Result<Box<dyn CaptureSession>>;
}

/// A running capture. Dropping it without `stop` also stops capture.
pub trait CaptureSession: Send {
    /// The device capture rate (Hz) of every chunk.
    fn sample_rate(&self) -> u32;
    /// The capturing device's name, when the implementation knows it
    /// (recorded in the journal's segment list).
    fn device_name(&self) -> Option<String> {
        None
    }
    /// Why capture ended on its own (device lost, stream error), if it did.
    fn failure(&self) -> Option<String>;
    /// Stops capture; returns once the sink has received its last chunk.
    fn stop(self: Box<Self>) -> Result<()>;
}

/// A process currently capturing from an input device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MicUser {
    pub pid: u32,
    /// The process's own bundle id (helpers have their own, e.g.
    /// `com.google.Chrome.helper`), when it has one.
    pub bundle_id: Option<String>,
    /// The process the system holds responsible for it (the app that launched
    /// a helper or XPC service), when it differs from `pid`.
    pub responsible_pid: Option<u32>,
    /// Bundle id of `responsible_pid`, when known.
    pub responsible_bundle_id: Option<String>,
}

/// Which processes are using a microphone (polled by the daemon's detector).
pub trait MicUsageMonitor: Send + Sync {
    fn mic_users(&self) -> Result<Vec<MicUser>>;
}

/// Single-producer single-consumer ring of f32 samples, preallocated.
///
/// The producer (a real-time audio callback) neither allocates nor locks:
/// samples are stored as `f32` bits in atomics and published with one
/// release store per block. When the consumer falls behind, new samples are
/// dropped and counted.
pub struct SampleRing {
    buf: Box<[AtomicU32]>,
    /// Total samples written (wrapping).
    write: AtomicUsize,
    /// Total samples read (wrapping).
    read: AtomicUsize,
    dropped: AtomicU64,
}

impl SampleRing {
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            buf: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Producer side: writes `n` samples produced by `sample(i)`; returns how
    /// many were stored (the rest were dropped). Call from one thread only.
    pub fn push_with(&self, n: usize, mut sample: impl FnMut(usize) -> f32) -> usize {
        let cap = self.buf.len();
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let free = cap - write.wrapping_sub(read).min(cap);
        let stored = n.min(free);
        for i in 0..stored {
            let slot = write.wrapping_add(i) % cap;
            self.buf[slot].store(sample(i).to_bits(), Ordering::Relaxed);
        }
        self.write
            .store(write.wrapping_add(stored), Ordering::Release);
        if stored < n {
            self.dropped
                .fetch_add((n - stored) as u64, Ordering::Relaxed);
        }
        stored
    }

    /// Consumer side: appends every available sample to `out`; returns the
    /// count. Call from one thread only.
    pub fn drain_into(&self, out: &mut Vec<f32>) -> usize {
        let cap = self.buf.len();
        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        let n = write.wrapping_sub(read).min(cap);
        out.reserve(n);
        for i in 0..n {
            let slot = read.wrapping_add(i) % cap;
            out.push(f32::from_bits(self.buf[slot].load(Ordering::Relaxed)));
        }
        self.read.store(read.wrapping_add(n), Ordering::Release);
        n
    }

    /// Samples dropped because the consumer fell behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_of_known_signals() {
        assert_eq!(levels(&[]), (0.0, 0.0));
        let (peak, rms) = levels(&[0.5, -0.5, 0.5, -0.5]);
        assert!((peak - 0.5).abs() < 1e-6);
        assert!((rms - 0.5).abs() < 1e-6);
        let (peak, rms) = levels(&[0.0, -1.0]);
        assert!((peak - 1.0).abs() < 1e-6);
        assert!((rms - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn ring_round_trips_and_counts_drops() {
        let ring = SampleRing::with_capacity(4);
        assert_eq!(ring.push_with(3, |i| i as f32), 3);
        let mut out = Vec::new();
        assert_eq!(ring.drain_into(&mut out), 3);
        assert_eq!(out, vec![0.0, 1.0, 2.0]);

        // Wraps around the end of the buffer.
        assert_eq!(ring.push_with(4, |i| 10.0 + i as f32), 4);
        assert_eq!(ring.push_with(2, |_| 99.0), 0);
        assert_eq!(ring.dropped(), 2);
        out.clear();
        assert_eq!(ring.drain_into(&mut out), 4);
        assert_eq!(out, vec![10.0, 11.0, 12.0, 13.0]);
        assert_eq!(ring.drain_into(&mut out), 0);
    }

    #[test]
    fn ring_across_threads() {
        let ring = std::sync::Arc::new(SampleRing::with_capacity(1024));
        let producer = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                let mut next = 0usize;
                while next < 10_000 {
                    let base = next;
                    next += ring.push_with(64.min(10_000 - base), |i| (base + i) as f32);
                    std::thread::yield_now();
                }
            })
        };
        let mut out = Vec::new();
        while out.len() < 10_000 {
            ring.drain_into(&mut out);
            std::thread::yield_now();
        }
        producer.join().unwrap();
        // Samples a full ring refused were offered again, so the stream is contiguous.
        assert!(out.iter().enumerate().all(|(i, &s)| s == i as f32));
    }
}
