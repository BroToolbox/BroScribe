//! Test helpers: synthetic WAV files and fake models (no real data).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use broscribe_core::config::{Config, Profile};
use broscribe_core::{Library, Result};
use broscribe_models::fakes::{FakeModelProvider, ToneEmbedder, sine};
use broscribe_models::{DiarSegment, Diarizer, ExecutionContext, ModelProvider, ModelSet};
use broscribe_platform::DecoderChain;
use broscribe_platform::fakes::{FakeProcessProbe, FakeProcessTreeControl, FakeTrustStore};
use broscribe_platform::symphonia_decoder::SymphoniaDecoder;

use crate::Env;

/// A model execution context of explicit fakes: synthetic process-tree
/// control and a trust store without roots (HTTPS refused before
/// connecting; loopback, agent CLIs and local models unaffected).
pub fn execution() -> ExecutionContext {
    execution_trusting(Arc::default())
}

/// [`execution`] reading its roots from `trust` (shared so a test can count
/// reads or provide roots).
pub fn execution_trusting(trust: Arc<FakeTrustStore>) -> ExecutionContext {
    ExecutionContext::new(Arc::new(FakeProcessTreeControl::default()), trust)
}

/// Writes 16-bit PCM mono WAV.
pub fn write_wav(path: &Path, rate: u32, samples: &[f32]) {
    let data_len = u32::try_from(samples.len() * 2).unwrap();
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for &x in samples {
        let v = (x.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// Tone frequencies of the synthetic voices (multiples of 20 Hz, far apart).
pub const VOICES: [f32; 4] = [200.0, 440.0, 760.0, 1100.0];

/// Diarizes synthetic audio by tone: 0.25 s windows labelled with the index
/// of the nearest [`VOICES`] frequency, silence skipped, equal neighbours joined.
pub struct ToneDiarizer;

impl Diarizer for ToneDiarizer {
    fn diarize(&self, samples_16k: &[f32]) -> Result<Vec<DiarSegment>> {
        const WIN: usize = 4_000;
        let mut out: Vec<DiarSegment> = Vec::new();
        for (i, w) in samples_16k.chunks(WIN).enumerate() {
            let rms = (w.iter().map(|x| x * x).sum::<f32>() / w.len() as f32).sqrt();
            if w.len() < WIN / 2 || rms < 1e-3 {
                continue;
            }
            let f = ToneEmbedder::dominant_frequency(w);
            let label = VOICES
                .iter()
                .enumerate()
                .min_by(|a, b| {
                    (f64::from(*a.1) - f)
                        .abs()
                        .total_cmp(&(f64::from(*b.1) - f).abs())
                })
                .map(|(k, _)| u32::try_from(k).unwrap())
                .unwrap();
            let start = (i * WIN) as f64 / 16_000.0;
            let end = start + w.len() as f64 / 16_000.0;
            match out.last_mut() {
                Some(last) if last.speaker == label && (last.end_s - start).abs() < 1e-9 => {
                    last.end_s = end;
                }
                _ => out.push(DiarSegment {
                    start_s: start,
                    end_s: end,
                    speaker: label,
                }),
            }
        }
        Ok(out)
    }
}

/// Fake models with the [`ToneDiarizer`] and the profile's embedding model ID.
pub fn models() -> ModelSet {
    let mut set = FakeModelProvider::default()
        .load(&Profile::default())
        .unwrap();
    set.diarizer = Box::new(ToneDiarizer);
    set
}

/// A 16 kHz recording of `(voice index, seconds)` turns; voice `None` is silence.
pub fn conversation(turns: &[(Option<usize>, f64)]) -> Vec<f32> {
    let mut audio = Vec::new();
    for &(voice, secs) in turns {
        match voice {
            Some(v) => audio.extend(sine(VOICES[v], secs, 0.5)),
            None => audio.extend(std::iter::repeat_n(0.0, (secs * 16_000.0) as usize)),
        }
    }
    audio
}

/// A temporary library with default config, the symphonia decoder, a fake
/// model execution context and a folder for input files.
pub struct Fixture {
    pub _tmp: tempfile::TempDir,
    pub lib: Library,
    pub config: Config,
    pub chain: DecoderChain,
    pub probe: FakeProcessProbe,
    pub execution: ExecutionContext,
    pub input: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("lib")).unwrap();
        let input = tmp.path().join("in");
        std::fs::create_dir_all(&input).unwrap();
        Self {
            _tmp: tmp,
            lib,
            config: Config::default(),
            chain: DecoderChain::new(vec![Box::new(SymphoniaDecoder::new())]),
            probe: FakeProcessProbe::new(std::process::id()),
            execution: execution(),
            input,
        }
    }

    pub fn env(&self) -> Env<'_> {
        Env {
            lib: &self.lib,
            config: &self.config,
            decoders: &self.chain,
            probe: &self.probe,
            execution: &self.execution,
            progress: None,
        }
    }

    /// Writes a synthetic conversation as `in/<name>`.
    pub fn wav(&self, name: &str, turns: &[(Option<usize>, f64)]) -> PathBuf {
        let p = self.input.join(name);
        write_wav(&p, 16_000, &conversation(turns));
        p
    }
}
