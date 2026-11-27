//! Residue gating: attenuate mic frames that are explained by echo.
//!
//! A mic frame is gated when the (aligned) system track has speech and the mic
//! energy is below the echo-path estimate plus a margin. The echo path is a
//! single broadband gain (ERL) applied to a reverberant envelope of the system
//! energy; the ERL is a low percentile of mic/system energy ratios over
//! system-active frames, so double-talk inflates it little. Mic frames of
//! digital silence (a muted or disconnected input) carry no echo-path
//! evidence: they neither enter the ERL nor count as gated. When the gated
//! track is an echo canceller's output, silence is judged on the captured
//! mic too, so sound the canceller generated is never taken for echo.

use crate::activity::{
    SILENCE_ENERGY, VadConfig, db, energy_vad, frame_energies, frame_len, quantile,
};

/// Gating settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GateConfig {
    /// Mic energy must exceed the echo estimate by this much to pass.
    pub margin_db: f32,
    /// Attenuation applied to gated frames.
    pub attenuation_db: f32,
    /// Percentile (0..1) of mic/system energy ratios used as the ERL.
    pub erl_percentile: f32,
    /// Reverberant decay of the system envelope per 10 ms frame.
    pub decay_db_per_frame: f32,
    /// Frames of system history considered (alignment lead + early echo).
    pub lookback_frames: usize,
    /// System frames quieter than this are ignored for the ERL.
    pub min_system_dbfs: f32,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            margin_db: 6.0,
            attenuation_db: 30.0,
            erl_percentile: 0.3,
            decay_db_per_frame: 2.0,
            lookback_frames: 4,
            min_system_dbfs: -60.0,
        }
    }
}

/// Gated mic track and per-frame decisions.
#[derive(Debug, Clone, PartialEq)]
pub struct GateOutput {
    /// Mic track with gated frames attenuated (same length as the input).
    pub output: Vec<f32>,
    /// Per 10 ms frame: whether it was gated.
    pub gated: Vec<bool>,
    /// Per 10 ms frame: whether the system track was active.
    pub system_active: Vec<bool>,
    /// Estimated echo return loss (mic/system energy, dB); `None` when no
    /// frame has both system activity and mic sound, in which case nothing is
    /// gated and `output` equals the input.
    pub erl_db: Option<f32>,
}

impl GateOutput {
    /// Fraction of system-active frames that were gated.
    pub fn gated_fraction(&self) -> f32 {
        gated_fraction(&self.gated, &self.system_active)
    }
}

/// Fraction of system-active frames that were gated.
fn gated_fraction(gated: &[bool], system_active: &[bool]) -> f32 {
    let active = system_active.iter().filter(|a| **a).count();
    if active == 0 {
        return 0.0;
    }
    let gated = gated
        .iter()
        .zip(system_active)
        .filter(|(g, a)| **g && **a)
        .count();
    gated as f32 / active as f32
}

/// Gates `mic` against `aligned_system` (same rate and timeline, see
/// [`crate::align_reference`]).
pub fn residue_gate(
    mic: &[f32],
    aligned_system: &[f32],
    rate: u32,
    vad: &VadConfig,
    cfg: &GateConfig,
) -> GateOutput {
    let decisions = gate_decisions(mic, mic, aligned_system, rate, vad, cfg);
    let mut output = mic.to_vec();
    apply_gate(&mut output, &decisions.gated, rate, cfg);
    GateOutput {
        output,
        gated: decisions.gated,
        system_active: decisions.system_active,
        erl_db: decisions.erl_db,
    }
}

/// [`residue_gate`]'s per-frame decisions, before any sample is touched, so
/// the echo chain can gate a track it owns in place.
pub(crate) struct GateDecisions {
    pub(crate) gated: Vec<bool>,
    pub(crate) system_active: Vec<bool>,
    pub(crate) erl_db: Option<f32>,
}

impl GateDecisions {
    pub(crate) fn gated_fraction(&self) -> f32 {
        gated_fraction(&self.gated, &self.system_active)
    }
}

/// Decisions for gating `input`, whose frames count as sound only where
/// `captured` (the mic as recorded, same timeline; `input` itself unless an
/// echo canceller produced `input`) has sound too.
pub(crate) fn gate_decisions(
    input: &[f32],
    captured: &[f32],
    aligned_system: &[f32],
    rate: u32,
    vad: &VadConfig,
    cfg: &GateConfig,
) -> GateDecisions {
    let frame = frame_len(rate);
    let mic_e = frame_energies(input, frame);
    let captured_e = if std::ptr::eq(input, captured) {
        None
    } else {
        Some(frame_energies(captured, frame))
    };
    let sound = |k: usize| {
        let captured = captured_e.as_ref().map_or(mic_e[k], |c| c[k]);
        mic_e[k] > SILENCE_ENERGY && captured > SILENCE_ENERGY
    };
    let mut sys_e = frame_energies(aligned_system, frame);
    sys_e.resize(mic_e.len(), 0.0);
    let system_active = energy_vad(&sys_e, vad);

    // Reverberant envelope: peak hold with exponential decay, then the max
    // over the look-back window (echo arrives up to a few frames later).
    let decay = 10f32.powf(-cfg.decay_db_per_frame / 10.0);
    let mut held = Vec::with_capacity(sys_e.len());
    let mut h = 0.0f32;
    for &e in &sys_e {
        h = e.max(h * decay);
        held.push(h);
    }
    let env: Vec<f32> = (0..held.len())
        .map(|k| {
            let from = k.saturating_sub(cfg.lookback_frames);
            held[from..=k].iter().copied().fold(0.0, f32::max)
        })
        .collect();

    let ratios: Vec<f32> = (0..env.len())
        .filter(|&k| system_active[k] && sound(k) && db(env[k]) >= cfg.min_system_dbfs)
        .map(|k| mic_e[k] / env[k])
        .collect();
    let erl = quantile(&ratios, cfg.erl_percentile);

    let margin = 10f32.powf(cfg.margin_db / 10.0);
    let gated: Vec<bool> = match erl {
        Some(erl) => (0..env.len())
            .map(|k| system_active[k] && sound(k) && mic_e[k] < erl * env[k] * margin)
            .collect(),
        None => vec![false; env.len()],
    };
    GateDecisions {
        gated,
        system_active,
        erl_db: erl.map(db),
    }
}

/// Applies the gate gains to `samples` in place: per-frame gain with a linear
/// ramp across each frame (no clicks).
pub(crate) fn apply_gate(samples: &mut [f32], gated: &[bool], rate: u32, cfg: &GateConfig) {
    let floor = 10f32.powf(-cfg.attenuation_db / 20.0);
    let mut prev = 1.0f32;
    for (k, chunk) in samples.chunks_mut(frame_len(rate)).enumerate() {
        let target = if gated[k] { floor } else { 1.0 };
        let n = chunk.len() as f32;
        for (i, x) in chunk.iter_mut().enumerate() {
            let g = prev + (target - prev) * (i + 1) as f32 / n;
            *x *= g;
        }
        prev = target;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;
    const HALF: usize = 4 * RATE as usize;

    fn gate(mic: &[f32], system: &[f32]) -> GateOutput {
        residue_gate(
            mic,
            system,
            RATE,
            &VadConfig::default(),
            &GateConfig::default(),
        )
    }

    #[test]
    fn digital_zeros_do_not_zero_the_echo_path_estimate() {
        let system = crate::synth::noise(2 * HALF, -20.0, 21);
        // Muted (exact zeros) for the first half, then echo only (-10.5 dB)
        // with the user talking over it from 5 to 6 s.
        let mut mic = vec![0.0f32; 2 * HALF];
        for (m, s) in mic[HALF..].iter_mut().zip(&system[HALF..]) {
            *m = 0.3 * s;
        }
        let near = crate::synth::noise(RATE as usize, -10.0, 22);
        for (m, n) in mic[5 * RATE as usize..].iter_mut().zip(&near) {
            *m += n;
        }
        let out = gate(&mic, &system);
        let erl = out.erl_db.expect("echo frames measure the echo path");
        assert!((-14.0..-10.0).contains(&erl), "ERL {erl} dB");
        assert_eq!(out.output.len(), mic.len());
        // Muted frames are neither evidence nor gated, and stay zero.
        assert!(out.gated[..400].iter().all(|g| !*g));
        assert!(out.output[..HALF].iter().all(|x| *x == 0.0));
        // Echo-only frames are gated; double-talk frames pass.
        let echo_only = out.gated[400..500]
            .iter()
            .chain(&out.gated[600..800])
            .filter(|g| **g)
            .count();
        assert!(echo_only >= 270, "{echo_only} of 300 echo frames gated");
        assert!(out.gated[500..600].iter().all(|g| !*g));
        // Gated fraction is over system-active frames, muted ones included.
        let fraction = out.gated_fraction();
        assert!((0.3..0.5).contains(&fraction), "{fraction}");
    }

    #[test]
    fn an_all_zero_mic_has_no_echo_path_and_passes_unchanged() {
        let system = crate::synth::noise(HALF, -20.0, 23);
        let mic = vec![0.0f32; HALF];
        let out = gate(&mic, &system);
        assert_eq!(out.erl_db, None);
        assert!(out.gated.iter().all(|g| !*g));
        assert!(out.system_active.iter().all(|a| *a));
        assert_eq!(out.gated_fraction(), 0.0);
        assert_eq!(out.output, mic);
    }

    #[test]
    fn an_untouched_gate_output_is_bit_identical() {
        // Without an echo-path estimate every gain is exactly 1.
        let system = vec![0.0f32; HALF];
        let mic = crate::synth::noise(HALF, -30.0, 24);
        let out = gate(&mic, &system);
        assert_eq!(out.erl_db, None);
        assert_eq!(out.output, mic);
    }
}
