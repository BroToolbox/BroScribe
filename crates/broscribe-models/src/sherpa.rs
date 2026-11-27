//! sherpa-onnx implementations of the adapter traits (design §6.3, plan §13.11),
//! built from installed model folders. Every model is loaded once, when the
//! adapter is created; runtime settings come from the registry's `params`.

use std::path::{Path, PathBuf};

use broscribe_core::config::Profile;
use broscribe_core::{Error, Result};
use sherpa_onnx as so;

use crate::adapters::{
    Asr, AsrOutput, AsrToken, DiarSegment, Diarizer, ModelProvider, ModelSet, SpeakerEmbedder,
    SpeechSegment, Vad, l2_normalize,
};
use crate::registry::{ModelEntry, Registry, Role};
use crate::store::{Health, ModelStore};
use crate::whisper::{self, WhisperAsr};

/// Every adapter takes 16 kHz mono input.
pub const SAMPLE_RATE: i32 = 16_000;
/// Speech regions longer than this are split at their quietest frame.
pub const MAX_SEGMENT_S: f64 = 30.0;
/// 32 ms at 16 kHz.
const FRAME: usize = 512;
/// One second of input per VAD feed.
const FEED_CHUNK: usize = 16_000;
/// Registry `runtime` of sherpa-onnx models.
pub const RUNTIME: &str = "onnx";
const PROVIDER: &str = "cpu";
const DEFAULT_THREADS: i64 = 6;

/// Loads registry models from `<data_dir>/models/` through sherpa-onnx, or
/// whisper.cpp for ASR models on the `whisper` runtime.
#[derive(Clone, Debug)]
pub struct SherpaProvider {
    registry: Registry,
    store: ModelStore,
}

impl SherpaProvider {
    pub fn new(registry: Registry, data_dir: &Path) -> Self {
        Self {
            registry,
            store: ModelStore::new(data_dir),
        }
    }

    /// The built-in registry plus `<data_dir>/models.toml`.
    pub fn from_data_dir(data_dir: &Path) -> Result<Self> {
        Ok(Self::new(Registry::load(data_dir)?, data_dir))
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// The entry and its content folder; `Error::ModelMissing` unless it is
    /// installed and matches the registry.
    fn installed(&self, id: &str, role: Role) -> Result<(&ModelEntry, PathBuf)> {
        self.installed_on(id, role, RUNTIME)
    }

    /// As [`Self::installed`], for a model on `runtime`.
    fn installed_on(&self, id: &str, role: Role, runtime: &str) -> Result<(&ModelEntry, PathBuf)> {
        let entry = self.registry.require(id)?;
        if !entry.has_role(role) {
            return Err(Error::InvalidInput(format!(
                "model {id} does not have the {role:?} role"
            )));
        }
        if entry.runtime != runtime {
            return Err(Error::Unsupported(format!(
                "model {id} runs on `{}`, not `{runtime}`",
                entry.runtime
            )));
        }
        match self.store.doctor(entry, false)? {
            Health::Installed => {}
            Health::Missing => {
                return Err(Error::ModelMissing(format!(
                    "{id} is not installed; run `broscribe models pull {id}`"
                )));
            }
            Health::Corrupt { reason } => {
                return Err(Error::ModelMissing(format!(
                    "{id} needs reinstalling ({reason}); run `broscribe models pull {id}`"
                )));
            }
        }
        Ok((entry, self.store.content_dir(entry)?))
    }

    /// Silero VAD with an explicit configuration (registry params, falling
    /// back to plan §13.11's values).
    pub fn load_vad(&self, id: &str) -> Result<SherpaVad> {
        let (entry, root) = self.installed(id, Role::Vad)?;
        let config = so::VadModelConfig {
            silero_vad: so::SileroVadModelConfig {
                model: Some(model_path(entry, &root, "model", &[])?),
                threshold: f32_param(entry, "threshold", 0.5)?,
                min_silence_duration: f32_param(entry, "min_silence_duration", 0.5)?,
                min_speech_duration: f32_param(entry, "min_speech_duration", 0.25)?,
                window_size: i32_param(entry, "window_size", FRAME as i64)?,
                max_speech_duration: f32_param(entry, "max_speech_duration", 28.0)?,
            },
            ten_vad: so::TenVadModelConfig::default(),
            sample_rate: SAMPLE_RATE,
            num_threads: threads(entry, 1)?,
            provider: Some(PROVIDER.into()),
            debug: false,
        };
        let buffer_s = f32_param(entry, "buffer_s", 60.0)?;
        let vad =
            so::VoiceActivityDetector::create(&config, buffer_s).ok_or_else(|| load_failed(id))?;
        Ok(SherpaVad { vad })
    }

    /// Offline transducer recognizer with greedy search and no hotwords.
    pub fn load_asr(&self, id: &str) -> Result<SherpaAsr> {
        let (entry, root) = self.installed(id, Role::Asr)?;
        transducer(id, entry, &root)
    }

    /// The live-preview recognizer: a model with the `live` role (see
    /// [`Registry::live_model`]), loaded like [`Self::load_asr`].
    pub fn load_live(&self, id: &str) -> Result<SherpaAsr> {
        let (entry, root) = self.installed(id, Role::Live)?;
        transducer(id, entry, &root)
    }

    /// pyannote segmentation + the given speaker-embedding model + fast
    /// clustering with an estimated speaker count.
    pub fn load_diarizer(&self, id: &str, embedding_id: &str) -> Result<SherpaDiarizer> {
        let (entry, root) = self.installed(id, Role::Diarization)?;
        let (emb_entry, emb_root) = self.installed(embedding_id, Role::SpeakerEmbedding)?;
        let num_threads = threads(entry, DEFAULT_THREADS)?;
        let config = so::OfflineSpeakerDiarizationConfig {
            segmentation: so::OfflineSpeakerSegmentationModelConfig {
                pyannote: so::OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: Some(model_path(
                        entry,
                        &root,
                        "segmentation",
                        &["model.onnx", "model.int8.onnx"],
                    )?),
                    ..Default::default()
                },
                num_threads,
                debug: false,
                provider: Some(PROVIDER.into()),
            },
            embedding: so::SpeakerEmbeddingExtractorConfig {
                model: Some(model_path(emb_entry, &emb_root, "model", &[])?),
                num_threads,
                debug: false,
                provider: Some(PROVIDER.into()),
            },
            clustering: so::FastClusteringConfig {
                num_clusters: -1,
                threshold: f32_param(entry, "threshold", 1.0)?,
                compute_confidence: false,
            },
            min_duration_on: f32_param(entry, "min_duration_on", 0.3)?,
            min_duration_off: f32_param(entry, "min_duration_off", 0.5)?,
        };
        let sd = so::OfflineSpeakerDiarization::create(&config).ok_or_else(|| load_failed(id))?;
        if sd.sample_rate() != SAMPLE_RATE {
            return Err(Error::Unsupported(format!(
                "model {id} expects {} Hz input, not {SAMPLE_RATE} Hz",
                sd.sample_rate()
            )));
        }
        Ok(SherpaDiarizer { sd })
    }

    pub fn load_speaker_embedder(&self, id: &str) -> Result<SherpaEmbedder> {
        let (entry, root) = self.installed(id, Role::SpeakerEmbedding)?;
        let max_input_samples = entry
            .param_f64("max_input_s")?
            .map(|seconds| {
                let samples = seconds * f64::from(SAMPLE_RATE);
                if !seconds.is_finite()
                    || seconds <= 0.0
                    || samples < 1.0
                    || samples >= usize::MAX as f64
                {
                    return Err(Error::InvalidInput(format!(
                        "model {id}: max_input_s must be a finite positive sample capacity"
                    )));
                }
                Ok(samples as usize)
            })
            .transpose()?;
        let config = so::SpeakerEmbeddingExtractorConfig {
            model: Some(model_path(entry, &root, "model", &[])?),
            num_threads: threads(entry, DEFAULT_THREADS)?,
            debug: false,
            provider: Some(PROVIDER.into()),
        };
        let extractor =
            so::SpeakerEmbeddingExtractor::create(&config).ok_or_else(|| load_failed(id))?;
        let dim = usize::try_from(extractor.dim())
            .ok()
            .filter(|d| *d > 0)
            .ok_or_else(|| Error::Inference(format!("model {id} reports no embedding size")))?;
        Ok(SherpaEmbedder {
            model_id: id.to_owned(),
            extractor,
            dim,
            max_input_samples,
        })
    }

    /// A whisper.cpp model (registry runtime `whisper`): the ggml file named
    /// by parameter `model` (else the single download), `beam_size` (1 =
    /// greedy) and `num_threads`.
    pub fn load_whisper(&self, id: &str) -> Result<WhisperAsr> {
        let (entry, root) = self.installed_on(id, Role::Asr, whisper::RUNTIME)?;
        let path = model_path(entry, &root, "model", &[])?;
        let beam_size = i32_param(entry, "beam_size", whisper::DEFAULT_BEAM_SIZE)?.clamp(1, 16);
        WhisperAsr::load(
            id,
            Path::new(&path),
            threads(entry, DEFAULT_THREADS)?,
            beam_size,
        )
    }

    /// The profile's ASR model on whichever runtime the registry names.
    fn load_any_asr(&self, id: &str) -> Result<Box<dyn Asr + Send>> {
        if self.registry.require(id)?.runtime == whisper::RUNTIME {
            Ok(Box::new(self.load_whisper(id)?))
        } else {
            Ok(Box::new(self.load_asr(id)?))
        }
    }
}

/// An offline transducer recognizer (greedy search, no hotwords) from an
/// installed model folder.
fn transducer(id: &str, entry: &ModelEntry, root: &Path) -> Result<SherpaAsr> {
    let mut config = so::OfflineRecognizerConfig::default();
    config.model_config.transducer = so::OfflineTransducerModelConfig {
        encoder: Some(model_path(
            entry,
            root,
            "encoder",
            &["encoder.int8.onnx", "encoder.onnx"],
        )?),
        decoder: Some(model_path(
            entry,
            root,
            "decoder",
            &["decoder.int8.onnx", "decoder.onnx"],
        )?),
        joiner: Some(model_path(
            entry,
            root,
            "joiner",
            &["joiner.int8.onnx", "joiner.onnx"],
        )?),
    };
    config.model_config.tokens = Some(model_path(entry, root, "tokens", &["tokens.txt"])?);
    config.model_config.model_type = Some(
        entry
            .param_str("model_type")?
            .unwrap_or("nemo_transducer")
            .to_owned(),
    );
    config.model_config.num_threads = threads(entry, DEFAULT_THREADS)?;
    config.model_config.provider = Some(PROVIDER.into());
    config.decoding_method = Some("greedy_search".into());
    config.hotwords_file = None;
    config.hotwords_score = 0.0;
    let recognizer = so::OfflineRecognizer::create(&config).ok_or_else(|| load_failed(id))?;
    Ok(SherpaAsr { recognizer })
}

impl ModelProvider for SherpaProvider {
    fn load(&self, profile: &Profile) -> Result<ModelSet> {
        Ok(ModelSet {
            vad: Box::new(self.load_vad(&profile.vad)?),
            asr: self.load_any_asr(&profile.asr)?,
            diarizer: Box::new(
                self.load_diarizer(&profile.diarization, &profile.speaker_embedding)?,
            ),
            embedder: Box::new(self.load_speaker_embedder(&profile.speaker_embedding)?),
        })
    }

    fn load_vad(&self, model_id: &str) -> Result<Box<dyn Vad + Send>> {
        // The inherent loader of the same name, boxed.
        Ok(Box::new(SherpaProvider::load_vad(self, model_id)?))
    }

    fn load_embedder(&self, model_id: &str) -> Result<Box<dyn SpeakerEmbedder + Send>> {
        Ok(Box::new(self.load_speaker_embedder(model_id)?))
    }
}

fn load_failed(id: &str) -> Error {
    Error::Inference(format!("sherpa-onnx could not load model {id}"))
}

fn f32_param(entry: &ModelEntry, key: &str, default: f32) -> Result<f32> {
    Ok(entry.param_f64(key)?.map_or(default, |v| v as f32))
}

fn i32_param(entry: &ModelEntry, key: &str, default: i64) -> Result<i32> {
    let v = entry.param_i64(key)?.unwrap_or(default);
    i32::try_from(v).map_err(|_| {
        Error::InvalidInput(format!(
            "model {}: parameter `{key}` is out of range",
            entry.id
        ))
    })
}

fn threads(entry: &ModelEntry, default: i64) -> Result<i32> {
    Ok(i32_param(entry, "num_threads", default)?.clamp(1, 64))
}

/// The file named by parameter `key`, else the first existing default, else
/// (no defaults) the entry's single-file downloads; relative to `root`.
fn model_path(entry: &ModelEntry, root: &Path, key: &str, defaults: &[&str]) -> Result<String> {
    let names: Vec<&str> = match entry.param_str(key)? {
        Some(name) => vec![name],
        None if !defaults.is_empty() => defaults.to_vec(),
        None => entry
            .files
            .iter()
            .filter(|f| !f.archive)
            .map(|f| f.file_name())
            .collect(),
    };
    let path = names
        .iter()
        .map(|n| root.join(n))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Error::ModelMissing(format!(
                "model {}: no {} in {}",
                entry.id,
                names.join(" or "),
                root.display()
            ))
        })?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidInput(format!("{} is not valid UTF-8", path.display())))
}

/// Silero VAD; the detector is reset around every call.
pub struct SherpaVad {
    vad: so::VoiceActivityDetector,
}

impl SherpaVad {
    /// Moves every queued segment into `spans` as sample ranges.
    fn drain(&self, spans: &mut Vec<(usize, usize)>, total: usize) {
        while let Some(seg) = self.vad.front() {
            let start = usize::try_from(seg.start()).unwrap_or(0).min(total);
            let n = usize::try_from(seg.n()).unwrap_or(0);
            drop(seg);
            self.vad.pop();
            let end = (start + n).min(total);
            if end > start {
                spans.push((start, end));
            }
        }
    }
}

impl Vad for SherpaVad {
    fn segments(&mut self, samples_16k: &[f32]) -> Result<Vec<SpeechSegment>> {
        if samples_16k.is_empty() {
            return Ok(Vec::new());
        }
        self.vad.clear();
        self.vad.reset();
        let total = samples_16k.len();
        let mut spans = Vec::new();
        for chunk in samples_16k.chunks(FEED_CHUNK) {
            self.vad.accept_waveform(chunk);
            self.drain(&mut spans, total);
        }
        self.vad.flush();
        self.drain(&mut spans, total);
        self.vad.reset();
        let max_len = (MAX_SEGMENT_S * f64::from(SAMPLE_RATE)) as usize;
        let sr = f64::from(SAMPLE_RATE);
        Ok(split_long(samples_16k, &spans, max_len, FRAME)
            .into_iter()
            .map(|(s, e)| SpeechSegment {
                start_s: s as f64 / sr,
                end_s: e as f64 / sr,
            })
            .collect())
    }
}

/// Splits every span longer than `max_len` samples at the centre of its
/// quietest `frame`-sample frame (frames aligned to the span start, the first
/// frame excluded; ties go to the later frame, giving longer pieces),
/// repeating until every piece fits.
pub(crate) fn split_long(
    samples: &[f32],
    spans: &[(usize, usize)],
    max_len: usize,
    frame: usize,
) -> Vec<(usize, usize)> {
    let (max_len, frame) = (max_len.max(1), frame.max(1));
    let mut out = Vec::with_capacity(spans.len());
    for &(start, end) in spans {
        let end = end.min(samples.len());
        let mut cur = start;
        while end.saturating_sub(cur) > max_len {
            let mut best: Option<(f32, usize)> = None;
            let mut f = cur + frame;
            while f + frame <= cur + max_len {
                let energy: f32 = samples[f..f + frame].iter().map(|x| x * x).sum();
                if best.is_none_or(|(b, _)| energy <= b) {
                    best = Some((energy, f + frame / 2));
                }
                f += frame;
            }
            let cut = best.map_or(cur + max_len, |(_, c)| c);
            out.push((cur, cut));
            cur = cut;
        }
        if end > cur {
            out.push((cur, end));
        }
    }
    out
}

/// Parakeet (or another offline transducer) with greedy search.
pub struct SherpaAsr {
    recognizer: so::OfflineRecognizer,
}

impl Asr for SherpaAsr {
    fn transcribe(&self, samples_16k: &[f32]) -> Result<AsrOutput> {
        if samples_16k.is_empty() {
            return Ok(AsrOutput::default());
        }
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE, samples_16k);
        self.recognizer.decode(&stream);
        let result = stream
            .get_result()
            .ok_or_else(|| Error::Inference("speech recognition returned no result".into()))?;
        Ok(AsrOutput {
            text: result.text.trim().to_owned(),
            tokens: tokens_from(
                &result.tokens,
                result.timestamps.as_deref(),
                result.durations.as_deref(),
            )?,
        })
    }

    /// Hotwords need modified beam search, which this adapter never uses.
    fn accepts_hotwords(&self) -> bool {
        false
    }
}

/// Pairs tokens with their start times and (positive) durations.
pub(crate) fn tokens_from(
    tokens: &[String],
    timestamps: Option<&[f32]>,
    durations: Option<&[f32]>,
) -> Result<Vec<AsrToken>> {
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let starts = timestamps
        .filter(|t| t.len() == tokens.len())
        .ok_or_else(|| Error::Inference("speech recognition returned no token times".into()))?;
    Ok(tokens
        .iter()
        .zip(starts)
        .enumerate()
        .map(|(i, (text, &start_s))| AsrToken {
            text: text.clone(),
            start_s,
            duration_s: durations
                .and_then(|d| d.get(i))
                .copied()
                .filter(|d| *d > 0.0),
        })
        .collect())
}

/// pyannote segmentation + embedding clustering.
pub struct SherpaDiarizer {
    sd: so::OfflineSpeakerDiarization,
}

impl Diarizer for SherpaDiarizer {
    fn diarize(&self, samples_16k: &[f32]) -> Result<Vec<DiarSegment>> {
        if samples_16k.is_empty() {
            return Ok(Vec::new());
        }
        let result = self
            .sd
            .process(samples_16k)
            .ok_or_else(|| Error::Inference("speaker diarization returned no result".into()))?;
        result
            .sort_by_start_time()
            .into_iter()
            .map(|s| {
                let speaker = u32::try_from(s.speaker).map_err(|_| {
                    Error::Inference(format!("diarization returned speaker {}", s.speaker))
                })?;
                Ok(DiarSegment {
                    start_s: f64::from(s.start),
                    end_s: f64::from(s.end),
                    speaker,
                })
            })
            .collect()
    }
}

/// Speaker embeddings, L2-normalised.
pub struct SherpaEmbedder {
    model_id: String,
    extractor: so::SpeakerEmbeddingExtractor,
    dim: usize,
    max_input_samples: Option<usize>,
}

impl SpeakerEmbedder for SherpaEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.dim
    }
    fn max_input_samples(&self) -> Option<usize> {
        self.max_input_samples
    }

    fn embed(&self, samples_16k: &[f32]) -> Result<Option<Vec<f32>>> {
        if samples_16k.is_empty() {
            return Ok(None);
        }
        if let Some(limit) = self.max_input_samples
            && samples_16k.len() > limit
        {
            return Err(Error::Inference(format!(
                "speaker-embedding input exceeds model {} capacity ({} samples, limit {limit}); \
                 split audio into bounded ranges",
                self.model_id,
                samples_16k.len()
            )));
        }
        let stream = self.extractor.create_stream().ok_or_else(|| {
            Error::Inference("could not create a speaker-embedding stream".into())
        })?;
        stream.accept_waveform(SAMPLE_RATE, samples_16k);
        stream.input_finished();
        if !self.extractor.is_ready(&stream) {
            return Ok(None);
        }
        let mut v = self
            .extractor
            .compute(&stream)
            .ok_or_else(|| Error::Inference("speaker embedding failed".into()))?;
        if v.len() != self.dim {
            return Err(Error::Inference(format!(
                "speaker embedding has {} values, expected {}",
                v.len(),
                self.dim
            )));
        }
        l2_normalize(&mut v);
        Ok(Some(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_long_cuts_at_the_quietest_frame() {
        // 100 frames of 10 samples; frame 37 is silent.
        let mut samples = vec![0.5f32; 1_000];
        samples[370..380].fill(0.0);
        let out = split_long(&samples, &[(0, 1_000)], 700, 10);
        assert_eq!(out, [(0, 375), (375, 1_000)]);
        // Short spans pass through; ends are clamped to the input.
        assert_eq!(
            split_long(&samples, &[(5, 50), (900, 2_000)], 700, 10),
            [(5, 50), (900, 1_000)]
        );
    }

    #[test]
    fn split_long_handles_many_pieces() {
        let samples = vec![0.1f32; 10_000];
        let out = split_long(&samples, &[(100, 10_000)], 1_000, 16);
        assert!(out.len() >= 10);
        assert_eq!(out.first().unwrap().0, 100);
        assert_eq!(out.last().unwrap().1, 10_000);
        for w in out.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
        assert!(out.iter().all(|(s, e)| e > s && e - s <= 1_000));
    }

    #[test]
    fn tokens_carry_times_and_durations() {
        let toks = vec!["\u{2581}hi".to_owned(), "\u{2581}there".to_owned()];
        let out = tokens_from(&toks, Some(&[0.1, 0.5]), Some(&[0.2, 0.0])).unwrap();
        assert_eq!(out[0].start_s, 0.1);
        assert_eq!(out[0].duration_s, Some(0.2));
        assert_eq!(out[1].duration_s, None);
        assert_eq!(
            tokens_from(&toks, Some(&[0.1, 0.5]), None).unwrap()[0].duration_s,
            None
        );
        assert_eq!(
            tokens_from(&toks, None, None).unwrap_err().code(),
            "inference"
        );
        assert!(tokens_from(&[], None, None).unwrap().is_empty());
    }

    #[test]
    fn uninstalled_models_are_reported_missing() {
        let data = tempfile::tempdir().unwrap();
        let p = SherpaProvider::new(Registry::builtin().unwrap(), data.path());
        assert_eq!(
            p.load_vad("silero-vad").err().unwrap().code(),
            "model_missing"
        );
        assert_eq!(p.load_vad("nope").err().unwrap().code(), "model_missing");
        assert_eq!(
            p.load_vad("parakeet-tdt-0.6b-v3").err().unwrap().code(),
            "invalid_input"
        );
        assert_eq!(
            p.load_embedder("nemo-titanet-large").err().unwrap().code(),
            "model_missing"
        );
        assert_eq!(
            ModelProvider::load_vad(&p, "silero-vad")
                .err()
                .unwrap()
                .code(),
            "model_missing"
        );
        assert_eq!(
            p.load(&Profile::default()).err().unwrap().code(),
            "model_missing"
        );
    }

    /// Real models (`#[ignore]`d): installed into `BROSCRIBE_DATA_DIR`, or a
    /// temporary folder, from `BROSCRIBE_MODEL_MIRROR` when set, else the
    /// network.
    mod real {
        use std::sync::Mutex;

        use super::*;
        use crate::pull::{PullOptions, pull};

        static PULL: Mutex<()> = Mutex::new(());

        fn provider(ids: &[&str]) -> SherpaProvider {
            let data_dir = std::env::var_os("BROSCRIBE_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("broscribe-model-tests"));
            let registry = Registry::builtin().unwrap();
            let _guard = PULL.lock().unwrap_or_else(|e| e.into_inner());
            let opts = PullOptions::from_env(false);
            let execution = crate::ExecutionContext::new(
                broscribe_platform_native::NativePlatform::process_tree_control(),
                broscribe_platform_native::NativePlatform::trust_store(),
            );
            for id in ids {
                pull(
                    registry.get(id).unwrap(),
                    &data_dir,
                    &opts,
                    &mut |_, _| {},
                    &execution,
                )
                .unwrap();
            }
            SherpaProvider::new(registry, &data_dir)
        }

        /// The archive's `en.wav` (24 kHz) at 16 kHz, resampled chunk by chunk
        /// with a flush at the end, as the pipeline does per decoded packet.
        fn en_wav(p: &SherpaProvider) -> Vec<f32> {
            let entry = p.registry().get("parakeet-tdt-0.6b-v3").unwrap();
            let path = p.store.content_dir(entry).unwrap().join("test_wavs/en.wav");
            let wave = so::Wave::read(path.to_str().unwrap()).expect("read en.wav");
            let samples = wave.samples();
            if wave.sample_rate() == SAMPLE_RATE {
                return samples.to_vec();
            }
            let resampler = so::LinearResampler::create(wave.sample_rate(), SAMPLE_RATE)
                .expect("create resampler");
            let mut out = Vec::with_capacity(
                samples.len() * SAMPLE_RATE as usize / wave.sample_rate() as usize + 1,
            );
            let chunks: Vec<&[f32]> = samples.chunks(4_096).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                out.extend(resampler.resample(chunk, i + 1 == chunks.len()));
            }
            let expected =
                samples.len() as f64 * f64::from(SAMPLE_RATE) / f64::from(wave.sample_rate());
            assert!(
                (out.len() as f64 - expected).abs() <= 2.0,
                "{} resampled samples, expected {expected}",
                out.len()
            );
            out
        }

        #[test]
        #[ignore = "needs downloaded models"]
        fn asr_transcribes_the_test_wav() {
            let p = provider(&["parakeet-tdt-0.6b-v3"]);
            let asr = p.load_asr("parakeet-tdt-0.6b-v3").unwrap();
            let audio = en_wav(&p);
            let duration = audio.len() as f32 / SAMPLE_RATE as f32;
            let out = asr.transcribe(&audio).unwrap();
            eprintln!(
                "asr: {} chars, {} tokens, {duration:.2} s",
                out.text.len(),
                out.tokens.len()
            );
            assert!(!out.text.is_empty());
            assert!(!out.tokens.is_empty());
            assert!(out.tokens.windows(2).all(|w| w[0].start_s <= w[1].start_s));
            assert!(
                out.tokens
                    .iter()
                    .all(|t| t.start_s >= 0.0 && t.start_s <= duration)
            );
            assert!(!asr.accepts_hotwords());
        }

        #[test]
        #[ignore = "needs downloaded models"]
        fn embedder_returns_a_unit_vector() {
            let p = provider(&["parakeet-tdt-0.6b-v3", "nemo-titanet-large"]);
            let e = p.load_embedder("nemo-titanet-large").unwrap();
            assert_eq!(e.model_id(), "nemo-titanet-large");
            assert_eq!(e.dim(), 192);
            let v = e.embed(&en_wav(&p)).unwrap().expect("embedding");
            assert_eq!(v.len(), 192);
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "norm {norm}");
            assert_eq!(e.embed(&[]).unwrap(), None);
        }

        #[test]
        #[ignore = "needs downloaded models"]
        fn vad_finds_speech_between_silences() {
            let p = provider(&["parakeet-tdt-0.6b-v3", "silero-vad"]);
            let mut vad = p.load_vad("silero-vad").unwrap();
            let mut audio = vec![0.0f32; 16_000];
            audio.extend(en_wav(&p));
            audio.extend(std::iter::repeat_n(0.0, 16_000));
            let total = audio.len() as f64 / 16_000.0;
            let segs = vad.segments(&audio).unwrap();
            eprintln!("vad: {} segments over {total:.2} s: {segs:?}", segs.len());
            assert!(!segs.is_empty());
            assert!(segs[0].start_s >= 0.5, "{segs:?}");
            assert!(
                segs.iter()
                    .all(|s| s.end_s <= total && s.end_s - s.start_s <= 30.0)
            );
            // State is reset between calls.
            assert_eq!(vad.segments(&audio).unwrap(), segs);
        }

        #[test]
        #[ignore = "needs downloaded models"]
        fn diarizer_and_full_profile_load() {
            let p = provider(&[
                "silero-vad",
                "parakeet-tdt-0.6b-v3",
                "pyannote-segmentation-3.0",
                "nemo-titanet-large",
            ]);
            let set = p.load(&Profile::default()).unwrap();
            let segs = set.diarizer.diarize(&en_wav(&p)).unwrap();
            let mut speakers: Vec<u32> = segs.iter().map(|s| s.speaker).collect();
            speakers.sort_unstable();
            speakers.dedup();
            eprintln!(
                "diarizer: {} segments, {} speakers",
                segs.len(),
                speakers.len()
            );
            assert!(!segs.is_empty());
            assert!(segs.iter().all(|s| s.end_s > s.start_s));
            assert!(set.diarizer.diarize(&[]).unwrap().is_empty());
        }
    }
}

#[cfg(test)]
mod native_capacity_tests {
    use super::*;

    #[test]
    #[ignore = "requires the installed native speaker model in BROSCRIBE_DATA_DIR"]
    fn declared_capacity_rejects_oversized_native_input_without_abort() {
        let data = std::env::var_os("BROSCRIBE_DATA_DIR").expect("model data directory");
        let provider = SherpaProvider::new(Registry::builtin().unwrap(), Path::new(&data));
        let model = provider
            .load_speaker_embedder("nemo-titanet-large")
            .unwrap();
        let limit = model
            .max_input_samples()
            .expect("declared runtime capacity");
        let input = vec![0.2; limit + 60 * SAMPLE_RATE as usize];
        let error = model.embed(&input).unwrap_err();
        assert_eq!(error.code(), "inference");
    }
}
