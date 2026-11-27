//! Offline DSP for two-track recordings (design §5, §6.2; plan M4 §4, §12.6–12.7).
//!
//! Everything here is pure computation over sample buffers: no device access,
//! no models. The capture side hands in mono `f32` tracks already placed on the
//! common timeline at one sample rate; the pipeline runs [`process_echo`] on the
//! mic track before ASR and [`classify_mic_segments`] after ASR and voice
//! matching.
//!
//! - [`align`]: system → mic delay and clock drift by GCC-PHAT cross-correlation.
//! - [`aec`]: AEC3 (`sonora`) in continuous 10 ms frames, ERLE measurement.
//! - [`gate`]: residue gating where the system track has speech.
//! - [`echo`]: the per-setup echo-handling chain with the AEC → gating fallback.
//! - [`duplicates`]: echo-duplicate dropping rules over transcript segments.
//! - [`setup`]: device info → audio setup, rules table in TOML.
//! - [`audio_check`]: test signal and leakage/delay analysis.
//! - [`synth`]: the seeded PRNG; with the `fixtures` feature (and in tests),
//!   deterministic synthetic fixtures (speech-like signal, room IR).

pub mod activity;
pub mod aec;
pub mod align;
pub mod audio_check;
pub mod duplicates;
pub mod echo;
mod fft;
pub mod gate;
pub mod setup;
pub mod synth;

pub use activity::{VadConfig, energy_vad, frame_energies, frame_len};
pub use aec::{AEC_MIN_ERLE_DB, erle_db, estimate_erle_db, run_aec};
pub use align::{AlignConfig, Alignment, WindowLag, align_reference, estimate_alignment};
pub use audio_check::{
    AudioCheckReport, AudioCheckThresholds, TestSignalSpec, analyze_audio_check, test_signal,
};
pub use duplicates::{
    DuplicateConfig, MicDecision, MicSegment, SystemSegment, classify_mic_segments,
};
pub use echo::{EchoConfig, EchoHandling, EchoReport, EchoResult, process_echo};
pub use fft::{Correlation, convolve, gcc_phat};
pub use gate::{GateConfig, GateOutput, residue_gate};
pub use setup::{
    DeviceDescriptor, Setup, SetupDetection, SetupInput, SetupRule, SetupRules, Transport,
};

/// Errors from the DSP functions.
#[derive(Debug, thiserror::Error)]
pub enum DspError {
    /// The AEC engine rejected the stream format or a frame.
    #[error("echo canceller: {0}")]
    Aec(String),
    /// A sample rate outside what the operation supports.
    #[error("unsupported sample rate {0} Hz")]
    UnsupportedRate(u32),
    /// A setup rules table that does not parse or validate.
    #[error("setup rules: {0}")]
    Rules(String),
}
