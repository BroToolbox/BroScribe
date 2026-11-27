//! Audio check (setup step 5, `broscribe audio-check`): a chirp + noise test
//! signal and the analysis of what the mic recorded while it played.
//!
//! Playback and recording are the caller's job (through the daemon); both
//! buffers must be mono at the same rate, the recording started no later than
//! the playback. Never run while a meeting tap is active (plan §13.6).

use serde::{Deserialize, Serialize};

use crate::activity::{db, frame_energies, frame_len};
use crate::echo::EchoHandling;
use crate::fft::gcc_phat;
use crate::synth::Rng;

/// Test signal layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TestSignalSpec {
    pub rate: u32,
    /// Silence before the signal (noise-floor reference).
    pub lead_silence_s: f64,
    /// Logarithmic sweep.
    pub chirp_s: f64,
    pub chirp_from_hz: f64,
    pub chirp_to_hz: f64,
    /// White noise burst after the sweep.
    pub noise_s: f64,
    /// Silence after the signal (room tail).
    pub tail_silence_s: f64,
    /// RMS of the active part.
    pub level_dbfs: f32,
}

impl TestSignalSpec {
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            lead_silence_s: 0.3,
            chirp_s: 1.5,
            chirp_from_hz: 200.0,
            chirp_to_hz: (rate as f64 * 0.45).min(8_000.0),
            noise_s: 1.0,
            tail_silence_s: 0.5,
            level_dbfs: -18.0,
        }
    }
}

/// The test signal for [`TestSignalSpec`].
pub fn test_signal(spec: &TestSignalSpec) -> Vec<f32> {
    let fs = spec.rate as f64;
    let lead = (spec.lead_silence_s * fs) as usize;
    let chirp = (spec.chirp_s * fs) as usize;
    let noise = (spec.noise_s * fs) as usize;
    let tail = (spec.tail_silence_s * fs) as usize;
    let amp = 10f64.powf(spec.level_dbfs as f64 / 20.0);
    let mut out = vec![0.0f32; lead + chirp + noise + tail];
    let k = (spec.chirp_to_hz / spec.chirp_from_hz).ln();
    let fade = (0.01 * fs) as usize;
    for i in 0..chirp {
        let t = i as f64 / fs;
        let phase = 2.0 * std::f64::consts::PI * spec.chirp_from_hz * spec.chirp_s / k
            * ((t * k / spec.chirp_s).exp() - 1.0);
        let edge = (i.min(chirp - i) as f64 / fade.max(1) as f64).min(1.0);
        out[lead + i] = (amp * std::f64::consts::SQRT_2 * phase.sin() * edge) as f32;
    }
    let mut rng = Rng::new(0xA0D1_0C4E);
    for i in 0..noise {
        let edge = (i.min(noise - i) as f64 / fade.max(1) as f64).min(1.0);
        out[lead + chirp + i] = (amp * rng.gaussian() * edge) as f32;
    }
    out
}

/// Decision thresholds on the measured leakage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioCheckThresholds {
    /// Leakage below this needs no echo handling.
    pub none_below_db: f32,
    /// Leakage below this (and above `none_below_db`) needs residue gating;
    /// above it full AEC.
    pub gating_below_db: f32,
    /// Correlation peak below this means no measurable echo path.
    pub min_peak: f32,
    /// Largest delay searched.
    pub max_delay_s: f64,
    /// Output sample rate at or below which a Bluetooth call profile is assumed.
    pub call_profile_max_rate_hz: u32,
}

impl Default for AudioCheckThresholds {
    fn default() -> Self {
        Self {
            none_below_db: -45.0,
            gating_below_db: -25.0,
            min_peak: 0.05,
            max_delay_s: 1.0,
            call_profile_max_rate_hz: 16_000,
        }
    }
}

/// Analysis result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioCheckReport {
    /// Output → mic delay (direct path), when the echo path was measurable.
    pub delay_ms: Option<f64>,
    /// Echo energy at the mic relative to the played signal (dB).
    pub leakage_db: Option<f32>,
    pub correlation_peak: f32,
    /// Mic level before the signal reached it.
    pub noise_floor_dbfs: f32,
    pub recommendation: EchoHandling,
    /// The output device runs at a call-profile rate (warn the user).
    pub bluetooth_call_profile: bool,
}

/// Analyses `recorded` (mic) against `played` (the test signal as sent to the
/// output), both mono at `rate`. `output_rate_hz` is the output device's
/// nominal rate when known.
pub fn analyze_audio_check(
    played: &[f32],
    recorded: &[f32],
    rate: u32,
    output_rate_hz: Option<u32>,
    th: &AudioCheckThresholds,
) -> AudioCheckReport {
    let frame = frame_len(rate);
    let bluetooth_call_profile = output_rate_hz.is_some_and(|r| r <= th.call_profile_max_rate_hz);
    let played_e = frame_energies(played, frame);
    let first_active = played_e.iter().position(|&e| db(e) > -80.0).unwrap_or(0);
    let last_active = played_e.iter().rposition(|&e| db(e) > -80.0).unwrap_or(0);
    let rec_e = frame_energies(recorded, frame);
    let noise_floor = mean(rec_e.get(..first_active.min(rec_e.len())).unwrap_or(&[]))
        .unwrap_or_else(|| crate::activity::quantile(&rec_e, 0.1).unwrap_or(0.0));
    let mut report = AudioCheckReport {
        delay_ms: None,
        leakage_db: None,
        correlation_peak: 0.0,
        noise_floor_dbfs: db(noise_floor),
        recommendation: EchoHandling::None,
        bluetooth_call_profile,
    };
    let max_lag = (th.max_delay_s * rate as f64) as usize;
    let Some(corr) = gcc_phat(played, recorded, max_lag) else {
        return report;
    };
    report.correlation_peak = corr.peak;
    // A zero-sample peak can refine just below zero through float roundoff.
    // Only the interior of that peak's half-sample resolution is zero delay;
    // an earlier integer peak remains inconclusive.
    if corr.peak < th.min_peak || corr.lag <= -0.5 {
        return report;
    }
    let lag = corr.lag.max(0.0);
    report.delay_ms = Some(lag / rate as f64 * 1000.0);
    // Energy over the played span shifted by the delay (+ a short room tail).
    let shift = (lag / frame as f64).round() as usize;
    let tail = 10usize;
    let from = (first_active + shift).min(rec_e.len());
    let to = (last_active + 1 + shift + tail).min(rec_e.len());
    let (Some(rec_mean), Some(played_mean)) = (
        mean(&rec_e[from..to]),
        mean(&played_e[first_active..=last_active.max(first_active)]),
    ) else {
        return report;
    };
    let excess = (rec_mean - noise_floor).max(0.0);
    let leakage = db(excess) - db(played_mean);
    report.leakage_db = Some(leakage);
    report.recommendation = if leakage < th.none_below_db {
        EchoHandling::None
    } else if leakage < th.gating_below_db {
        EchoHandling::ResidueGating
    } else {
        EchoHandling::FullAec
    };
    report
}

fn mean(v: &[f32]) -> Option<f32> {
    if v.is_empty() {
        None
    } else {
        Some(v.iter().sum::<f32>() / v.len() as f32)
    }
}
