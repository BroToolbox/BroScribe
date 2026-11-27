//! Deterministic fake models for tests: an energy VAD, a fixed-word ASR, a
//! schedule-driven diarizer and a tone-based speaker embedder (two sine
//! frequencies make two "voices").

use broscribe_core::Result;
use broscribe_core::config::Profile;

use crate::adapters::{
    Asr, AsrOutput, AsrToken, DiarSegment, Diarizer, ModelProvider, ModelSet, SpeakerEmbedder,
    SpeechSegment, Vad, l2_normalize,
};

pub const SAMPLE_RATE: u32 = 16_000;
const SR: f64 = SAMPLE_RATE as f64;

fn duration_s(samples: &[f32]) -> f64 {
    samples.len() as f64 / SR
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt()
}

/// A sine tone at 16 kHz, for building synthetic test audio.
pub fn sine(freq_hz: f32, seconds: f64, amplitude: f32) -> Vec<f32> {
    let n = (seconds * SR).round() as usize;
    (0..n)
        .map(|i| {
            amplitude * (std::f32::consts::TAU * freq_hz * i as f32 / SAMPLE_RATE as f32).sin()
        })
        .collect()
}

/// Speech wherever 32 ms frames exceed an RMS threshold; gaps shorter than
/// `min_silence_s` are bridged, regions shorter than `min_speech_s` dropped.
#[derive(Clone, Debug)]
pub struct EnergyVad {
    pub threshold: f32,
    pub min_silence_s: f64,
    pub min_speech_s: f64,
}

impl Default for EnergyVad {
    fn default() -> Self {
        Self {
            threshold: 0.01,
            min_silence_s: 0.3,
            min_speech_s: 0.1,
        }
    }
}

impl Vad for EnergyVad {
    fn segments(&mut self, samples_16k: &[f32]) -> Result<Vec<SpeechSegment>> {
        const FRAME: usize = 512;
        let total = duration_s(samples_16k);
        let mut out: Vec<SpeechSegment> = Vec::new();
        for (i, frame) in samples_16k.chunks(FRAME).enumerate() {
            if rms(frame) < self.threshold {
                continue;
            }
            let start = (i * FRAME) as f64 / SR;
            let end = (start + frame.len() as f64 / SR).min(total);
            match out.last_mut() {
                Some(last) if start - last.end_s < self.min_silence_s => last.end_s = end,
                _ => out.push(SpeechSegment {
                    start_s: start,
                    end_s: end,
                }),
            }
        }
        out.retain(|s| s.end_s - s.start_s >= self.min_speech_s);
        Ok(out)
    }
}

/// Spreads a fixed list of words evenly over every input; silent or empty
/// input gives no words.
#[derive(Clone, Debug)]
pub struct FakeAsr {
    pub words: Vec<String>,
}

impl FakeAsr {
    pub fn new(words: &[&str]) -> Self {
        Self {
            words: words.iter().map(|w| (*w).to_owned()).collect(),
        }
    }
}

impl Asr for FakeAsr {
    fn transcribe(&self, samples_16k: &[f32]) -> Result<AsrOutput> {
        if self.words.is_empty() || rms(samples_16k) == 0.0 {
            return Ok(AsrOutput::default());
        }
        let slot = (duration_s(samples_16k) / self.words.len() as f64) as f32;
        let tokens = self
            .words
            .iter()
            .enumerate()
            .map(|(i, w)| AsrToken {
                text: format!("\u{2581}{w}"),
                start_s: i as f32 * slot,
                duration_s: Some(slot * 0.8),
            })
            .collect();
        Ok(AsrOutput {
            text: self.words.join(" "),
            tokens,
        })
    }

    fn accepts_hotwords(&self) -> bool {
        false
    }
}

/// Returns a fixed schedule clipped to the input; with an empty schedule the
/// whole input is speaker 0.
#[derive(Clone, Debug, Default)]
pub struct ScheduleDiarizer {
    pub schedule: Vec<DiarSegment>,
}

impl Diarizer for ScheduleDiarizer {
    fn diarize(&self, samples_16k: &[f32]) -> Result<Vec<DiarSegment>> {
        let total = duration_s(samples_16k);
        if self.schedule.is_empty() {
            return Ok(if total > 0.0 {
                vec![DiarSegment {
                    start_s: 0.0,
                    end_s: total,
                    speaker: 0,
                }]
            } else {
                Vec::new()
            });
        }
        Ok(self
            .schedule
            .iter()
            .filter(|s| s.start_s < total)
            .map(|s| DiarSegment {
                end_s: s.end_s.min(total),
                ..*s
            })
            .collect())
    }
}

/// Embeds the dominant frequency (zero-crossing estimate, 20 Hz buckets) as a
/// deterministic pseudo-random unit vector: equal tones give identical
/// vectors, tones in different buckets near-orthogonal ones. Use multiples of
/// 20 Hz well apart (e.g. 200 Hz and 440 Hz). Under 0.25 s or silent → `None`.
#[derive(Clone, Debug)]
pub struct ToneEmbedder {
    pub model_id: String,
    pub dim: usize,
    pub max_samples: Option<usize>,
}

impl ToneEmbedder {
    pub const MIN_SAMPLES: usize = 4_000;

    pub fn new(model_id: impl Into<String>, dim: usize) -> Self {
        Self {
            model_id: model_id.into(),
            dim,
            max_samples: None,
        }
    }

    /// Dominant frequency in Hz from zero crossings.
    pub fn dominant_frequency(samples_16k: &[f32]) -> f64 {
        let crossings = samples_16k
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        crossings as f64 / 2.0 / duration_s(samples_16k)
    }
}

impl SpeakerEmbedder for ToneEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }
    fn max_input_samples(&self) -> Option<usize> {
        self.max_samples
    }

    fn embed(&self, samples_16k: &[f32]) -> Result<Option<Vec<f32>>> {
        if self
            .max_samples
            .is_some_and(|limit| samples_16k.len() > limit)
        {
            return Err(broscribe_core::Error::Inference(
                "synthetic embedder input exceeds its declared capacity".into(),
            ));
        }
        if samples_16k.len() < Self::MIN_SAMPLES || rms(samples_16k) < 1e-4 {
            return Ok(None);
        }
        let bucket = (Self::dominant_frequency(samples_16k) / 20.0).round() as u64;
        let mut state = bucket.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        let mut v: Vec<f32> = (0..self.dim)
            .map(|_| {
                // splitmix64
                state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^= z >> 31;
                (z >> 40) as f32 / (1u64 << 23) as f32 - 1.0
            })
            .collect();
        l2_normalize(&mut v);
        Ok(Some(v))
    }
}

/// Builds fake model sets; the embedder takes the profile's
/// `speaker_embedding` ID so records name the configured model.
#[derive(Clone, Debug)]
pub struct FakeModelProvider {
    pub asr_words: Vec<String>,
    pub schedule: Vec<DiarSegment>,
    pub embed_dim: usize,
}

impl Default for FakeModelProvider {
    fn default() -> Self {
        Self {
            asr_words: vec!["hello".into(), "world".into()],
            schedule: Vec::new(),
            embed_dim: 16,
        }
    }
}

impl ModelProvider for FakeModelProvider {
    fn load(&self, profile: &Profile) -> Result<ModelSet> {
        Ok(ModelSet {
            vad: Box::new(EnergyVad::default()),
            asr: Box::new(FakeAsr {
                words: self.asr_words.clone(),
            }),
            diarizer: Box::new(ScheduleDiarizer {
                schedule: self.schedule.clone(),
            }),
            embedder: Box::new(ToneEmbedder::new(
                profile.speaker_embedding.clone(),
                self.embed_dim,
            )),
        })
    }

    fn load_vad(&self, _model_id: &str) -> Result<Box<dyn Vad + Send>> {
        Ok(Box::new(EnergyVad::default()))
    }

    fn load_embedder(&self, model_id: &str) -> Result<Box<dyn SpeakerEmbedder + Send>> {
        Ok(Box::new(ToneEmbedder::new(model_id, self.embed_dim)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cos(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn vad_finds_tone_between_silences() {
        let mut audio = vec![0.0; 16_000];
        audio.extend(sine(200.0, 2.0, 0.5));
        audio.extend(std::iter::repeat_n(0.0, 16_000));
        let segs = EnergyVad::default().segments(&audio).unwrap();
        assert_eq!(segs.len(), 1);
        assert!((segs[0].start_s - 1.0).abs() < 0.05 && (segs[0].end_s - 3.0).abs() < 0.05);
    }

    #[test]
    fn asr_spreads_words() {
        let out = FakeAsr::new(&["hello", "world"])
            .transcribe(&sine(200.0, 2.0, 0.5))
            .unwrap();
        assert_eq!(out.text, "hello world");
        assert_eq!(out.tokens[1].text, "\u{2581}world");
        assert!((out.tokens[1].start_s - 1.0).abs() < 1e-4);
        assert!(
            FakeAsr::new(&["x"])
                .transcribe(&[0.0; 100])
                .unwrap()
                .tokens
                .is_empty()
        );
    }

    #[test]
    fn diarizer_clips_schedule() {
        let d = ScheduleDiarizer {
            schedule: vec![
                DiarSegment {
                    start_s: 0.0,
                    end_s: 1.0,
                    speaker: 0,
                },
                DiarSegment {
                    start_s: 1.0,
                    end_s: 5.0,
                    speaker: 1,
                },
                DiarSegment {
                    start_s: 9.0,
                    end_s: 10.0,
                    speaker: 0,
                },
            ],
        };
        let segs = d.diarize(&[0.0; 32_000]).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[1].end_s, 2.0);
    }

    #[test]
    fn embedder_separates_tones() {
        let e = ToneEmbedder::new("fake", 32);
        let a1 = e.embed(&sine(200.0, 1.0, 0.5)).unwrap().unwrap();
        let a2 = e.embed(&sine(200.0, 2.0, 0.3)).unwrap().unwrap();
        let b = e.embed(&sine(440.0, 1.0, 0.5)).unwrap().unwrap();
        assert!((cos(&a1, &a1) - 1.0).abs() < 1e-5);
        assert!(cos(&a1, &a2) > 0.99);
        assert!(cos(&a1, &b) < 0.6);
        assert!(e.embed(&sine(200.0, 0.1, 0.5)).unwrap().is_none());
        assert!(e.embed(&[0.0; 16_000]).unwrap().is_none());
    }

    #[test]
    fn provider_uses_profile_embedding_id() {
        let set = FakeModelProvider::default()
            .load(&Profile::default())
            .unwrap();
        assert_eq!(set.embedder.model_id(), "nemo-titanet-large");
        assert!(!set.asr.accepts_hotwords());
        let e = FakeModelProvider::default()
            .load_embedder("nemo-titanet-small")
            .unwrap();
        assert_eq!(e.model_id(), "nemo-titanet-small");
        assert_eq!(e.dim(), 16);
    }

    #[test]
    fn provider_loads_a_vad_alone() {
        let mut vad = FakeModelProvider::default().load_vad("silero-vad").unwrap();
        let mut audio = vec![0.0; 16_000];
        audio.extend(sine(200.0, 2.0, 0.5));
        assert_eq!(vad.segments(&audio).unwrap().len(), 1);
    }
}
