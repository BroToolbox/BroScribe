//! Shared by the live-progress tests: synthetic audio, fake dependencies,
//! speech recognition held at a gate and an error stream read line by line.

use std::io::Write;
use std::path::Path;
use std::sync::mpsc;

use broscribe_cli::Deps;
use broscribe_core::config::Profile;
use broscribe_models::fakes::{FakeModelProvider, sine};
use broscribe_models::{
    Asr, AsrOutput, DiarSegment, ModelProvider, ModelSet, SpeakerEmbedder, Vad,
};
use broscribe_platform::DecoderChain;
use broscribe_platform::fakes::{
    FakeMicCapture, FakePaths, FakeProcessProbe, FakeProcessTreeControl, FakeSecretStore,
    FakeTrustStore,
};
use broscribe_platform::symphonia_decoder::SymphoniaDecoder;

/// Writes two synthetic voices (200 Hz, then 440 Hz) as 16-bit PCM mono WAV
/// at 16 kHz.
pub fn write_conversation(path: &Path) {
    let mut samples = vec![0.0; 8_000];
    samples.extend(sine(200.0, 4.0, 0.5));
    samples.extend(vec![0.0; 16_000]);
    samples.extend(sine(440.0, 4.0, 0.5));
    samples.extend(vec![0.0; 8_000]);
    let rate: u32 = 16_000;
    let data_len = u32::try_from(samples.len() * 2).unwrap();
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for x in samples {
        let v = (x.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// Fake models whose diarization matches [`write_conversation`].
pub fn fake_models() -> FakeModelProvider {
    FakeModelProvider {
        asr_words: vec!["pricing".into(), "roadmap".into()],
        schedule: vec![
            DiarSegment {
                start_s: 0.5,
                end_s: 4.5,
                speaker: 0,
            },
            DiarSegment {
                start_s: 5.5,
                end_s: 9.5,
                speaker: 1,
            },
        ],
        embed_dim: 16,
    }
}

/// Fake dependencies under `root`; nothing of the user's is read.
pub fn deps(root: &Path, models: Box<dyn ModelProvider>) -> Deps {
    Deps {
        paths: Box::new(FakePaths::new(root.join("root"))),
        updates: broscribe_cli::UpdateDeps::with_transport(
            std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                root.join("root").join("install"),
                "synthetic-test-target",
            )),
            std::sync::Arc::new(broscribe_updates::FakeTransport::default()),
        ),
        models,
        decoder: DecoderChain::new(vec![Box::new(SymphoniaDecoder::new())]),
        process: Box::new(FakeProcessProbe::new(std::process::id())),
        mic: Box::new(FakeMicCapture::new(16_000)),
        secrets: std::sync::Arc::new(FakeSecretStore::default()),
        browser: std::sync::Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
        autostart: Box::new(broscribe_platform::fakes::FakeAutostart::default()),
        agents: broscribe_cli::AgentEnv::isolated(root),
        execution: broscribe_models::ExecutionContext::new(
            std::sync::Arc::new(FakeProcessTreeControl::default()),
            std::sync::Arc::new(FakeTrustStore::default()),
        ),
        clock: broscribe_cli::system_clock(),
    }
}

/// Speech recognition that announces its first call on `entered`, then
/// waits for `release`; a dropped release (a failing test) lets it go on, so
/// the command can never stay blocked.
pub struct GatedModels {
    pub entered: mpsc::SyncSender<()>,
    pub release: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
}

struct GatedAsr {
    inner: Box<dyn Asr + Send>,
    entered: mpsc::SyncSender<()>,
    release: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
}

impl Asr for GatedAsr {
    fn transcribe(&self, samples_16k: &[f32]) -> broscribe_core::Result<AsrOutput> {
        let release = self.release.lock().unwrap().take();
        if let Some(release) = release {
            let _ = self.entered.try_send(());
            let _ = release.recv();
        }
        self.inner.transcribe(samples_16k)
    }

    fn accepts_hotwords(&self) -> bool {
        self.inner.accepts_hotwords()
    }

    fn set_hotwords(&mut self, terms: &[String]) -> broscribe_core::Result<()> {
        self.inner.set_hotwords(terms)
    }
}

impl ModelProvider for GatedModels {
    fn load(&self, profile: &Profile) -> broscribe_core::Result<ModelSet> {
        let mut set = fake_models().load(profile)?;
        set.asr = Box::new(GatedAsr {
            inner: set.asr,
            entered: self.entered.clone(),
            release: std::sync::Mutex::new(self.release.lock().unwrap().take()),
        });
        Ok(set)
    }

    fn load_vad(&self, model_id: &str) -> broscribe_core::Result<Box<dyn Vad + Send>> {
        fake_models().load_vad(model_id)
    }

    fn load_embedder(
        &self,
        model_id: &str,
    ) -> broscribe_core::Result<Box<dyn SpeakerEmbedder + Send>> {
        fake_models().load_embedder(model_id)
    }
}

/// An error stream that hands over each complete line as it is written.
pub struct Lines {
    pending: Vec<u8>,
    sink: mpsc::Sender<String>,
}

impl Lines {
    pub fn new(sink: mpsc::Sender<String>) -> Self {
        Self {
            pending: Vec::new(),
            sink,
        }
    }
}

impl Write for Lines {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(buf);
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let _ = self
                .sink
                .send(String::from_utf8_lossy(&line[..end]).into_owned());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
