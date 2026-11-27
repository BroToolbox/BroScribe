//! Fallback [`AudioEncoder`] through `ffmpeg` subprocesses, for platforms
//! without a native AAC encoder in use (Windows, Linux).
//!
//! Each track is encoded on its own (mono f32le on ffmpeg's stdin → AAC in a
//! temporary `.m4a`, so long recordings never need raw PCM on disk), then the
//! encoded tracks are muxed with `-c copy` into one MP4. ffmpeg's MP4 muxer
//! enables only the first track of each media type and puts every audio
//! track in alternate group 1, so players would play one track only; the
//! `tkhd` boxes are patched afterwards so every track is enabled and in no
//! alternate group (players mix them; decoders still read each one alone).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::ffmpeg_decoder::{file_url, find_binary, tail};
use crate::{AudioEncoder, EncodeSettings, EncodedTrack, EncoderTrack, Error, Result};

/// Samples handed to ffmpeg per write.
const WRITE_CHUNK: usize = 16 * 1024;

#[derive(Clone, Debug, Default)]
pub struct FfmpegEncoder {
    ffmpeg: Option<PathBuf>,
}

impl FfmpegEncoder {
    pub const NAME: &'static str = "ffmpeg-aac";

    /// Finds `ffmpeg` in `PATH`, then in the usual extra folders.
    pub fn locate() -> Self {
        Self {
            ffmpeg: find_binary("ffmpeg"),
        }
    }

    pub fn with_binary(ffmpeg: Option<PathBuf>) -> Self {
        Self { ffmpeg }
    }

    pub fn is_available(&self) -> bool {
        self.ffmpeg.is_some()
    }

    fn ffmpeg(&self) -> Result<&Path> {
        self.ffmpeg.as_deref().ok_or_else(|| {
            Error::Unsupported("ffmpeg is not installed; cannot encode audio".into())
        })
    }

    /// Encodes one track from `source` into `part` (AAC in MP4).
    fn encode_part(
        &self,
        ffmpeg: &Path,
        part: &Path,
        settings: &EncodeSettings,
        track: &mut EncoderTrack<'_>,
    ) -> Result<u64> {
        let mut child = Command::new(ffmpeg)
            .args([
                "-nostdin", "-v", "error", "-n", "-f", "f32le", "-ac", "1", "-ar",
            ])
            .arg(settings.sample_rate.to_string())
            .args(["-i", "pipe:0", "-c:a", "aac", "-b:a"])
            .arg(settings.bitrate_per_track.to_string())
            .args(["-f", "mp4"])
            .arg(file_url(part))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::io(format!("running {}", ffmpeg.display()), e))?;
        let stderr_reader = drain_stderr(&mut child);
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Internal("ffmpeg stdin not captured".into()))?;

        let mut frames = 0u64;
        let mut samples = vec![0.0f32; WRITE_CHUNK];
        let mut bytes = Vec::with_capacity(WRITE_CHUNK * 4);
        let fed: Result<()> = (|| {
            loop {
                let n = track.source.read(&mut samples)?;
                if n == 0 {
                    return Ok(());
                }
                bytes.clear();
                for s in &samples[..n] {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                stdin
                    .write_all(&bytes)
                    .map_err(|e| Error::io("writing PCM to ffmpeg", e))?;
                frames += n as u64;
            }
        })();
        drop(stdin);
        if fed.is_err() {
            let _ = child.kill();
        }
        let status = child.wait();
        let stderr = stderr_reader.join().unwrap_or_default();
        fed?;
        let status =
            status.map_err(|e| Error::io(format!("waiting for {}", ffmpeg.display()), e))?;
        if !status.success() {
            return Err(Error::Internal(format!(
                "ffmpeg failed to encode track {} ({status}): {}",
                track.title,
                tail(&stderr)
            )));
        }
        Ok(frames)
    }

    fn mux(&self, ffmpeg: &Path, parts: &[PathBuf], titles: &[&str], out: &Path) -> Result<()> {
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-nostdin", "-v", "error", "-n"]);
        for part in parts {
            cmd.arg("-i").arg(file_url(part));
        }
        for i in 0..parts.len() {
            cmd.arg("-map").arg(format!("{i}:a:0"));
        }
        cmd.args(["-c", "copy", "-map_metadata", "-1"]);
        for (i, title) in titles.iter().enumerate() {
            cmd.arg(format!("-metadata:s:a:{i}"))
                .arg(format!("title={title}"));
        }
        cmd.args(["-f", "mp4"]).arg(file_url(out));
        let output = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| Error::io(format!("running {}", ffmpeg.display()), e))?;
        if !output.status.success() {
            return Err(Error::Internal(format!(
                "ffmpeg failed to mux tracks ({}): {}",
                output.status,
                tail(&output.stderr)
            )));
        }
        Ok(())
    }
}

impl AudioEncoder for FfmpegEncoder {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn extension(&self) -> &str {
        "m4a"
    }

    fn encode(
        &self,
        out: &Path,
        settings: &EncodeSettings,
        tracks: &mut [EncoderTrack<'_>],
    ) -> Result<Vec<EncodedTrack>> {
        let ffmpeg = self.ffmpeg()?;
        if out.exists() {
            return Err(Error::AlreadyExists(out.display().to_string()));
        }
        if tracks.is_empty() || settings.sample_rate == 0 || settings.bitrate_per_track == 0 {
            return Err(Error::InvalidInput(
                "tracks, sample rate and bit rate must be nonzero".into(),
            ));
        }
        let work = work_dir(out)?;
        let mut parts = Vec::with_capacity(tracks.len());
        let mut encoded = Vec::with_capacity(tracks.len());
        for (i, track) in tracks.iter_mut().enumerate() {
            let part = work.path().join(format!("track-{i}.m4a"));
            let frames = self.encode_part(ffmpeg, &part, settings, track)?;
            parts.push(part);
            encoded.push(EncodedTrack {
                title: track.title.to_string(),
                frames,
            });
        }
        let titles: Vec<&str> = tracks.iter().map(|t| t.title).collect();
        let muxed = work.path().join("audio.m4a");
        self.mux(ffmpeg, &parts, &titles, &muxed)?;
        enable_all_tracks(&muxed)?;
        // Publish only the complete file; never replace a concurrent output.
        tempfile::TempPath::try_from_path(&muxed)
            .map_err(|e| Error::io_at(&muxed, e))?
            .persist_noclobber(out)
            .map_err(|e| {
                if e.error.kind() == std::io::ErrorKind::AlreadyExists {
                    Error::AlreadyExists(out.display().to_string())
                } else {
                    Error::io_at(out, e.error)
                }
            })?;
        Ok(encoded)
    }
}

/// A private scratch folder next to `out` (same volume, removed afterwards).
fn work_dir(out: &Path) -> Result<tempfile::TempDir> {
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    tempfile::Builder::new()
        .prefix(".broscribe-encode-")
        .tempdir_in(parent)
        .map_err(|e| Error::io_at(parent, e))
}

fn drain_stderr(child: &mut Child) -> std::thread::JoinHandle<Vec<u8>> {
    let stderr = child.stderr.take();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_end(&mut buf);
        }
        buf
    })
}

/// `tkhd` flag: the track is enabled.
const TKHD_ENABLED: u32 = 0x1;

/// Sets the enabled flag and clears the alternate group of every track
/// header in the file's `moov` box, in place. Returns the number of tracks.
pub fn enable_all_tracks(path: &Path) -> Result<usize> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| Error::io_at(path, e))?;
    let len = file.metadata().map_err(|e| Error::io_at(path, e))?.len();
    let (moov_start, moov_len) = find_top_level(&mut file, len, b"moov")
        .map_err(|e| Error::io_at(path, e))?
        .ok_or_else(|| Error::Decode(format!("{}: no moov box", path.display())))?;
    let mut moov = vec![0u8; usize::try_from(moov_len).map_err(|_| bad_mp4(path))?];
    file.seek(SeekFrom::Start(moov_start))
        .and_then(|_| file.read_exact(&mut moov))
        .map_err(|e| Error::io_at(path, e))?;
    let tracks = patch_moov(&mut moov).ok_or_else(|| bad_mp4(path))?;
    file.seek(SeekFrom::Start(moov_start))
        .and_then(|_| file.write_all(&moov))
        .and_then(|_| file.sync_all())
        .map_err(|e| Error::io_at(path, e))?;
    Ok(tracks)
}

fn bad_mp4(path: &Path) -> Error {
    Error::Decode(format!("{}: malformed MP4 boxes", path.display()))
}

/// Offset and size (header included) of the first top-level box of `kind`.
fn find_top_level(
    file: &mut File,
    len: u64,
    kind: &[u8; 4],
) -> std::io::Result<Option<(u64, u64)>> {
    let mut pos = 0u64;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8])?;
        let mut size = u64::from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]));
        let mut header_len = 8;
        if size == 1 {
            header_len = 16;
            file.read_exact(&mut header[8..16])?;
            size = u64::from_be_bytes(header[8..16].try_into().expect("8 bytes"));
        } else if size == 0 {
            size = len - pos;
        }
        if size < header_len || size > len - pos {
            return Ok(None);
        }
        if &header[4..8] == kind {
            return Ok(Some((pos, size)));
        }
        pos += size;
    }
    Ok(None)
}

type Mp4Box = ([u8; 4], usize, usize, usize);

/// Child boxes of `data` as `(kind, header_len, start, end)`.
fn boxes(data: &[u8]) -> Option<Vec<Mp4Box>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let mut size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().ok()?;
        let mut header = 8;
        if size == 1 {
            size = usize::try_from(u64::from_be_bytes(
                data.get(pos + 8..pos + 16)?.try_into().ok()?,
            ))
            .ok()?;
            header = 16;
        } else if size == 0 {
            size = data.len() - pos;
        }
        if size < header || size > data.len() - pos {
            return None;
        }
        out.push((kind, header, pos, pos + size));
        pos += size;
    }
    (pos == data.len()).then_some(out)
}

/// Patches every `trak/tkhd` inside a whole `moov` box (header included).
fn patch_moov(moov: &mut [u8]) -> Option<usize> {
    let (_, header, _, end) = *boxes(moov)?.first()?;
    let body_start = header;
    let mut tracks = 0;
    for (kind, h, start, end) in boxes(&moov[body_start..end])? {
        if &kind != b"trak" {
            continue;
        }
        let trak = &mut moov[body_start + start + h..body_start + end];
        for (kind, h, start, end) in boxes(trak)? {
            if &kind != b"tkhd" {
                continue;
            }
            let tkhd = &mut trak[start + h..end];
            if tkhd.len() < 4 {
                return None;
            }
            let version = tkhd[0];
            let flags = u32::from_be_bytes([0, tkhd[1], tkhd[2], tkhd[3]]) | TKHD_ENABLED;
            tkhd[1..4].copy_from_slice(&flags.to_be_bytes()[1..]);
            // creation, modification, track id, reserved, duration (32/64-bit
            // fields by version), 8 reserved bytes, layer, then the group.
            let group = 4 + if version == 1 { 32 } else { 20 } + 8 + 2;
            tkhd.get_mut(group..group + 2)?.copy_from_slice(&[0, 0]);
            tracks += 1;
        }
    }
    Some(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MediaDecoder;
    use crate::encoder::SliceSource;
    use crate::fakes::CollectSink;
    use crate::ffmpeg_decoder::FfmpegDecoder;

    fn make_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn tkhd(version: u8, flags: u8, group: u16) -> Vec<u8> {
        let mut body = vec![version, 0, 0, flags];
        body.extend(std::iter::repeat_n(0u8, if version == 1 { 32 } else { 20 }));
        body.extend([0u8; 8]);
        body.extend([0, 0]); // layer
        body.extend(group.to_be_bytes());
        body.extend([0u8; 2 + 2 + 36 + 8]);
        make_box(b"tkhd", &body)
    }

    fn read_tkhd(moov: &[u8]) -> Vec<(u32, u16)> {
        let (_, h, _, end) = boxes(moov).unwrap()[0];
        let mut out = Vec::new();
        for (kind, th, start, tend) in boxes(&moov[h..end]).unwrap() {
            if &kind != b"trak" {
                continue;
            }
            let trak = &moov[h + start + th..h + tend];
            let (_, kh, kstart, _) = boxes(trak).unwrap()[0];
            let t = &trak[kstart + kh..];
            let flags = u32::from_be_bytes([0, t[1], t[2], t[3]]);
            let g = 4 + if t[0] == 1 { 32 } else { 20 } + 10;
            out.push((flags, u16::from_be_bytes([t[g], t[g + 1]])));
        }
        out
    }

    #[test]
    fn patches_flags_and_groups_of_every_track() {
        let trak0 = make_box(b"trak", &tkhd(0, 3, 1));
        let trak1 = make_box(
            b"trak",
            &[tkhd(1, 2, 1), make_box(b"mdia", &[0; 4])].concat(),
        );
        let mut moov = make_box(
            b"moov",
            &[make_box(b"mvhd", &[0; 100]), trak0, trak1].concat(),
        );
        assert_eq!(patch_moov(&mut moov), Some(2));
        assert_eq!(read_tkhd(&moov), vec![(3, 0), (3, 0)]);
        // Truncated input is refused, not patched past its end.
        let mut short = make_box(
            b"moov",
            &make_box(b"trak", &make_box(b"tkhd", &[0, 0, 0, 2])),
        );
        assert_eq!(patch_moov(&mut short), None);
        // Extended lengths cannot wrap bounds or leave trailing garbage.
        let mut overflowing = make_box(b"moov", &[1, 0, 0, 0, b't', b'r', b'a', b'k']);
        overflowing[8..12].copy_from_slice(&1u32.to_be_bytes());
        overflowing.extend_from_slice(&u64::MAX.to_be_bytes());
        overflowing[..4].copy_from_slice(&24u32.to_be_bytes());
        assert_eq!(patch_moov(&mut overflowing), None);
        let mut trailing = make_box(
            b"moov",
            &[make_box(b"trak", &tkhd(0, 0, 1)), vec![0]].concat(),
        );
        assert_eq!(patch_moov(&mut trailing), None);
    }

    #[test]
    fn missing_ffmpeg_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = SliceSource::new(vec![0.0; 10]);
        let err = FfmpegEncoder::with_binary(None)
            .encode(
                &dir.path().join("a.m4a"),
                &EncodeSettings::speech(16_000),
                &mut [EncoderTrack {
                    title: "mic",
                    source: &mut src,
                }],
            )
            .unwrap_err();
        assert_eq!(err.code(), "unsupported");
    }

    #[test]
    fn encodes_two_enabled_tracks_when_ffmpeg_is_installed() {
        let encoder = FfmpegEncoder::locate();
        let decoder = FfmpegDecoder::locate();
        if !encoder.is_available() || !decoder.is_available() {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("audio.m4a");
        let tone = |f: f32| -> Vec<f32> {
            (0..16_000)
                .map(|i| 0.5 * (std::f32::consts::TAU * f * i as f32 / 16_000.0).sin())
                .collect()
        };
        let mut mic = SliceSource::new(tone(440.0));
        let mut system = SliceSource::new(vec![0.0; 8_000]);
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
        assert_eq!(written[0].frames, 16_000);
        assert_eq!(written[1].frames, 8_000);
        assert!(!dir.path().read_dir().unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".broscribe-encode-")
        }));

        let mut file = File::open(&out).unwrap();
        let len = file.metadata().unwrap().len();
        let (start, size) = find_top_level(&mut file, len, b"moov").unwrap().unwrap();
        let mut moov = vec![0; size as usize];
        file.seek(SeekFrom::Start(start)).unwrap();
        file.read_exact(&mut moov).unwrap();
        for (flags, group) in read_tkhd(&moov) {
            assert_eq!(flags & TKHD_ENABLED, TKHD_ENABLED);
            assert_eq!(group, 0);
        }

        let info = decoder.probe(&out).unwrap();
        assert_eq!(info.audio_tracks, 2);
        let mut sink = CollectSink::default();
        decoder.decode_track(&out, 0, &mut sink).unwrap();
        let peak = sink.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.3, "mic track peak {peak}");

        // Never overwrites.
        let mut again = SliceSource::new(vec![0.0; 10]);
        assert!(
            encoder
                .encode(
                    &out,
                    &EncodeSettings::speech(16_000),
                    &mut [EncoderTrack {
                        title: "mic",
                        source: &mut again
                    }],
                )
                .is_err()
        );
    }

    #[test]
    fn concurrent_output_is_preserved() {
        let encoder = FfmpegEncoder::locate();
        if !encoder.is_available() {
            eprintln!("ffmpeg not installed; skipping");
            return;
        }
        struct RacingSource {
            out: PathBuf,
            wrote: bool,
        }
        impl crate::PcmSource for RacingSource {
            fn read(&mut self, out: &mut [f32]) -> Result<usize> {
                if self.wrote {
                    return Ok(0);
                }
                std::fs::write(&self.out, b"concurrent output").unwrap();
                self.wrote = true;
                out[..1024].fill(0.0);
                Ok(1024)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("audio.m4a");
        let mut source = RacingSource {
            out: out.clone(),
            wrote: false,
        };
        let error = encoder
            .encode(
                &out,
                &EncodeSettings::speech(16_000),
                &mut [EncoderTrack {
                    title: "system",
                    source: &mut source,
                }],
            )
            .unwrap_err();
        assert_eq!(error.code(), "already_exists");
        assert_eq!(std::fs::read(out).unwrap(), b"concurrent output");
        assert_eq!(dir.path().read_dir().unwrap().count(), 1);
    }
}
