//! Synthetic setup sound, captured only when no meeting tap is running.
use broscribe_core::{Error, Result};
use broscribe_dsp::{
    AudioCheckReport, AudioCheckThresholds, TestSignalSpec, analyze_audio_check, test_signal,
};
use broscribe_platform::{AudioPlayer, MicCapture, MicChunk, PlayerState};
use std::{
    io::Write,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// A synchronous device operation, invoked on a blocking thread. Fakes can
/// inject microphone PCM and a player without opening real devices.
pub trait AudioCheck: Send + Sync {
    /// `levels(heard, played)` receives, while the check runs, the mic
    /// samples and test-sound samples since its previous call; it never
    /// blocks. `mic_device` is the microphone recording would open (a
    /// connected pinned one); `None`: the default input.
    fn run(
        &self,
        seconds: f64,
        cache_dir: &Path,
        mic_device: Option<&str>,
        levels: &mut dyn FnMut(&[f32], &[f32]),
    ) -> Result<AudioCheckReport>;
}

pub struct DeviceAudioCheck {
    pub mic: Arc<dyn MicCapture>,
    pub devices: Arc<dyn broscribe_platform::DeviceInfo>,
    /// The !Send player is created and used entirely on the checking thread.
    pub player: Arc<dyn Fn() -> Box<dyn AudioPlayer> + Send + Sync>,
}
impl crate::Daemon {
    pub fn set_audio_check(&self, check: Arc<dyn AudioCheck>) {
        *crate::daemon::lock(&self.shared.audio_check) = check;
    }
}

pub(crate) fn run(
    shared: &crate::daemon::Shared,
    seconds: f64,
) -> Result<serde_json::Value, broscribe_ipc::IpcError> {
    use crate::daemon::lock;
    use broscribe_ipc::IpcError;
    if !seconds.is_finite() || !(0.1..=30.0).contains(&seconds) {
        return Err(IpcError::new(
            "invalid_input",
            "audio check duration must be 0.1–30 seconds",
        ));
    }
    // Synchronize admission with record.start; release before using devices.
    let session = lock(&shared.session);
    let _slot = shared.admit_capture(&session, "audio check")?;
    drop(session);
    let config =
        broscribe_core::config::Config::load(&shared.cfg.data_dir).unwrap_or_else(|error| {
            tracing::warn!(
                code = error.code(),
                "audio check: config unreadable; using defaults"
            );
            broscribe_core::config::Config::default()
        });
    // The microphone a recording would open (RT06): the check measures the
    // pair recording uses, and classification describes the same one.
    let mic = lock(&shared.sample_mic).clone();
    let mic_device = crate::setup::capture_mic(mic.as_ref(), &config);
    let check = lock(&shared.audio_check).clone();
    let mut meter = crate::sample::LevelMeter::new(shared, None, true);
    let report = check
        .run(
            seconds,
            &shared.cfg.cache_dir,
            mic_device.as_deref(),
            &mut |heard, played| {
                meter.others(played);
                meter.me(heard);
                meter.tick(Instant::now());
            },
        )
        .map_err(IpcError::from)?;
    // No level event once the check ended.
    drop(meter);
    let mut value =
        serde_json::to_value(report).map_err(|e| IpcError::new("internal", e.to_string()))?;
    value["setup"] = detected_setup(shared, &config, mic_device.as_deref()).into();
    value["device_id"] = mic_device.into();
    Ok(value)
}

/// The setup a remote meeting on the current devices (the microphone
/// recording would open) would be recorded with (design §5); advisory,
/// like detection at record start.
fn detected_setup(
    shared: &crate::daemon::Shared,
    config: &broscribe_core::config::Config,
    mic_device: Option<&str>,
) -> Option<String> {
    let devices = crate::daemon::lock(&shared.devices).clone();
    // Only an explicit setup can be refused, and none is given.
    crate::setup::resolve(
        devices.as_ref(),
        &shared.cfg.data_dir,
        config,
        None,
        true,
        mic_device,
    )
    .ok()
    .map(|resolved| resolved.setup)
}

impl AudioCheck for DeviceAudioCheck {
    fn run(
        &self,
        seconds: f64,
        cache_dir: &Path,
        mic_device: Option<&str>,
        levels: &mut dyn FnMut(&[f32], &[f32]),
    ) -> Result<AudioCheckReport> {
        const RATE: u32 = 16_000;
        let mut spec = TestSignalSpec::new(RATE);
        spec.lead_silence_s = seconds * 0.1;
        spec.chirp_s = seconds * 0.45;
        spec.noise_s = seconds * 0.3;
        spec.tail_silence_s = seconds * 0.15;
        let played = test_signal(&spec);
        std::fs::create_dir_all(cache_dir).map_err(|e| Error::io_at(cache_dir, e))?;
        let mut wav = tempfile::Builder::new()
            .prefix("audio-check-")
            .rand_bytes(6)
            .suffix(".wav")
            .tempfile_in(cache_dir)
            .map_err(|e| Error::io_at(cache_dir, e))?;
        write_wav(wav.as_file_mut(), &played, RATE)?;
        let (send, receive) = mpsc::sync_channel(256);
        let overflow = Arc::new(AtomicBool::new(false));
        let dropped = overflow.clone();
        let session = self
            .mic
            .start(
                mic_device,
                Box::new(move |chunk| {
                    if send.try_send(chunk).is_err() {
                        dropped.store(true, Ordering::Relaxed);
                    }
                }),
            )
            .map_err(|e| Error::Internal(format!("microphone: {e}")))?;
        let mut player = (self.player)();
        let played_at = Instant::now();
        if let Err(e) = player.play(wav.path(), 0.0, Some(seconds)) {
            let _ = session.stop();
            return Err(Error::Internal(format!("test playback: {e}")));
        }
        let mut chunks = Vec::new();
        let mut count = 0usize;
        let maximum = (seconds * 384_000.0) as usize + 384_000;
        let mut playback_error = false;
        let mut played_cursor = 0usize;
        while played_at.elapsed().as_secs_f64() < seconds {
            let position = ((played_at.elapsed().as_secs_f64() * f64::from(RATE)) as usize)
                .clamp(played_cursor, played.len());
            levels(&[], &played[played_cursor..position]);
            played_cursor = position;
            for chunk in receive.try_iter() {
                count += chunk.samples_mono_f32.len();
                if count > maximum {
                    overflow.store(true, Ordering::Relaxed);
                    break;
                }
                levels(&chunk.samples_mono_f32, &[]);
                chunks.push(chunk);
            }
            if matches!(player.poll(), PlayerState::Failed { .. }) {
                playback_error = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        player.stop();
        let failure = session.failure();
        session
            .stop()
            .map_err(|e| Error::Internal(format!("stopping microphone: {e}")))?;
        for chunk in receive.try_iter() {
            count += chunk.samples_mono_f32.len();
            if count > maximum {
                overflow.store(true, Ordering::Relaxed);
                break;
            }
            chunks.push(chunk);
        }
        if playback_error {
            return Err(Error::Internal("test playback failed".into()));
        }
        if failure.is_some() {
            return Err(Error::Internal(
                "microphone stopped during audio check".into(),
            ));
        }
        if overflow.load(Ordering::Relaxed) {
            return Err(Error::Internal("audio check capture overrun".into()));
        }
        if chunks.is_empty() {
            return Err(Error::Internal("microphone returned no audio".into()));
        }
        let recorded = place_chunks(chunks, played_at, played.len(), RATE)?;
        let output_rate = self
            .devices
            .default_output()
            .ok()
            .flatten()
            .filter(|device| {
                matches!(
                    device.transport,
                    broscribe_platform::DeviceTransport::Bluetooth
                        | broscribe_platform::DeviceTransport::BluetoothLe
                )
            })
            .map(|device| device.nominal_rate)
            .filter(|rate| *rate > 0);
        Ok(analyze_audio_check(
            &played,
            &recorded,
            RATE,
            output_rate,
            &AudioCheckThresholds::default(),
        ))
    }
}

pub(crate) fn place_chunks(
    chunks: Vec<MicChunk>,
    epoch: Instant,
    length: usize,
    rate: u32,
) -> Result<Vec<f32>> {
    let mut recorded = vec![0.0; length];
    let mut cursor = 0usize;
    for chunk in chunks {
        if !(8_000..=384_000).contains(&chunk.sample_rate) {
            return Err(Error::InvalidInput(
                "unsupported audio check capture rate".into(),
            ));
        }
        let mut resampler = broscribe_capture::resample::Resampler::new(
            f64::from(chunk.sample_rate),
            f64::from(rate),
        );
        let n = resampler.output_len(chunk.samples_mono_f32.len() as u64) as usize;
        let mut samples = vec![0.0; n];
        resampler.push(&chunk.samples_mono_f32);
        resampler.finish();
        let n = resampler.pull(&mut samples);
        let offset = chunk.host_time.map(|at| {
            if at >= epoch {
                at.duration_since(epoch).as_secs_f64()
            } else {
                -epoch.duration_since(at).as_secs_f64()
            }
        });
        let signed_start = offset
            .map(|seconds| (seconds * f64::from(rate)).round() as i64)
            .unwrap_or(cursor as i64);
        let skip = (-signed_start).max(0) as usize;
        let start = signed_start.max(0) as usize;
        if start < length && skip < n {
            let take = (n - skip).min(length - start);
            recorded[start..start + take].copy_from_slice(&samples[skip..skip + take]);
            cursor = start + take;
        }
    }
    Ok(recorded)
}

pub(crate) fn write_wav(file: &mut std::fs::File, samples: &[f32], rate: u32) -> Result<()> {
    let length = samples.len() as u32 * 4;
    let result = (|| -> std::io::Result<()> {
        let mut out = std::io::BufWriter::new(file);
        out.write_all(b"RIFF")?;
        out.write_all(&(length + 36).to_le_bytes())?;
        out.write_all(b"WAVEfmt ")?;
        out.write_all(&16u32.to_le_bytes())?;
        out.write_all(&3u16.to_le_bytes())?;
        out.write_all(&1u16.to_le_bytes())?;
        out.write_all(&rate.to_le_bytes())?;
        out.write_all(&(rate * 4).to_le_bytes())?;
        out.write_all(&4u16.to_le_bytes())?;
        out.write_all(&32u16.to_le_bytes())?;
        out.write_all(b"data")?;
        out.write_all(&length.to_le_bytes())?;
        for sample in samples {
            out.write_all(&sample.to_le_bytes())?;
        }
        out.flush()
    })();
    result.map_err(|e| Error::Internal(format!("writing audio check signal: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::{AudioDevice, CaptureSession, MicSink};
    use std::sync::atomic::AtomicUsize;
    /// Captured PCM, and the device each start asked for.
    struct Mic(Vec<f32>, std::sync::Mutex<Vec<Option<String>>>);
    struct Capture;
    impl CaptureSession for Capture {
        fn sample_rate(&self) -> u32 {
            16_000
        }
        fn failure(&self) -> Option<String> {
            None
        }
        fn stop(self: Box<Self>) -> broscribe_platform::Result<()> {
            Ok(())
        }
    }
    impl MicCapture for Mic {
        fn devices(&self) -> broscribe_platform::Result<Vec<AudioDevice>> {
            Ok(Vec::new())
        }
        fn start(
            &self,
            device: Option<&str>,
            mut sink: MicSink,
        ) -> broscribe_platform::Result<Box<dyn CaptureSession>> {
            self.1.lock().unwrap().push(device.map(str::to_owned));
            sink(MicChunk::new(self.0.clone(), 16_000));
            Ok(Box::new(Capture))
        }
    }
    /// Plays nothing, but prepares through the real request bookkeeping: its
    /// probe checks that the signal written is a WAV file.
    struct Player(
        Arc<AtomicUsize>,
        broscribe_platform::player::PlaybackRequests,
    );
    impl AudioPlayer for Player {
        fn can_play_natively(&self, _: &Path) -> bool {
            true
        }
        fn prepare(
            &mut self,
            path: &Path,
        ) -> broscribe_platform::Result<broscribe_platform::player::PlaybackPreparation> {
            Ok(self.1.begin(
                path,
                broscribe_platform::player::PREPARE_TIMEOUT,
                Box::new(|path: &Path, _: &std::sync::atomic::AtomicBool| {
                    let bytes = std::fs::read(path)?;
                    let wav = bytes.get(..4) == Some(&b"RIFF"[..])
                        && bytes.get(8..12) == Some(&b"WAVE"[..]);
                    if !wav {
                        return Err(Error::Decode("the audio check signal is not a WAV".into()));
                    }
                    Ok(broscribe_platform::player::ProbedPlayback {
                        tracks: vec![0],
                        duration_s: None,
                    })
                }),
            ))
        }
        fn play_prepared(
            &mut self,
            prepared: broscribe_platform::player::PreparedPlayback,
            _: f64,
            _: Option<f64>,
        ) -> broscribe_platform::Result<()> {
            self.1.accept(&prepared)?;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn set_rate(&mut self, rate: f32) -> broscribe_platform::Result<()> {
            broscribe_platform::player::check_rate(rate)
        }
        fn pause(&mut self) {}
        fn resume(&mut self) -> broscribe_platform::Result<()> {
            Ok(())
        }
        fn stop(&mut self) {
            self.1.supersede();
        }
        fn position_s(&self) -> Option<f64> {
            Some(0.0)
        }
        fn is_playing(&self) -> bool {
            true
        }
        fn poll(&mut self) -> PlayerState {
            PlayerState::Playing { position_s: 0.0 }
        }
    }
    #[test]
    fn plays_synthetic_signal_and_analyzes_captured_mic_with_fakes() {
        let seconds = 0.1;
        let mut spec = TestSignalSpec::new(16_000);
        spec.lead_silence_s = 0.01;
        spec.chirp_s = 0.045;
        spec.noise_s = 0.03;
        spec.tail_silence_s = 0.015;
        let captured = test_signal(&spec).iter().map(|s| s * 0.02).collect();
        let plays = Arc::new(AtomicUsize::new(0));
        let player = plays.clone();
        let mic = Arc::new(Mic(captured, Default::default()));
        let devices = DeviceAudioCheck {
            mic: mic.clone(),
            devices: Arc::new(broscribe_platform::devices::FakeDeviceInfo::default()),
            player: Arc::new(move || {
                Box::new(Player(
                    player.clone(),
                    broscribe_platform::player::PlaybackRequests::new(),
                ))
            }),
        };
        let tmp = tempfile::tempdir().unwrap();
        let mut heard = 0usize;
        let mut played = 0usize;
        let report = devices
            .run(seconds, tmp.path(), Some("usb-mic"), &mut |h, p| {
                heard += h.len();
                played += p.len();
            })
            .unwrap();
        // The pinned microphone recording would open, never the default.
        assert_eq!(*mic.1.lock().unwrap(), [Some("usb-mic".to_owned())]);
        assert!(heard > 0 && played > 0);
        assert_eq!(plays.load(Ordering::SeqCst), 1);
        assert!(
            report.delay_ms.is_some_and(|delay| delay.abs() < 1.0),
            "{report:?}"
        );
        assert!(report.correlation_peak > 0.5);
        assert!((-35.0..-33.0).contains(&report.leakage_db.unwrap()));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }
}
