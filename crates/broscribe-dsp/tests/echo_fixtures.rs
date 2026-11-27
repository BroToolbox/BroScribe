//! Echo-handling acceptance on synthetic fixtures (plan M4 §4, §9):
//! alignment error ≤ 1 ms, AEC ERLE ≥ 15 dB (or a documented degraded
//! fallback), residue-gating precision.

use broscribe_dsp::synth::{EchoSceneSpec, echo_scene};
use broscribe_dsp::{
    AEC_MIN_ERLE_DB, AlignConfig, EchoConfig, EchoHandling, align_reference, erle_db,
    estimate_alignment, frame_energies, frame_len, process_echo, run_aec,
};

fn db(e: f32) -> f32 {
    10.0 * e.max(1e-12).log10()
}

#[test]
fn alignment_error_within_1ms_with_drift_and_double_talk() {
    let mut spec = EchoSceneSpec::laptop(60.0);
    spec.drift_ppm = 100.0;
    spec.near_spans = vec![(12.0, 15.0), (33.0, 36.0), (50.0, 52.0)];
    let scene = echo_scene(&spec);
    let cfg = AlignConfig {
        window_s: 5.0,
        hop_s: 5.0,
        ..AlignConfig::default()
    };
    let a = estimate_alignment(&scene.mic, &scene.system, scene.rate, &cfg).expect("aligned");
    for t in [0.0, 15.0, 30.0, 45.0, 60.0] {
        let truth = spec.echo_delay_s - spec.drift_ppm * 1e-6 * t;
        let err_ms = (a.lag_s_at(t) - truth).abs() * 1000.0;
        assert!(
            err_ms <= 1.0,
            "t={t}: lag {:.4} s vs {truth:.4} s ({err_ms:.3} ms)",
            a.lag_s_at(t)
        );
    }
    assert!(
        (a.drift_ppm + spec.drift_ppm).abs() < 10.0,
        "drift {} ppm",
        a.drift_ppm
    );
}

#[test]
fn alignment_without_drift_is_sample_accurate() {
    let mut spec = EchoSceneSpec::laptop(20.0);
    spec.echo_delay_s = 0.0912;
    let scene = echo_scene(&spec);
    let a = estimate_alignment(
        &scene.mic,
        &scene.system,
        scene.rate,
        &AlignConfig::default(),
    )
    .expect("aligned");
    let err_ms = (a.lag_s_at(10.0) - 0.0912).abs() * 1000.0;
    assert!(err_ms <= 0.1, "{err_ms} ms");
}

#[test]
fn aec_erle_meets_bar_or_falls_back_degraded() {
    let mut spec = EchoSceneSpec::laptop(20.0);
    spec.near_spans = vec![(8.0, 10.5), (14.0, 16.0)];
    let scene = echo_scene(&spec);
    let rate = scene.rate;
    let cfg = EchoConfig::default();
    let a = estimate_alignment(&scene.mic, &scene.system, rate, &cfg.align).expect("aligned");
    let reference = align_reference(
        &scene.system,
        scene.mic.len(),
        rate,
        &a,
        cfg.render_lead_ms / 1000.0,
    );
    let out = run_aec(&scene.mic, &reference, rate, cfg.render_lead_ms as i32).unwrap();

    // Ground truth: far-end-only frames after 3 s of convergence.
    let frame = frame_len(rate);
    let far = frame_energies(&scene.system, frame);
    let near = frame_energies(&scene.near, frame);
    let mask: Vec<bool> = (0..far.len())
        .map(|k| k >= 300 && db(far[k]) > -50.0 && near[k] == 0.0)
        .collect();
    let erle = erle_db(&scene.mic, &out, rate, &mask).expect("far-end-only frames");
    eprintln!("sonora AEC3 ERLE on the synthetic laptop fixture: {erle:.1} dB");

    let result =
        process_echo(&scene.mic, &scene.system, rate, EchoHandling::FullAec, &cfg).unwrap();
    eprintln!("chain report: {:?}", result.report);
    if erle >= AEC_MIN_ERLE_DB {
        assert_eq!(result.report.applied, EchoHandling::FullAec);
        assert!(!result.report.degraded);
    } else {
        // Documented fallback: AEC below the bar → residue gating, degraded.
        assert_eq!(result.report.applied, EchoHandling::ResidueGating);
        assert!(result.report.degraded);
    }
    // Near-end speech survives: energy over near-end frames stays within 6 dB.
    let near_mask: Vec<bool> = near.iter().map(|&e| db(e) > -40.0).collect();
    let loss = erle_db(&scene.mic, &result.mic, rate, &near_mask).expect("near-end frames");
    assert!(loss < 6.0, "near-end attenuated by {loss:.1} dB");
}

#[test]
fn residue_gating_precision_on_speakerphone_fixture() {
    let mut spec = EchoSceneSpec::laptop(40.0);
    // Device AEC leaves a residue 30 dB down.
    spec.echo_gain_db = -36.0;
    spec.far_spans = vec![(0.0, 12.0), (16.0, 28.0), (31.0, 40.0)];
    spec.near_spans = vec![(10.0, 18.0), (26.0, 33.0), (36.0, 38.0)];
    let scene = echo_scene(&spec);
    let rate = scene.rate;
    let result = process_echo(
        &scene.mic,
        &scene.system,
        rate,
        EchoHandling::ResidueGating,
        &EchoConfig::default(),
    )
    .unwrap();
    assert_eq!(result.report.applied, EchoHandling::ResidueGating);
    assert!(!result.report.degraded);

    let frame = frame_len(rate);
    let near = frame_energies(&scene.near, frame);
    let echo = frame_energies(&scene.echo, frame);
    let mic_in = frame_energies(&scene.mic, frame);
    let mic_out = frame_energies(&result.mic, frame);
    let gated: Vec<bool> = mic_in
        .iter()
        .zip(&mic_out)
        .map(|(i, o)| db(*i) - db(*o) > 10.0)
        .collect();

    // Precision: gated frames that carry clear user speech (10 dB over the residue).
    let n_gated = gated.iter().filter(|g| **g).count();
    let wrong = (0..gated.len())
        .filter(|&k| gated[k] && near[k] > 10.0 * (echo[k] + 1e-9))
        .count();
    let precision = 1.0 - wrong as f32 / n_gated.max(1) as f32;
    // Recall: residue-only frames (echo audible, no user) that were gated.
    let residue_only: Vec<usize> = (0..gated.len())
        .filter(|&k| near[k] == 0.0 && db(echo[k]) > -65.0)
        .collect();
    let recall = residue_only.iter().filter(|&&k| gated[k]).count() as f32
        / residue_only.len().max(1) as f32;
    eprintln!("gating: {n_gated} frames gated, precision {precision:.3}, recall {recall:.3}");
    assert!(precision >= 0.95, "precision {precision}");
    assert!(recall >= 0.7, "recall {recall}");
}

#[test]
fn headphones_setup_leaves_mic_untouched() {
    let scene = echo_scene(&EchoSceneSpec::laptop(5.0));
    let out = process_echo(
        &scene.mic,
        &scene.system,
        scene.rate,
        EchoHandling::for_setup(broscribe_dsp::Setup::Headphones),
        &EchoConfig::default(),
    )
    .unwrap();
    assert_eq!(out.mic, scene.mic);
}
