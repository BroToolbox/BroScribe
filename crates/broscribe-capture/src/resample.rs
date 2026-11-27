//! Streaming band-limited resampler with an arbitrary ratio, used to bring
//! each journal segment from its (skew-corrected) device rate to the common
//! timeline rate, and by the daemon to feed 16 kHz to the live preview.
//!
//! Windowed sinc (Blackman) read from an oversampled table with linear
//! interpolation between phases. The kernel is centred on each output
//! instant, so resampling adds no delay. An exact 1:1 ratio is a copy.

use std::collections::VecDeque;

/// Zero crossings of the sinc on each side.
const ZERO_CROSSINGS: usize = 16;
/// Table entries per input sample.
const PHASES: usize = 256;

pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    passthrough: bool,
    /// Kernel half-width in input samples.
    half: usize,
    /// Kernel at `t = -half + k / PHASES`, `k = 0..=2·half·PHASES`.
    table: Vec<f32>,
    buf: VecDeque<f32>,
    /// Absolute input index of `buf[0]`.
    buf_start: u64,
    /// Input samples received.
    fed: u64,
    /// Output samples produced.
    produced: u64,
    ended: bool,
}

impl Resampler {
    /// Converts from `in_rate` to `out_rate` (both in Hz, may be fractional
    /// for skew-corrected rates).
    pub fn new(in_rate: f64, out_rate: f64) -> Self {
        let step = in_rate / out_rate;
        let passthrough = (step - 1.0).abs() < 1e-12;
        // Cut-off as a fraction of the input Nyquist frequency.
        let fc = 0.95 * (1.0 / step).min(1.0);
        let half = if passthrough {
            0
        } else {
            (ZERO_CROSSINGS as f64 / fc).ceil() as usize
        };
        let table = if passthrough {
            Vec::new()
        } else {
            (0..=2 * half * PHASES)
                .map(|k| {
                    let t = k as f64 / PHASES as f64 - half as f64;
                    let x = fc * t;
                    let sinc = if x.abs() < 1e-12 {
                        1.0
                    } else {
                        (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                    };
                    let u = t / half as f64;
                    let window = 0.42
                        + 0.5 * (std::f64::consts::PI * u).cos()
                        + 0.08 * (2.0 * std::f64::consts::PI * u).cos();
                    (fc * sinc * window) as f32
                })
                .collect()
        };
        Self {
            step,
            passthrough,
            half,
            table,
            buf: VecDeque::new(),
            buf_start: 0,
            fed: 0,
            produced: 0,
            ended: false,
        }
    }

    /// Forgets every input and output, keeping the kernel table and buffer
    /// capacity: the next input is resampled exactly as by a new resampler
    /// for the same rates.
    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf_start = 0;
        self.fed = 0;
        self.produced = 0;
        self.ended = false;
    }

    /// Resamples `input` as one whole signal into `out` (replacing its
    /// contents), exactly as [`resample`] does: [`Self::output_len`] samples
    /// anchored at the first input sample. Reuses this resampler's kernel and
    /// buffers, so callers converting many blocks at one rate build it once.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.reset();
        let len = self.output_len(input.len() as u64) as usize;
        out.clear();
        out.resize(len, 0.0);
        self.push(input);
        self.finish();
        let mut filled = 0;
        while filled < len {
            let n = self.pull(&mut out[filled..]);
            if n == 0 {
                break;
            }
            filled += n;
        }
    }

    /// Output length for `input_frames` input samples.
    pub fn output_len(&self, input_frames: u64) -> u64 {
        (input_frames as f64 / self.step).round() as u64
    }

    pub fn push(&mut self, input: &[f32]) {
        self.buf.extend(input);
        self.fed += input.len() as u64;
    }

    /// No more input: missing samples after the end read as zero.
    pub fn finish(&mut self) {
        self.ended = true;
    }

    /// Fills `out` with as many output samples as the input allows; returns
    /// the count.
    pub fn pull(&mut self, out: &mut [f32]) -> usize {
        if self.passthrough {
            let n = out.len().min(self.buf.len());
            for (o, s) in out.iter_mut().zip(self.buf.drain(..n)) {
                *o = s;
            }
            self.buf_start += n as u64;
            self.produced += n as u64;
            return n;
        }
        let half = self.half as i64;
        let mut n = 0;
        while n < out.len() {
            let p = self.produced as f64 * self.step;
            let base = p.floor() as i64;
            let frac = p - base as f64;
            if !self.ended && base + half >= self.fed as i64 {
                break;
            }
            let mut acc = 0.0f32;
            for j in (1 - half)..=half {
                let w = self.kernel(j as f64 - frac);
                if w != 0.0 {
                    acc += w * self.sample(base + j);
                }
            }
            out[n] = acc;
            n += 1;
            self.produced += 1;
        }
        // Forget input no future output can reach.
        let keep_from = (self.produced as f64 * self.step).floor() as i64 - half;
        if keep_from > self.buf_start as i64 {
            let drop = ((keep_from as u64 - self.buf_start) as usize).min(self.buf.len());
            self.buf.drain(..drop);
            self.buf_start += drop as u64;
        }
        n
    }

    fn sample(&self, index: i64) -> f32 {
        if index < self.buf_start as i64 {
            return 0.0;
        }
        self.buf
            .get((index - self.buf_start as i64) as usize)
            .copied()
            .unwrap_or(0.0)
    }

    fn kernel(&self, t: f64) -> f32 {
        let x = (t + self.half as f64) * PHASES as f64;
        if x < 0.0 {
            return 0.0;
        }
        let i = x.floor() as usize;
        if i + 1 >= self.table.len() {
            return 0.0;
        }
        let f = (x - i as f64) as f32;
        self.table[i] * (1.0 - f) + self.table[i + 1] * f
    }
}

/// Resamples a whole buffer. Converting many blocks at one rate, keep a
/// [`Resampler`] and use [`Resampler::process`] instead.
pub fn resample(input: &[f32], in_rate: f64, out_rate: f64) -> Vec<f32> {
    let mut out = Vec::new();
    Resampler::new(in_rate, out_rate).process(input, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: f64, freq: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin() as f32)
            .collect()
    }

    #[test]
    fn identity_is_a_copy() {
        let x = sine(48_000.0, 440.0, 1000);
        assert_eq!(resample(&x, 48_000.0, 48_000.0), x);
    }

    /// A reused resampler gives every block exactly what a fresh one does,
    /// also after streaming a partial signal, so live anchors stay put.
    #[test]
    fn reused_resampler_matches_a_fresh_one_exactly() {
        let mut reused = Resampler::new(44_100.0, 16_000.0);
        reused.push(&sine(44_100.0, 300.0, 5_000));
        let mut partial = [0.0f32; 100];
        reused.pull(&mut partial);
        let mut out = Vec::new();
        for (n, freq) in [(220_500, 440.0), (1_234, 90.0), (7, 1_000.0), (0, 1.0)] {
            let block = sine(44_100.0, freq, n);
            reused.process(&block, &mut out);
            let fresh = resample(&block, 44_100.0, 16_000.0);
            assert_eq!(out.len() as u64, reused.output_len(n as u64));
            assert_eq!(out, fresh, "{n} samples");
        }
    }

    #[test]
    fn converts_rate_without_delay() {
        let x = sine(48_000.0, 440.0, 48_000);
        let y = resample(&x, 48_000.0, 16_000.0);
        assert_eq!(y.len(), 16_000);
        let want = sine(16_000.0, 440.0, 16_000);
        // Skip the edges, where the kernel sees the zero padding.
        let err = y[200..15_800]
            .iter()
            .zip(&want[200..15_800])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 0.01, "max error {err}");
    }

    #[test]
    fn small_skew_stretches_exactly() {
        // 100 ppm faster device clock.
        let x = sine(48_004.8, 300.0, 480_048);
        let mut r = Resampler::new(48_004.8, 48_000.0);
        assert_eq!(r.output_len(480_048), 480_000);
        // Fed in uneven blocks, pulled in uneven blocks.
        let mut out = Vec::new();
        let mut buf = [0.0f32; 777];
        for block in x.chunks(1_013) {
            r.push(block);
            loop {
                let n = r.pull(&mut buf);
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
        }
        r.finish();
        while out.len() < 480_000 {
            let n = r.pull(&mut buf[..(480_000 - out.len()).min(777)]);
            out.extend_from_slice(&buf[..n]);
        }
        let want = sine(48_000.0, 300.0, 480_000);
        let err = out[100..479_900]
            .iter()
            .zip(&want[100..479_900])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 0.01, "max error {err}");
    }
}
