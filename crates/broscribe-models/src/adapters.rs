//! Runtime adapter traits. All audio is 16 kHz mono f32 (design §6.3).

use broscribe_core::Result;
use broscribe_core::config::Profile;

/// Speech region in seconds from the start of the input.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeechSegment {
    pub start_s: f64,
    pub end_s: f64,
}

pub trait Vad {
    /// Speech regions of the whole input (stateful runtimes are reset per call).
    fn segments(&mut self, samples_16k: &[f32]) -> Result<Vec<SpeechSegment>>;
}

/// A recognised token; times are seconds from the start of the input.
#[derive(Clone, Debug, PartialEq)]
pub struct AsrToken {
    /// BPE piece; a leading `▁` or space starts a new word.
    pub text: String,
    pub start_s: f32,
    pub duration_s: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AsrOutput {
    pub text: String,
    pub tokens: Vec<AsrToken>,
}

pub trait Asr {
    /// Transcribes one segment (under 30 s).
    fn transcribe(&self, samples_16k: &[f32]) -> Result<AsrOutput>;

    /// Transcribes one segment in a known language (BCP 47 tag; `None` lets
    /// the runtime detect it). The pipeline splits its input at language-span
    /// boundaries, so one call never mixes languages. Runtimes without a
    /// language setting ignore it.
    fn transcribe_with(&self, samples_16k: &[f32], language: Option<&str>) -> Result<AsrOutput> {
        let _ = language;
        self.transcribe(samples_16k)
    }

    fn accepts_hotwords(&self) -> bool;

    /// Biases recognition towards `terms` (attendee names, known people,
    /// configured terms; earlier terms take priority when the runtime's prompt
    /// limit is reached). Ignored unless `accepts_hotwords()`; replaces any
    /// previous terms.
    fn set_hotwords(&mut self, terms: &[String]) -> Result<()> {
        let _ = terms;
        Ok(())
    }
}

/// A diarized turn; `speaker` is a 0-based cluster label.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiarSegment {
    pub start_s: f64,
    pub end_s: f64,
    pub speaker: u32,
}

pub trait Diarizer {
    fn diarize(&self, samples_16k: &[f32]) -> Result<Vec<DiarSegment>>;
}

pub trait SpeakerEmbedder {
    /// Registry ID; embeddings are only compared within one model.
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    /// Declared limit of one native inference; callers must subdivide longer audio.
    /// `None` means the runtime has not declared a limit.
    fn max_input_samples(&self) -> Option<usize> {
        None
    }
    /// L2-normalised embedding, or `None` when the input is too short.
    fn embed(&self, samples_16k: &[f32]) -> Result<Option<Vec<f32>>>;
}

/// The models of one profile, loaded once per process.
pub struct ModelSet {
    pub vad: Box<dyn Vad + Send>,
    pub asr: Box<dyn Asr + Send>,
    pub diarizer: Box<dyn Diarizer + Send>,
    pub embedder: Box<dyn SpeakerEmbedder + Send>,
}

pub trait ModelProvider: Send + Sync {
    /// Loads the profile's models; `Error::ModelMissing` when one is not installed.
    fn load(&self, profile: &Profile) -> Result<ModelSet>;

    /// Loads only one VAD model, for commands that need speech regions
    /// without transcribing, diarizing or embedding (e.g. the enrollment
    /// readiness check).
    fn load_vad(&self, model_id: &str) -> Result<Box<dyn Vad + Send>>;

    /// Loads only one speaker-embedding model, for commands that embed audio
    /// without transcribing or diarizing it (e.g. re-embedding after a switch).
    fn load_embedder(&self, model_id: &str) -> Result<Box<dyn SpeakerEmbedder + Send>>;
}

/// Scales `v` to unit length (left as is when all zeros).
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
}
