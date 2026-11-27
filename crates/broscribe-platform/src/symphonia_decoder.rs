//! Portable decoder on symphonia (isomp4, aac, mp3, pcm, wav, flac, ogg/vorbis).
//!
//! Decodes the default audio track packet by packet, downmixes every decoded
//! buffer to mono (average across channels) and hands it to the sink at the
//! stream's own sample rate. A packet that fails to decode, or a hole between
//! consecutive packet timestamps, becomes silence of the same length so the
//! output keeps the source's timeline; without timing to size that silence
//! the decode fails rather than returning time-compressed audio.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, Track, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTag};

use crate::{Error, MediaDecoder, MediaInfo, PcmSink, Result};

/// Pure-Rust decoder; the first entry of every decoder chain.
#[derive(Clone, Copy, Debug, Default)]
pub struct SymphoniaDecoder;

impl SymphoniaDecoder {
    pub const NAME: &'static str = "symphonia";

    pub fn new() -> Self {
        Self
    }
}

impl MediaDecoder for SymphoniaDecoder {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        let mut format = open(path)?;
        let track = audio_track(format.as_ref(), path)?.clone();
        media_info(&mut *format, &track, path)
    }

    fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        decode_selected(path, None, sink)
    }

    fn decode_track(&self, path: &Path, index: usize, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        decode_selected(path, Some(index), sink)
    }
}

/// Decodes the default audio track (`index` = `None`) or the `index`-th
/// audio track in container order.
fn decode_selected(path: &Path, index: Option<usize>, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
    let mut format = open(path)?;
    let mut track = select_track(format.as_ref(), path, index)?.clone();
    let mut info = media_info(&mut *format, &track, path)?;
    let mut decoder = make_decoder(&track, path)?;

    let mut rate: Option<u32> = None;
    let mut planes: Vec<Vec<f32>> = Vec::new();
    let mut mono: Vec<f32> = Vec::new();
    let mut frames: u64 = 0;
    let mut decoded: u64 = 0;
    let mut skipped: u64 = 0;
    let mut timeline = Timeline::new(&track);

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymError::ResetRequired) => {
                // The track list changed (e.g. chained OGG streams): pick the
                // same audio track again and rebuild its decoder.
                track = select_track(format.as_ref(), path, index)?.clone();
                decoder = make_decoder(&track, path)?;
                timeline = Timeline::new(&track);
                continue;
            }
            // Truncated files end with an unexpected EOF: keep what was decoded.
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => return Err(convert(e, path)),
        };
        if packet.track_id != track.id {
            continue;
        }
        let (pts, dur) = (packet.pts.get(), packet.dur.get());
        let result = decoder.decode(&packet);
        if let Err(SymError::DecodeError(_) | SymError::IoError(_)) = result {
            skipped += 1;
            let r = timeline_rate(rate, info.sample_rate, path)?;
            let Some(silence) = timeline.lost(pts, dur, r) else {
                return Err(Error::Decode(format!(
                    "{}: an audio packet failed to decode and its duration is unknown",
                    path.display()
                )));
            };
            if rate.is_none() {
                sink.format(r)?;
                rate = Some(r);
            }
            frames += emit_silence(sink, silence, &mut mono)?;
            continue;
        }
        // Silence for a hole since the previous packet; nothing is emitted
        // before the first audio, so a leading offset is not padded.
        if let Some(r) = rate {
            frames += emit_silence(sink, timeline.gap_before(pts, r), &mut mono)?;
        }
        timeline.advance(pts, dur);
        let buf = match result {
            Ok(buf) => buf,
            Err(SymError::ResetRequired) => {
                decoder = make_decoder(&track, path)?;
                continue;
            }
            Err(e) => return Err(convert(e, path)),
        };
        if buf.frames() == 0 {
            continue;
        }
        let buf_rate = buf.spec().rate();
        match rate {
            None => {
                sink.format(buf_rate)?;
                rate = Some(buf_rate);
            }
            Some(r) if r != buf_rate => {
                return Err(Error::Decode(format!(
                    "{}: sample rate changed mid-stream ({r} Hz to {buf_rate} Hz)",
                    path.display()
                )));
            }
            Some(_) => {}
        }
        buf.copy_to_vecs_planar::<f32>(&mut planes);
        downmix(&planes, buf.frames(), &mut mono);
        sink.samples(&mono)?;
        frames += mono.len() as u64;
        decoded += mono.len() as u64;
    }

    if decoded == 0 && skipped > 0 {
        return Err(Error::Decode(format!(
            "{}: every audio packet failed to decode ({skipped} packets)",
            path.display()
        )));
    }
    if skipped > 0 {
        tracing::warn!(
            path = %path.display(),
            skipped,
            "replaced audio packets that failed to decode with silence"
        );
    }
    let rate = match rate {
        Some(r) => r,
        None => {
            // No audio at all: still report the stream rate once.
            if info.sample_rate == 0 {
                return Err(Error::Decode(format!(
                    "{}: audio track has no sample rate",
                    path.display()
                )));
            }
            sink.format(info.sample_rate)?;
            info.sample_rate
        }
    };
    info.sample_rate = rate;
    // Keep the container's own duration: callers compare it with the emitted
    // length to catch streams decoded at the wrong rate. Only a container
    // without one gets the decoded length.
    if info.duration_s.is_none() {
        info.duration_s = Some(frames as f64 / f64::from(rate));
    }
    Ok(info)
}

/// The rate silence is sized in: the decoded rate, else the codec's declared
/// rate.
fn timeline_rate(decoded: Option<u32>, declared: u32, path: &Path) -> Result<u32> {
    match decoded {
        Some(r) => Ok(r),
        None if declared > 0 => Ok(declared),
        None => Err(Error::Decode(format!(
            "{}: an audio packet failed to decode and the sample rate is unknown",
            path.display()
        ))),
    }
}

/// Sends `frames` zero samples to `sink` in bounded chunks, reusing `scratch`.
fn emit_silence(sink: &mut dyn PcmSink, frames: u64, scratch: &mut Vec<f32>) -> Result<u64> {
    const CHUNK: u64 = 8_192;
    let mut left = frames;
    while left > 0 {
        let n = left.min(CHUNK) as usize;
        scratch.clear();
        scratch.resize(n, 0.0);
        sink.samples(scratch)?;
        left -= n as u64;
    }
    Ok(frames)
}

/// The stream position in time-base ticks, used to size the silence that
/// stands in for lost packets and for holes between packet timestamps.
#[derive(Clone, Copy, Debug)]
struct Timeline {
    /// Time base as `(numer, denom)` seconds per tick; `None` when the track
    /// does not declare one.
    time_base: Option<(u32, u32)>,
    /// End (`pts + dur`) of the last packet, once one has been seen.
    next: Option<i64>,
}

impl Timeline {
    fn new(track: &Track) -> Self {
        Self::with_time_base(track.time_base.map(|tb| (tb.numer.get(), tb.denom.get())))
    }

    fn with_time_base(time_base: Option<(u32, u32)>) -> Self {
        Self {
            time_base,
            next: None,
        }
    }

    /// Frames (at `rate`) of the hole between the previous packet's end and
    /// `pts`; 0 for the first packet, for overlaps and without a time base.
    fn gap_before(&self, pts: i64, rate: u32) -> u64 {
        match (self.next, self.time_base) {
            (Some(next), Some(tb)) if pts > next => frames_between(next, pts, tb, rate),
            _ => 0,
        }
    }

    /// Moves the position past a packet.
    fn advance(&mut self, pts: i64, dur: u64) {
        let end = pts.saturating_add_unsigned(dur);
        self.next = Some(self.next.map_or(end, |n| n.max(end)));
    }

    /// Frames of silence that replace a packet which failed to decode, plus
    /// any hole before it; `None` when its length cannot be known (no time
    /// base or no packet duration).
    fn lost(&mut self, pts: i64, dur: u64, rate: u32) -> Option<u64> {
        let tb = self.time_base?;
        if dur == 0 {
            return None;
        }
        // From the previous packet's end (covering any hole, skipping any
        // overlap) to this packet's end.
        let from = self.next.unwrap_or(pts);
        let end = pts.saturating_add_unsigned(dur);
        self.advance(pts, dur);
        Some(if end > from {
            frames_between(from, end, tb, rate)
        } else {
            0
        })
    }
}

/// Frames at `rate` between tick positions `from < to`, rounding each
/// position the same way so consecutive spans add up exactly.
fn frames_between(from: i64, to: i64, (numer, denom): (u32, u32), rate: u32) -> u64 {
    let at = |ts: i64| i128::from(ts) * i128::from(numer) * i128::from(rate) / i128::from(denom);
    u64::try_from(at(to) - at(from)).unwrap_or(0)
}

/// Averages `planes` (one per channel, each at least `frames` long) into `out`.
fn downmix(planes: &[Vec<f32>], frames: usize, out: &mut Vec<f32>) {
    out.clear();
    match planes {
        [] => out.resize(frames, 0.0),
        [only] => out.extend_from_slice(&only[..frames]),
        [first, rest @ ..] => {
            out.extend_from_slice(&first[..frames]);
            for plane in rest {
                for (o, s) in out.iter_mut().zip(&plane[..frames]) {
                    *o += *s;
                }
            }
            let scale = 1.0 / planes.len() as f32;
            for o in out.iter_mut() {
                *o *= scale;
            }
        }
    }
}

fn open(path: &Path) -> Result<Box<dyn FormatReader>> {
    let file = File::open(path).map_err(|e| Error::io_at(path, e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| convert(e, path))
}

fn audio_track<'a>(format: &'a dyn FormatReader, path: &Path) -> Result<&'a Track> {
    format
        .default_track(TrackType::Audio)
        .ok_or_else(|| Error::Decode(format!("{}: no audio track", path.display())))
}

/// The default audio track (`None`) or the `index`-th audio track in
/// container order.
fn select_track<'a>(
    format: &'a dyn FormatReader,
    path: &Path,
    index: Option<usize>,
) -> Result<&'a Track> {
    let Some(index) = index else {
        return audio_track(format, path);
    };
    format
        .tracks()
        .iter()
        .filter(|t| is_audio(t))
        .nth(index)
        .ok_or_else(|| Error::Decode(format!("{}: no audio track {index}", path.display())))
}

fn is_audio(track: &Track) -> bool {
    track
        .codec_params
        .as_ref()
        .is_some_and(|p| p.audio().is_some())
}

fn make_decoder(track: &Track, path: &Path) -> Result<Box<dyn AudioDecoder>> {
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| Error::Decode(format!("{}: no audio codec parameters", path.display())))?;
    symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| convert(e, path))
}

fn media_info(format: &mut dyn FormatReader, track: &Track, path: &Path) -> Result<MediaInfo> {
    let params = track.codec_params.as_ref().and_then(|p| p.audio());
    let sample_rate = params.and_then(|p| p.sample_rate).unwrap_or(0);
    let channels = params
        .and_then(|p| p.channels.as_ref())
        .map(|c| u16::try_from(c.count()).unwrap_or(u16::MAX))
        .unwrap_or(0);
    let duration_s = match (track.num_frames, sample_rate) {
        (Some(n), r) if r > 0 => Some(n as f64 / f64::from(r)),
        _ => track
            .time_base
            .zip(track.duration)
            .and_then(|(tb, d)| tb.calc_duration(d))
            .map(|t| t.as_secs_f64()),
    };
    let mut tracks: Vec<(u32, crate::TrackInfo)> = format
        .tracks()
        .iter()
        .filter(|track| is_audio(track))
        .enumerate()
        .map(|(index, track)| {
            let params = track
                .codec_params
                .as_ref()
                .and_then(|params| params.audio());
            (
                track.id,
                crate::TrackInfo {
                    index,
                    sample_rate: params.and_then(|params| params.sample_rate).unwrap_or(0),
                    channels: params
                        .and_then(|params| params.channels.as_ref())
                        .map(|channels| u16::try_from(channels.count()).unwrap_or(u16::MAX))
                        .unwrap_or(0),
                    language: track.language.clone(),
                    title: None,
                },
            )
        })
        .collect();
    let container = container_name(format.format_info().short_name);
    let created_at = {
        let mut metadata = format.metadata();
        if let Some(rev) = metadata.skip_to_latest() {
            let single_track = tracks.len() == 1;
            for (id, info) in &mut tracks {
                let per_track = rev
                    .per_track
                    .iter()
                    .filter(|tags| tags.track_id == u64::from(*id))
                    .flat_map(|tags| tags.metadata.tags.iter());
                let global = rev.media.tags.iter().filter(|_| single_track);
                for tag in per_track.chain(global) {
                    match &tag.std {
                        Some(StandardTag::TrackTitle(title)) if info.title.is_none() => {
                            info.title = Some(title.to_string())
                        }
                        Some(StandardTag::Language(language)) if info.language.is_none() => {
                            info.language = Some(language.to_string())
                        }
                        _ => {}
                    }
                }
            }
            created_at(rev, track.id)
        } else {
            None
        }
    };
    // Symphonia 0.6 reads mdhd language and hdlr names internally, but does not
    // expose them on its public MP4 Track. Read just those small metadata boxes
    // (and per-track title tags), never audio payloads, to retain recorder labels.
    if container == "mp4" {
        for metadata in mp4_track_metadata(path)? {
            if let Some((_, info)) = tracks.iter_mut().find(|(id, _)| *id == metadata.id) {
                if info.language.is_none() {
                    info.language = metadata.language;
                }
                if info.title.is_none() {
                    info.title = metadata.title;
                }
            }
        }
    }
    if sample_rate == 0 {
        tracing::debug!(path = %path.display(), "audio track does not declare a sample rate");
    }
    Ok(MediaInfo {
        sample_rate,
        channels,
        duration_s,
        created_at,
        audio_tracks: u32::try_from(tracks.len()).unwrap_or(u32::MAX),
        tracks: tracks.into_iter().map(|(_, info)| info).collect(),
        container,
    })
}

struct Mp4TrackMetadata {
    id: u32,
    language: Option<String>,
    title: Option<String>,
}

#[derive(Clone, Copy)]
struct Mp4Box {
    kind: [u8; 4],
    payload: u64,
    end: u64,
}

fn invalid_mp4() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid MP4 metadata box")
}

/// ISO BMFF boxes have an 8-byte header, optionally followed by a 64-bit
/// extended size. Size zero means the remainder of the enclosing box.
fn mp4_children(file: &mut File, start: u64, end: u64) -> std::io::Result<Vec<Mp4Box>> {
    let mut boxes = Vec::new();
    let mut at = start;
    while end.saturating_sub(at) >= 8 {
        file.seek(SeekFrom::Start(at))?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let size = u32::from_be_bytes(header[..4].try_into().unwrap());
        let (size, header_size) = match size {
            0 => (end - at, 8),
            1 => {
                let mut extended = [0u8; 8];
                file.read_exact(&mut extended)?;
                (u64::from_be_bytes(extended), 16)
            }
            size => (u64::from(size), 8),
        };
        let next = at
            .checked_add(size)
            .filter(|next| *next <= end)
            .ok_or_else(invalid_mp4)?;
        if size < header_size {
            return Err(invalid_mp4());
        }
        boxes.push(Mp4Box {
            kind: header[4..].try_into().unwrap(),
            payload: at + header_size,
            end: next,
        });
        at = next;
    }
    Ok(boxes)
}

fn mp4_fixed<const N: usize>(
    file: &mut File,
    atom: Mp4Box,
    offset: u64,
) -> std::io::Result<[u8; N]> {
    let start = atom.payload.checked_add(offset).ok_or_else(invalid_mp4)?;
    if start.checked_add(N as u64).is_none_or(|end| end > atom.end) {
        return Err(invalid_mp4());
    }
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = [0; N];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn mp4_string(file: &mut File, atom: Mp4Box, offset: u64) -> std::io::Result<Option<String>> {
    let start = atom.payload.checked_add(offset).ok_or_else(invalid_mp4)?;
    let len = atom
        .end
        .checked_sub(start)
        .filter(|len| *len <= 65_536)
        .ok_or_else(invalid_mp4)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; len as usize];
    file.read_exact(&mut bytes)?;
    let bytes = bytes.split(|b| *b == 0).next().unwrap_or_default();
    Ok(std::str::from_utf8(bytes)
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned))
}

fn mp4_title(file: &mut File, parent: Mp4Box, depth: u8) -> std::io::Result<Option<String>> {
    if depth > 6 {
        return Err(invalid_mp4());
    }
    let skip = if parent.kind == *b"meta" { 4 } else { 0 };
    for atom in mp4_children(file, parent.payload + skip, parent.end)? {
        if parent.kind == *b"\xa9nam" && atom.kind == *b"data" {
            return mp4_string(file, atom, 8); // data type/locale, then UTF-8
        }
        if matches!(&atom.kind, b"udta" | b"meta" | b"ilst" | b"\xa9nam")
            && let Some(title) = mp4_title(file, atom, depth + 1)?
        {
            return Ok(Some(title));
        }
    }
    Ok(None)
}

fn mp4_track_metadata(path: &Path) -> Result<Vec<Mp4TrackMetadata>> {
    let read = || -> std::io::Result<Vec<Mp4TrackMetadata>> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        let mut result = Vec::new();
        for moov in mp4_children(&mut file, 0, length)?
            .into_iter()
            .filter(|atom| atom.kind == *b"moov")
        {
            for trak in mp4_children(&mut file, moov.payload, moov.end)?
                .into_iter()
                .filter(|atom| atom.kind == *b"trak")
            {
                let children = mp4_children(&mut file, trak.payload, trak.end)?;
                let Some(tkhd) = children.iter().find(|atom| atom.kind == *b"tkhd").copied() else {
                    continue;
                };
                let version = mp4_fixed::<1>(&mut file, tkhd, 0)?[0];
                let id = mp4_fixed::<4>(&mut file, tkhd, if version == 1 { 20 } else { 12 })?;
                let mut info = Mp4TrackMetadata {
                    id: u32::from_be_bytes(id),
                    language: None,
                    title: mp4_title(&mut file, trak, 0)?,
                };
                if let Some(mdia) = children.iter().find(|atom| atom.kind == *b"mdia") {
                    for atom in mp4_children(&mut file, mdia.payload, mdia.end)? {
                        if atom.kind == *b"mdhd" {
                            let version = mp4_fixed::<1>(&mut file, atom, 0)?[0];
                            let language = mp4_fixed::<2>(
                                &mut file,
                                atom,
                                if version == 1 { 32 } else { 20 },
                            )?;
                            let packed = u16::from_be_bytes(language);
                            let language = [
                                ((packed >> 10) & 31) as u8 + 0x60,
                                ((packed >> 5) & 31) as u8 + 0x60,
                                (packed & 31) as u8 + 0x60,
                            ];
                            if language.iter().all(u8::is_ascii_lowercase) && language != *b"und" {
                                info.language =
                                    Some(std::str::from_utf8(&language).unwrap().to_owned());
                            }
                        } else if atom.kind == *b"hdlr" && info.title.is_none() {
                            info.title = mp4_string(&mut file, atom, 24)?;
                        }
                    }
                }
                result.push(info);
            }
        }
        Ok(result)
    };
    read().map_err(|error| Error::io("reading MP4 track metadata", error))
}

fn container_name(short_name: &str) -> String {
    match short_name {
        "wave" => "wav",
        "isomp4" => "mp4",
        other => other,
    }
    .to_string()
}

/// Recording date from media-level tags, then the track's own tags.
fn created_at(rev: &MetadataRevision, track_id: u32) -> Option<DateTime<FixedOffset>> {
    let track_tags = rev
        .per_track
        .iter()
        .filter(|t| t.track_id == u64::from(track_id))
        .flat_map(|t| t.metadata.tags.iter());
    rev.media
        .tags
        .iter()
        .chain(track_tags)
        .filter_map(|tag| match tag.std.as_ref()? {
            StandardTag::RecordingDate(s)
            | StandardTag::EncodingDate(s)
            | StandardTag::OriginalRecordingDate(s) => parse_media_date(s),
            _ => None,
        })
        .next()
}

/// Parses container date strings: RFC 3339, `YYYY-MM-DD[ T]HH:MM:SS[.fff]`
/// (taken as UTC) or `YYYY-MM-DD` (midnight UTC). Anything vaguer is `None`.
pub(crate) fn parse_media_date(s: &str) -> Option<DateTime<FixedOffset>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt);
    }
    let utc = FixedOffset::east_opt(0)?;
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive.and_utc().with_timezone(&utc));
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|naive| naive.and_utc().with_timezone(&utc))
}

fn convert(e: SymError, path: &Path) -> Error {
    match e {
        SymError::IoError(io) => Error::io_at(path, io),
        SymError::Unsupported(what) => Error::Unsupported(format!("{}: {what}", path.display())),
        other => Error::Decode(format!("{}: {other}", path.display())),
    }
}

/// Synthetic audio for tests across this crate.
#[cfg(test)]
pub(crate) mod test_audio {
    use std::path::Path;

    /// Writes a 16-bit PCM WAV where every channel carries the same sine of
    /// `amplitude` (0..1) at `freq` Hz.
    pub(crate) fn write_sine_wav(
        path: &Path,
        sample_rate: u32,
        channels: u16,
        frames: u32,
        freq: f32,
        amplitude: f32,
    ) {
        let block_align = channels * 2;
        let data_len = frames * u32::from(block_align);
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let t = i as f32 / sample_rate as f32;
            let v = (amplitude * (2.0 * std::f32::consts::PI * freq * t).sin() * 32767.0) as i16;
            for _ in 0..channels {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        std::fs::write(path, out).unwrap();
    }

    pub(crate) fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::test_audio::{peak, write_sine_wav};
    use super::*;
    use crate::DecoderChain;
    use crate::fakes::{CollectSink, FakeDecoder};

    #[test]
    fn decodes_stereo_wav_to_mono_at_source_rate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sine.wav");
        write_sine_wav(&path, 44_100, 2, 44_100, 440.0, 0.5);

        let info = SymphoniaDecoder.probe(&path).unwrap();
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.audio_tracks, 1);
        assert_eq!(
            info.tracks,
            [crate::TrackInfo {
                index: 0,
                channels: 2,
                sample_rate: 44_100,
                ..Default::default()
            }]
        );
        assert_eq!(info.container, "wav");
        assert!((info.duration_s.unwrap() - 1.0).abs() < 1e-6);

        let mut sink = CollectSink::default();
        let decoded = SymphoniaDecoder.decode(&path, &mut sink).unwrap();
        assert_eq!(sink.format_calls, 1);
        assert_eq!(sink.sample_rate, Some(44_100));
        assert_eq!(decoded.sample_rate, 44_100);
        assert_eq!(sink.samples.len(), 44_100);
        let p = peak(&sink.samples);
        assert!((p - 0.5).abs() < 0.01, "peak {p}");
    }

    /// The reported duration is the container's, not the emitted length, so
    /// callers can tell a short or wrong-rate decode from the file itself.
    #[test]
    fn decode_reports_the_container_duration_not_the_emitted_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.wav");
        write_sine_wav(&path, 16_000, 1, 16_000, 440.0, 0.5);
        // The header now promises 2 s; the data holds 1 s.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[40..44].copy_from_slice(&64_000u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&(36 + 64_000u32).to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let mut sink = CollectSink::default();
        let info = SymphoniaDecoder.decode(&path, &mut sink).unwrap();
        assert_eq!(sink.samples.len(), 16_000);
        let d = info.duration_s.unwrap();
        assert!((d - 2.0).abs() < 1e-6, "{d}");
    }

    /// A synthetic stream of 1024-tick packets at a 1/48 kHz time base
    /// decoded at 16 kHz: a failed middle packet and a missing packet both
    /// become silence, so the total length matches the timeline.
    #[test]
    fn lost_and_missing_packets_keep_the_timeline() {
        const RATE: u32 = 16_000;
        let mut t = Timeline::with_time_base(Some((1, 48_000)));
        let mut total = 0u64;
        // (pts, decodes?) for packets of 1024 ticks; 3072 is never sent.
        for (pts, ok) in [(0, true), (1024, false), (2048, true), (4096, true)] {
            if ok {
                total += t.gap_before(pts, RATE);
                t.advance(pts, 1024);
                total += frames_between(pts, pts + 1024, (1, 48_000), RATE);
            } else {
                let silence = t.lost(pts, 1024, RATE).unwrap();
                assert_eq!(silence, 341, "1024 ticks at 1/48k is 341.3 frames at 16k");
                total += silence;
            }
        }
        assert_eq!(total, frames_between(0, 5 * 1024, (1, 48_000), RATE));
        assert_eq!(total, 1706);
    }

    #[test]
    fn timeline_gaps_and_overlaps() {
        let mut t = Timeline::with_time_base(Some((1, 16_000)));
        assert_eq!(
            t.gap_before(500, 16_000),
            0,
            "no gap before the first packet"
        );
        t.advance(500, 100);
        assert_eq!(t.gap_before(600, 16_000), 0);
        assert_eq!(t.gap_before(550, 16_000), 0, "overlaps are not padded");
        assert_eq!(t.gap_before(700, 16_000), 100);
        assert_eq!(t.gap_before(700, 8_000), 50);
        // A lost packet after a hole covers the hole too.
        assert_eq!(t.lost(700, 100, 16_000), Some(200));
        // A lost packet overlapping the previous one covers only the new part.
        assert_eq!(t.lost(750, 100, 16_000), Some(50));
    }

    #[test]
    fn lost_packet_without_timing_is_unknown() {
        assert_eq!(Timeline::with_time_base(None).lost(0, 1024, 16_000), None);
        assert_eq!(
            Timeline::with_time_base(Some((1, 16_000))).lost(0, 0, 16_000),
            None
        );
        assert_eq!(Timeline::with_time_base(None).gap_before(10, 16_000), 0);
        let err = timeline_rate(None, 0, Path::new("x.wav")).unwrap_err();
        assert_eq!(err.code(), "decode");
        assert_eq!(
            timeline_rate(None, 8_000, Path::new("x.wav")).unwrap(),
            8_000
        );
        assert_eq!(
            timeline_rate(Some(16_000), 8_000, Path::new("x.wav")).unwrap(),
            16_000
        );
    }

    #[test]
    fn silence_is_sent_in_chunks() {
        let mut sink = CollectSink::default();
        sink.format(16_000).unwrap();
        let mut scratch = Vec::new();
        assert_eq!(
            emit_silence(&mut sink, 20_000, &mut scratch).unwrap(),
            20_000
        );
        assert_eq!(sink.samples.len(), 20_000);
        assert!(sink.samples.iter().all(|s| *s == 0.0));
        assert!(scratch.len() <= 8_192);
    }

    #[test]
    fn averages_channels() {
        let planes = vec![vec![1.0, 0.5, 0.0], vec![0.0, 0.5, -1.0]];
        let mut out = Vec::new();
        downmix(&planes, 3, &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.5]);
    }

    #[test]
    fn rejects_non_audio_before_emitting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.wav");
        std::fs::write(&path, b"definitely not audio").unwrap();
        let mut sink = CollectSink::default();
        assert!(SymphoniaDecoder.decode(&path, &mut sink).is_err());
        assert_eq!(sink.format_calls, 0);
        assert!(SymphoniaDecoder.probe(&path).is_err());
    }

    #[test]
    fn chain_falls_back_from_failing_decoder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sine.wav");
        write_sine_wav(&path, 22_050, 1, 2_205, 300.0, 0.25);

        let chain = DecoderChain::new(vec![
            Box::new(FakeDecoder::failing("broken", 0)),
            Box::new(SymphoniaDecoder),
        ]);
        let mut sink = CollectSink::default();
        let info = chain.decode(&path, &mut sink).unwrap();
        assert_eq!(info.sample_rate, 22_050);
        assert_eq!(sink.format_calls, 1);
        assert_eq!(sink.samples.len(), 2_205);
        assert_eq!(chain.probe(&path).unwrap().sample_rate, 22_050);
    }

    #[test]
    fn sink_errors_pass_through() {
        struct Stop;
        impl PcmSink for Stop {
            fn format(&mut self, _: u32) -> Result<()> {
                Ok(())
            }
            fn samples(&mut self, _: &[f32]) -> Result<()> {
                Err(Error::Internal("cancelled".into()))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sine.wav");
        write_sine_wav(&path, 16_000, 1, 1_600, 300.0, 0.25);
        let err = SymphoniaDecoder.decode(&path, &mut Stop).unwrap_err();
        assert_eq!(err.code(), "internal");
    }

    #[test]
    fn parses_container_dates() {
        let d = parse_media_date("2024-03-05T10:20:30Z").unwrap();
        assert_eq!(d.to_rfc3339(), "2024-03-05T10:20:30+00:00");
        let d = parse_media_date("2024-03-05T10:20:30.000000Z").unwrap();
        assert_eq!(d.timestamp(), 1_709_634_030);
        assert!(parse_media_date("2024-03-05 10:20:30").is_some());
        assert!(parse_media_date("2024-03-05").is_some());
        assert!(parse_media_date("2024").is_none());
    }

    /// AAC in MP4 (the main import format). Needs `ffmpeg` to build the
    /// fixture; skipped when it is not installed.
    #[test]
    fn decodes_aac_mp4_when_ffmpeg_can_build_fixture() {
        let ff = crate::ffmpeg_decoder::FfmpegDecoder::locate();
        let Some(ffmpeg) = ff.ffmpeg_path() else {
            eprintln!("skipping: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("sine.wav");
        let m4a = dir.path().join("sine.m4a");
        write_sine_wav(&wav, 48_000, 2, 96_000, 440.0, 0.5);
        let status = std::process::Command::new(ffmpeg)
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(&wav)
            .args(["-c:a", "aac", "-b:a", "128k"])
            .arg(&m4a)
            .status()
            .unwrap();
        assert!(status.success());

        let info = SymphoniaDecoder.probe(&m4a).unwrap();
        assert_eq!(info.container, "mp4");
        assert_eq!(info.channels, 2);
        let mut sink = CollectSink::default();
        let decoded = SymphoniaDecoder.decode(&m4a, &mut sink).unwrap();
        assert_eq!(sink.sample_rate, Some(48_000));
        assert_eq!(decoded.sample_rate, 48_000);
        // AAC adds priming/padding frames; allow a few frames of slack.
        let n = sink.samples.len() as i64;
        assert!((n - 96_000).abs() <= 4_096, "decoded {n} samples");
        let p = peak(&sink.samples);
        assert!((p - 0.5).abs() < 0.05, "peak {p}");
    }

    /// Two AAC tracks in one MP4 (the recorder's `audio.m4a` layout), read
    /// one at a time by both decoders. Needs `ffmpeg`; skipped otherwise.
    #[test]
    fn decode_track_reads_each_audio_track() {
        let ff = crate::ffmpeg_decoder::FfmpegDecoder::locate();
        let Some(ffmpeg) = ff.ffmpeg_path() else {
            eprintln!("skipping: ffmpeg not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let loud = dir.path().join("loud.wav");
        let quiet = dir.path().join("quiet.wav");
        let m4a = dir.path().join("two.m4a");
        write_sine_wav(&loud, 48_000, 1, 48_000, 440.0, 0.6);
        write_sine_wav(&quiet, 48_000, 1, 48_000, 880.0, 0.15);
        let status = std::process::Command::new(ffmpeg)
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(&loud)
            .arg("-i")
            .arg(&quiet)
            .args(["-map", "0:a", "-map", "1:a", "-c:a", "aac", "-b:a", "96k"])
            .args([
                "-metadata:s:a:0",
                "language=eng",
                "-metadata:s:a:1",
                "language=por",
            ])
            .args([
                "-metadata:s:a:0",
                "handler_name=mic",
                "-metadata:s:a:1",
                "handler_name=system",
            ])
            .arg(&m4a)
            .status()
            .unwrap();
        assert!(status.success());

        let decoders: Vec<Box<dyn MediaDecoder>> = if ff.is_available() {
            vec![Box::new(SymphoniaDecoder), Box::new(ff.clone())]
        } else {
            vec![Box::new(SymphoniaDecoder)]
        };
        for decoder in decoders {
            for (index, amplitude, frequency) in [(0, 0.6f64, 440.0), (1, 0.15, 880.0)] {
                let mut sink = CollectSink::default();
                let info = decoder.decode_track(&m4a, index, &mut sink).unwrap();
                assert_eq!(info.audio_tracks, 2, "{}", decoder.name());
                assert_eq!(info.tracks.len(), 2);
                assert_eq!(info.tracks[0].index, 0);
                assert_eq!(info.tracks[1].index, 1);
                assert_eq!(info.tracks[0].channels, 1);
                assert_eq!(info.tracks[1].sample_rate, 48_000);
                assert_eq!(
                    info.tracks[0].language.as_deref(),
                    Some("eng"),
                    "{}",
                    decoder.name()
                );
                assert_eq!(
                    info.tracks[1].language.as_deref(),
                    Some("por"),
                    "{}",
                    decoder.name()
                );
                assert_eq!(
                    info.tracks[0].title.as_deref(),
                    Some("mic"),
                    "{}",
                    decoder.name()
                );
                assert_eq!(
                    info.tracks[1].title.as_deref(),
                    Some("system"),
                    "{}",
                    decoder.name()
                );
                assert_eq!(info.sample_rate, 48_000);
                assert!(sink.samples.len() >= 48_000);
                // AAC can overshoot at packet/stream boundaries. Steady-state
                // RMS, plus the expected tone's energy, verifies the tracks
                // without mistaking a codec transient for a gain change.
                let middle = &sink.samples[12_000..36_000];
                let rms = (middle.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>()
                    / middle.len() as f64)
                    .sqrt();
                let (sin, cos) =
                    middle
                        .iter()
                        .enumerate()
                        .fold((0.0, 0.0), |(sin, cos), (i, s)| {
                            let phase = std::f64::consts::TAU * frequency * i as f64 / 48_000.0;
                            (
                                sin + f64::from(*s) * phase.sin(),
                                cos + f64::from(*s) * phase.cos(),
                            )
                        });
                let tone_amplitude = 2.0 * sin.hypot(cos) / middle.len() as f64;
                assert!(
                    (rms - amplitude / 2.0f64.sqrt()).abs() < 0.015,
                    "{} track {index}: RMS {rms}",
                    decoder.name()
                );
                assert!(
                    (tone_amplitude - amplitude).abs() < 0.025,
                    "{} track {index}: tone amplitude {tone_amplitude}",
                    decoder.name()
                );
                assert!(
                    tone_amplitude / (rms * 2.0f64.sqrt()) > 0.95,
                    "{} track {index}: expected frequency is not dominant",
                    decoder.name()
                );
            }
            let mut sink = CollectSink::default();
            assert!(decoder.decode_track(&m4a, 2, &mut sink).is_err());
        }
    }
}
