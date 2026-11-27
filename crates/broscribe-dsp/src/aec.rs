//! Offline AEC3 (`sonora`) over whole tracks and ERLE measurement.

use sonora::config::EchoCanceller;
use sonora::{AudioProcessing, Config, StreamConfig};

use crate::DspError;
use crate::activity::{db, frame_energies, frame_len};

/// Acceptance bar for full AEC (plan §12.9): echo return loss enhancement on
/// far-end-only speech. Below it the chain falls back to residue gating and
/// reports degraded mode.
pub const AEC_MIN_ERLE_DB: f32 = 15.0;

/// Runs AEC3 continuously in 10 ms frames over `mic` with `reference` (the
/// aligned system track, same rate, same timeline) as the far end. Returns
/// the echo-cancelled mic track (same length).
///
/// `stream_delay_ms` is the render → capture delay hint; after
/// [`crate::align_reference`] it is the lead used there.
pub fn run_aec(
    mic: &[f32],
    reference: &[f32],
    rate: u32,
    stream_delay_ms: i32,
) -> Result<Vec<f32>, DspError> {
    if !(8_000..=384_000).contains(&rate) || !rate.is_multiple_of(100) {
        return Err(DspError::UnsupportedRate(rate));
    }
    let stream = StreamConfig::new(rate, 1);
    let config = Config {
        echo_canceller: Some(EchoCanceller::default()),
        ..Config::default()
    };
    let mut apm = AudioProcessing::builder()
        .config(config)
        .capture_config(stream)
        .render_config(stream)
        .build();
    let n = stream.num_frames();
    let mut out = vec![0.0f32; mic.len()];
    let mut render_in = vec![0.0f32; n];
    let mut render_out = vec![0.0f32; n];
    let mut capture_in = vec![0.0f32; n];
    let mut capture_out = vec![0.0f32; n];
    let mut start = 0usize;
    while start < mic.len() {
        let end = (start + n).min(mic.len());
        fill(&mut capture_in, &mic[start..end]);
        fill(
            &mut render_in,
            reference
                .get(start..end.min(reference.len()))
                .unwrap_or(&[]),
        );
        apm.process_render_f32(&[&render_in], &mut [&mut render_out])
            .map_err(|e| DspError::Aec(e.to_string()))?;
        // Clamping (outside 0..=500 ms) is reported as an error but processing
        // proceeds with the clamped value.
        let _ = apm.set_stream_delay_ms(stream_delay_ms);
        apm.process_capture_f32(&[&capture_in], &mut [&mut capture_out])
            .map_err(|e| DspError::Aec(e.to_string()))?;
        out[start..end].copy_from_slice(&capture_out[..end - start]);
        start = end;
    }
    Ok(out)
}

/// Copies `src` into the frame buffer and zero-pads the rest.
fn fill(frame: &mut [f32], src: &[f32]) {
    let k = src.len().min(frame.len());
    frame[..k].copy_from_slice(&src[..k]);
    frame[k..].fill(0.0);
}

/// ERLE in dB over the 10 ms frames selected by `mask` (frames where only the
/// far end is active): `10·log10(Σ before² / Σ after²)`. `None` when no frame
/// is selected or `before` is silent there.
pub fn erle_db(before: &[f32], after: &[f32], rate: u32, mask: &[bool]) -> Option<f32> {
    let frame = frame_len(rate);
    let eb = frame_energies(before, frame);
    let ea = frame_energies(after, frame);
    let (mut sb, mut sa) = (0.0f64, 0.0f64);
    for ((b, a), m) in eb.iter().zip(&ea).zip(mask) {
        if *m {
            sb += *b as f64;
            sa += *a as f64;
        }
    }
    if sb <= 0.0 {
        return None;
    }
    Some((10.0 * (sb / sa.max(1e-20)).log10()) as f32)
}

/// ERLE estimate without ground truth: median per-frame reduction (dB) over
/// frames where the system track is active (`system_active`), the mic is
/// above `min_mic_dbfs`, and at least `skip_s` have passed (AEC convergence).
/// A reduction is not proof of echo removal: AEC3's suppressor also removes
/// near-end sound that it takes for echo, so the echo chain accepts the
/// estimate only where the measured frames carry the system's sound.
pub fn estimate_erle_db(
    before: &[f32],
    after: &[f32],
    rate: u32,
    system_active: &[bool],
    skip_s: f64,
    min_mic_dbfs: f32,
) -> Option<f32> {
    measured_erle_db(before, after, rate, system_active, skip_s, min_mic_dbfs).0
}

/// [`estimate_erle_db`] and its per-frame mask of the frames measured.
pub(crate) fn measured_erle_db(
    before: &[f32],
    after: &[f32],
    rate: u32,
    system_active: &[bool],
    skip_s: f64,
    min_mic_dbfs: f32,
) -> (Option<f32>, Vec<bool>) {
    let frame = frame_len(rate);
    let skip = (skip_s * 100.0) as usize;
    let eb = frame_energies(before, frame);
    let ea = frame_energies(after, frame);
    let measured: Vec<bool> = (0..eb.len())
        .map(|i| i >= skip && system_active.get(i) == Some(&true) && db(eb[i]) >= min_mic_dbfs)
        .collect();
    let reductions: Vec<f32> = eb
        .iter()
        .zip(&ea)
        .zip(&measured)
        .filter(|(_, m)| **m)
        .map(|((b, a), _)| db(*b) - db(*a))
        .collect();
    (crate::activity::quantile(&reductions, 0.5), measured)
}
