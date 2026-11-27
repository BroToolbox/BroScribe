//! Media decoding: every decoder emits mono f32 PCM at the stream's decoded
//! sample rate. Resampling to 16 kHz happens in the pipeline.

use std::path::Path;

use chrono::{DateTime, FixedOffset};

use crate::{Error, Result};

/// Metadata for one audio track, indexed independently of video streams.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrackInfo {
    pub index: usize,
    pub channels: u16,
    pub sample_rate: u32,
    pub language: Option<String>,
    pub title: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaInfo {
    /// Rate of the decoded (mono) output.
    pub sample_rate: u32,
    /// Channels in the source before downmixing.
    pub channels: u16,
    pub duration_s: Option<f64>,
    /// Container creation time, when the file records one.
    pub created_at: Option<DateTime<FixedOffset>>,
    pub audio_tracks: u32,
    /// Audio tracks in the same order accepted by `decode_track`.
    pub tracks: Vec<TrackInfo>,
    /// Container format name, e.g. `mp4`, `wav`.
    pub container: String,
}

/// Receives decoded audio. `format` is called exactly once, before any `samples`.
pub trait PcmSink {
    fn format(&mut self, sample_rate: u32) -> Result<()>;
    fn samples(&mut self, mono: &[f32]) -> Result<()>;
}

pub trait MediaDecoder: Send + Sync {
    fn name(&self) -> &str;
    /// Reads metadata without decoding the audio.
    fn probe(&self, path: &Path) -> Result<MediaInfo>;
    /// Decodes the first audio track, downmixed to mono, into `sink`. Errors
    /// returned by the sink are passed through unchanged.
    fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo>;
    /// Decodes the `index`-th audio track (0-based, in container order),
    /// downmixed to mono, into `sink` (plan M4 §12.4). Decoders that read only
    /// one track report `unsupported` for `index > 0`.
    fn decode_track(&self, path: &Path, index: usize, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        if index == 0 {
            self.decode(path, sink)
        } else {
            Err(Error::Unsupported(format!(
                "{}: decoder {} reads only the first audio track",
                path.display(),
                self.name()
            )))
        }
    }
}

/// Decoders tried in order (macOS M1: symphonia, then ffmpeg).
#[derive(Default)]
pub struct DecoderChain(pub Vec<Box<dyn MediaDecoder>>);

impl DecoderChain {
    pub fn new(decoders: Vec<Box<dyn MediaDecoder>>) -> Self {
        Self(decoders)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|d| d.name()).collect()
    }

    /// The first successful probe; the last error otherwise.
    pub fn probe(&self, path: &Path) -> Result<MediaInfo> {
        let mut last = None;
        for d in &self.0 {
            match d.probe(path) {
                Ok(info) => return Ok(info),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| no_decoder(path)))
    }

    /// Decodes with the first decoder that works. Falls back to the next one
    /// only while nothing has reached `sink`; after the first `format` or
    /// `samples` call, and for any error returned by the sink, the error is final.
    pub fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        self.decode_with(path, sink, |d, sink| d.decode(path, sink))
    }

    /// [`decode`](Self::decode) for the `index`-th audio track, with the same
    /// fallback rules.
    pub fn decode_track(
        &self,
        path: &Path,
        index: usize,
        sink: &mut dyn PcmSink,
    ) -> Result<MediaInfo> {
        self.decode_with(path, sink, |d, sink| d.decode_track(path, index, sink))
    }

    fn decode_with(
        &self,
        path: &Path,
        sink: &mut dyn PcmSink,
        run: impl Fn(&dyn MediaDecoder, &mut dyn PcmSink) -> Result<MediaInfo>,
    ) -> Result<MediaInfo> {
        let mut last = None;
        for d in &self.0 {
            let mut tracked = TrackingSink {
                inner: &mut *sink,
                emitted: false,
                sink_failed: false,
            };
            match run(d.as_ref(), &mut tracked) {
                Ok(info) => return Ok(info),
                Err(e) if tracked.emitted || tracked.sink_failed => return Err(e),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| no_decoder(path)))
    }
}

fn no_decoder(path: &Path) -> Error {
    Error::Unsupported(format!("no media decoder available for {}", path.display()))
}

struct TrackingSink<'a> {
    inner: &'a mut dyn PcmSink,
    emitted: bool,
    sink_failed: bool,
}

impl PcmSink for TrackingSink<'_> {
    fn format(&mut self, sample_rate: u32) -> Result<()> {
        self.emitted = true;
        self.inner
            .format(sample_rate)
            .inspect_err(|_| self.sink_failed = true)
    }

    fn samples(&mut self, mono: &[f32]) -> Result<()> {
        self.emitted = true;
        self.inner
            .samples(mono)
            .inspect_err(|_| self.sink_failed = true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::{CollectSink, FakeDecoder};

    #[test]
    fn falls_back_only_before_emission() {
        let path = Path::new("/tmp/fake.wav");

        // Fails before emitting: the next decoder is used.
        let chain = DecoderChain::new(vec![
            Box::new(FakeDecoder::failing("broken", 0)),
            Box::new(FakeDecoder::new("good", 8000, vec![0.5; 100])),
        ]);
        let mut sink = CollectSink::default();
        let info = chain.decode(path, &mut sink).unwrap();
        assert_eq!(info.sample_rate, 8000);
        assert_eq!(sink.sample_rate, Some(8000));
        assert_eq!(sink.samples.len(), 100);

        // Emits one chunk then fails: the error is final, no second decode.
        let chain = DecoderChain::new(vec![
            Box::new(FakeDecoder::failing("partial", 1)),
            Box::new(FakeDecoder::new("good", 8000, vec![0.5; 100])),
        ]);
        let mut sink = CollectSink::default();
        let err = chain.decode(path, &mut sink).unwrap_err();
        assert_eq!(err.code(), "decode");
        assert_eq!(sink.format_calls, 1);
        assert_eq!(sink.samples.len(), FakeDecoder::CHUNK);
    }

    #[test]
    fn sink_errors_propagate_without_fallback() {
        struct Refuse;
        impl PcmSink for Refuse {
            fn format(&mut self, _: u32) -> Result<()> {
                Err(Error::Internal("stop".into()))
            }
            fn samples(&mut self, _: &[f32]) -> Result<()> {
                unreachable!()
            }
        }
        let chain = DecoderChain::new(vec![
            Box::new(FakeDecoder::new("a", 16000, vec![0.0; 10])),
            Box::new(FakeDecoder::new("b", 16000, vec![0.0; 10])),
        ]);
        assert_eq!(
            chain
                .decode(Path::new("x"), &mut Refuse)
                .unwrap_err()
                .code(),
            "internal"
        );
    }

    #[test]
    fn empty_chain_is_unsupported() {
        let chain = DecoderChain::default();
        let mut sink = CollectSink::default();
        assert_eq!(
            chain.decode(Path::new("x"), &mut sink).unwrap_err().code(),
            "unsupported"
        );
        assert_eq!(
            chain.probe(Path::new("x")).unwrap_err().code(),
            "unsupported"
        );
    }
}
