//! Microphone capture through cpal (Core Audio).
//!
//! The real-time callback downmixes into a preallocated timestamped block
//! ring (no allocation, no locks). A worker drains it every 20 ms; each block
//! retains the Core Audio host time of its first sample.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use broscribe_platform::devices::collect_devices;
use broscribe_platform::{
    AudioDevice, CaptureSession, Error, MicCapture, MicChunk, MicSink, Result,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

/// Seconds of audio the ring holds if the worker stalls.
const RING_SECONDS: usize = 2;
const DRAIN_INTERVAL: Duration = Duration::from_millis(20);
/// Rates reported for devices that support a range.
const COMMON_RATES: [u32; 12] = [
    8_000, 11_025, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
];

fn audio_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Internal(format!("{context}: {e}"))
}

pub struct MacMicCapture;

impl MacMicCapture {
    fn find_device(host: &cpal::Host, id: Option<&str>) -> Result<cpal::Device> {
        match id {
            None => host
                .default_input_device()
                .ok_or_else(|| Error::NotFound("no audio input device".into())),
            Some(id) => host
                .input_devices()
                .map_err(|e| audio_err("listing audio input devices", e))?
                .find(|d| d.id().is_ok_and(|d_id| d_id.to_string() == id))
                .ok_or_else(|| Error::NotFound(format!("audio input device {id}"))),
        }
    }
}

fn sample_rates(device: &cpal::Device) -> Vec<u32> {
    let mut rates = Vec::new();
    if let Ok(configs) = device.supported_input_configs() {
        for range in configs {
            let (lo, hi) = (range.min_sample_rate(), range.max_sample_rate());
            rates.extend(
                COMMON_RATES
                    .iter()
                    .copied()
                    .filter(|r| (lo..=hi).contains(r)),
            );
            rates.push(lo);
            rates.push(hi);
        }
    }
    if let Ok(default) = device.default_input_config() {
        rates.push(default.sample_rate());
    }
    rates.retain(|&r| r > 0);
    rates.sort_unstable();
    rates.dedup();
    rates
}

impl MicCapture for MacMicCapture {
    /// Every input device through the shared collector: one whose identifier
    /// cannot be read is left out (logged by position), and only a listing
    /// where no device is readable fails. Names and rates are optional facts.
    fn devices(&self) -> Result<Vec<AudioDevice>> {
        let host = cpal::default_host();
        let default_id = host
            .default_input_device()
            .and_then(|d| d.id().ok())
            .map(|id| id.to_string());
        let devices = host
            .input_devices()
            .map_err(|e| audio_err("listing audio input devices", e))?;
        collect_devices("microphones", devices, |device| -> Result<_> {
            let id = device
                .id()
                .map_err(|e| audio_err("reading the input device identifier", e))?
                .to_string();
            let name = device
                .description()
                .map(|d| d.name().to_owned())
                .unwrap_or_else(|_| device.to_string());
            Ok(Some(AudioDevice {
                is_default: default_id.as_deref() == Some(id.as_str()),
                sample_rates: sample_rates(&device),
                id,
                name,
            }))
        })
    }

    fn start(&self, device: Option<&str>, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        let host = cpal::default_host();
        let follows_default = device.is_none();
        let device = Self::find_device(&host, device)?;
        let device_id = device.id().ok().map(|id| id.to_string());
        let device_name = device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|_| device.to_string());
        let config = device
            .default_input_config()
            .map_err(|e| audio_err("reading the input configuration", e))?;
        let sample_rate = config.sample_rate();
        let channels = usize::from(config.channels()).max(1);
        let ring = Arc::new(TimedRing::new(sample_rate as usize * RING_SECONDS));
        // Initialize the shared mach-to-Instant mapping outside the callback.
        let _ = crate::tap::host_instant(crate::tap::current_host_ticks());
        let failure = Arc::new(StreamFailure::default());

        let on_error = {
            let failure = failure.clone();
            move |e: cpal::Error| failure.record(|| e.to_string())
        };
        let stream_config = config.config();
        let stream = match config.sample_format() {
            SampleFormat::F32 => {
                build::<f32>(&device, stream_config, channels, ring.clone(), on_error)
            }
            SampleFormat::I16 => {
                build::<i16>(&device, stream_config, channels, ring.clone(), on_error)
            }
            SampleFormat::U16 => {
                build::<u16>(&device, stream_config, channels, ring.clone(), on_error)
            }
            other => {
                return Err(Error::Unsupported(format!(
                    "microphone sample format {other}"
                )));
            }
        }?;
        stream
            .play()
            .map_err(|e| audio_err("starting microphone capture", e))?;

        let stop = Arc::new(AtomicBool::new(false));
        let worker = spawn_worker(ring, sample_rate, sink, stop.clone(), failure.clone())?;
        Ok(Box::new(MacCaptureSession {
            stream: Some(stream),
            worker: Some(worker),
            stop,
            failure,
            sample_rate,
            device_id: follows_default.then_some(device_id).flatten(),
            device_name,
        }))
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    ring: Arc<TimedRing>,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let scale = 1.0 / channels as f32;
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                let frames = data.len() / channels;
                // cpal's CoreAudio backend reports mach absolute time in ns,
                // including its input-latency correction. Keep it with the block.
                let host_ns = info
                    .timestamp()
                    .capture
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64;
                ring.push(frames, host_ns, |i| {
                    let frame = &data[i * channels..(i + 1) * channels];
                    frame.iter().map(|&s| f32::from_sample_(s)).sum::<f32>() * scale
                });
            },
            on_error,
            None,
        )
        .map_err(|e| audio_err("opening the microphone", e))
}

fn spawn_worker(
    ring: Arc<TimedRing>,
    sample_rate: u32,
    mut sink: MicSink,
    stop: Arc<AtomicBool>,
    failure: Arc<StreamFailure>,
) -> Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("broscribe-mic".into())
        .spawn(move || {
            if let Err(error) = broscribe_platform::ProcessPriority::set_current_thread(
                &crate::priority::MacProcessPriority,
                broscribe_platform::PriorityClass::Capture,
            ) {
                tracing::warn!(%error, "microphone worker priority unavailable");
            }
            let mut reported_drops = 0;
            loop {
                // Read the flags before draining so the final drain after a
                // stop still delivers everything captured.
                let stopping = stop.load(Ordering::Acquire);
                let failing = failure.failed();
                let mut samples = Vec::new();
                while let Some(host_ns) = ring.pop(&mut samples) {
                    let ticks = unsafe { objc2_core_audio::AudioConvertNanosToHostTime(host_ns) };
                    let mut chunk = MicChunk::new(std::mem::take(&mut samples), sample_rate);
                    chunk.host_time = Some(crate::tap::host_instant(ticks));
                    sink(chunk);
                }
                let dropped = ring.dropped();
                if dropped > reported_drops {
                    tracing::warn!(
                        samples = dropped - reported_drops,
                        "microphone samples dropped: the capture worker fell behind"
                    );
                    reported_drops = dropped;
                }
                if stopping || failing {
                    // Dropping the sink tells its owner capture is over.
                    return;
                }
                std::thread::sleep(DRAIN_INTERVAL);
            }
        })
        .map_err(|e| Error::Io {
            context: "starting the microphone worker".into(),
            source: e,
        })
}

/// What the stream's error callback reported. The flag is authoritative: the
/// callback may run on the audio thread, so it only `try_lock`s to keep the
/// first message, and a message it could not store still reads as a failure
/// (capture never ends silently while the call goes on).
#[derive(Default)]
struct StreamFailure {
    failed: AtomicBool,
    message: Mutex<Option<String>>,
}

impl StreamFailure {
    const UNDESCRIBED: &'static str = "microphone stream error";

    /// Never waits; builds the message only when it can be stored.
    fn record(&self, message: impl FnOnce() -> String) {
        if let Ok(mut stored) = self.message.try_lock()
            && stored.is_none()
        {
            *stored = Some(message());
        }
        self.failed.store(true, Ordering::Release);
    }

    fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    fn message(&self) -> Option<String> {
        if !self.failed() {
            return None;
        }
        let stored = self.message.lock().unwrap_or_else(|e| e.into_inner());
        Some(stored.clone().unwrap_or_else(|| Self::UNDESCRIBED.into()))
    }
}

struct MacCaptureSession {
    stream: Option<cpal::Stream>,
    worker: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    failure: Arc<StreamFailure>,
    sample_rate: u32,
    device_id: Option<String>,
    device_name: String,
}

impl MacCaptureSession {
    fn finish(&mut self) -> Result<()> {
        let mut result = Ok(());
        if let Some(stream) = self.stream.take() {
            if let Err(e) = stream.pause() {
                tracing::debug!("pausing the microphone stream: {e}");
            }
            // Dropping the stream closes the device; no callback runs after.
            drop(stream);
        }
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            result = Err(Error::Internal("microphone worker panicked".into()));
        }
        result
    }
}

impl CaptureSession for MacCaptureSession {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn device_name(&self) -> Option<String> {
        Some(self.device_name.clone())
    }

    fn failure(&self) -> Option<String> {
        if let Some(failure) = self.failure.message() {
            return Some(failure);
        }
        if let Some(id) = &self.device_id {
            let current = cpal::default_host().default_input_device();
            if current
                .as_ref()
                .and_then(|d| d.id().ok())
                .map(|id| id.to_string())
                .as_ref()
                != Some(id)
            {
                return Some("default microphone changed".into());
            }
            if current
                .and_then(|d| d.default_input_config().ok())
                .is_some_and(|config| config.sample_rate() != self.sample_rate)
            {
                return Some("microphone sample rate changed".into());
            }
        }
        None
    }

    fn stop(mut self: Box<Self>) -> Result<()> {
        self.finish()
    }
}

impl Drop for MacCaptureSession {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// SPSC blocks: timestamps and samples are published together. The callback
/// writes only atomics into preallocated storage and drops whole blocks on
/// overflow, so a later block keeps its true host time rather than closing a gap.
struct TimedBlock {
    host_ns: AtomicU64,
    start: AtomicUsize,
    frames: AtomicUsize,
}

struct TimedRing {
    samples: Box<[AtomicU32]>,
    blocks: Box<[TimedBlock]>,
    sample_read: AtomicUsize,
    sample_write: AtomicUsize,
    block_read: AtomicUsize,
    block_write: AtomicUsize,
    dropped: AtomicU64,
}

impl TimedRing {
    fn new(capacity: usize) -> Self {
        Self {
            samples: (0..capacity.max(1)).map(|_| AtomicU32::new(0)).collect(),
            blocks: (0..1024)
                .map(|_| TimedBlock {
                    host_ns: AtomicU64::new(0),
                    start: AtomicUsize::new(0),
                    frames: AtomicUsize::new(0),
                })
                .collect(),
            sample_read: AtomicUsize::new(0),
            sample_write: AtomicUsize::new(0),
            block_read: AtomicUsize::new(0),
            block_write: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    fn push(&self, frames: usize, host_ns: u64, mut sample: impl FnMut(usize) -> f32) {
        if frames == 0 {
            return;
        }
        let sw = self.sample_write.load(Ordering::Relaxed);
        let bw = self.block_write.load(Ordering::Relaxed);
        if frames
            > self
                .samples
                .len()
                .saturating_sub(sw.wrapping_sub(self.sample_read.load(Ordering::Acquire)))
            || bw.wrapping_sub(self.block_read.load(Ordering::Acquire)) >= self.blocks.len()
        {
            self.dropped.fetch_add(frames as u64, Ordering::Relaxed);
            return;
        }
        for i in 0..frames {
            self.samples[sw.wrapping_add(i) % self.samples.len()]
                .store(sample(i).to_bits(), Ordering::Relaxed);
        }
        let block = &self.blocks[bw % self.blocks.len()];
        block.host_ns.store(host_ns, Ordering::Relaxed);
        block.start.store(sw, Ordering::Relaxed);
        block.frames.store(frames, Ordering::Relaxed);
        self.sample_write
            .store(sw.wrapping_add(frames), Ordering::Relaxed);
        self.block_write
            .store(bw.wrapping_add(1), Ordering::Release);
    }

    fn pop(&self, samples: &mut Vec<f32>) -> Option<u64> {
        let br = self.block_read.load(Ordering::Relaxed);
        if br == self.block_write.load(Ordering::Acquire) {
            return None;
        }
        let block = &self.blocks[br % self.blocks.len()];
        let host = block.host_ns.load(Ordering::Relaxed);
        let start = block.start.load(Ordering::Relaxed);
        let frames = block.frames.load(Ordering::Relaxed);
        samples.clear();
        samples.extend((0..frames).map(|i| {
            f32::from_bits(
                self.samples[start.wrapping_add(i) % self.samples.len()].load(Ordering::Relaxed),
            )
        }));
        self.sample_read
            .store(start.wrapping_add(frames), Ordering::Release);
        self.block_read.store(br.wrapping_add(1), Ordering::Release);
        Some(host)
    }

    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_ring_keeps_callback_boundaries_and_reports_whole_block_drops() {
        let ring = TimedRing::new(4);
        ring.push(3, 100, |i| i as f32);
        ring.push(2, 200, |_| 9.0);
        assert_eq!(ring.dropped(), 2);
        let mut out = Vec::new();
        assert_eq!(ring.pop(&mut out), Some(100));
        assert_eq!(out, [0.0, 1.0, 2.0]);
        ring.push(2, 300, |i| 3.0 + i as f32);
        assert_eq!(ring.pop(&mut out), Some(300));
        assert_eq!(out, [3.0, 4.0]);
        assert_eq!(ring.pop(&mut out), None);
    }

    fn session(failure: Arc<StreamFailure>) -> Box<dyn CaptureSession> {
        Box::new(MacCaptureSession {
            stream: None,
            worker: None,
            stop: Arc::new(AtomicBool::new(false)),
            failure,
            sample_rate: 48_000,
            // A pinned device: no default-device comparison in these tests.
            device_id: None,
            device_name: "synthetic".into(),
        })
    }

    #[test]
    fn stream_error_is_reported_even_when_its_message_was_lost() {
        let failure = Arc::new(StreamFailure::default());
        let capture = session(failure.clone());
        assert_eq!(capture.failure(), None);
        {
            // The realtime callback finds the message slot busy.
            let _busy = failure.message.lock().unwrap();
            failure.record(|| unreachable!("no message is built when it cannot be stored"));
        }
        assert_eq!(
            capture.failure().as_deref(),
            Some(StreamFailure::UNDESCRIBED)
        );
        capture.stop().unwrap();

        let failure = Arc::new(StreamFailure::default());
        let capture = session(failure.clone());
        failure.record(|| "device unplugged".into());
        failure.record(|| "a later error".into());
        assert_eq!(capture.failure().as_deref(), Some("device unplugged"));
    }

    #[test]
    fn failed_stream_delivers_captured_audio_then_ends_the_sink() {
        let ring = Arc::new(TimedRing::new(16));
        // SAFETY: converts the current host time to nanoseconds.
        let now_ns = unsafe {
            objc2_core_audio::AudioConvertHostTimeToNanos(crate::tap::current_host_ticks())
        };
        ring.push(4, now_ns, |i| i as f32);
        let failure = Arc::new(StreamFailure::default());
        failure.record(|| "synthetic failure".into());
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = spawn_worker(
            ring,
            16_000,
            Box::new(move |chunk| tx.send(chunk).unwrap()),
            Arc::new(AtomicBool::new(false)),
            failure,
        )
        .unwrap();
        let chunk = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(chunk.samples_mono_f32, [0.0, 1.0, 2.0, 3.0]);
        // The worker returns without a stop request; dropping the sink tells
        // its owner capture ended instead of leaving it waiting silently.
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
        worker.join().unwrap();
    }

    #[test]
    #[ignore = "enumerates this Mac's audio hardware"]
    fn devices_lists_without_error() {
        // May be empty on machines without an input device.
        let devices = MacMicCapture.devices().unwrap();
        for d in &devices {
            assert!(!d.id.is_empty());
        }
        assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
    }

    #[test]
    #[ignore = "needs a microphone and permission to use it"]
    fn captures_two_seconds() {
        let (tx, rx) = std::sync::mpsc::channel();
        let session = MacMicCapture
            .start(None, Box::new(move |c| tx.send(c).unwrap()))
            .unwrap();
        let rate = session.sample_rate();
        std::thread::sleep(Duration::from_secs(2));
        session.stop().unwrap();
        let chunks: Vec<MicChunk> = rx.iter().collect();
        let total: usize = chunks.iter().map(|c| c.samples_mono_f32.len()).sum();
        assert!(chunks.iter().all(|c| c.sample_rate == rate));
        let expected = rate as usize * 2;
        assert!(
            total > expected * 8 / 10 && total < expected * 12 / 10,
            "{total} samples at {rate} Hz"
        );
        let peak = chunks.iter().map(|c| c.level_peak).fold(0.0f32, f32::max);
        eprintln!("captured {total} samples at {rate} Hz, peak {peak}");
    }
}
