//! System → mic alignment: per-window GCC-PHAT lags and a linear drift fit.
//!
//! The capture side already places both tracks on the host-time timeline and
//! corrects clock skew (plan §12.6); what remains is the acoustic path delay
//! plus any residual offset and drift. Alignment measures it per window and
//! fits `lag(t) = offset + drift · t`, so the AEC reference can be shifted
//! continuously (no step shifts between windows).

use crate::activity::db;
use crate::fft::{Correlator, interpolate};

/// Alignment search settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlignConfig {
    /// Correlation window length.
    pub window_s: f64,
    /// Step between window starts.
    pub hop_s: f64,
    /// Largest |lag| searched.
    pub max_lag_s: f64,
    /// Windows whose whitened peak is below this are ignored.
    pub min_peak: f32,
    /// Windows whose system track is quieter than this (RMS) are ignored.
    pub min_system_dbfs: f32,
    /// Windows further than this from the first fit are dropped before refitting.
    pub outlier_s: f64,
}

impl Default for AlignConfig {
    fn default() -> Self {
        Self {
            window_s: 30.0,
            hop_s: 30.0,
            max_lag_s: 0.5,
            min_peak: 0.05,
            min_system_dbfs: -60.0,
            outlier_s: 0.002,
        }
    }
}

/// Lag measured in one window.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowLag {
    /// Window centre on the mic timeline.
    pub center_s: f64,
    /// `mic(t) ≈ h * system(t - lag)`.
    pub lag_s: f64,
    /// Whitened correlation peak.
    pub peak: f32,
    /// Whether the window was used in the final fit.
    pub used: bool,
}

/// Fitted system → mic alignment.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Alignment {
    /// Lag at t = 0 (seconds; positive when the mic hears the system later).
    pub offset_s: f64,
    /// Lag change per second of mic time, in parts per million.
    pub drift_ppm: f64,
    /// Every measured window, including rejected ones.
    pub windows: Vec<WindowLag>,
}

impl Alignment {
    /// No offset, no drift, no measurements: used when the system track never
    /// correlates with the mic (e.g. a speakerphone that already removed echo).
    pub fn zero() -> Self {
        Self {
            offset_s: 0.0,
            drift_ppm: 0.0,
            windows: Vec::new(),
        }
    }

    /// Lag at mic time `t_s`.
    pub fn lag_s_at(&self, t_s: f64) -> f64 {
        self.offset_s + self.drift_ppm * 1e-6 * t_s
    }

    /// Whether the alignment comes from measurements.
    pub fn is_measured(&self) -> bool {
        self.windows.iter().any(|w| w.used)
    }
}

/// Measures the system → mic lag per window and fits offset + drift.
///
/// `None` when no window has system sound that correlates with the mic.
pub fn estimate_alignment(
    mic: &[f32],
    system: &[f32],
    rate: u32,
    cfg: &AlignConfig,
) -> Option<Alignment> {
    let fs = rate as f64;
    let len = mic.len().min(system.len());
    if len == 0 {
        return None;
    }
    let window = ((cfg.window_s * fs) as usize).clamp(1, len);
    let hop = ((cfg.hop_s * fs) as usize).max(1);
    let max_lag = (cfg.max_lag_s * fs) as usize;
    let mut windows = Vec::new();
    // Every window but a shorter tail has one length: plans and buffers are
    // made once.
    let mut correlator = Correlator::new();
    let mut start = 0usize;
    loop {
        let end = (start + window).min(len);
        // A short tail window is folded into the previous one's coverage.
        if end - start < window / 2 && start > 0 {
            break;
        }
        let sys = &system[start..end];
        // Mean square of the whole window, as one frame.
        let rms_db = db(sys.iter().map(|v| v * v).sum::<f32>() / sys.len() as f32);
        if rms_db >= cfg.min_system_dbfs
            && let Some(c) = correlator.correlate(sys, &mic[start..end], max_lag)
            && c.peak >= cfg.min_peak
        {
            windows.push(WindowLag {
                center_s: (start + end) as f64 / 2.0 / fs,
                lag_s: c.lag / fs,
                peak: c.peak,
                used: true,
            });
        }
        if end == len {
            break;
        }
        start += hop;
    }
    if windows.is_empty() {
        return None;
    }
    let (mut offset, mut slope) = fit(&windows);
    for w in &mut windows {
        w.used = (w.lag_s - (offset + slope * w.center_s)).abs() <= cfg.outlier_s;
    }
    if !windows.iter().any(|w| w.used) {
        // Every window disagrees with the fit: keep the strongest one.
        let best = windows
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.peak.total_cmp(&b.1.peak))
            .map(|(i, _)| i)?;
        windows[best].used = true;
    }
    (offset, slope) = fit(&windows);
    Some(Alignment {
        offset_s: offset,
        drift_ppm: slope * 1e6,
        windows,
    })
}

/// Peak-weighted least squares over the `used` windows → (offset, slope).
fn fit(windows: &[WindowLag]) -> (f64, f64) {
    let used: Vec<&WindowLag> = windows.iter().filter(|w| w.used).collect();
    let sw: f64 = used.iter().map(|w| w.peak as f64).sum();
    if used.len() < 2 || sw <= 0.0 {
        let lag = used.first().map_or(0.0, |w| w.lag_s);
        return (lag, 0.0);
    }
    let mt = used.iter().map(|w| w.peak as f64 * w.center_s).sum::<f64>() / sw;
    let ml = used.iter().map(|w| w.peak as f64 * w.lag_s).sum::<f64>() / sw;
    let stt: f64 = used
        .iter()
        .map(|w| w.peak as f64 * (w.center_s - mt).powi(2))
        .sum();
    if stt <= 1e-12 {
        return (ml, 0.0);
    }
    let stl: f64 = used
        .iter()
        .map(|w| w.peak as f64 * (w.center_s - mt) * (w.lag_s - ml))
        .sum();
    let slope = stl / stt;
    (ml - slope * mt, slope)
}

/// The system track re-timed onto the mic timeline (`len` samples) so that its
/// echo appears `lead_s` after it: `out[n] = system[n - (lag(n) - lead)]`.
///
/// A small positive lead keeps the AEC reference causal. A constant shift is
/// rounded to whole samples; a drifting one is interpolated (windowed sinc).
pub fn align_reference(
    system: &[f32],
    len: usize,
    rate: u32,
    alignment: &Alignment,
    lead_s: f64,
) -> Vec<f32> {
    let fs = rate as f64;
    let shift_at = |n: usize| (alignment.lag_s_at(n as f64 / fs) - lead_s) * fs;
    let span = (shift_at(len) - shift_at(0)).abs();
    if span < 0.5 {
        let shift = shift_at(0).round() as i64;
        return (0..len as i64)
            .map(|n| {
                let src = n - shift;
                if src < 0 || src >= system.len() as i64 {
                    0.0
                } else {
                    system[src as usize]
                }
            })
            .collect();
    }
    (0..len)
        .map(|n| interpolate(system, n as f64 - shift_at(n)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_is_shifted_by_lag_minus_lead() {
        let system: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let a = Alignment {
            offset_s: 0.010,
            drift_ppm: 0.0,
            windows: Vec::new(),
        };
        // rate 1 kHz: lag 10 samples, lead 2 samples → shift 8.
        let out = align_reference(&system, 100, 1000, &a, 0.002);
        assert_eq!(out[8], 0.0);
        assert_eq!(out[9], 1.0);
        assert_eq!(out[50], 42.0);
    }

    #[test]
    fn fit_recovers_line() {
        let windows: Vec<WindowLag> = (0..5)
            .map(|i| WindowLag {
                center_s: i as f64 * 10.0,
                lag_s: 0.03 + 50e-6 * i as f64 * 10.0,
                peak: 0.5,
                used: true,
            })
            .collect();
        let (o, s) = fit(&windows);
        assert!((o - 0.03).abs() < 1e-9);
        assert!((s - 50e-6).abs() < 1e-12);
    }

    const RATE: u32 = 16_000;

    #[test]
    fn windows_equal_fresh_per_window_correlation() {
        // 10 s, 2 s windows every 1.5 s: six full windows and a 1 s tail of
        // another FFT length (replanned, then reused buffers again).
        let len = 10 * RATE as usize;
        let system = crate::synth::noise(len, -20.0, 41);
        let mut mic = crate::synth::noise(len, -70.0, 42);
        for (m, s) in mic[300..].iter_mut().zip(&system) {
            *m += 0.5 * s;
        }
        let cfg = AlignConfig {
            window_s: 2.0,
            hop_s: 1.5,
            ..AlignConfig::default()
        };
        let a = estimate_alignment(&mic, &system, RATE, &cfg).expect("aligned");
        let (window, hop) = (2 * RATE as usize, 3 * RATE as usize / 2);
        let max_lag = RATE as usize / 2;
        let expected: Vec<(f64, f64, f32)> = (0..7)
            .map(|i| {
                let start = i * hop;
                let end = (start + window).min(len);
                let c = crate::fft::gcc_phat(&system[start..end], &mic[start..end], max_lag)
                    .expect("correlates");
                (
                    (start + end) as f64 / 2.0 / f64::from(RATE),
                    c.lag / f64::from(RATE),
                    c.peak,
                )
            })
            .collect();
        let measured: Vec<(f64, f64, f32)> = a
            .windows
            .iter()
            .map(|w| (w.center_s, w.lag_s, w.peak))
            .collect();
        assert_eq!(measured, expected);
        assert!(a.windows.iter().all(|w| w.used));
        assert!((a.offset_s - 300.0 / f64::from(RATE)).abs() < 1e-5, "{a:?}");
        assert!(a.drift_ppm.abs() < 1.0, "{a:?}");
    }

    #[test]
    fn drifting_reference_places_each_source_sample() {
        // A smooth signal, so the windowed sinc stays close to the truth.
        let system: Vec<f32> = (0..10_000).map(|i| (i as f32 * 0.01).sin()).collect();
        let a = Alignment {
            offset_s: 0.010,
            drift_ppm: 200.0,
            windows: Vec::new(),
        };
        // rate 1 kHz, lead 2 ms: shift(n) = 8 + 0.0002·n samples.
        let out = align_reference(&system, system.len(), 1000, &a, 0.002);
        assert_eq!(out.len(), system.len());
        for n in (100..9_900).step_by(97) {
            let src = n as f64 - (8.0 + 0.0002 * n as f64);
            let truth = (src * 0.01).sin() as f32;
            // One sample of misplacement would be off by about 1e-2.
            assert!(
                (out[n] - truth).abs() < 2e-3,
                "n {n}: {} vs {truth}",
                out[n]
            );
        }
    }

    #[test]
    fn alignment_needs_overlap_and_system_sound() {
        let cfg = AlignConfig::default();
        assert_eq!(estimate_alignment(&[], &[0.1; 10], RATE, &cfg), None);
        let silent = vec![0.0f32; RATE as usize];
        let mic = crate::synth::noise(RATE as usize, -30.0, 43);
        assert_eq!(estimate_alignment(&mic, &silent, RATE, &cfg), None);
    }
}
