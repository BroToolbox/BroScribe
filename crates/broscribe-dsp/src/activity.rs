//! 10 ms frame energies and an energy voice-activity detector.
//!
//! The detector only answers "does this track carry sound well above its own
//! noise floor"; it is used to find system-track sound for gating, ERLE and
//! the final setup, never to segment speech for ASR (the pipeline's VAD does
//! that). It cannot tell speech from music: any audible system sound can
//! echo into the mic, so music counts as activity. Steady low-level sound
//! (a meeting app's comfort noise, line hiss) stays below the threshold.

/// Samples per 10 ms frame at `rate` (at least 1).
pub fn frame_len(rate: u32) -> usize {
    (rate as usize / 100).max(1)
}

/// Mean-square energy of each `frame`-sample frame; the last partial frame
/// counts as one frame.
pub fn frame_energies(x: &[f32], frame: usize) -> Vec<f32> {
    x.chunks(frame.max(1))
        .map(|c| c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32)
        .collect()
}

/// Energy VAD settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadConfig {
    /// A frame is active when it is this far above the track's noise floor.
    pub above_floor_db: f32,
    /// ... and above this absolute level.
    pub min_level_dbfs: f32,
    /// Frames kept active after the last active frame (bridges syllable gaps).
    pub hangover_frames: usize,
    /// The measured noise floor is capped at this level. A track that is
    /// audible nearly all the time (music, talk without pauses) has no quiet
    /// frames to measure a floor from; its own level is not noise.
    pub max_floor_dbfs: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            above_floor_db: 12.0,
            min_level_dbfs: -55.0,
            hangover_frames: 20,
            max_floor_dbfs: -55.0,
        }
    }
}

/// Mean-square energy at or below which a frame is digital silence (-120 dB).
pub(crate) const SILENCE_ENERGY: f32 = 1e-12;

/// Energy in dBFS for a mean-square value (floored at -120 dB).
pub(crate) fn db(energy: f32) -> f32 {
    10.0 * energy.max(SILENCE_ENERGY).log10()
}

/// Per-frame activity of a track from its [`frame_energies`]. The noise floor
/// is the 10th percentile of the frame levels, capped at
/// [`VadConfig::max_floor_dbfs`], so a continuously audible track (fewer than
/// 10% quiet frames) is active rather than taken for its own floor.
pub fn energy_vad(energies: &[f32], cfg: &VadConfig) -> Vec<bool> {
    if energies.is_empty() {
        return Vec::new();
    }
    let mut levels: Vec<f32> = energies.iter().map(|&e| db(e)).collect();
    levels.sort_by(f32::total_cmp);
    let floor = levels[levels.len() / 10].min(cfg.max_floor_dbfs);
    let threshold = (floor + cfg.above_floor_db).max(cfg.min_level_dbfs);
    let mut out = Vec::with_capacity(energies.len());
    let mut hang = 0usize;
    for &e in energies {
        if db(e) > threshold {
            hang = cfg.hangover_frames + 1;
        }
        out.push(hang > 0);
        hang = hang.saturating_sub(1);
    }
    out
}

/// `p`-quantile (0..=1) of `values` (sorted copy; `None` when empty).
pub(crate) fn quantile(values: &[f32], p: f32) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f32::total_cmp);
    let idx = ((v.len() - 1) as f32 * p.clamp(0.0, 1.0)).round() as usize;
    Some(v[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vad_marks_loud_frames_with_hangover() {
        let mut e = vec![1e-8f32; 100];
        e[50] = 1e-2;
        let cfg = VadConfig {
            hangover_frames: 3,
            ..VadConfig::default()
        };
        let active = energy_vad(&e, &cfg);
        assert!(!active[49]);
        assert!(active[50] && active[53]);
        assert!(!active[54]);
    }

    #[test]
    fn frames_cover_partial_tail() {
        assert_eq!(frame_energies(&[1.0; 25], 10).len(), 3);
        assert_eq!(frame_len(16_000), 160);
    }

    const RATE: u32 = 16_000;

    fn active_fraction(track: &[f32]) -> f32 {
        let active = energy_vad(
            &frame_energies(track, frame_len(RATE)),
            &VadConfig::default(),
        );
        active.iter().filter(|a| **a).count() as f32 / active.len() as f32
    }

    #[test]
    fn continuous_audible_signal_is_active_not_its_own_floor() {
        // Music-like sound without pauses: no quiet frame at all, then with
        // 5% digital silence (under the 10% the floor percentile needs).
        let len = 10 * RATE as usize;
        let music = crate::synth::noise(len, -20.0, 11);
        assert!(active_fraction(&music) > 0.99);
        let mut gapped = music.clone();
        gapped[..len / 20].fill(0.0);
        let fraction = active_fraction(&gapped);
        assert!(fraction > 0.9 && fraction < 0.97, "{fraction}");
    }

    #[test]
    fn steady_comfort_noise_stays_inactive() {
        let len = 10 * RATE as usize;
        for level in [-70.0, -60.0, -50.0] {
            let noise = crate::synth::noise(len, level, 12);
            assert_eq!(active_fraction(&noise), 0.0, "{level} dBFS");
        }
    }

    #[test]
    fn bursts_over_comfort_noise_keep_their_floor() {
        let len = 12 * RATE as usize;
        let mut talk = crate::synth::noise(len, -25.0, 13);
        crate::synth::keep_spans(&mut talk, RATE, &[(1.0, 3.0), (7.0, 9.0)]);
        let floor = crate::synth::noise(len, -65.0, 14);
        let track: Vec<f32> = talk.iter().zip(&floor).map(|(a, b)| a + b).collect();
        let active = energy_vad(
            &frame_energies(&track, frame_len(RATE)),
            &VadConfig::default(),
        );
        // Frames are 10 ms: talk at 1..3 s and 7..9 s, hangover 200 ms.
        assert!(active[110..290].iter().all(|a| *a));
        assert!(active[710..890].iter().all(|a| *a));
        assert!(active[..95].iter().all(|a| !*a));
        assert!(active[330..690].iter().all(|a| !*a));
        assert!(active[930..].iter().all(|a| !*a));
    }
}
