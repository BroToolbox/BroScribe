//! Echo-handling chain for the mic track (design §6.2, plan §4, §12.6, §14.5).
//!
//! align → (full AEC | residue gating | nothing) per setup. Full AEC is
//! accepted only when its measured ERLE reaches [`AEC_MIN_ERLE_DB`] on frames
//! shown to carry the system's sound; otherwise the chain gates the residue
//! and reports **degraded** mode (never an AEC pass). Gating requested for a
//! speakerphone with device echo cancellation is the designed mode for that
//! setup and is not degraded.
//!
//! Energy alone cannot tell echo from the user talking over continuous system
//! sound (music, a remote side that never pauses): the canceller's suppressor
//! and the gate would both take the user for echo. So the frames an ERLE is
//! measured on, and the frames to gate, must also correlate with the aligned
//! system track. An AEC pass without that proof is dropped (its output is not
//! kept). Gating without it, or without any mic sound where the system plays
//! (a muted mic), returns the mic untouched and reported as not processed; an
//! AEC output that missed the bar is not kept either.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::DspError;
use crate::activity::{VadConfig, energy_vad, frame_energies, frame_len};
use crate::aec::{AEC_MIN_ERLE_DB, measured_erle_db, run_aec};
use crate::align::{AlignConfig, Alignment, align_reference, estimate_alignment};
use crate::fft::Correlator;
use crate::gate::{GateConfig, apply_gate, gate_decisions};
use crate::setup::Setup;

/// Echo handling for the mic track; also the audio-check recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoHandling {
    None,
    ResidueGating,
    FullAec,
}

impl EchoHandling {
    /// Default handling per setup (design §5).
    pub fn for_setup(setup: Setup) -> Self {
        match setup {
            Setup::Headphones | Setup::BluetoothCall | Setup::InPerson => Self::None,
            Setup::SpeakerphoneAec => Self::ResidueGating,
            // A hybrid room hears the remote side through room speakers.
            Setup::SpeakersMic | Setup::HybridRoom => Self::FullAec,
        }
    }
}

/// Chain settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoConfig {
    /// Alignment settings; `min_peak` also bounds the normalised amplitude
    /// evidence used to accept echo processing.
    pub align: AlignConfig,
    /// Echo placed this long after the aligned reference (AEC causality).
    pub render_lead_ms: f64,
    /// Minimum ERLE to accept full AEC.
    pub min_erle_db: f32,
    /// AEC convergence time excluded from the ERLE estimate.
    pub convergence_s: f64,
    /// Mic frames quieter than this are excluded from the ERLE estimate.
    pub min_mic_dbfs: f32,
    pub vad: VadConfig,
    pub gate: GateConfig,
}

impl Default for EchoConfig {
    fn default() -> Self {
        Self {
            align: AlignConfig::default(),
            render_lead_ms: 10.0,
            min_erle_db: AEC_MIN_ERLE_DB,
            convergence_s: 2.0,
            min_mic_dbfs: -60.0,
            vad: VadConfig::default(),
            gate: GateConfig::default(),
        }
    }
}

/// What the chain did (stored with the processing metadata).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EchoReport {
    pub requested: EchoHandling,
    /// The handling whose effect is in [`EchoResult::mic`]; `None` when the
    /// mic is returned untouched.
    pub applied: EchoHandling,
    /// Full AEC was requested but not achieved.
    pub degraded: bool,
    /// Measured or the zero alignment (see [`Alignment::is_measured`]).
    pub alignment: Alignment,
    /// AEC ERLE estimate (dB), when AEC ran and the system track had speech.
    pub erle_db: Option<f32>,
    /// Fraction of system-active frames gated (0 when gating did not run).
    pub gated_fraction: f32,
    /// Fraction of 10 ms frames where the system track had sound.
    pub system_active_fraction: f32,
    /// Human-readable reason for a fallback.
    pub reason: Option<String>,
}

/// Processed mic track + report. The mic is borrowed from the input when the
/// chain left it untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct EchoResult<'a> {
    pub mic: Cow<'a, [f32]>,
    pub report: EchoReport,
}

/// Runs the chain on mono `mic` and `system` tracks at one `rate` on one
/// timeline (the capture finalizer's output). The result has `mic.len()`
/// samples.
pub fn process_echo<'a>(
    mic: &'a [f32],
    system: &[f32],
    rate: u32,
    requested: EchoHandling,
    cfg: &EchoConfig,
) -> Result<EchoResult<'a>, DspError> {
    let frame = frame_len(rate);
    let mut sys_e = frame_energies(system, frame);
    sys_e.resize(mic.len().div_ceil(frame), 0.0);
    let system_active = energy_vad(&sys_e, &cfg.vad);
    let system_active_fraction = if system_active.is_empty() {
        0.0
    } else {
        system_active.iter().filter(|a| **a).count() as f32 / system_active.len() as f32
    };
    let mut report = EchoReport {
        requested,
        applied: EchoHandling::None,
        degraded: false,
        alignment: Alignment::zero(),
        erle_db: None,
        gated_fraction: 0.0,
        system_active_fraction,
        reason: None,
    };
    if requested == EchoHandling::None {
        return Ok(EchoResult {
            mic: Cow::Borrowed(mic),
            report,
        });
    }
    if system_active_fraction == 0.0 {
        report.reason = Some("system track has no sound: nothing to cancel".into());
        return Ok(EchoResult {
            mic: Cow::Borrowed(mic),
            report,
        });
    }

    let alignment = estimate_alignment(mic, system, rate, &cfg.align).unwrap_or_else(|| {
        report.reason = Some("system track does not correlate with the mic; zero alignment".into());
        Alignment::zero()
    });
    let lead_s = cfg.render_lead_ms / 1000.0;
    let reference = align_reference(system, mic.len(), rate, &alignment, lead_s);
    report.alignment = alignment;

    // The track to gate: the mic itself, or the AEC output that missed the
    // bar (owned, so it is gated in place).
    let gate_input: Cow<'a, [f32]> = match requested {
        EchoHandling::None => unreachable!("returned above"),
        EchoHandling::ResidueGating => Cow::Borrowed(mic),
        EchoHandling::FullAec => {
            match run_aec(mic, &reference, rate, cfg.render_lead_ms.round() as i32) {
                Ok(cancelled) => {
                    let (erle, measured) = measured_erle_db(
                        mic,
                        &cancelled,
                        rate,
                        &system_active,
                        cfg.convergence_s,
                        cfg.min_mic_dbfs,
                    );
                    report.erle_db = erle;
                    match erle {
                        Some(e) if e >= cfg.min_erle_db => {
                            if frames_carry_echo(mic, &reference, &measured, rate, cfg) {
                                report.applied = EchoHandling::FullAec;
                                return Ok(EchoResult {
                                    mic: Cow::Owned(cancelled),
                                    report,
                                });
                            }
                            // The canceller removed sound that is not the
                            // system's (near-end it took for echo): its output
                            // is dropped and the captured mic is gated.
                            report.degraded = true;
                            report.reason = Some(format!(
                                "AEC removed {e:.1} dB of mic sound that does not carry the \
                                 system track's sound: residue gating"
                            ));
                            Cow::Borrowed(mic)
                        }
                        Some(e) => {
                            report.degraded = true;
                            report.reason = Some(format!(
                                "AEC ERLE {e:.1} dB below {:.1} dB: residue gating",
                                cfg.min_erle_db
                            ));
                            Cow::Owned(cancelled)
                        }
                        // No echo-bearing frames to measure: AEC is not shown
                        // to work, so it never counts as a pass.
                        None => {
                            report.degraded = true;
                            report.reason =
                                Some("AEC ERLE could not be measured: residue gating".into());
                            Cow::Owned(cancelled)
                        }
                    }
                }
                Err(e) => {
                    report.degraded = true;
                    report.reason = Some(format!("{e}: residue gating"));
                    Cow::Borrowed(mic)
                }
            }
        }
    };
    let decisions = gate_decisions(&gate_input, mic, &reference, rate, &cfg.vad, &cfg.gate);
    let unproven = if decisions.erl_db.is_none() {
        Some("no mic sound where the system track plays")
    } else if !decisions.gated.contains(&true) {
        Some("no mic frame is explained by the system track")
    } else if frames_carry_echo(mic, &reference, &decisions.gated, rate, cfg) {
        None
    } else {
        Some("the mic frames to gate do not carry the system track's sound")
    };
    if let Some(why) = unproven {
        return Ok(left_unprocessed(mic, report, why));
    }
    report.applied = EchoHandling::ResidueGating;
    report.gated_fraction = decisions.gated_fraction();
    // Copies only a borrowed mic: the gated track is new audio.
    let mut gated = gate_input.into_owned();
    apply_gate(&mut gated, &decisions.gated, rate, &cfg.gate);
    Ok(EchoResult {
        mic: Cow::Owned(gated),
        report,
    })
}

/// The mic as captured, reported as not processed because of `why` (after any
/// earlier reason).
fn left_unprocessed<'a>(mic: &'a [f32], mut report: EchoReport, why: &str) -> EchoResult<'a> {
    let why = format!("{why}: mic left unprocessed");
    report.reason = Some(match report.reason.take() {
        Some(prior) => format!("{prior}; {why}"),
        None => why,
    });
    EchoResult {
        mic: Cow::Borrowed(mic),
        report,
    }
}

/// Whether selected captured mic frames carry the system's sound at a
/// causal lag. Only the mic is masked: common mask edges and whitening
/// must not turn unrelated narrow-band signals into echo evidence.
fn frames_carry_echo(
    mic: &[f32],
    reference: &[f32],
    frames: &[bool],
    rate: u32,
    cfg: &EchoConfig,
) -> bool {
    let fs = f64::from(rate);
    let frame = frame_len(rate);
    let len = mic.len().min(reference.len());
    let window = ((cfg.align.window_s * fs) as usize).clamp(1, len.max(1));
    let max_lag = (cfg.align.max_lag_s * fs) as usize;
    let min_lag = ((cfg.render_lead_ms / 1000.0 - 0.001) * fs).ceil() as isize;
    let mut correlator = Correlator::new();
    let (mut m, mut r) = (Vec::with_capacity(window), Vec::with_capacity(window));
    for start in (0..len).step_by(window) {
        let end = (start + window).min(len);
        // A tiny tail cannot vouch for the whole recording. Overlap the
        // previous coverage so every evidence window has the same length.
        let start = end.saturating_sub(window);
        let (ms, rs) = (&mic[start..end], &reference[start..end]);
        m.clear();
        r.clear();
        let mut count = 0usize;
        let (mut mic_sum, mut reference_sum) = (0.0f64, 0.0f64);
        for (n, (&x, &y)) in ms.iter().zip(rs).enumerate() {
            if frames[(start + n) / frame] {
                count += 1;
                mic_sum += f64::from(x);
            }
            reference_sum += f64::from(y);
        }
        if count == 0 {
            continue;
        }
        let mic_mean = (mic_sum / count as f64) as f32;
        let reference_mean = (reference_sum / rs.len() as f64) as f32;
        for (n, (&x, &y)) in ms.iter().zip(rs).enumerate() {
            m.push(if frames[(start + n) / frame] {
                x - mic_mean
            } else {
                0.0
            });
            r.push(y - reference_mean);
        }
        if let Some(peak) = correlator.amplitude_correlation(&r, &m, min_lag, max_lag)
            && peak >= f64::from(cfg.align.min_peak)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handling_per_setup() {
        assert_eq!(
            EchoHandling::for_setup(Setup::Headphones),
            EchoHandling::None
        );
        assert_eq!(
            EchoHandling::for_setup(Setup::SpeakerphoneAec),
            EchoHandling::ResidueGating
        );
        assert_eq!(
            EchoHandling::for_setup(Setup::SpeakersMic),
            EchoHandling::FullAec
        );
    }

    #[test]
    fn none_returns_input_unchanged() {
        let mic = vec![0.1f32; 1600];
        let out = process_echo(
            &mic,
            &mic,
            16_000,
            EchoHandling::None,
            &EchoConfig::default(),
        )
        .unwrap();
        assert_eq!(out.mic, mic);
        assert!(matches!(out.mic, Cow::Borrowed(_)));
        assert_eq!(out.report.applied, EchoHandling::None);
    }

    #[test]
    fn silent_system_track_skips_processing() {
        let mic = vec![0.1f32; 1600];
        let sys = vec![0.0f32; 1600];
        let out = process_echo(
            &mic,
            &sys,
            16_000,
            EchoHandling::FullAec,
            &EchoConfig::default(),
        )
        .unwrap();
        assert_eq!(out.mic, mic);
        assert!(matches!(out.mic, Cow::Borrowed(_)));
        assert!(!out.report.degraded);
    }

    #[test]
    fn unmeasurable_erle_is_degraded_not_a_pass() {
        // A silent mic leaves no frame to measure the echo reduction on, and
        // no echo path for the gate: nothing is applied.
        let mic = vec![0.0f32; 16_000 * 4];
        let mut seed = 1u32;
        let sys: Vec<f32> = (0..mic.len())
            .map(|n| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                // Half-second bursts of noise between silences.
                if (n / 8_000) % 2 == 0 {
                    (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
                } else {
                    0.0
                }
            })
            .collect();
        let out = process_echo(
            &mic,
            &sys,
            16_000,
            EchoHandling::FullAec,
            &EchoConfig::default(),
        )
        .unwrap();
        assert_eq!(out.report.erle_db, None);
        assert!(out.report.degraded, "{:?}", out.report);
        assert_eq!(out.report.applied, EchoHandling::None);
        assert_eq!(out.report.gated_fraction, 0.0);
        assert!(matches!(out.mic, Cow::Borrowed(_)));
        assert_eq!(out.mic, mic);
    }

    const RATE: u32 = 16_000;
    /// 37.5 ms, as a laptop speaker → mic path.
    const DELAY: usize = 600;

    /// The system track delayed by [`DELAY`] at -10.5 dB plus a noise floor.
    fn echo_of(system: &[f32], seed: u64) -> Vec<f32> {
        let mut mic = crate::synth::noise(system.len(), -75.0, seed);
        for (m, s) in mic[DELAY..].iter_mut().zip(system) {
            *m += 0.3 * s;
        }
        mic
    }

    #[test]
    fn muted_mic_gating_reports_nothing_applied() {
        // Exact digital zeros on the mic while the system track plays.
        let system = crate::synth::noise(4 * RATE as usize, -20.0, 31);
        let mic = vec![0.0f32; system.len()];
        let out = process_echo(
            &mic,
            &system,
            RATE,
            EchoHandling::ResidueGating,
            &EchoConfig::default(),
        )
        .unwrap();
        assert_eq!(out.report.requested, EchoHandling::ResidueGating);
        assert_eq!(out.report.applied, EchoHandling::None, "{:?}", out.report);
        assert!(!out.report.degraded);
        assert_eq!(out.report.gated_fraction, 0.0);
        let reason = out.report.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("mic left unprocessed"), "{reason}");
        assert!(matches!(out.mic, Cow::Borrowed(_)));
    }

    #[test]
    fn continuous_system_sound_is_handled_not_taken_for_silence() {
        // Music-like system sound with no pause at all.
        let system = crate::synth::noise(6 * RATE as usize, -20.0, 32);
        let mic = echo_of(&system, 33);
        let out = process_echo(
            &mic,
            &system,
            RATE,
            EchoHandling::ResidueGating,
            &EchoConfig::default(),
        )
        .unwrap();
        let report = &out.report;
        assert!(report.system_active_fraction > 0.95, "{report:?}");
        assert_eq!(report.applied, EchoHandling::ResidueGating, "{report:?}");
        assert!(report.alignment.is_measured());
        let err = (report.alignment.lag_s_at(3.0) - DELAY as f64 / f64::from(RATE)).abs();
        assert!(err < 1e-3, "alignment off by {err} s");
        assert!(report.gated_fraction > 0.8, "{report:?}");
        assert!(matches!(out.mic, Cow::Owned(_)));
        assert_eq!(out.mic.len(), mic.len());
    }

    /// The mic must come back as captured, with nothing reported applied.
    fn assert_untouched(mic: &[f32], system: &[f32], requested: EchoHandling) {
        let out = process_echo(mic, system, RATE, requested, &EchoConfig::default()).unwrap();
        let report = &out.report;
        assert!(report.system_active_fraction > 0.95, "{report:?}");
        assert_eq!(report.applied, EchoHandling::None, "{report:?}");
        assert_eq!(report.gated_fraction, 0.0);
        let reason = report.reason.as_deref().unwrap_or_default();
        assert!(reason.contains("mic left unprocessed"), "{reason}");
        assert!(matches!(out.mic, Cow::Borrowed(_)));
        assert_eq!(out.mic, mic);
    }

    #[test]
    fn user_talking_over_continuous_system_sound_is_never_gated() {
        // No echo at all and no echo-only frame: every system-active mic
        // frame is the user, whose level alone looks like an echo path.
        let system = crate::synth::noise(6 * RATE as usize, -20.0, 34);
        let mic = crate::synth::noise(system.len(), -22.0, 35);
        assert_untouched(&mic, &system, EchoHandling::ResidueGating);
        assert_untouched(&mic, &system, EchoHandling::FullAec);
    }

    #[test]
    fn a_short_tail_does_not_turn_unrelated_audio_into_echo() {
        let system = crate::synth::noise(RATE as usize * 61 / 10, -20.0, 34);
        let mic = crate::synth::noise(system.len(), -22.0, 35);
        let cfg = EchoConfig {
            align: AlignConfig {
                window_s: 2.0,
                hop_s: 2.0,
                ..AlignConfig::default()
            },
            ..EchoConfig::default()
        };
        for handling in [EchoHandling::FullAec, EchoHandling::ResidueGating] {
            let out = process_echo(&mic, &system, RATE, handling, &cfg).unwrap();
            assert_eq!(out.report.applied, EchoHandling::None, "{:?}", out.report);
            assert_eq!(out.report.gated_fraction, 0.0);
            assert!(matches!(out.mic, Cow::Borrowed(_)));
            assert_eq!(out.mic, mic);
        }
    }

    /// One sample of a 0.5-amplitude tone at 16 kHz.
    fn tone(hz: f32, n: usize) -> f32 {
        0.5 * (2.0 * std::f32::consts::PI * hz * n as f32 / 16e3).sin()
    }

    #[test]
    fn independent_tones_after_padding_keep_the_mic() {
        // A gap-free system tone; the mic is 1 s of offset padding, then
        // another tone that is not its echo.
        let len = 4 * RATE as usize;
        let system: Vec<f32> = (0..len).map(|n| tone(220.0, n)).collect();
        let mut mic = vec![0.0f32; len];
        for (n, m) in mic.iter_mut().enumerate().skip(RATE as usize) {
            *m = tone(330.0, n);
        }
        assert_untouched(&mic, &system, EchoHandling::FullAec);
        assert_untouched(&mic, &system, EchoHandling::ResidueGating);
    }

    #[test]
    fn a_delayed_tone_echo_is_still_reduced() {
        let system: Vec<f32> = (0..4 * RATE as usize).map(|n| tone(220.0, n)).collect();
        let mic = echo_of(&system, 36);
        let power = |samples: &[f32]| samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
        for handling in [EchoHandling::FullAec, EchoHandling::ResidueGating] {
            let out = process_echo(&mic, &system, RATE, handling, &EchoConfig::default()).unwrap();
            assert_ne!(out.report.applied, EchoHandling::None, "{:?}", out.report);
            assert!(power(&out.mic) < power(&mic), "{:?}", out.report);
        }
    }
}
