//! Deterministic synthesis. [`Rng`] is production code (the audio-check test
//! signal's noise burst). The fixtures (speech-like signal, room impulse
//! response, two-track echo scene) exist only for tests: this crate's, or
//! another crate's through the `fixtures` feature. No recorded voices
//! (AGENTS.md privacy rules); every generator is seeded.

#[cfg(any(test, feature = "fixtures"))]
mod fixtures;

#[cfg(any(test, feature = "fixtures"))]
pub use fixtures::{
    EchoScene, EchoSceneSpec, Voice, apply_drift, echo_scene, keep_spans, noise,
    room_impulse_response, speech_like,
};

/// Small seeded PRNG (xorshift64*), so neither the test signal nor the
/// fixtures need an extra crate.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [lo, hi).
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.uniform()
    }

    /// Standard normal (Box–Muller).
    pub fn gaussian(&mut self) -> f64 {
        let u = self.uniform().max(1e-12);
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
}
