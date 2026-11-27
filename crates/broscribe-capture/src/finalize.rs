//! Final audio (plan M4 §12.5): the journal's common timeline encoded as one
//! file with two tracks (`mic`, `system`), verified by decoding each track
//! back. The journal is deleted by the caller only after the meeting that
//! references the file is committed ([`crate::journal::remove`]).

use std::path::{Path, PathBuf};

use broscribe_platform::{
    AudioEncoder, DecoderChain, EncodeSettings, EncodedTrack, EncoderTrack, PcmSink, PcmSource,
};

use crate::journal::{SessionManifest, SessionState, Track, load_manifest};
use crate::timeline::{Timeline, TimelineOptions, TrackStream};
use crate::{Error, Result};

/// What was encoded for one track.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackLevel {
    pub track: Track,
    pub frames: u64,
    pub rms: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EncodeReport {
    pub path: PathBuf,
    pub sample_rate: u32,
    /// Frames of every track.
    pub frames: u64,
    /// Mic, then system (the file's track order).
    pub tracks: Vec<TrackLevel>,
}

/// What decoding the file back found for one track.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackVerification {
    pub track: Track,
    pub sample_rate: u32,
    pub frames: u64,
    pub rms: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FinalizeReport {
    pub manifest: SessionManifest,
    pub timeline: Timeline,
    pub encoded: EncodeReport,
    pub verified: Vec<TrackVerification>,
}

/// Measures what flows through a source.
struct Measured<S> {
    inner: S,
    frames: u64,
    sum_sq: f64,
}

impl<S: PcmSource> PcmSource for Measured<S> {
    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let n = self.inner.read(out)?;
        self.frames += n as u64;
        self.sum_sq += out[..n]
            .iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>();
        Ok(n)
    }
}

fn rms(sum_sq: f64, frames: u64) -> f64 {
    if frames == 0 {
        0.0
    } else {
        (sum_sq / frames as f64).sqrt()
    }
}

/// Encodes both tracks of `timeline` into `out` (mic first, then system).
pub fn encode(timeline: &Timeline, out: &Path, encoder: &dyn AudioEncoder) -> Result<EncodeReport> {
    let measure = |s: TrackStream| Measured {
        inner: s,
        frames: 0,
        sum_sq: 0.0,
    };
    let mut mic = measure(timeline.stream(Track::Mic));
    let mut system = measure(timeline.stream(Track::System));
    let settings = EncodeSettings::speech(timeline.sample_rate);
    let written: Vec<EncodedTrack> = encoder.encode(
        out,
        &settings,
        &mut [
            EncoderTrack {
                title: Track::Mic.as_str(),
                source: &mut mic,
            },
            EncoderTrack {
                title: Track::System.as_str(),
                source: &mut system,
            },
        ],
    )?;
    if written.len() != 2 {
        let _ = std::fs::remove_file(out);
        return Err(Error::Internal(format!(
            "{} wrote {} tracks instead of 2",
            encoder.name(),
            written.len()
        )));
    }
    Ok(EncodeReport {
        path: out.to_path_buf(),
        sample_rate: timeline.sample_rate,
        frames: timeline.frames,
        tracks: vec![
            TrackLevel {
                track: Track::Mic,
                frames: mic.frames,
                rms: rms(mic.sum_sq, mic.frames),
            },
            TrackLevel {
                track: Track::System,
                frames: system.frames,
                rms: rms(system.sum_sq, system.frames),
            },
        ],
    })
}

#[derive(Default)]
struct MeasureSink {
    rate: Option<u32>,
    frames: u64,
    sum_sq: f64,
}

impl PcmSink for MeasureSink {
    fn format(&mut self, sample_rate: u32) -> Result<()> {
        self.rate = Some(sample_rate);
        Ok(())
    }

    fn samples(&mut self, mono: &[f32]) -> Result<()> {
        self.frames += mono.len() as u64;
        self.sum_sq += mono
            .iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>();
        Ok(())
    }
}

/// Decodes each track of the encoded file and checks rate, length (codec
/// priming/padding allowed) and level against what was encoded, which also
/// catches swapped or mixed tracks when their levels differ.
pub fn verify(decoders: &DecoderChain, report: &EncodeReport) -> Result<Vec<TrackVerification>> {
    let mut out = Vec::with_capacity(report.tracks.len());
    for (index, expected) in report.tracks.iter().enumerate() {
        let mut sink = MeasureSink::default();
        let info = decoders.decode_track(&report.path, index, &mut sink)?;
        let fail = |why: String| {
            Err(Error::Decode(format!(
                "{}: {} track does not verify: {why}",
                report.path.display(),
                expected.track.as_str()
            )))
        };
        if info.audio_tracks as usize != report.tracks.len() {
            return fail(format!(
                "the file has {} audio tracks, expected {}",
                info.audio_tracks,
                report.tracks.len()
            ));
        }
        let rate = sink.rate.unwrap_or(info.sample_rate);
        if rate != report.sample_rate {
            return fail(format!(
                "decoded at {rate} Hz, encoded at {} Hz",
                report.sample_rate
            ));
        }
        let tolerance = (expected.frames / 500).max(4096);
        if sink.frames.abs_diff(expected.frames) > tolerance {
            return fail(format!(
                "decoded {} frames, encoded {}",
                sink.frames, expected.frames
            ));
        }
        let decoded_rms = rms(sink.sum_sq, sink.frames);
        let level_ok = if expected.rms > 1e-3 {
            (0.5..=2.0).contains(&(decoded_rms / expected.rms))
        } else {
            decoded_rms < 1e-2
        };
        if !level_ok {
            return fail(format!(
                "decoded level {decoded_rms:.5}, encoded {:.5}",
                expected.rms
            ));
        }
        out.push(TrackVerification {
            track: expected.track,
            sample_rate: rate,
            frames: sink.frames,
            rms: decoded_rms,
        });
    }
    Ok(out)
}

/// Plans the timeline of the stopped journal in `dir`, encodes it to `out`
/// and verifies the result; a file that fails verification is removed.
/// The journal is left in place.
pub fn finalize(
    dir: &Path,
    out: &Path,
    encoder: &dyn AudioEncoder,
    decoders: &DecoderChain,
    options: &TimelineOptions,
) -> Result<FinalizeReport> {
    let manifest = load_manifest(dir)?;
    if manifest.state != SessionState::Stopped {
        return Err(Error::InvalidInput(format!(
            "recording session {} has not stopped (recover it first)",
            manifest.session_id
        )));
    }
    if options.cutoff.is_some() {
        return Err(Error::InvalidInput(
            "final audio covers the whole journal; a cutoff is for process-now jobs".into(),
        ));
    }
    let timeline = Timeline::plan(dir, &manifest, options)?;
    if timeline.frames == 0 {
        return Err(Error::InvalidInput(format!(
            "recording session {} captured no audio",
            manifest.session_id
        )));
    }
    let encoded = encode(&timeline, out, encoder)?;
    let verified = match verify(decoders, &encoded) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(out);
            return Err(e);
        }
    };
    Ok(FinalizeReport {
        manifest,
        timeline,
        encoded,
        verified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{JournalWriter, SessionMeta};
    use broscribe_platform::encoder::{FakeEncodedDecoder, FakeEncoder};

    fn stopped_journal(root: &Path) -> PathBuf {
        let mut w = JournalWriter::create(root, "f", SessionMeta::default(), 1.0, 1).unwrap();
        w.begin_segment(Track::Mic, 0, None, 1000, 0).unwrap();
        w.begin_segment(Track::System, 0, None, 1000, 0).unwrap();
        w.append(Track::Mic, 0, &vec![0.5; 2500]).unwrap();
        w.append(Track::System, 0, &vec![-0.1; 2000]).unwrap();
        let dir = w.dir().to_path_buf();
        w.finish(2_500_000_000, "user").unwrap();
        dir
    }

    #[test]
    fn encodes_two_tracks_and_verifies_each() {
        let root = tempfile::tempdir().unwrap();
        let dir = stopped_journal(root.path());
        let out = root.path().join("audio.json");
        let encoder = FakeEncoder::default();
        let decoders = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let report =
            finalize(&dir, &out, &encoder, &decoders, &TimelineOptions::default()).unwrap();
        assert_eq!(report.encoded.frames, 2500);
        assert_eq!(report.verified.len(), 2);
        assert_eq!(report.verified[0].track, Track::Mic);
        assert!((report.verified[0].rms - 0.5).abs() < 1e-6);
        assert_eq!(
            report.verified[1].frames, 2500,
            "system padded to the mic length"
        );
        assert_eq!(
            encoder.calls.lock().unwrap()[0],
            vec!["mic".to_string(), "system".to_string()]
        );
        // The journal stays until the caller removes it.
        assert!(dir.exists());
        crate::journal::remove(&dir).unwrap();
    }

    #[test]
    fn swapped_tracks_fail_verification() {
        let root = tempfile::tempdir().unwrap();
        let dir = stopped_journal(root.path());
        let out = root.path().join("audio.json");
        let manifest = load_manifest(&dir).unwrap();
        let timeline = Timeline::plan(&dir, &manifest, &TimelineOptions::default()).unwrap();
        let mut report = encode(&timeline, &out, &FakeEncoder::default()).unwrap();
        report.tracks.swap(0, 1);
        let decoders = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        assert_eq!(verify(&decoders, &report).unwrap_err().code(), "decode");
    }

    #[test]
    fn refuses_a_running_session() {
        let root = tempfile::tempdir().unwrap();
        let w = JournalWriter::create(root.path(), "r", SessionMeta::default(), 1.0, 1).unwrap();
        let decoders = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let err = finalize(
            w.dir(),
            &root.path().join("a.json"),
            &FakeEncoder::default(),
            &decoders,
            &TimelineOptions::default(),
        )
        .unwrap_err();
        assert_eq!(err.code(), "invalid_input");
    }
}
