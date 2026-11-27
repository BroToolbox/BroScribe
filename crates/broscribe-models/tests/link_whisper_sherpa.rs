//! Link check (plan §12.7, §12.12): sherpa-onnx and whisper.cpp in one test
//! binary. Building it in debug and release proves the two native libraries
//! link together without symbol clashes; the default test calls into both,
//! and the ignored one loads a real model of each in one process.

use std::path::PathBuf;

use broscribe_models::{Asr, ExecutionContext, PullOptions, Registry, SherpaProvider, pull};
use broscribe_platform_native::NativePlatform;

#[test]
fn both_native_runtimes_are_callable_from_one_binary() {
    assert!(!sherpa_onnx::version().is_empty());
    let config = sherpa_onnx::OfflineRecognizerConfig::default();
    assert!(config.model_config.transducer.encoder.is_none());

    assert_eq!(whisper_rs::get_lang_id("en"), Some(0));
    let missing = std::env::temp_dir().join("broscribe-no-such-model.bin");
    assert!(
        whisper_rs::WhisperContext::new_with_params(
            &missing,
            whisper_rs::WhisperContextParameters::default()
        )
        .is_err()
    );
}

#[test]
#[ignore = "needs downloaded models"]
fn parakeet_and_whisper_load_in_one_process() {
    let data_dir = std::env::var_os("BROSCRIBE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("broscribe-model-tests"));
    let registry = Registry::builtin().unwrap();
    let opts = PullOptions::from_env(false);
    let execution = ExecutionContext::new(
        NativePlatform::process_tree_control(),
        NativePlatform::trust_store(),
    );
    for id in ["parakeet-tdt-0.6b-v3", "whisper-large-v3-turbo"] {
        pull(
            registry.get(id).unwrap(),
            &data_dir,
            &opts,
            &mut |_, _| {},
            &execution,
        )
        .unwrap();
    }
    let p = SherpaProvider::new(registry, &data_dir);
    let parakeet = p.load_asr("parakeet-tdt-0.6b-v3").unwrap();
    let whisper = p.load_whisper("whisper-large-v3-turbo").unwrap();
    // One second of a quiet 440 Hz tone: both runtimes run; text is not checked.
    let tone: Vec<f32> = (0..16_000)
        .map(|i| 0.05 * (i as f32 * 440.0 * std::f32::consts::TAU / 16_000.0).sin())
        .collect();
    parakeet.transcribe(&tone).unwrap();
    whisper.transcribe_with(&tone, Some("en")).unwrap();
}
