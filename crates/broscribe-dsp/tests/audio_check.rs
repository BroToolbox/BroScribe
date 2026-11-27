//! Audio check analysis on synthetic recordings.

use broscribe_dsp::synth::{noise, room_impulse_response};
use broscribe_dsp::{
    AudioCheckThresholds, EchoHandling, TestSignalSpec, analyze_audio_check, convolve, test_signal,
};

const RATE: u32 = 48_000;

/// What a mic records: lead-in, the played signal through a room at `gain_db`, noise.
fn record(played: &[f32], delay_s: f64, gain_db: f32) -> Vec<f32> {
    let h = room_impulse_response(RATE, delay_s, 0.2, 3.0, 0.25, 9);
    let g = 10f32.powf(gain_db / 20.0);
    let mut rec: Vec<f32> = convolve(played, &h).into_iter().map(|v| v * g).collect();
    rec.resize(played.len() + RATE as usize / 2, 0.0);
    let n = noise(rec.len(), -70.0, 4);
    for (r, n) in rec.iter_mut().zip(n) {
        *r += n;
    }
    rec
}

#[test]
fn laptop_speakers_need_full_aec() {
    let played = test_signal(&TestSignalSpec::new(RATE));
    let rec = record(&played, 0.020, -8.0);
    let r = analyze_audio_check(
        &played,
        &rec,
        RATE,
        Some(48_000),
        &AudioCheckThresholds::default(),
    );
    let delay = r.delay_ms.expect("delay");
    assert!((delay - 20.0).abs() < 1.0, "{r:?}");
    assert_eq!(r.recommendation, EchoHandling::FullAec, "{r:?}");
    assert!(!r.bluetooth_call_profile);
}

#[test]
fn speakerphone_residue_needs_gating() {
    let played = test_signal(&TestSignalSpec::new(RATE));
    let rec = record(&played, 0.012, -35.0);
    let r = analyze_audio_check(
        &played,
        &rec,
        RATE,
        Some(48_000),
        &AudioCheckThresholds::default(),
    );
    assert_eq!(r.recommendation, EchoHandling::ResidueGating, "{r:?}");
}

#[test]
fn headphones_need_nothing_and_call_profile_warns() {
    let played = test_signal(&TestSignalSpec::new(RATE));
    let rec = noise(played.len() + RATE as usize / 2, -70.0, 5);
    let r = analyze_audio_check(
        &played,
        &rec,
        RATE,
        Some(16_000),
        &AudioCheckThresholds::default(),
    );
    assert_eq!(r.recommendation, EchoHandling::None, "{r:?}");
    assert!(r.bluetooth_call_profile);
}

#[test]
fn a_signal_before_playback_stays_inconclusive() {
    let played = test_signal(&TestSignalSpec::new(RATE));
    let advance = 1; // An earlier integer peak, not the zero peak's refinement.
    let mut recorded = played[advance..].to_vec();
    recorded.resize(played.len(), 0.0);
    let report = analyze_audio_check(
        &played,
        &recorded,
        RATE,
        None,
        &AudioCheckThresholds::default(),
    );
    assert!(report.correlation_peak > 0.5, "{report:?}");
    assert!(report.delay_ms.is_none(), "{report:?}");
    assert!(report.leakage_db.is_none(), "{report:?}");
}

#[test]
fn fractional_refinement_of_the_zero_peak_reports_zero_delay() {
    let played = test_signal(&TestSignalSpec::new(RATE));
    let mut recorded = Vec::with_capacity(played.len());
    // Advance by a quarter sample: the sampled peak is still at zero,
    // with a deterministic negative refinement rather than platform roundoff.
    recorded.extend(played.windows(2).map(|p| p[0] * 0.75 + p[1] * 0.25));
    recorded.push(0.0);
    let report = analyze_audio_check(
        &played,
        &recorded,
        RATE,
        None,
        &AudioCheckThresholds::default(),
    );
    assert!(report.correlation_peak > 0.5, "{report:?}");
    assert_eq!(report.delay_ms, Some(0.0), "{report:?}");
    assert!(report.leakage_db.is_some(), "{report:?}");
}
