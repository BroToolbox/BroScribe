//! The live-preview preflight behind `models doctor --live` (design §8.3):
//! loads the profile's VAD and declared live model from their installed
//! files and runs them over a deterministic synthetic fixture, so a broken
//! or missing live model shows up before a recording needs it. Nothing is
//! downloaded or installed and no recorded audio is used.

use std::time::Instant;

use broscribe_core::config::Profile;
use broscribe_core::{Error, Result};
use serde::Serialize;

use crate::adapters::{Asr, Vad};
use crate::sherpa::{SAMPLE_RATE, SherpaProvider};

/// Length of [`synthetic_fixture`] in seconds.
pub const FIXTURE_S: f64 = 3.0;

/// What the preflight ran and measured.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LiveCheck {
    pub vad: String,
    pub live_model: String,
    pub fixture_s: f64,
    /// Speech regions the VAD found in the fixture (a tone may count as
    /// none: only that the VAD ran is checked).
    pub speech_regions: usize,
    /// Time to load both models.
    pub load_ms: u64,
    /// Time to run the VAD and the recognizer over the fixture.
    pub run_ms: u64,
}

/// 0.5 s of silence, 2 s of a harmonic tone with a syllable-rate (4 Hz)
/// envelope, 0.5 s of silence, at 16 kHz: deterministic and synthetic, never
/// a recording.
pub fn synthetic_fixture() -> Vec<f32> {
    let rate = SAMPLE_RATE as f32;
    let total = (FIXTURE_S * f64::from(SAMPLE_RATE)) as usize;
    let (from, to) = (SAMPLE_RATE as usize / 2, total - SAMPLE_RATE as usize / 2);
    (0..total)
        .map(|i| {
            if i < from || i >= to {
                return 0.0;
            }
            let t = (i - from) as f32 / rate;
            let envelope = 0.5 - 0.5 * (std::f32::consts::TAU * 4.0 * t).cos();
            let voice: f32 = [(150.0, 1.0), (300.0, 0.5), (450.0, 0.25)]
                .iter()
                .map(|(f, a)| a * (std::f32::consts::TAU * f * t).sin())
                .sum();
            0.2 * envelope * voice
        })
        .collect()
}

/// Runs `vad` and the live recognizer `asr` over `audio` (shorter than the
/// recognizer's 30 s segment limit): the VAD's regions must lie inside the
/// input and recognition must complete; the text is not judged. Returns the
/// number of speech regions.
pub fn run_fixture(vad: &mut dyn Vad, asr: &dyn Asr, audio: &[f32]) -> Result<usize> {
    if audio.is_empty() {
        return Err(Error::InvalidInput("the live fixture is empty".into()));
    }
    let duration = audio.len() as f64 / f64::from(SAMPLE_RATE);
    let regions = vad.segments(audio)?;
    // One 32 ms frame of slack for frame-aligned ends.
    let slack = 512.0 / f64::from(SAMPLE_RATE);
    if regions.iter().any(|r| {
        !(r.start_s.is_finite() && r.end_s.is_finite())
            || r.start_s < 0.0
            || r.end_s <= r.start_s
            || r.end_s > duration + slack
    }) {
        return Err(Error::Inference(
            "the VAD returned a region outside the live fixture".into(),
        ));
    }
    asr.transcribe(audio)?;
    Ok(regions.len())
}

impl SherpaProvider {
    /// Loads `profile`'s VAD and its declared live model (see
    /// [`crate::Registry::live_model`]) and runs them over `audio`.
    /// `Error::ModelMissing` names a model that is not installed.
    pub fn live_check(&self, profile: &Profile, audio: &[f32]) -> Result<LiveCheck> {
        let live = self.registry().live_model(profile)?.id.clone();
        let started = Instant::now();
        let mut vad = self.load_vad(&profile.vad)?;
        let asr = self.load_live(&live)?;
        let load_ms = started.elapsed().as_millis() as u64;
        let started = Instant::now();
        let speech_regions = run_fixture(&mut vad, &asr, audio)?;
        Ok(LiveCheck {
            vad: profile.vad.clone(),
            live_model: live,
            fixture_s: audio.len() as f64 / f64::from(SAMPLE_RATE),
            speech_regions,
            load_ms,
            run_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{AsrOutput, SpeechSegment};
    use crate::fakes::{EnergyVad, FakeAsr};
    use crate::registry::Registry;

    #[test]
    fn fixture_is_deterministic_synthetic_speech_between_silences() {
        let a = synthetic_fixture();
        assert_eq!(a, synthetic_fixture());
        assert_eq!(a.len(), 48_000);
        assert!(a[..8_000].iter().all(|x| *x == 0.0));
        assert!(a[40_000..].iter().all(|x| *x == 0.0));
        assert!(a[8_000..40_000].iter().any(|x| x.abs() > 0.05));
        assert!(a.iter().all(|x| x.is_finite() && x.abs() <= 1.0));
    }

    #[test]
    fn fixture_runs_through_vad_and_recognizer() {
        let audio = synthetic_fixture();
        let regions =
            run_fixture(&mut EnergyVad::default(), &FakeAsr::new(&["tone"]), &audio).unwrap();
        assert!(regions >= 1);
        assert_eq!(
            run_fixture(&mut EnergyVad::default(), &FakeAsr::new(&["x"]), &[])
                .unwrap_err()
                .code(),
            "invalid_input"
        );
    }

    struct Broken;
    impl Asr for Broken {
        fn transcribe(&self, _: &[f32]) -> Result<AsrOutput> {
            Err(Error::Inference("synthetic recognizer failure".into()))
        }
        fn accepts_hotwords(&self) -> bool {
            false
        }
    }

    struct Outside;
    impl Vad for Outside {
        fn segments(&mut self, _: &[f32]) -> Result<Vec<SpeechSegment>> {
            Ok(vec![SpeechSegment {
                start_s: 1.0,
                end_s: 99.0,
            }])
        }
    }

    #[test]
    fn a_failing_recognizer_or_vad_fails_the_check() {
        let audio = synthetic_fixture();
        assert_eq!(
            run_fixture(&mut EnergyVad::default(), &Broken, &audio)
                .unwrap_err()
                .code(),
            "inference"
        );
        assert_eq!(
            run_fixture(&mut Outside, &FakeAsr::new(&["x"]), &audio)
                .unwrap_err()
                .code(),
            "inference"
        );
    }

    #[test]
    fn live_check_names_missing_models_without_installing_them() {
        let data = tempfile::tempdir().unwrap();
        let registry = Registry::builtin().unwrap();
        let multi = registry
            .profile("multilingual")
            .unwrap()
            .apply(&Profile::default());
        let provider = SherpaProvider::new(registry, data.path());
        let err = provider
            .live_check(&multi, &synthetic_fixture())
            .unwrap_err();
        assert_eq!(err.code(), "model_missing");
        assert!(
            std::fs::read_dir(data.path()).unwrap().next().is_none(),
            "a preflight never creates or downloads anything"
        );
    }

    /// The real VAD and live model of the multilingual profile, from
    /// `BROSCRIBE_DATA_DIR` (models pulled beforehand).
    #[test]
    #[ignore = "needs downloaded models"]
    fn real_live_check_runs_the_installed_models() {
        let data = std::path::PathBuf::from(std::env::var("BROSCRIBE_DATA_DIR").unwrap());
        let provider = SherpaProvider::from_data_dir(&data).unwrap();
        let multi = provider
            .registry()
            .profile("multilingual")
            .unwrap()
            .apply(&Profile::default());
        let check = provider.live_check(&multi, &synthetic_fixture()).unwrap();
        assert_eq!(check.live_model, "parakeet-tdt-0.6b-v3");
        assert_eq!(check.fixture_s, FIXTURE_S);
    }
}
