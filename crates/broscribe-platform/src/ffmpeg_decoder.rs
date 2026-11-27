//! Fallback decoder through `ffprobe`/`ffmpeg` subprocesses.
//!
//! `ffprobe` reports the first audio stream's sample rate and channel count
//! before decoding; `ffmpeg` then streams that stream as interleaved
//! `f32le` at the probed rate and channel count, and this module averages the
//! channels to mono, the same downmix the symphonia decoder applies (ffmpeg's
//! own `-ac 1` scales correlated stereo by 1/√2 per channel, so the same file
//! would come out louder than through symphonia).

use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::symphonia_decoder::parse_media_date;
use crate::{Error, MediaDecoder, MediaInfo, PcmSink, Result};

/// Folders searched after `PATH` (Homebrew on Apple silicon and Intel).
pub const EXTRA_BIN_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// Bytes read from ffmpeg's stdout per chunk.
const READ_CHUNK: usize = 64 * 1024;
/// Most of ffmpeg's stderr kept for error messages.
const STDERR_TAIL: usize = 4 * 1024;

#[derive(Clone, Debug, Default)]
pub struct FfmpegDecoder {
    ffmpeg: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
}

impl FfmpegDecoder {
    pub const NAME: &'static str = "ffmpeg";

    /// Finds `ffmpeg` and `ffprobe` in `PATH`, then in [`EXTRA_BIN_DIRS`].
    pub fn locate() -> Self {
        Self {
            ffmpeg: find_binary("ffmpeg"),
            ffprobe: find_binary("ffprobe"),
        }
    }

    /// Uses the given binaries (`None` = not installed).
    pub fn with_binaries(ffmpeg: Option<PathBuf>, ffprobe: Option<PathBuf>) -> Self {
        Self { ffmpeg, ffprobe }
    }

    /// Both `ffmpeg` and `ffprobe` were found.
    pub fn is_available(&self) -> bool {
        self.ffmpeg.is_some() && self.ffprobe.is_some()
    }

    pub fn ffmpeg_path(&self) -> Option<&Path> {
        self.ffmpeg.as_deref()
    }

    pub fn ffprobe_path(&self) -> Option<&Path> {
        self.ffprobe.as_deref()
    }

    fn binaries(&self) -> Result<(&Path, &Path)> {
        match (&self.ffmpeg, &self.ffprobe) {
            (Some(ffmpeg), Some(ffprobe)) => Ok((ffmpeg, ffprobe)),
            _ => Err(Error::Unsupported(
                "ffmpeg and ffprobe are not installed".into(),
            )),
        }
    }

    /// `MediaInfo` for the `index`-th audio stream.
    fn probe_track(&self, path: &Path, index: usize) -> Result<MediaInfo> {
        let (_, ffprobe) = self.binaries()?;
        let output = Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ])
            .arg(file_url(path))
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Error::io(format!("running {}", ffprobe.display()), e))?;
        if !output.status.success() {
            return Err(Error::Decode(format!(
                "{}: ffprobe failed ({}): {}",
                path.display(),
                output.status,
                tail(&output.stderr)
            )));
        }
        let json = String::from_utf8_lossy(&output.stdout);
        parse_probe(&json, path, index)
    }
}

impl MediaDecoder for FfmpegDecoder {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        self.probe_track(path, 0)
    }

    fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        self.decode_track(path, 0, sink)
    }

    fn decode_track(&self, path: &Path, index: usize, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        let (ffmpeg, _) = self.binaries()?;
        let mut info = self.probe_track(path, index)?;
        let rate = info.sample_rate;
        let channels = info.channels.max(1);

        let mut child = Command::new(ffmpeg)
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(file_url(path))
            .arg("-map")
            .arg(format!("0:a:{index}"))
            .arg("-ac")
            .arg(channels.to_string())
            .arg("-ar")
            .arg(rate.to_string())
            .args(["-f", "f32le", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::io(format!("running {}", ffmpeg.display()), e))?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Internal("ffmpeg stdout not captured".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Internal("ffmpeg stderr not captured".into()))?;
        // Drained on its own thread so a chatty ffmpeg never blocks on a full pipe.
        let stderr_reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });

        let mut formatted = false;
        let pumped = pump(&mut stdout, channels, rate, sink, &mut formatted);
        if pumped.is_err() {
            let _ = child.kill();
        }
        drop(stdout);
        let status = child.wait();
        let stderr = stderr_reader.join().unwrap_or_default();
        let frames = pumped?;
        let status =
            status.map_err(|e| Error::io(format!("waiting for {}", ffmpeg.display()), e))?;
        if !status.success() {
            return Err(Error::Decode(format!(
                "{}: ffmpeg failed ({status}): {}",
                path.display(),
                tail(&stderr)
            )));
        }
        if !formatted {
            sink.format(rate)?;
        }
        info.duration_s = Some(frames as f64 / f64::from(rate));
        Ok(info)
    }
}

/// Reads interleaved f32le frames of `channels` channels from `reader` until
/// EOF, emitting them as mono (channel average) chunks. Calls `sink.format`
/// before the first samples and sets `formatted`. Returns the frame count.
fn pump(
    reader: &mut dyn Read,
    channels: u16,
    rate: u32,
    sink: &mut dyn PcmSink,
    formatted: &mut bool,
) -> Result<u64> {
    let channels = usize::from(channels.max(1));
    let frame_bytes = 4 * channels;
    let scale = 1.0 / channels as f32;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut mono = Vec::with_capacity(READ_CHUNK / frame_bytes);
    let mut pending = 0;
    let mut frames = 0u64;
    loop {
        let n = match reader.read(&mut buf[pending..]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::io("reading ffmpeg output", e)),
        };
        let total = pending + n;
        let whole = total - total % frame_bytes;
        mono.clear();
        mono.extend(buf[..whole].chunks_exact(frame_bytes).map(|frame| {
            frame
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .sum::<f32>()
                * scale
        }));
        if !mono.is_empty() {
            if !*formatted {
                sink.format(rate)?;
                *formatted = true;
            }
            sink.samples(&mono)?;
            frames += mono.len() as u64;
        }
        buf.copy_within(whole..total, 0);
        pending = total - whole;
    }
    Ok(frames)
}

/// `MediaInfo` for the `index`-th audio stream of an `ffprobe -print_format
/// json -show_format -show_streams` report.
fn parse_probe(json: &str, path: &Path, index: usize) -> Result<MediaInfo> {
    let bad = |what: &str| Error::Decode(format!("{}: ffprobe {what}", path.display()));
    let v: Value = serde_json::from_str(json).map_err(|e| bad(&format!("output: {e}")))?;
    let audio: Vec<&Value> = v["streams"]
        .as_array()
        .map(|s| {
            s.iter()
                .filter(|s| s["codec_type"].as_str() == Some("audio"))
                .collect()
        })
        .unwrap_or_default();
    let stream = audio
        .get(index)
        .ok_or_else(|| Error::Decode(format!("{}: no audio stream {index}", path.display())))?;
    let sample_rate = number(&stream["sample_rate"])
        .and_then(|r| u32::try_from(r as u64).ok())
        .filter(|r| *r > 0)
        .ok_or_else(|| bad("reports no sample rate for the audio stream"))?;
    let channels = number(&stream["channels"])
        .and_then(|c| u16::try_from(c as u64).ok())
        .filter(|c| *c > 0)
        .ok_or_else(|| bad("reports no channel count for the audio stream"))?;
    let format = &v["format"];
    let duration_s = number(&stream["duration"])
        .or_else(|| number(&format["duration"]))
        .filter(|d| d.is_finite() && *d >= 0.0);
    let created_at = [&format["tags"], &stream["tags"]]
        .into_iter()
        .filter_map(|tags| tags["creation_time"].as_str())
        .find_map(parse_media_date);
    let container = container_name(format["format_name"].as_str().unwrap_or(""));
    Ok(MediaInfo {
        sample_rate,
        channels,
        duration_s,
        created_at,
        audio_tracks: u32::try_from(audio.len()).unwrap_or(u32::MAX),
        container,
        tracks: audio
            .iter()
            .enumerate()
            .map(|(index, stream)| crate::TrackInfo {
                index,
                sample_rate: number(&stream["sample_rate"])
                    .and_then(|n| u32::try_from(n as u64).ok())
                    .unwrap_or(0),
                channels: number(&stream["channels"])
                    .and_then(|n| u16::try_from(n as u64).ok())
                    .unwrap_or(0),
                language: stream["tags"]["language"].as_str().map(str::to_owned),
                title: stream["tags"]["title"]
                    .as_str()
                    .or_else(|| stream["tags"]["handler_name"].as_str())
                    .map(str::to_owned),
            })
            .collect(),
    })
}

/// ffprobe prints most numbers as strings.
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `mov,mp4,m4a,3gp,3g2,mj2` → `mp4`; otherwise the first listed name.
fn container_name(format_name: &str) -> String {
    if format_name.split(',').any(|n| n == "mp4") {
        return "mp4".into();
    }
    format_name.split(',').next().unwrap_or("").to_string()
}

/// `file:` URL so names containing `:` are never taken for another protocol.
pub(crate) fn file_url(path: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(path.as_os_str());
    url
}

pub(crate) fn tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let start = text.len().saturating_sub(STDERR_TAIL);
    let start = (start..=text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    text[start..].to_string()
}

pub(crate) fn find_binary(name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    path_dirs
        .into_iter()
        .chain(EXTRA_BIN_DIRS.iter().map(PathBuf::from))
        .map(|dir| dir.join(&file))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::CollectSink;
    use crate::symphonia_decoder::test_audio::{peak, write_sine_wav};

    const PROBE_JSON: &str = r#"{
        "streams": [
            { "index": 0, "codec_type": "video", "codec_name": "h264" },
            { "index": 1, "codec_type": "audio", "codec_name": "aac",
              "sample_rate": "48000", "channels": 2, "duration": "12.500000",
              "tags": { "creation_time": "2025-06-01T09:30:00.000000Z", "language": "eng", "title": "mic" } },
            { "index": 2, "codec_type": "audio", "codec_name": "aac",
              "sample_rate": "44100", "channels": 1, "tags": { "language": "por", "title": "system" } }
        ],
        "format": { "format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "12.600000",
                    "tags": { "major_brand": "isom" } }
    }"#;

    #[test]
    fn parses_ffprobe_report() {
        let info = parse_probe(PROBE_JSON, Path::new("call.mp4"), 0).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 2);
        assert_eq!(info.audio_tracks, 2);
        assert_eq!(
            info.tracks,
            [
                crate::TrackInfo {
                    index: 0,
                    channels: 2,
                    sample_rate: 48_000,
                    language: Some("eng".into()),
                    title: Some("mic".into())
                },
                crate::TrackInfo {
                    index: 1,
                    channels: 1,
                    sample_rate: 44_100,
                    language: Some("por".into()),
                    title: Some("system".into())
                },
            ]
        );
        assert_eq!(info.container, "mp4");
        assert_eq!(info.duration_s, Some(12.5));
        assert_eq!(
            info.created_at.unwrap().to_rfc3339(),
            "2025-06-01T09:30:00+00:00"
        );

        let second = parse_probe(PROBE_JSON, Path::new("call.mp4"), 1).unwrap();
        assert_eq!(second.sample_rate, 44_100);
        assert_eq!(second.channels, 1);
        assert_eq!(second.duration_s, Some(12.6));
        assert_eq!(
            parse_probe(PROBE_JSON, Path::new("call.mp4"), 2)
                .unwrap_err()
                .code(),
            "decode"
        );

        let no_audio = r#"{"streams":[{"codec_type":"video"}],"format":{"format_name":"mov"}}"#;
        assert_eq!(
            parse_probe(no_audio, Path::new("x"), 0).unwrap_err().code(),
            "decode"
        );
        assert_eq!(container_name("wav"), "wav");
        assert_eq!(container_name("matroska,webm"), "matroska");
    }

    /// Hands out a few bytes per read so frames straddle read boundaries.
    struct Trickle<'a> {
        data: &'a [u8],
        step: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self.step.min(self.data.len()).min(out.len());
            out[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    #[test]
    fn pump_averages_channels_across_read_boundaries() {
        let frames: [[f32; 2]; 4] = [[1.0, 0.0], [0.5, 0.5], [-1.0, 0.0], [0.25, 0.75]];
        let bytes: Vec<u8> = frames
            .iter()
            .flatten()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut sink = CollectSink::default();
        let mut formatted = false;
        let mut reader = Trickle {
            data: &bytes,
            step: 3,
        };
        let n = pump(&mut reader, 2, 48_000, &mut sink, &mut formatted).unwrap();
        assert_eq!(n, 4);
        assert!(formatted);
        assert_eq!(sink.format_calls, 1);
        assert_eq!(sink.sample_rate, Some(48_000));
        assert_eq!(sink.samples, vec![0.5, 0.5, -0.5, 0.5]);
    }

    #[test]
    fn missing_binaries_fail_before_emitting() {
        let ff = FfmpegDecoder::with_binaries(None, None);
        assert!(!ff.is_available());
        let mut sink = CollectSink::default();
        let err = ff.decode(Path::new("x.mp4"), &mut sink).unwrap_err();
        assert_eq!(err.code(), "unsupported");
        assert_eq!(sink.format_calls, 0);
        assert_eq!(
            ff.probe(Path::new("x.mp4")).unwrap_err().code(),
            "unsupported"
        );
    }

    /// Runs the real binaries; skipped when ffmpeg is not installed.
    #[test]
    fn decodes_48k_source_at_its_own_rate() {
        let ff = FfmpegDecoder::locate();
        if !ff.is_available() {
            eprintln!("skipping: ffmpeg/ffprobe not found");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // A `:` in the name checks the `file:` prefix.
        let path = dir.path().join("sine:48k.wav");
        write_sine_wav(&path, 48_000, 2, 48_000, 440.0, 0.5);

        let info = ff.probe(&path).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 2);
        assert_eq!(info.container, "wav");

        let mut sink = CollectSink::default();
        let decoded = ff.decode(&path, &mut sink).unwrap();
        assert_eq!(sink.format_calls, 1);
        assert_eq!(sink.sample_rate, Some(48_000));
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(sink.samples.len(), 48_000);
        let p = peak(&sink.samples);
        assert!((p - 0.5).abs() < 0.01, "peak {p}");
    }

    #[test]
    fn reports_ffmpeg_failure_on_garbage() {
        let ff = FfmpegDecoder::locate();
        if !ff.is_available() {
            eprintln!("skipping: ffmpeg/ffprobe not found");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.m4a");
        std::fs::write(&path, b"not a media file").unwrap();
        let mut sink = CollectSink::default();
        assert_eq!(ff.decode(&path, &mut sink).unwrap_err().code(), "decode");
        assert_eq!(sink.format_calls, 0);
    }
}
