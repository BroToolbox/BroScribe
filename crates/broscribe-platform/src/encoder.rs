//! Final-audio encoding (plan M4 §3, §12.5): one file holding one track per
//! recorded source (`mic`, `system`), never mixed.

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::{Error, MediaDecoder, MediaInfo, PcmSink, Result};

/// A pull source of mono f32 PCM.
pub trait PcmSource: Send {
    /// Fills the front of `out`; returns how many samples were written, 0 at
    /// the end of the stream.
    fn read(&mut self, out: &mut [f32]) -> Result<usize>;
}

/// A [`PcmSource`] over samples in memory.
pub struct SliceSource {
    samples: Vec<f32>,
    pos: usize,
}

impl SliceSource {
    pub fn new(samples: Vec<f32>) -> Self {
        Self { samples, pos: 0 }
    }
}

impl PcmSource for SliceSource {
    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let n = out.len().min(self.samples.len() - self.pos);
        out[..n].copy_from_slice(&self.samples[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// One track to encode.
pub struct EncoderTrack<'a> {
    /// Title stored with the track (`mic`, `system`).
    pub title: &'a str,
    pub source: &'a mut dyn PcmSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeSettings {
    /// Rate of every input track and of the output.
    pub sample_rate: u32,
    /// Target bit rate per track (bits per second).
    pub bitrate_per_track: u32,
}

impl EncodeSettings {
    /// AAC for speech: 96 kbit/s per mono track (transparent), or the
    /// highest rate AAC-LC accepts for a mono track at `sample_rate` when
    /// that is lower (see [`aac_lc_mono_max_bitrate`]).
    pub fn speech(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            bitrate_per_track: aac_lc_mono_max_bitrate(sample_rate).min(96_000),
        }
    }
}

/// Highest bit rate an AAC-LC encoder accepts for one mono channel at
/// `sample_rate` (Apple's applicable encode bit rates: 48 kbit/s at 16 kHz,
/// 64 kbit/s at 22.05–24 kHz; about 6 bits per sample pair of bandwidth).
/// Encoders clamp requested rates to it.
pub fn aac_lc_mono_max_bitrate(sample_rate: u32) -> u32 {
    match sample_rate {
        0..=8_000 => 24_000,
        8_001..=12_000 => 32_000,
        12_001..=16_000 => 48_000,
        16_001..=24_000 => 64_000,
        24_001..=32_000 => 96_000,
        _ => 160_000,
    }
}

/// What was written for one track.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedTrack {
    pub title: String,
    /// Input frames consumed (at [`EncodeSettings::sample_rate`]).
    pub frames: u64,
}

pub trait AudioEncoder: Send + Sync {
    fn name(&self) -> &str;
    /// Extension of the files it writes, without the dot (`m4a`).
    fn extension(&self) -> &str;
    /// Writes every track into `out` (which must not exist yet) as a separate
    /// enabled track, in order, without alternate groups, so players play
    /// them together and decoders can read each one on its own. Partial
    /// output is removed on error.
    fn encode(
        &self,
        out: &Path,
        settings: &EncodeSettings,
        tracks: &mut [EncoderTrack<'_>],
    ) -> Result<Vec<EncodedTrack>>;
}

/// Test encoder: writes the tracks losslessly as JSON; read them back with
/// [`FakeEncodedDecoder`]. Every call is also recorded in `calls`.
#[derive(Default)]
pub struct FakeEncoder {
    /// Titles of each `encode` call.
    pub calls: Arc<Mutex<Vec<Vec<String>>>>,
}

impl AudioEncoder for FakeEncoder {
    fn name(&self) -> &str {
        "fake"
    }

    fn extension(&self) -> &str {
        "json"
    }

    fn encode(
        &self,
        out: &Path,
        settings: &EncodeSettings,
        tracks: &mut [EncoderTrack<'_>],
    ) -> Result<Vec<EncodedTrack>> {
        if out.exists() {
            return Err(Error::AlreadyExists(out.display().to_string()));
        }
        let mut encoded = Vec::new();
        let mut json_tracks = Vec::new();
        let mut buf = vec![0.0f32; 4096];
        for track in tracks.iter_mut() {
            let mut samples = Vec::new();
            loop {
                let n = track.source.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                samples.extend_from_slice(&buf[..n]);
            }
            encoded.push(EncodedTrack {
                title: track.title.to_string(),
                frames: samples.len() as u64,
            });
            json_tracks.push(json!({ "title": track.title, "samples": samples }));
        }
        let doc = json!({ "sample_rate": settings.sample_rate, "tracks": json_tracks });
        std::fs::write(out, doc.to_string()).map_err(|e| Error::io_at(out, e))?;
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(encoded.iter().map(|t| t.title.clone()).collect());
        Ok(encoded)
    }
}

/// Reads files written by [`FakeEncoder`], one audio track per encoded track.
#[derive(Default)]
pub struct FakeEncodedDecoder;

impl FakeEncodedDecoder {
    fn load(path: &Path) -> Result<(u32, Vec<Vec<f32>>)> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io_at(path, e))?;
        let doc: Value = serde_json::from_str(&text)
            .map_err(|e| Error::Decode(format!("{}: {e}", path.display())))?;
        let rate = doc["sample_rate"].as_u64().unwrap_or(0) as u32;
        let tracks = doc["tracks"]
            .as_array()
            .map(|tracks| {
                tracks
                    .iter()
                    .map(|t| {
                        t["samples"]
                            .as_array()
                            .map(|s| s.iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok((rate, tracks))
    }

    fn info(rate: u32, tracks: &[Vec<f32>], index: usize) -> MediaInfo {
        MediaInfo {
            sample_rate: rate,
            channels: 1,
            duration_s: tracks
                .get(index)
                .map(|t| t.len() as f64 / f64::from(rate.max(1))),
            created_at: None,
            audio_tracks: tracks.len() as u32,
            container: "json".into(),
            tracks: (0..tracks.len())
                .map(|index| crate::TrackInfo {
                    index,
                    channels: 1,
                    sample_rate: rate,
                    ..Default::default()
                })
                .collect(),
        }
    }
}

impl MediaDecoder for FakeEncodedDecoder {
    fn name(&self) -> &str {
        "fake-encoded"
    }

    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        let (rate, tracks) = Self::load(path)?;
        Ok(Self::info(rate, &tracks, 0))
    }

    fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        self.decode_track(path, 0, sink)
    }

    fn decode_track(&self, path: &Path, index: usize, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        let (rate, tracks) = Self::load(path)?;
        let track = tracks
            .get(index)
            .ok_or_else(|| Error::Decode(format!("{}: no audio track {index}", path.display())))?;
        sink.format(rate)?;
        sink.samples(track)?;
        Ok(Self::info(rate, &tracks, index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::CollectSink;

    #[test]
    fn speech_bitrate_fits_aac_lc_at_call_rates() {
        let bitrate = |rate| EncodeSettings::speech(rate).bitrate_per_track;
        assert_eq!(bitrate(16_000), 48_000);
        assert_eq!(bitrate(22_050), 64_000);
        assert_eq!(bitrate(24_000), 64_000);
        assert_eq!(bitrate(44_100), 96_000);
        assert_eq!(bitrate(48_000), 96_000);
    }

    #[test]
    fn fake_encoder_round_trips_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("audio.json");
        let mut mic = SliceSource::new(vec![0.25; 10]);
        let mut system = SliceSource::new(vec![-0.5; 7]);
        let encoder = FakeEncoder::default();
        let written = encoder
            .encode(
                &out,
                &EncodeSettings::speech(16_000),
                &mut [
                    EncoderTrack {
                        title: "mic",
                        source: &mut mic,
                    },
                    EncoderTrack {
                        title: "system",
                        source: &mut system,
                    },
                ],
            )
            .unwrap();
        assert_eq!(written[0].frames, 10);
        assert_eq!(written[1].frames, 7);

        let mut sink = CollectSink::default();
        let info = FakeEncodedDecoder.decode_track(&out, 1, &mut sink).unwrap();
        assert_eq!(info.audio_tracks, 2);
        assert_eq!(sink.samples, vec![-0.5; 7]);
        assert!(FakeEncodedDecoder.decode_track(&out, 2, &mut sink).is_err());
        // Refuses to overwrite.
        let mut empty = SliceSource::new(Vec::new());
        assert!(
            encoder
                .encode(
                    &out,
                    &EncodeSettings::speech(16_000),
                    &mut [EncoderTrack {
                        title: "mic",
                        source: &mut empty
                    }],
                )
                .is_err()
        );
    }
}
