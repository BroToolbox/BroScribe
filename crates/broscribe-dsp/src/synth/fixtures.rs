//! Synthetic test fixtures: a speech-like signal, a room impulse response and
//! a two-track echo scene. Compiled for this crate's tests and, through the
//! `fixtures` feature, for other crates' tests; never part of a production
//! build.

use super::Rng;
use crate::fft::{convolve, interpolate};

/// Voice parameters for [`speech_like`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Voice {
    pub seed: u64,
    /// Mean pitch.
    pub f0_hz: f64,
    /// RMS over the voiced parts.
    pub level_dbfs: f32,
}

/// Speech-like signal: syllables (voiced pulse trains through two moving
/// formant resonators, some fricative noise bursts) with syllabic envelopes,
/// short gaps and phrase pauses.
pub fn speech_like(rate: u32, duration_s: f64, voice: Voice) -> Vec<f32> {
    let fs = rate as f64;
    let len = (duration_s * fs) as usize;
    let mut out = vec![0.0f32; len];
    let mut rng = Rng::new(voice.seed);
    let mut t = rng.range(0.0, 0.2);
    let mut in_phrase = 0usize;
    let mut phrase_len = 4 + (rng.uniform() * 7.0) as usize;
    while t < duration_s {
        let syl = rng.range(0.12, 0.26);
        let start = (t * fs) as usize;
        let n = ((syl * fs) as usize).min(len.saturating_sub(start));
        if n == 0 {
            break;
        }
        let fricative = rng.uniform() < 0.2;
        let f1 = rng.range(300.0, 800.0);
        let f2 = rng.range(900.0, 2300.0);
        let f0_start = voice.f0_hz * rng.range(0.85, 1.15);
        let f0_end = voice.f0_hz * rng.range(0.8, 1.2);
        let mut r1 = Resonator::new(f1, 90.0, fs);
        let mut r2 = Resonator::new(f2, 120.0, fs);
        let mut phase = 0.0f64;
        let mut prev_noise = 0.0f64;
        for i in 0..n {
            let x = i as f64 / n as f64;
            let env = (std::f64::consts::PI * x).sin().powf(0.7);
            let excitation = if fricative {
                let w = rng.gaussian();
                let hp = w - prev_noise;
                prev_noise = w;
                hp * 0.5
            } else {
                let f0 = f0_start + (f0_end - f0_start) * x;
                phase += f0 / fs;
                let pulse = if phase >= 1.0 {
                    phase -= 1.0;
                    1.0
                } else {
                    0.0
                };
                pulse + 0.05 * rng.gaussian()
            };
            let y = if fricative {
                excitation
            } else {
                r2.step(r1.step(excitation))
            };
            out[start + i] += (env * y) as f32;
        }
        t += syl + rng.range(0.03, 0.08);
        in_phrase += 1;
        if in_phrase >= phrase_len {
            t += rng.range(0.3, 0.7);
            in_phrase = 0;
            phrase_len = 4 + (rng.uniform() * 7.0) as usize;
        }
    }
    normalize_active(&mut out, voice.level_dbfs);
    out
}

/// Two-pole resonator with unity peak-ish gain.
struct Resonator {
    a1: f64,
    a2: f64,
    gain: f64,
    y1: f64,
    y2: f64,
}

impl Resonator {
    fn new(freq: f64, bandwidth: f64, fs: f64) -> Self {
        let r = (-std::f64::consts::PI * bandwidth / fs).exp();
        let theta = 2.0 * std::f64::consts::PI * freq / fs;
        Self {
            a1: 2.0 * r * theta.cos(),
            a2: -r * r,
            gain: 1.0 - r,
            y1: 0.0,
            y2: 0.0,
        }
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.gain * x + self.a1 * self.y1 + self.a2 * self.y2;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Scales `x` so the RMS over its non-silent samples is `level_dbfs`.
fn normalize_active(x: &mut [f32], level_dbfs: f32) {
    let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak == 0.0 {
        return;
    }
    let thr = peak * 1e-3;
    let (sum, count) = x
        .iter()
        .filter(|v| v.abs() > thr)
        .fold((0.0f64, 0usize), |(s, c), v| {
            (s + (*v as f64).powi(2), c + 1)
        });
    let rms = (sum / count.max(1) as f64).sqrt();
    let target = 10f64.powf(level_dbfs as f64 / 20.0);
    let g = (target / rms) as f32;
    for v in x {
        *v *= g;
    }
}

/// Room impulse response: a unit direct path at `direct_delay_s` followed by
/// an exponentially decaying noise tail (`rt60_s`) scaled to the given
/// direct-to-reverberant ratio.
pub fn room_impulse_response(
    rate: u32,
    direct_delay_s: f64,
    rt60_s: f64,
    drr_db: f64,
    length_s: f64,
    seed: u64,
) -> Vec<f32> {
    let fs = rate as f64;
    let d = (direct_delay_s * fs).round() as usize;
    let len = d + (length_s * fs) as usize + 1;
    let mut h = vec![0.0f32; len];
    h[d] = 1.0;
    let mut rng = Rng::new(seed);
    let tail_start = d + (0.002 * fs) as usize;
    let mut tail = vec![0.0f64; len];
    for (i, v) in tail.iter_mut().enumerate().skip(tail_start) {
        let t = (i - tail_start) as f64 / fs;
        *v = rng.gaussian() * (-6.907 * t / rt60_s).exp();
    }
    let energy: f64 = tail.iter().map(|v| v * v).sum();
    if energy > 0.0 {
        let g = (10f64.powf(-drr_db / 10.0) / energy).sqrt();
        for (dst, v) in h.iter_mut().zip(&tail) {
            *dst += (v * g) as f32;
        }
    }
    h
}

/// Gaussian noise at an RMS level.
pub fn noise(len: usize, level_dbfs: f32, seed: u64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let a = 10f64.powf(level_dbfs as f64 / 20.0);
    (0..len).map(|_| (rng.gaussian() * a) as f32).collect()
}

/// Keeps `x` only inside `spans` (seconds), with 10 ms ramps.
pub fn keep_spans(x: &mut [f32], rate: u32, spans: &[(f64, f64)]) {
    let fs = rate as f64;
    let ramp = (0.01 * fs).max(1.0);
    for (i, v) in x.iter_mut().enumerate() {
        let t = i as f64;
        let g = spans
            .iter()
            .map(|&(a, b)| {
                let (a, b) = (a * fs, b * fs);
                if t < a || t >= b {
                    0.0
                } else {
                    ((t - a) / ramp).min((b - t) / ramp).min(1.0)
                }
            })
            .fold(0.0f64, f64::max);
        *v *= g as f32;
    }
}

/// Re-clocks `x` by `ppm`: `out[n] = x[n · (1 + ppm·1e-6)]` (windowed sinc).
/// Positive `ppm` makes the content run earlier as time goes on.
pub fn apply_drift(x: &[f32], ppm: f64) -> Vec<f32> {
    let ratio = 1.0 + ppm * 1e-6;
    (0..x.len())
        .map(|n| interpolate(x, n as f64 * ratio))
        .collect()
}

/// Settings for [`echo_scene`].
#[derive(Debug, Clone, PartialEq)]
pub struct EchoSceneSpec {
    pub rate: u32,
    pub duration_s: f64,
    /// Far-end (system track) talk spans.
    pub far_spans: Vec<(f64, f64)>,
    /// Near-end (user at the mic) talk spans.
    pub near_spans: Vec<(f64, f64)>,
    /// Direct-path delay of the loudspeaker → mic path.
    pub echo_delay_s: f64,
    /// Echo level relative to the far-end signal (broadband gain, dB).
    pub echo_gain_db: f32,
    pub rt60_s: f64,
    pub drr_db: f64,
    /// Mic-clock drift applied to the echo component.
    pub drift_ppm: f64,
    pub far_level_dbfs: f32,
    pub near_level_dbfs: f32,
    pub noise_dbfs: f32,
    pub seed: u64,
}

impl EchoSceneSpec {
    /// Laptop speakers + built-in mic, 16 kHz.
    pub fn laptop(duration_s: f64) -> Self {
        Self {
            rate: 16_000,
            duration_s,
            far_spans: vec![(0.0, duration_s)],
            near_spans: Vec::new(),
            echo_delay_s: 0.0375,
            echo_gain_db: -6.0,
            rt60_s: 0.25,
            drr_db: 3.0,
            drift_ppm: 0.0,
            far_level_dbfs: -20.0,
            near_level_dbfs: -22.0,
            noise_dbfs: -75.0,
            seed: 1,
        }
    }
}

/// A two-track scene with every component kept for ground truth.
#[derive(Debug, Clone, PartialEq)]
pub struct EchoScene {
    pub rate: u32,
    /// The system track (far end, as the tap captures it).
    pub system: Vec<f32>,
    /// The clean near-end voice at the mic.
    pub near: Vec<f32>,
    /// The echo component at the mic.
    pub echo: Vec<f32>,
    /// `near + echo + noise`.
    pub mic: Vec<f32>,
}

/// Builds a far-end/near-end echo scene (independent voices).
pub fn echo_scene(spec: &EchoSceneSpec) -> EchoScene {
    let len = (spec.duration_s * spec.rate as f64) as usize;
    let far_voice = Voice {
        seed: spec.seed,
        f0_hz: 120.0,
        level_dbfs: spec.far_level_dbfs,
    };
    let near_voice = Voice {
        seed: spec.seed.wrapping_add(1000),
        f0_hz: 210.0,
        level_dbfs: spec.near_level_dbfs,
    };
    let mut system = speech_like(spec.rate, spec.duration_s, far_voice);
    keep_spans(&mut system, spec.rate, &spec.far_spans);
    let mut near = speech_like(spec.rate, spec.duration_s, near_voice);
    keep_spans(&mut near, spec.rate, &spec.near_spans);
    let h = room_impulse_response(
        spec.rate,
        spec.echo_delay_s,
        spec.rt60_s,
        spec.drr_db,
        spec.rt60_s * 1.2,
        spec.seed.wrapping_add(2000),
    );
    let gain = 10f32.powf(spec.echo_gain_db / 20.0);
    let mut echo: Vec<f32> = convolve(&system, &h)
        .into_iter()
        .take(len)
        .map(|v| v * gain)
        .collect();
    echo.resize(len, 0.0);
    if spec.drift_ppm != 0.0 {
        echo = apply_drift(&echo, spec.drift_ppm);
    }
    let n = noise(len, spec.noise_dbfs, spec.seed.wrapping_add(3000));
    let mic = (0..len).map(|i| near[i] + echo[i] + n[i]).collect();
    EchoScene {
        rate: spec.rate,
        system,
        near,
        echo,
        mic,
    }
}
