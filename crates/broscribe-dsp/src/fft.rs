//! FFT convolution, GCC-PHAT cross-correlation and fractional interpolation.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Linear convolution of `a` and `b` (length `a.len() + b.len() - 1`) via FFT.
pub fn convolve(a: &[f32], b: &[f32]) -> Vec<f32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let out_len = a.len() + b.len() - 1;
    let n = out_len.next_power_of_two();
    let mut planner = FftPlanner::<f32>::new();
    let forward = planner.plan_fft_forward(n);
    let inverse = planner.plan_fft_inverse(n);
    let mut x = padded(a, n);
    let mut y = padded(b, n);
    forward.process(&mut x);
    forward.process(&mut y);
    for (p, q) in x.iter_mut().zip(&y) {
        *p *= *q;
    }
    inverse.process(&mut x);
    let scale = 1.0 / n as f32;
    x[..out_len].iter().map(|c| c.re * scale).collect()
}

/// Result of a cross-correlation search.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Correlation {
    /// Lag `d` in samples (sub-sample by parabolic interpolation) such that
    /// `signal[n] ≈ h * reference[n - d]`; positive when `signal` lags.
    pub lag: f64,
    /// Height of the whitened correlation peak, about 0..1: 1 for a pure
    /// delayed copy, near 0 for unrelated signals.
    pub peak: f32,
}

/// Generalised cross-correlation with phase transform (GCC-PHAT) between
/// `reference` and `signal`, searching lags in `-max_lag..=max_lag`.
///
/// Whitening makes the peak sharp for coloured signals (speech, room echo) and
/// lands on the direct path of an impulse response. `None` when either input is
/// empty or silent. Repeated searches in this crate share one `Correlator`.
pub fn gcc_phat(reference: &[f32], signal: &[f32], max_lag: usize) -> Option<Correlation> {
    Correlator::new().correlate(reference, signal, max_lag)
}

/// Forward and inverse plans of one FFT length.
struct Plans {
    len: usize,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}

/// [`gcc_phat`] state kept across searches: a run of equal-length windows
/// plans its FFTs once and reuses its buffers. Results equal [`gcc_phat`]'s.
pub(crate) struct Correlator {
    planner: FftPlanner<f32>,
    plans: Option<Plans>,
    reference: Vec<Complex32>,
    cross: Vec<Complex32>,
    scratch: Vec<Complex32>,
}

impl Correlator {
    pub(crate) fn new() -> Self {
        Self {
            planner: FftPlanner::new(),
            plans: None,
            reference: Vec::new(),
            cross: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Plans for length `n`, replanning only when the length changes.
    fn plans(&mut self, n: usize) -> (Arc<dyn Fft<f32>>, Arc<dyn Fft<f32>>) {
        if let Some(p) = &self.plans
            && p.len == n
        {
            return (Arc::clone(&p.forward), Arc::clone(&p.inverse));
        }
        let forward = self.planner.plan_fft_forward(n);
        let inverse = self.planner.plan_fft_inverse(n);
        let scratch = forward
            .get_inplace_scratch_len()
            .max(inverse.get_inplace_scratch_len());
        self.scratch.resize(scratch, Complex32::new(0.0, 0.0));
        self.plans = Some(Plans {
            len: n,
            forward: Arc::clone(&forward),
            inverse: Arc::clone(&inverse),
        });
        (forward, inverse)
    }

    /// [`gcc_phat`] with this correlator's plans and buffers.
    pub(crate) fn correlate(
        &mut self,
        reference: &[f32],
        signal: &[f32],
        max_lag: usize,
    ) -> Option<Correlation> {
        let (n, inverse) = self.cross_spectrum(reference, signal)?;
        let cross = &mut self.cross;
        let mean_mag = cross.iter().map(|c| c.norm()).sum::<f32>() / n as f32;
        if mean_mag.is_nan() || mean_mag <= 0.0 {
            return None;
        }
        // Magnitude floor: bins with almost no energy carry noise only, so
        // they are not lifted to unit weight.
        let floor = mean_mag * 1e-3;
        for c in cross.iter_mut() {
            let mag = c.norm().max(floor);
            *c /= mag;
        }
        inverse.process_with_scratch(cross, &mut self.scratch);
        let cross = &self.cross;
        let max_lag = max_lag.min(n / 2 - 1) as i64;
        let at = |lag: i64| -> f32 {
            let idx = if lag >= 0 {
                lag as usize
            } else {
                (n as i64 + lag) as usize
            };
            cross[idx].re / n as f32
        };
        let mut best_lag = 0i64;
        let mut best = f32::NEG_INFINITY;
        for lag in -max_lag..=max_lag {
            let v = at(lag);
            if v > best {
                best = v;
                best_lag = lag;
            }
        }
        let mut lag = best_lag as f64;
        if best_lag > -max_lag && best_lag < max_lag {
            let (ym, y0, yp) = (
                at(best_lag - 1) as f64,
                best as f64,
                at(best_lag + 1) as f64,
            );
            let denom = ym - 2.0 * y0 + yp;
            if denom < 0.0 {
                let delta = 0.5 * (ym - yp) / denom;
                if delta.abs() <= 0.5 {
                    lag += delta;
                }
            }
        }
        Some(Correlation {
            lag,
            peak: best.max(0.0),
        })
    }

    /// Maximum absolute amplitude correlation over the allowed lags.
    /// Callers centre the inputs before masking. The full-window energies
    /// keep partial overlap from inflating the evidence at large lags.
    pub(crate) fn amplitude_correlation(
        &mut self,
        reference: &[f32],
        signal: &[f32],
        min_lag: isize,
        max_lag: usize,
    ) -> Option<f64> {
        let energy = |samples: &[f32]| samples.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>();
        let scale = (energy(reference) * energy(signal)).sqrt();
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        let (n, inverse) = self.cross_spectrum(reference, signal)?;
        inverse.process_with_scratch(&mut self.cross, &mut self.scratch);
        let max_lag = max_lag.min(n / 2 - 1) as isize;
        let min_lag = min_lag.max(-max_lag);
        if min_lag > max_lag {
            return None;
        }
        let mut best = 0.0f64;
        for lag in min_lag..=max_lag {
            let index = if lag >= 0 {
                lag as usize
            } else {
                n - lag.unsigned_abs()
            };
            best = best.max(f64::from(self.cross[index].re).abs());
        }
        Some(best / (n as f64 * scale))
    }

    fn cross_spectrum(
        &mut self,
        reference: &[f32],
        signal: &[f32],
    ) -> Option<(usize, Arc<dyn Fft<f32>>)> {
        if reference.is_empty() || signal.is_empty() {
            return None;
        }
        let n = (reference.len() + signal.len()).next_power_of_two();
        let (forward, inverse) = self.plans(n);
        fill_padded(&mut self.reference, reference, n);
        fill_padded(&mut self.cross, signal, n);
        forward.process_with_scratch(&mut self.reference, &mut self.scratch);
        forward.process_with_scratch(&mut self.cross, &mut self.scratch);
        for (s, r) in self.cross.iter_mut().zip(&self.reference) {
            *s *= r.conj();
        }
        Some((n, inverse))
    }
}

/// `x` zero-padded to `n` complex samples in a reused buffer.
fn fill_padded(buf: &mut Vec<Complex32>, x: &[f32], n: usize) {
    buf.clear();
    buf.resize(n, Complex32::new(0.0, 0.0));
    for (dst, &src) in buf.iter_mut().zip(x) {
        dst.re = src;
    }
}

fn padded(x: &[f32], n: usize) -> Vec<Complex32> {
    let mut v = vec![Complex32::new(0.0, 0.0); n];
    for (dst, &src) in v.iter_mut().zip(x) {
        dst.re = src;
    }
    v
}

/// Half-width of the windowed-sinc interpolator (taps = 2 × this).
const SINC_HALF: i64 = 16;

/// Value of `x` at fractional position `pos` (Hann-windowed sinc, zeros
/// outside the buffer).
pub(crate) fn interpolate(x: &[f32], pos: f64) -> f32 {
    let base = pos.floor();
    let frac = pos - base;
    let base = base as i64;
    let get = |i: i64| -> f64 {
        if i < 0 || i >= x.len() as i64 {
            0.0
        } else {
            x[i as usize] as f64
        }
    };
    if frac < 1e-9 {
        return get(base) as f32;
    }
    let half = SINC_HALF as f64;
    let mut acc = 0.0f64;
    for k in -(SINC_HALF - 1)..=SINC_HALF {
        let t = k as f64 - frac;
        let sinc = (std::f64::consts::PI * t).sin() / (std::f64::consts::PI * t);
        let window = 0.5 * (1.0 + (std::f64::consts::PI * t / half).cos());
        acc += get(base + k) * sinc * window;
    }
    acc as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convolve_matches_direct_sum() {
        let a = [1.0, 2.0, 3.0];
        let b = [0.0, 1.0, 0.5];
        let c = convolve(&a, &b);
        let expected = [0.0, 1.0, 2.5, 4.0, 1.5];
        assert_eq!(c.len(), expected.len());
        for (x, y) in c.iter().zip(expected) {
            assert!((x - y).abs() < 1e-5, "{c:?}");
        }
    }

    #[test]
    fn gcc_phat_finds_integer_delay() {
        let mut rng = crate::synth::Rng::new(7);
        let reference: Vec<f32> = (0..8000).map(|_| rng.gaussian() as f32).collect();
        let mut signal = vec![0.0f32; 8000];
        signal[123..].copy_from_slice(&reference[..8000 - 123]);
        let c = gcc_phat(&reference, &signal, 1000).unwrap();
        assert!((c.lag - 123.0).abs() < 0.1, "{c:?}");
        assert!(c.peak > 0.5, "{c:?}");
        let back = gcc_phat(&signal, &reference, 1000).unwrap();
        assert!((back.lag + 123.0).abs() < 0.1, "{back:?}");
    }

    #[test]
    fn interpolate_hits_samples_and_midpoints() {
        let x: Vec<f32> = (0..200).map(|i| (i as f32 * 0.05).sin()).collect();
        assert_eq!(interpolate(&x, 50.0), x[50]);
        let mid = interpolate(&x, 100.5);
        let truth = (100.5f32 * 0.05).sin();
        assert!((mid - truth).abs() < 1e-3, "{mid} vs {truth}");
    }

    /// The per-call planning GCC-PHAT this crate used before [`Correlator`]:
    /// the equivalence reference.
    fn planned_per_call(reference: &[f32], signal: &[f32], max_lag: usize) -> Option<Correlation> {
        if reference.is_empty() || signal.is_empty() {
            return None;
        }
        let n = (reference.len() + signal.len()).next_power_of_two();
        let mut planner = FftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(n);
        let inverse = planner.plan_fft_inverse(n);
        let mut r = padded(reference, n);
        let mut s = padded(signal, n);
        forward.process(&mut r);
        forward.process(&mut s);
        let mut cross: Vec<Complex32> = s.iter().zip(&r).map(|(s, r)| s * r.conj()).collect();
        let mean_mag = cross.iter().map(|c| c.norm()).sum::<f32>() / n as f32;
        if mean_mag.is_nan() || mean_mag <= 0.0 {
            return None;
        }
        let floor = mean_mag * 1e-3;
        for c in &mut cross {
            let mag = c.norm().max(floor);
            *c /= mag;
        }
        inverse.process(&mut cross);
        let max_lag = max_lag.min(n / 2 - 1) as i64;
        let at = |lag: i64| -> f32 {
            let idx = if lag >= 0 {
                lag as usize
            } else {
                (n as i64 + lag) as usize
            };
            cross[idx].re / n as f32
        };
        let mut best_lag = 0i64;
        let mut best = f32::NEG_INFINITY;
        for lag in -max_lag..=max_lag {
            let v = at(lag);
            if v > best {
                best = v;
                best_lag = lag;
            }
        }
        let mut lag = best_lag as f64;
        if best_lag > -max_lag && best_lag < max_lag {
            let (ym, y0, yp) = (
                at(best_lag - 1) as f64,
                best as f64,
                at(best_lag + 1) as f64,
            );
            let denom = ym - 2.0 * y0 + yp;
            if denom < 0.0 {
                let delta = 0.5 * (ym - yp) / denom;
                if delta.abs() <= 0.5 {
                    lag += delta;
                }
            }
        }
        Some(Correlation {
            lag,
            peak: best.max(0.0),
        })
    }

    /// A delayed noise pair silenced over 16 000..24 000, and windows of
    /// equal lengths, the silent one, a shorter one, then the full length.
    fn windows() -> (Vec<f32>, Vec<f32>, Vec<(usize, usize)>) {
        let mut system = crate::synth::noise(48_000, -20.0, 51);
        let mut mic = crate::synth::noise(48_000, -70.0, 52);
        for (m, s) in mic[211..].iter_mut().zip(&system) {
            *m += 0.4 * s;
        }
        system[16_000..24_000].fill(0.0);
        let spans = vec![
            (0, 8_000),
            (8_000, 16_000),
            (16_000, 24_000),
            (24_000, 28_000),
            (40_000, 48_000),
        ];
        (system, mic, spans)
    }

    #[test]
    fn reused_correlator_equals_per_call_planning() {
        let (system, mic, spans) = windows();
        let mut correlator = Correlator::new();
        for (from, to) in spans {
            let (r, s) = (&system[from..to], &mic[from..to]);
            let expected = planned_per_call(r, s, 1_000);
            assert_eq!(correlator.correlate(r, s, 1_000), expected, "{from}..{to}");
            assert_eq!(gcc_phat(r, s, 1_000), expected, "{from}..{to}");
        }
        // The silent window has no correlation; the others find the delay.
        let (r, s) = (&system[16_000..24_000], &mic[16_000..24_000]);
        assert_eq!(correlator.correlate(r, s, 1_000), None);
        let (r, s) = (&system[..8_000], &mic[..8_000]);
        let c = correlator.correlate(r, s, 1_000).unwrap();
        assert!((c.lag - 211.0).abs() < 0.1, "{c:?}");
        assert_eq!(correlator.correlate(&[], &mic[..10], 1_000), None);
    }

    /// Non-default timing harness:
    /// `cargo test -p broscribe-dsp --release -- --ignored --nocapture bench_`.
    #[test]
    #[ignore = "benchmark; run explicitly with --ignored"]
    fn bench_alignment_windows_planner_reuse() {
        let rate = 16_000usize;
        let (window, count) = (30 * rate, 8);
        let system = crate::synth::noise(window * count, -20.0, 53);
        let mut mic = crate::synth::noise(window * count, -70.0, 54);
        for (m, s) in mic[600..].iter_mut().zip(&system) {
            *m += 0.3 * s;
        }
        let started = std::time::Instant::now();
        let per_call: Vec<_> = (0..count)
            .map(|i| {
                let span = i * window..(i + 1) * window;
                planned_per_call(&system[span.clone()], &mic[span], rate / 2)
            })
            .collect();
        let per_call_time = started.elapsed();
        let started = std::time::Instant::now();
        let mut correlator = Correlator::new();
        let reused: Vec<_> = (0..count)
            .map(|i| {
                let span = i * window..(i + 1) * window;
                correlator.correlate(&system[span.clone()], &mic[span], rate / 2)
            })
            .collect();
        let reused_time = started.elapsed();
        assert_eq!(reused, per_call);
        eprintln!("{count} windows of 30 s: per call {per_call_time:?}, reused {reused_time:?}");
    }
}
