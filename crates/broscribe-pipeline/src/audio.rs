//! Audio input: decode through the [`DecoderChain`], convert to 16 kHz mono
//! f32 (design §6.3) and hash the source file.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use broscribe_core::{Error, Result};
use broscribe_platform::ffmpeg_decoder::FfmpegDecoder;
use broscribe_platform::{DecoderChain, MediaDecoder, MediaInfo, PcmSink};
use sha2::{Digest, Sha256};
use sherpa_onnx::LinearResampler;

/// Rate every model runs at.
pub const TARGET_RATE: u32 = 16_000;

/// One analysis track's address in stored or externally referenced audio.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackSource {
    pub track: String,
    pub path: std::path::PathBuf,
    pub track_index: usize,
    pub offset_s: f64,
    pub sha256: String,
}

/// Resolve logical tracks without mixing audio. Relative paths are relative to
/// the meeting directory; callers with a Library resolve that directory.
pub fn meeting_tracks(meeting: &broscribe_core::schema::Meeting) -> Vec<TrackSource> {
    let Some(recording) = meeting
        .recording
        .as_ref()
        .filter(|r| r.state == broscribe_core::schema::RecordingState::Available)
    else {
        return Vec::new();
    };
    if !recording.sources.is_empty() {
        return recording
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| TrackSource {
                track: meeting
                    .tracks
                    .get(i)
                    .map_or_else(|| format!("t{i}"), |t| t.id.clone()),
                path: s.path.clone(),
                track_index: s.track_index,
                offset_s: s.offset_s,
                sha256: s.sha256.clone(),
            })
            .collect();
    }
    if meeting.tracks.is_empty() {
        return vec![TrackSource {
            track: "t0".into(),
            path: recording.path.clone(),
            track_index: 0,
            offset_s: 0.0,
            sha256: recording.sha256.clone(),
        }];
    }
    meeting
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| TrackSource {
            track: t.id.clone(),
            path: recording.path.clone(),
            track_index: i,
            offset_s: 0.0,
            sha256: recording.sha256.clone(),
        })
        .collect()
}

pub fn decode_meeting_track(
    lib: &broscribe_core::Library,
    decoders: &DecoderChain,
    meeting: &broscribe_core::schema::Meeting,
    source: &TrackSource,
) -> Result<Vec<f32>> {
    let path = if source.path.is_absolute() {
        source.path.clone()
    } else {
        lib.meeting_dir(&meeting.id)?.join(&source.path)
    };
    crate::multitrack::decode_source(
        decoders,
        &crate::manifest::ManifestSource {
            path,
            audio_track_index: Some(source.track_index),
            role: broscribe_core::schema::TrackRole::Mixed,
            person_hint: None,
            offset_s: source.offset_s,
        },
    )
}

/// Playback alone mixes tracks. Analysis and voice identity never do.
pub fn playback_mix(
    lib: &broscribe_core::Library,
    decoders: &DecoderChain,
    meeting: &broscribe_core::schema::Meeting,
) -> Result<Vec<f32>> {
    let mut mix = Vec::new();
    for source in meeting_tracks(meeting) {
        let samples = decode_meeting_track(lib, decoders, meeting, &source)?;
        let offset = (source.offset_s * f64::from(TARGET_RATE)).round() as i64;
        let skip = offset.saturating_neg().max(0) as usize;
        let start = offset.max(0) as usize;
        let samples = &samples[skip.min(samples.len())..];
        mix.resize(mix.len().max(start + samples.len()), 0.0);
        for (out, input) in mix[start..].iter_mut().zip(samples) {
            *out += input;
        }
    }
    let peak = mix.iter().fold(1.0f32, |a, b| a.max(b.abs()));
    if peak > 1.0 {
        for sample in &mut mix {
            *sample /= peak;
        }
    }
    Ok(mix)
}
/// Emitted duration and container duration may differ by this share before
/// the file is decoded again with ffmpeg (some AAC streams decode at the
/// wrong rate).
const DURATION_TOLERANCE: f64 = 0.02;

/// Bytes read per hashing step.
const HASH_CHUNK: usize = 1 << 20;

/// A decoded recording at [`TARGET_RATE`].
#[derive(Clone, Debug)]
pub struct DecodedAudio {
    /// 16 kHz mono f32.
    pub samples: Vec<f32>,
    /// What the decoder reported (its `sample_rate` is the decoded rate,
    /// before conversion).
    pub info: MediaInfo,
    /// Whether the duration check sent the file through ffmpeg again.
    pub redecoded_with_ffmpeg: bool,
}

impl DecodedAudio {
    pub fn duration_s(&self) -> f64 {
        self.samples.len() as f64 / f64::from(TARGET_RATE)
    }
}

/// Streams decoded audio through a resampler (skipped at 16 kHz).
struct Resample16k {
    rate: Option<u32>,
    resampler: Option<LinearResampler>,
    /// Source-rate samples received.
    received: u64,
    out: Vec<f32>,
}

impl Resample16k {
    fn new() -> Self {
        Self {
            rate: None,
            resampler: None,
            received: 0,
            out: Vec::new(),
        }
    }

    /// Flushes the resampler's tail; returns the 16 kHz samples.
    fn finish(mut self) -> Vec<f32> {
        if let Some(r) = self.resampler.take() {
            let tail = r.resample(&[], true);
            self.out.extend_from_slice(&tail);
        }
        self.out
    }

    fn emitted_s(&self) -> Option<f64> {
        let rate = self.rate.filter(|r| *r > 0)?;
        Some(self.received as f64 / f64::from(rate))
    }
}

impl PcmSink for Resample16k {
    fn format(&mut self, sample_rate: u32) -> Result<()> {
        if sample_rate == 0 {
            return Err(Error::Decode("decoder reported a sample rate of 0".into()));
        }
        self.rate = Some(sample_rate);
        self.resampler = if sample_rate == TARGET_RATE {
            None
        } else {
            let rate_in = i32::try_from(sample_rate)
                .map_err(|_| Error::Decode(format!("sample rate {sample_rate} out of range")))?;
            Some(
                LinearResampler::create(rate_in, TARGET_RATE as i32).ok_or_else(|| {
                    Error::Inference(format!(
                        "could not create a {sample_rate} Hz → {TARGET_RATE} Hz resampler"
                    ))
                })?,
            )
        };
        Ok(())
    }

    fn samples(&mut self, mono: &[f32]) -> Result<()> {
        if self.rate.is_none() {
            return Err(Error::Internal("samples before format".into()));
        }
        self.received += mono.len() as u64;
        match &self.resampler {
            Some(r) => {
                let out = r.resample(mono, false);
                self.out.extend_from_slice(&out);
            }
            None => self.out.extend_from_slice(mono),
        }
        Ok(())
    }
}

/// Decodes `path` to 16 kHz mono f32.
///
/// After decoding, the emitted duration (samples ÷ decoded rate) is compared
/// with the container's duration; when they differ by more than 2 %, the
/// result is thrown away and the file is decoded again with the chain's ffmpeg
/// decoder, explicitly (the chain itself never falls back after emitting).
/// Without ffmpeg the first result is kept and a warning is logged.
pub fn decode_16k(chain: &DecoderChain, path: &Path) -> Result<DecodedAudio> {
    decode_checked(chain, path, None)
}

/// [`decode_16k`] for the `index`-th audio track (container order), with the
/// same duration check and ffmpeg fallback.
pub fn decode_track_16k(chain: &DecoderChain, path: &Path, index: usize) -> Result<DecodedAudio> {
    decode_checked(chain, path, Some(index))
}

fn decode_checked(chain: &DecoderChain, path: &Path, index: Option<usize>) -> Result<DecodedAudio> {
    let mut sink = Resample16k::new();
    let info = match index {
        None => chain.decode(path, &mut sink)?,
        Some(i) => chain.decode_track(path, i, &mut sink)?,
    };
    if duration_mismatch(sink.emitted_s(), info.duration_s) {
        match chain.0.iter().find(|d| d.name() == FfmpegDecoder::NAME) {
            Some(ffmpeg) => {
                tracing::warn!(
                    path = %path.display(),
                    track = index,
                    emitted_s = sink.emitted_s(),
                    container_s = info.duration_s,
                    "decoded duration differs from the container's; decoding again with ffmpeg"
                );
                drop(sink);
                return decode_with(ffmpeg.as_ref(), path, index).map(|mut d| {
                    d.redecoded_with_ffmpeg = true;
                    d
                });
            }
            None => tracing::warn!(
                path = %path.display(),
                track = index,
                emitted_s = sink.emitted_s(),
                container_s = info.duration_s,
                "decoded duration differs from the container's and ffmpeg is not installed"
            ),
        }
    }
    Ok(DecodedAudio {
        samples: sink.finish(),
        info,
        redecoded_with_ffmpeg: false,
    })
}

fn decode_with(
    decoder: &dyn MediaDecoder,
    path: &Path,
    index: Option<usize>,
) -> Result<DecodedAudio> {
    let mut sink = Resample16k::new();
    let info = match index {
        None => decoder.decode(path, &mut sink)?,
        Some(i) => decoder.decode_track(path, i, &mut sink)?,
    };
    Ok(DecodedAudio {
        samples: sink.finish(),
        info,
        redecoded_with_ffmpeg: false,
    })
}

fn duration_mismatch(emitted: Option<f64>, container: Option<f64>) -> bool {
    match (emitted, container) {
        (Some(e), Some(c)) if c > 0.0 && c.is_finite() => (e - c).abs() / c > DURATION_TOLERANCE,
        _ => false,
    }
}

/// Lowercase hex SHA-256 of the file's content (streamed) and its size.
pub fn file_sha256(path: &Path) -> Result<(String, u64)> {
    let mut f = File::open(path).map_err(|e| Error::io_at(path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    let mut bytes = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| Error::io_at(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok((hex(&hasher.finalize()), bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// `samples[start..end]` for a range in seconds, clamped to the buffer.
pub fn slice_s(samples: &[f32], start_s: f64, end_s: f64) -> &[f32] {
    let rate = f64::from(TARGET_RATE);
    let len = samples.len();
    let a = ((start_s.max(0.0) * rate).round() as usize).min(len);
    let b = ((end_s.max(0.0) * rate).round() as usize).clamp(a, len);
    &samples[a..b]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::write_wav;
    use broscribe_platform::fakes::FakeDecoder;
    use broscribe_platform::symphonia_decoder::SymphoniaDecoder;

    /// Emits one second for any track but reports a container duration twice
    /// as long (a stream decoded at the wrong rate).
    struct LyingDecoder;

    impl MediaDecoder for LyingDecoder {
        fn name(&self) -> &str {
            "lying"
        }
        fn probe(&self, _: &Path) -> Result<MediaInfo> {
            unreachable!()
        }
        fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
            self.decode_track(path, 0, sink)
        }
        fn decode_track(&self, _: &Path, _: usize, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
            sink.format(16_000)?;
            sink.samples(&[0.1; 16_000])?;
            Ok(MediaInfo {
                sample_rate: 16_000,
                channels: 1,
                duration_s: Some(2.0),
                created_at: None,
                audio_tracks: 2,
                container: "mp4".into(),
                ..Default::default()
            })
        }
    }

    /// A stand-in for ffmpeg that reads every track at the right length;
    /// track `i` carries the value `0.1 * (i + 1)`.
    struct TrackFfmpeg;

    impl MediaDecoder for TrackFfmpeg {
        fn name(&self) -> &str {
            FfmpegDecoder::NAME
        }
        fn probe(&self, _: &Path) -> Result<MediaInfo> {
            unreachable!()
        }
        fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
            self.decode_track(path, 0, sink)
        }
        fn decode_track(
            &self,
            _: &Path,
            index: usize,
            sink: &mut dyn PcmSink,
        ) -> Result<MediaInfo> {
            sink.format(16_000)?;
            sink.samples(&vec![0.1 * (index + 1) as f32; 32_000])?;
            Ok(MediaInfo {
                sample_rate: 16_000,
                channels: 1,
                duration_s: Some(2.0),
                audio_tracks: 2,
                container: "mp4".into(),
                ..Default::default()
            })
        }
    }

    /// Track-addressed decoding (confirm, re-embed, reprocess, multi-track
    /// imports) runs the same duration check and redecodes that track.
    #[test]
    fn track_duration_mismatch_redecodes_the_same_track_with_ffmpeg() {
        let chain = DecoderChain::new(vec![Box::new(LyingDecoder), Box::new(TrackFfmpeg)]);
        let d = decode_track_16k(&chain, Path::new("/tmp/x.m4a"), 1).unwrap();
        assert!(d.redecoded_with_ffmpeg);
        assert_eq!(d.samples.len(), 32_000);
        assert!((d.samples[0] - 0.2).abs() < 1e-6);

        let source = crate::manifest::ManifestSource {
            path: "/tmp/x.m4a".into(),
            audio_track_index: Some(1),
            role: broscribe_core::schema::TrackRole::Mixed,
            person_hint: None,
            offset_s: 0.0,
        };
        let samples = crate::multitrack::decode_source(&chain, &source).unwrap();
        assert_eq!(samples.len(), 32_000);
        assert!((samples[0] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn wav_at_16k_passes_through_and_8k_is_resampled() {
        let dir = tempfile::tempdir().unwrap();
        let chain = DecoderChain::new(vec![Box::new(SymphoniaDecoder::new())]);

        let p16 = dir.path().join("a16.wav");
        write_wav(&p16, 16_000, &vec![0.25; 16_000]);
        let d = decode_16k(&chain, &p16).unwrap();
        assert_eq!(d.samples.len(), 16_000);
        assert!(!d.redecoded_with_ffmpeg);

        let p8 = dir.path().join("a8.wav");
        let tone: Vec<f32> = (0..8_000)
            .map(|i| 0.5 * (std::f32::consts::TAU * 200.0 * i as f32 / 8_000.0).sin())
            .collect();
        write_wav(&p8, 8_000, &tone);
        let d = decode_16k(&chain, &p8).unwrap();
        let n = d.samples.len() as f64;
        assert!((n - 16_000.0).abs() < 160.0, "{n}");
    }

    #[test]
    fn duration_mismatch_redecodes_with_ffmpeg() {
        let chain = DecoderChain::new(vec![
            Box::new(LyingDecoder),
            Box::new(FakeDecoder::new(
                FfmpegDecoder::NAME,
                16_000,
                vec![0.2; 32_000],
            )),
        ]);
        let d = decode_16k(&chain, Path::new("/tmp/x.m4a")).unwrap();
        assert!(d.redecoded_with_ffmpeg);
        assert_eq!(d.samples.len(), 32_000);

        // Without ffmpeg the first result stays.
        let chain = DecoderChain::new(vec![Box::new(LyingDecoder)]);
        let d = decode_16k(&chain, Path::new("/tmp/x.m4a")).unwrap();
        assert_eq!(d.samples.len(), 16_000);
    }

    #[test]
    fn sha256_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.bin");
        std::fs::write(&p, b"abc").unwrap();
        let (sha, bytes) = file_sha256(&p).unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(
            sha,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn slices_clamp() {
        let s = vec![0.0; 16_000];
        assert_eq!(slice_s(&s, 0.5, 2.0).len(), 8_000);
        assert_eq!(slice_s(&s, 2.0, 3.0).len(), 0);
    }
}
