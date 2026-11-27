//! AVFoundation decoding, the first entry of the macOS decoder chain (design
//! §3.2: AVFoundation, then symphonia, then optional ffmpeg).
//!
//! Exactly one audio track is read per request, through its own
//! `AVAssetReaderTrackOutput`, as interleaved native-endian 32-bit float PCM
//! at the track's own sample rate and channel count. Tracks are never mixed
//! together (no `AVAssetReaderAudioMixOutput`); only the channels of the one
//! selected track are averaged to mono, as every [`MediaDecoder`] does.
//!
//! Audio tracks are indexed in container order, video and other media left
//! out, matching symphonia and ffmpeg so a fallback reads the same track.
//! Nothing reaches the sink before the first decoded buffer, so a file
//! AVFoundation cannot open, read or start decoding leaves the chain free to
//! try the next decoder. A hole between buffer timestamps becomes silence of
//! the same length so the output keeps the source's timeline; nothing is
//! emitted before the first buffer, so a leading offset is not padded.
//!
//! A file with several audio tracks and iTunes gapless info (as AAC `.m4a`
//! files written by `AVAssetWriter` carry) is refused before any sample:
//! AVFoundation applies that one gapless length to every track and stops a
//! longer track early, so the chain's next decoder reads such tracks whole.
//! Its track list, labels, rates and channels are still reported.

use std::io::Read;
use std::path::Path;
use std::ptr::NonNull;

use broscribe_platform::symphonia_decoder::SymphoniaDecoder;
use broscribe_platform::{Error, MediaDecoder, MediaInfo, PcmSink, Result, TrackInfo};
use chrono::{DateTime, FixedOffset};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAsset, AVAssetReader, AVAssetReaderStatus, AVAssetReaderTrackOutput, AVAssetTrack,
    AVMediaTypeAudio, AVMetadataCommonKeyTitle, AVURLAsset,
    AVURLAssetPreferPreciseDurationAndTimingKey,
};
use objc2_avf_audio::{
    AVFormatIDKey, AVLinearPCMBitDepthKey, AVLinearPCMIsBigEndianKey, AVLinearPCMIsFloatKey,
    AVLinearPCMIsNonInterleaved,
};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMFormatDescription, CMSampleBuffer, CMTime,
    CMTimeFlags,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

/// More channels than any Core Audio layout tag describes: a corrupt format.
const MAX_CHANNELS: u32 = 64;
/// Zero samples per `samples` call while filling a timestamp hole.
const SILENCE_CHUNK: u64 = 8_192;
/// AVFoundation's identifier for iTunes gapless playback info, the
/// `----` item `com.apple.iTunes.iTunSMPB` in the file's `udta/meta/ilst`
/// (priming, padding and original length of one audio stream).
const ITUNES_GAPLESS_IDENTIFIER: &str = "itlk/com.apple.iTunes.iTunSMPB";

/// Decoder on `AVAssetReader`; see the module documentation.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacMediaDecoder;

impl MacMediaDecoder {
    pub const NAME: &'static str = "avfoundation";
}

impl MediaDecoder for MacMediaDecoder {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        autoreleasepool(|_| Ok(Asset::open(path)?.media_info(0)))
    }

    fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        self.decode_track(path, 0, sink)
    }

    fn decode_track(&self, path: &Path, index: usize, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        autoreleasepool(|_| decode_track(path, index, sink))
    }
}

/// An opened asset and the facts of its audio tracks, in container order.
struct Asset {
    asset: Retained<AVURLAsset>,
    tracks: Vec<Retained<AVAssetTrack>>,
    infos: Vec<TrackInfo>,
    /// Per track: start and end of its range in the asset timeline (seconds).
    ranges: Vec<Option<(f64, f64)>>,
    asset_duration_s: Option<f64>,
    created_at: Option<DateTime<FixedOffset>>,
    container: String,
    /// The asset carries iTunes gapless info (`iTunSMPB`), which describes
    /// one audio stream for the whole file.
    asset_gapless: bool,
}

impl Asset {
    fn open(path: &Path) -> Result<Self> {
        // Also reports a missing or unreadable file as an I/O error.
        let container = sniff_container(path)?;
        let url = NSURL::from_file_path(path).ok_or_else(|| {
            Error::InvalidInput(format!("{}: not a usable file path", path.display()))
        })?;
        let precise = NSNumber::new_bool(true);
        // SAFETY: a framework constant key with its documented boolean value.
        let options: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
            &[unsafe { AVURLAssetPreferPreciseDurationAndTimingKey }],
            &[precise.as_ref()],
        );
        // SAFETY: a file URL and an options dictionary of documented keys.
        let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&url, Some(&options)) };
        // Synchronous property reads load the asset on this (decoder) thread.
        // SAFETY: plain property reads on a live asset.
        if !unsafe { asset.isReadable() } {
            return Err(Error::Unsupported(format!(
                "{}: AVFoundation cannot read this file",
                path.display()
            )));
        }
        // SAFETY: a framework-owned constant.
        let audio = unsafe { AVMediaTypeAudio }
            .ok_or_else(|| Error::Unsupported("AVMediaTypeAudio unavailable".into()))?;
        // SAFETY: property reads on a readable asset and its tracks.
        let tracks: Vec<Retained<AVAssetTrack>> = unsafe { asset.tracks() }
            .iter()
            .filter(|track| unsafe { track.mediaType() }.isEqualToString(audio))
            .collect();
        if tracks.is_empty() {
            return Err(Error::Decode(format!("{}: no audio track", path.display())));
        }
        let mut infos = Vec::with_capacity(tracks.len());
        let mut ranges = Vec::with_capacity(tracks.len());
        for (index, track) in tracks.iter().enumerate() {
            let format = track_format(track);
            infos.push(TrackInfo {
                index,
                channels: format.map_or(0, |f| {
                    u16::try_from(f.mChannelsPerFrame).unwrap_or(u16::MAX)
                }),
                sample_rate: format.map_or(0, |f| rate(f.mSampleRate)),
                language: language(track),
                title: title(track),
            });
            // SAFETY: a property read on a live track.
            let range = unsafe { track.timeRange() };
            ranges.push(
                seconds(range.start)
                    .zip(seconds(range.duration))
                    .map(|(start, duration)| (start, start + duration)),
            );
        }
        // SAFETY: a property read on a live asset.
        let asset_duration_s = seconds(unsafe { asset.duration() });
        // SAFETY: property reads on a live asset and its metadata items.
        let asset_gapless = unsafe { asset.metadata() }.iter().any(|item| {
            unsafe { item.identifier() }
                .is_some_and(|identifier| identifier.to_string() == ITUNES_GAPLESS_IDENTIFIER)
        });
        let mut opened = Self {
            created_at: created_at(&asset),
            asset,
            tracks,
            infos,
            ranges,
            asset_duration_s,
            container,
            asset_gapless,
        };
        opened.merge_container_metadata(path);
        Ok(opened)
    }

    /// Fills facts AVFoundation does not report from what symphonia reads in
    /// the same file's metadata (never its audio): AVFoundation exposes no
    /// RIFF INFO dates, and symphonia can name a track AVFoundation leaves
    /// untitled. Facts AVFoundation reported are kept (see
    /// [`merge_track_metadata`]).
    fn merge_container_metadata(&mut self, path: &Path) {
        let Ok(other) = SymphoniaDecoder::new().probe(path) else {
            return;
        };
        if self.created_at.is_none() {
            self.created_at = other.created_at;
        }
        merge_track_metadata(&mut self.infos, other.tracks);
    }

    /// AVFoundation applies a file's single iTunes gapless length to every
    /// audio track: in a file with several audio tracks of different
    /// lengths it reports each track (and the asset) at the first one's
    /// length and its reader stops each track there, even with an explicit
    /// unbounded reader range. The per-track timing it reports for such a
    /// file cannot be trusted.
    fn per_track_timing_unreliable(&self) -> bool {
        self.asset_gapless && self.tracks.len() > 1
    }

    fn duration_s(&self, index: usize) -> Option<f64> {
        if self.per_track_timing_unreliable() {
            return None;
        }
        self.ranges[index]
            .map(|(start, end)| end - start)
            .filter(|duration| *duration > 0.0)
            .or(self.asset_duration_s)
    }

    /// Metadata for the `index`-th audio track, which must exist. A
    /// duration AVFoundation cannot report truthfully stays unknown.
    fn media_info(&self, index: usize) -> MediaInfo {
        let track = &self.infos[index];
        MediaInfo {
            sample_rate: track.sample_rate,
            channels: track.channels,
            duration_s: self.duration_s(index),
            created_at: self.created_at,
            audio_tracks: u32::try_from(self.infos.len()).unwrap_or(u32::MAX),
            tracks: self.infos.clone(),
            container: self.container.clone(),
        }
    }
}

/// Per-track facts from symphonia's reading of the container (`container`)
/// filling the gaps in AVFoundation's. AVFoundation's facts win: its title
/// is the label stored in the track's metadata (e.g. the ISO `udta/titl`
/// box `MacAudioEncoder` writes, as `uiso/titl`), which symphonia does not
/// read and replaces with the handler's generic name ("Core Media Audio").
/// Nothing is merged unless both list the same number of audio tracks, so
/// no fact moves to another track.
fn merge_track_metadata(infos: &mut [TrackInfo], container: Vec<TrackInfo>) {
    if container.len() != infos.len() {
        return;
    }
    for (info, other) in infos.iter_mut().zip(container) {
        if info.title.is_none() {
            info.title = other.title;
        }
        if info.language.is_none() {
            info.language = other.language;
        }
    }
}

fn decode_track(path: &Path, index: usize, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
    let asset = Asset::open(path)?;
    let Some(track) = asset.tracks.get(index) else {
        return Err(Error::Decode(format!(
            "{}: no audio track {index}",
            path.display()
        )));
    };
    // Refuse before touching the sink: the chain's next decoder reads the
    // whole of every track instead of a silently shortened one.
    if asset.per_track_timing_unreliable() {
        return Err(Error::Unsupported(format!(
            "{}: AVFoundation applies the file's single gapless length to each of its {} \
             audio tracks and would shorten them",
            path.display(),
            asset.tracks.len()
        )));
    }
    let mut info = asset.media_info(index);
    // SAFETY: a readable asset that is not mutated while it is read.
    let reader = unsafe { AVAssetReader::assetReaderWithAsset_error(&asset.asset) }
        .map_err(|e| Error::Decode(format!("{}: AVAssetReader: {e}", path.display())))?;
    let settings = pcm_settings()?;
    // SAFETY: an audio track of this asset and documented LPCM settings.
    let output = unsafe {
        AVAssetReaderTrackOutput::assetReaderTrackOutputWithTrack_outputSettings(
            track,
            Some(&settings),
        )
    };
    // SAFETY: setters on an output not yet added to a reader; the samples
    // are copied out before the next buffer is requested.
    unsafe {
        output.setAlwaysCopiesSampleData(false);
        if !reader.canAddOutput(&output) {
            return Err(Error::Unsupported(format!(
                "{}: AVFoundation cannot decode audio track {index} to PCM",
                path.display()
            )));
        }
        reader.addOutput(&output);
        if !reader.startReading() {
            return Err(reader_error(&reader, path));
        }
    }
    // Any early return below (a sink error included) stops the reader.
    let reading = StopOnDrop(&reader);
    let mut emitter = Emitter {
        sink,
        rate: None,
        timeline: Timeline::default(),
        range_end_s: asset.ranges[index].map(|(_, end)| end),
        frames: 0,
        pcm: Vec::new(),
        mono: Vec::new(),
        path,
    };
    let mut buffers = 0u64;
    loop {
        let more = autoreleasepool(|_| -> Result<bool> {
            // SAFETY: the reader is reading and owns this output.
            let Some(buffer) = (unsafe { output.copyNextSampleBuffer() }) else {
                return Ok(false);
            };
            emitter.push(&buffer)?;
            Ok(true)
        })?;
        if !more {
            break;
        }
        buffers += 1;
    }
    // SAFETY: a status read after the last buffer.
    if unsafe { reader.status() } != AVAssetReaderStatus::Completed {
        return Err(reader_error(&reader, path));
    }
    drop(reading);
    let rate = match emitter.rate {
        Some(rate) => rate,
        None => {
            // A track that declares audio but yields none is not decoded:
            // fail before touching the sink so the chain can fall back.
            if (buffers == 0 && info.duration_s.is_some_and(|d| d > 0.0)) || info.sample_rate == 0 {
                return Err(Error::Decode(format!(
                    "{}: AVFoundation returned no audio for track {index}",
                    path.display()
                )));
            }
            emitter.sink.format(info.sample_rate)?;
            info.sample_rate
        }
    };
    info.sample_rate = rate;
    // Keep the container's own duration: callers compare it with the
    // emitted length to catch streams decoded at the wrong rate.
    if info.duration_s.is_none() {
        info.duration_s = Some(emitter.frames as f64 / f64::from(rate));
    }
    Ok(info)
}

/// Cancels an unfinished read, e.g. when the sink stops the decode.
struct StopOnDrop<'a>(&'a AVAssetReader);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        // SAFETY: status read and cancel on a live reader; no buffer request
        // is in flight once the decode loop has returned.
        unsafe {
            if self.0.status() == AVAssetReaderStatus::Reading {
                self.0.cancelReading();
            }
        }
    }
}

fn reader_error(reader: &AVAssetReader, path: &Path) -> Error {
    // SAFETY: property reads on a live reader.
    let (status, error) = unsafe { (reader.status(), reader.error()) };
    Error::Decode(match error {
        Some(error) => format!("{}: AVFoundation: {error}", path.display()),
        None => format!(
            "{}: AVFoundation stopped reading (status {})",
            path.display(),
            status.0
        ),
    })
}

/// Turns sample buffers into mono samples on the sink.
struct Emitter<'a, 's> {
    sink: &'s mut dyn PcmSink,
    rate: Option<u32>,
    timeline: Timeline,
    range_end_s: Option<f64>,
    frames: u64,
    pcm: Vec<f32>,
    mono: Vec<f32>,
    path: &'a Path,
}

impl Emitter<'_, '_> {
    fn fail(&self, what: impl std::fmt::Display) -> Error {
        Error::Decode(format!("{}: {what}", self.path.display()))
    }

    fn push(&mut self, buffer: &CMSampleBuffer) -> Result<()> {
        // SAFETY: getters on a live sample buffer; the format description is
        // retained for the duration of this call.
        let description = unsafe { buffer.format_description() }
            .ok_or_else(|| self.fail("decoded audio without a format"))?;
        let format = stream_description(&description)
            .ok_or_else(|| self.fail("decoded audio is not audio"))?;
        let (rate, channels) = pcm_layout(&format).map_err(|what| self.fail(what))?;
        // SAFETY: a getter on a live sample buffer.
        let frames = usize::try_from(unsafe { buffer.num_samples() })
            .map_err(|_| self.fail("decoded audio with a negative frame count"))?;
        if frames == 0 {
            return Ok(());
        }
        // SAFETY: a getter on a live sample buffer.
        let block = unsafe { buffer.data_buffer() }
            .ok_or_else(|| self.fail("decoded audio without data"))?;
        let values = frames
            .checked_mul(channels)
            .ok_or_else(|| self.fail("decoded audio buffer too large"))?;
        // SAFETY: a getter on a live block buffer.
        let bytes = unsafe { block.data_length() };
        if Some(bytes) != values.checked_mul(size_of::<f32>()) {
            return Err(self.fail(format_args!(
                "decoded audio holds {bytes} bytes for {frames} frames of {channels} channels"
            )));
        }
        self.pcm.clear();
        self.pcm.resize(values, 0.0);
        // SAFETY: `pcm` provides exactly `bytes` writable bytes, and float
        // samples accept any bit pattern.
        let status = unsafe {
            block.copy_data_bytes(0, bytes, NonNull::from(self.pcm.as_mut_slice()).cast())
        };
        if status != 0 {
            return Err(self.fail(format_args!("copying decoded audio: OSStatus {status}")));
        }
        match self.rate {
            None => {
                self.sink.format(rate)?;
                self.rate = Some(rate);
                self.timeline.end = self
                    .range_end_s
                    .map(|end| (end * f64::from(rate)).ceil() as i128 + i128::from(rate));
            }
            Some(previous) if previous != rate => {
                return Err(self.fail(format_args!(
                    "sample rate changed mid-stream ({previous} Hz to {rate} Hz)"
                )));
            }
            Some(_) => {}
        }
        // SAFETY: a getter on a live sample buffer.
        let pts = frame_position(unsafe { buffer.presentation_time_stamp() }, rate);
        let gap = self
            .timeline
            .gap_before(pts)
            .map_err(|what| self.fail(what))?;
        self.emit_silence(gap)?;
        downmix_interleaved(&self.pcm, channels, &mut self.mono);
        self.sink.samples(&self.mono)?;
        self.timeline.advance(pts, frames);
        self.frames += frames as u64;
        Ok(())
    }

    fn emit_silence(&mut self, frames: u64) -> Result<()> {
        let mut left = frames;
        while left > 0 {
            let n = left.min(SILENCE_CHUNK) as usize;
            self.mono.clear();
            self.mono.resize(n, 0.0);
            self.sink.samples(&self.mono)?;
            left -= n as u64;
        }
        self.frames += frames;
        Ok(())
    }
}

/// Decoded positions in frames at the output rate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Timeline {
    /// End of the last buffer, once one has been placed.
    next: Option<i128>,
    /// Latest plausible position: the track's end plus one second. A buffer
    /// stamped beyond it is a corrupt timestamp, not minutes of silence.
    end: Option<i128>,
}

impl Timeline {
    /// Frames of silence for a hole before a buffer at `pts`; 0 for the first
    /// buffer, an overlap or an unknown timestamp.
    fn gap_before(&self, pts: Option<i128>) -> std::result::Result<u64, String> {
        let (Some(next), Some(pts)) = (self.next, pts) else {
            return Ok(0);
        };
        if pts <= next {
            return Ok(0);
        }
        if self.end.is_some_and(|end| pts > end) {
            return Err(format!(
                "decoded audio jumps {} frames, past the end of the track",
                pts - next
            ));
        }
        u64::try_from(pts - next).map_err(|_| "decoded audio jumps too far".to_owned())
    }

    /// Moves past a buffer of `frames` placed at `pts` (or right after the
    /// previous one when its timestamp is unknown).
    fn advance(&mut self, pts: Option<i128>, frames: usize) {
        let frames = frames as i128;
        self.next = match (pts, self.next) {
            (Some(pts), Some(next)) => Some(next.max(pts + frames)),
            (Some(pts), None) => Some(pts + frames),
            (None, next) => next.map(|next| next + frames),
        };
    }
}

/// Exact position of `time` in frames at `rate`; `None` unless it is a
/// valid, finite time.
fn frame_position(time: CMTime, rate: u32) -> Option<i128> {
    if !time.flags.contains(CMTimeFlags::Valid)
        || time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || time.timescale <= 0
    {
        return None;
    }
    Some((i128::from(time.value) * i128::from(rate)).div_euclid(i128::from(time.timescale)))
}

/// `time` in seconds when it is a valid, finite, non-negative time.
fn seconds(time: CMTime) -> Option<f64> {
    if !time.flags.contains(CMTimeFlags::Valid)
        || time.flags.intersects(CMTimeFlags::ImpliedValueFlagsMask)
        || time.timescale <= 0
        || time.value < 0
    {
        return None;
    }
    Some(time.value as f64 / f64::from(time.timescale))
}

fn rate(rate: f64) -> u32 {
    if rate.is_finite() && rate >= 1.0 && rate <= f64::from(u32::MAX) {
        rate.round() as u32
    } else {
        0
    }
}

/// Rate and channel count of the PCM requested by [`pcm_settings`], refusing
/// anything else rather than misreading its bytes.
fn pcm_layout(format: &AudioStreamBasicDescription) -> std::result::Result<(u32, usize), String> {
    let flags = format.mFormatFlags;
    if format.mFormatID != kAudioFormatLinearPCM
        || flags & kAudioFormatFlagIsFloat == 0
        || flags & (kAudioFormatFlagIsNonInterleaved | kAudioFormatFlagIsBigEndian) != 0
        || format.mBitsPerChannel != 32
    {
        return Err("AVFoundation returned audio other than interleaved 32-bit float".into());
    }
    let channels = format.mChannelsPerFrame;
    if channels == 0 || channels > MAX_CHANNELS {
        return Err(format!("decoded audio has {channels} channels"));
    }
    if format.mBytesPerFrame != channels * 4 {
        return Err(format!(
            "decoded audio has {} bytes per frame for {channels} channels",
            format.mBytesPerFrame
        ));
    }
    match rate(format.mSampleRate) {
        0 => Err("decoded audio has no sample rate".into()),
        rate => Ok((rate, channels as usize)),
    }
}

/// Averages each interleaved frame of one track into `out`.
fn downmix_interleaved(pcm: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    if channels == 1 {
        out.extend_from_slice(pcm);
        return;
    }
    let scale = 1.0 / channels as f32;
    out.extend(
        pcm.chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() * scale),
    );
}

/// Interleaved native-endian 32-bit float at the track's own rate and
/// channel count (no rate, channel or layout key: nothing is resampled or
/// remixed before analysis).
fn pcm_settings() -> Result<Retained<NSDictionary<NSString, AnyObject>>> {
    // SAFETY: framework constants available on supported macOS versions.
    let keys = unsafe {
        [
            AVFormatIDKey,
            AVLinearPCMBitDepthKey,
            AVLinearPCMIsFloatKey,
            AVLinearPCMIsBigEndianKey,
            AVLinearPCMIsNonInterleaved,
        ]
    }
    .map(|key| {
        key.ok_or_else(|| Error::Unsupported("AVFoundation PCM settings unavailable".into()))
    });
    let [format, depth, float, big_endian, non_interleaved] = keys;
    let keys = [format?, depth?, float?, big_endian?, non_interleaved?];
    let values = [
        NSNumber::new_u32(kAudioFormatLinearPCM),
        NSNumber::new_u32(32),
        NSNumber::new_bool(true),
        // Little-endian: native on every supported Mac.
        NSNumber::new_bool(false),
        NSNumber::new_bool(false),
    ];
    Ok(NSDictionary::from_slices(
        &keys,
        &[
            values[0].as_ref(),
            values[1].as_ref(),
            values[2].as_ref(),
            values[3].as_ref(),
            values[4].as_ref(),
        ],
    ))
}

fn stream_description(description: &CMFormatDescription) -> Option<AudioStreamBasicDescription> {
    // SAFETY: returns NULL for a non-audio description, else a pointer into
    // the description, which outlives this copy.
    let format = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(description) };
    // SAFETY: non-null pointers reference a complete description.
    (!format.is_null()).then(|| unsafe { *format })
}

/// The source format of a track (its first format description).
fn track_format(track: &AVAssetTrack) -> Option<AudioStreamBasicDescription> {
    // SAFETY: a property read on a live track.
    let first = unsafe { track.formatDescriptions() }.firstObject()?;
    // SAFETY: `formatDescriptions` holds CMFormatDescription objects, retained
    // by `first` while this reference is used.
    let description = unsafe { &*Retained::as_ptr(&first).cast::<CMFormatDescription>() };
    stream_description(description)
}

/// ISO 639-2/T code, leaving out the "undetermined" placeholder.
fn language(track: &AVAssetTrack) -> Option<String> {
    // SAFETY: a property read on a live track.
    let code = unsafe { track.languageCode() }?.to_string();
    let code = code.trim();
    (!code.is_empty() && code != "und").then(|| code.to_owned())
}

/// The track's title: a metadata item whose common key is title. Format
/// items keep their own identifier (an ISO `udta/titl` box reads as
/// `uiso/titl`, not `common/title`), so the common key is what marks them.
fn title(track: &AVAssetTrack) -> Option<String> {
    // SAFETY: a framework-owned constant.
    let wanted = unsafe { AVMetadataCommonKeyTitle }?;
    // SAFETY: property reads on a live track and its metadata items.
    unsafe { track.commonMetadata() }.iter().find_map(|item| {
        let key = unsafe { item.commonKey() }?;
        if !key.isEqualToString(wanted) {
            return None;
        }
        let value = unsafe { item.stringValue() }?.to_string();
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    })
}

/// The asset's creation date: its recorded text (keeping the recorded UTC
/// offset), else AVFoundation's own date conversion in UTC.
fn created_at(asset: &AVAsset) -> Option<DateTime<FixedOffset>> {
    // SAFETY: property reads on a live asset and its metadata item.
    let item = unsafe { asset.creationDate() }?;
    if let Some(text) = unsafe { item.stringValue() }
        && let Some(date) = parse_zoned_date(&text.to_string())
    {
        return Some(date);
    }
    let since_epoch = unsafe { item.dateValue() }?.timeIntervalSince1970();
    utc_date(since_epoch)
}

/// Dates that state their own offset; a zone-less text is left to
/// AVFoundation's date conversion instead of guessing a zone here.
fn parse_zoned_date(text: &str) -> Option<DateTime<FixedOffset>> {
    let text = text.trim();
    DateTime::parse_from_rfc3339(text).ok().or_else(|| {
        [
            "%Y-%m-%dT%H:%M:%S%.f%z",
            "%Y-%m-%dT%H:%M:%S%z",
            "%Y-%m-%d %H:%M:%S%z",
        ]
        .iter()
        .find_map(|format| DateTime::parse_from_str(text, format).ok())
    })
}

fn utc_date(since_epoch: f64) -> Option<DateTime<FixedOffset>> {
    if !since_epoch.is_finite() {
        return None;
    }
    let whole = since_epoch.floor();
    let nanos = ((since_epoch - whole) * 1e9).round().min(999_999_999.0) as u32;
    DateTime::from_timestamp(whole as i64, nanos).map(|date| date.fixed_offset())
}

/// Container name from the file's leading bytes (the names symphonia and
/// ffmpeg report), else its lower-case extension.
fn sniff_container(path: &Path) -> Result<String> {
    let mut head = Vec::with_capacity(12);
    std::fs::File::open(path)
        .and_then(|file| file.take(12).read_to_end(&mut head))
        .map_err(|e| Error::io_at(path, e))?;
    Ok(container_name(
        &head,
        path.extension().and_then(|extension| extension.to_str()),
    ))
}

fn container_name(head: &[u8], extension: Option<&str>) -> String {
    let name = match head {
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'A',
            b'V',
            b'E',
            ..,
        ] => "wav",
        [b'F', b'O', b'R', b'M', _, _, _, _, b'A', b'I', b'F', ..] => "aiff",
        [b'c', b'a', b'f', b'f', ..] => "caf",
        [b'f', b'L', b'a', b'C', ..] => "flac",
        _ if head.get(4..8).is_some_and(is_mp4_box) => "mp4",
        [b'I', b'D', b'3', ..] => "mp3",
        // ADTS (layer bits 00) before MPEG audio frames sharing its sync.
        [0xFF, second, ..] if second & 0xF6 == 0xF0 => "aac",
        [0xFF, second, ..] if second & 0xE0 == 0xE0 => "mp3",
        _ => {
            return extension.map(str::to_ascii_lowercase).unwrap_or_default();
        }
    };
    name.to_owned()
}

fn is_mp4_box(kind: &[u8]) -> bool {
    matches!(
        kind,
        b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::encoder::SliceSource;
    use broscribe_platform::fakes::{CollectSink, FakeDecoder};
    use broscribe_platform::{AudioEncoder, DecoderChain, EncodeSettings, EncoderTrack};

    fn time(value: i64, timescale: i32) -> CMTime {
        CMTime {
            value,
            timescale,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        }
    }

    fn float_format(rate: f64, channels: u32) -> AudioStreamBasicDescription {
        AudioStreamBasicDescription {
            mSampleRate: rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat,
            mBytesPerPacket: channels * 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: channels * 4,
            mChannelsPerFrame: channels,
            mBitsPerChannel: 32,
            mReserved: 0,
        }
    }

    /// 16-bit PCM WAV with `channels` interleaved channels.
    fn write_wav(path: &Path, rate: u32, channels: u16, interleaved: &[i16]) {
        let data = (interleaved.len() * 2) as u32;
        let mut bytes = Vec::with_capacity(44 + data as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data.to_le_bytes());
        for sample in interleaved {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn energy(samples: &[f32], frequency: f64, rate: f64) -> f64 {
        let (mut real, mut imaginary) = (0.0, 0.0);
        for (index, sample) in samples.iter().enumerate() {
            let phase = std::f64::consts::TAU * frequency * index as f64 / rate;
            real += f64::from(*sample) * phase.cos();
            imaginary += f64::from(*sample) * phase.sin();
        }
        real * real + imaginary * imaginary
    }

    fn tone(frequency: f32, rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|n| 0.3 * (std::f32::consts::TAU * frequency * n as f32 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn pcm_layout_accepts_only_the_requested_interleaved_float() {
        assert_eq!(pcm_layout(&float_format(44_100.0, 2)), Ok((44_100, 2)));
        assert_eq!(pcm_layout(&float_format(16_000.0, 1)), Ok((16_000, 1)));
        let mut planar = float_format(48_000.0, 2);
        planar.mFormatFlags |= kAudioFormatFlagIsNonInterleaved;
        assert!(pcm_layout(&planar).is_err());
        let mut integer = float_format(48_000.0, 2);
        integer.mFormatFlags = 0;
        assert!(pcm_layout(&integer).is_err());
        let mut packed_wrong = float_format(48_000.0, 2);
        packed_wrong.mBytesPerFrame = 4;
        assert!(pcm_layout(&packed_wrong).is_err());
        assert!(pcm_layout(&float_format(48_000.0, 0)).is_err());
        assert!(pcm_layout(&float_format(48_000.0, MAX_CHANNELS + 1)).is_err());
        assert!(pcm_layout(&float_format(f64::NAN, 1)).is_err());
        assert!(pcm_layout(&float_format(0.0, 1)).is_err());
    }

    #[test]
    fn timestamps_map_exactly_and_invalid_times_stay_unknown() {
        assert_eq!(frame_position(time(48_000, 48_000), 16_000), Some(16_000));
        assert_eq!(frame_position(time(1, 3), 48_000), Some(16_000));
        assert_eq!(frame_position(time(-1, 48_000), 48_000), Some(-1));
        let mut indefinite = time(0, 1);
        indefinite.flags |= CMTimeFlags::Indefinite;
        assert_eq!(frame_position(indefinite, 48_000), None);
        let invalid = CMTime {
            flags: CMTimeFlags::empty(),
            ..time(10, 10)
        };
        assert_eq!(frame_position(invalid, 48_000), None);
        assert_eq!(frame_position(time(10, 0), 48_000), None);
        assert_eq!(seconds(time(3, 2)), Some(1.5));
        assert_eq!(seconds(time(-3, 2)), None);
    }

    #[test]
    fn timeline_fills_holes_keeps_overlaps_and_refuses_jumps_past_the_track() {
        let mut timeline = Timeline {
            next: None,
            end: Some(48_000 + 48_000),
        };
        // The first buffer is never preceded by padding, even off zero.
        assert_eq!(timeline.gap_before(Some(1_000)), Ok(0));
        timeline.advance(Some(1_000), 1_024);
        assert_eq!(timeline.gap_before(Some(2_024)), Ok(0));
        // A 976-frame hole becomes silence of exactly that length.
        assert_eq!(timeline.gap_before(Some(3_000)), Ok(976));
        timeline.advance(Some(3_000), 1_000);
        // Overlaps and unknown timestamps add nothing.
        assert_eq!(timeline.gap_before(Some(3_500)), Ok(0));
        assert_eq!(timeline.gap_before(None), Ok(0));
        timeline.advance(None, 500);
        assert_eq!(timeline.next, Some(4_500));
        // A corrupt timestamp beyond the track end is an error, not silence.
        assert!(timeline.gap_before(Some(96_001)).is_err());
        assert_eq!(timeline.gap_before(Some(96_000)), Ok(91_500));
    }

    #[test]
    fn downmix_averages_the_channels_of_one_frame() {
        let mut out = Vec::new();
        downmix_interleaved(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0], 2, &mut out);
        assert_eq!(out, [0.5, 0.5, 0.0]);
        downmix_interleaved(&[0.25, -0.25], 1, &mut out);
        assert_eq!(out, [0.25, -0.25]);
    }

    #[test]
    fn containers_are_named_from_their_bytes() {
        assert_eq!(container_name(b"RIFF\0\0\0\0WAVEfmt", Some("m4a")), "wav");
        assert_eq!(container_name(b"\0\0\0\x20ftypM4A ", None), "mp4");
        assert_eq!(container_name(b"FORM\0\0\0\0AIFC", None), "aiff");
        assert_eq!(container_name(b"caff\0\x01", None), "caf");
        assert_eq!(container_name(b"ID3\x04", None), "mp3");
        assert_eq!(container_name(&[0xFF, 0xFB, 0x90], None), "mp3");
        assert_eq!(container_name(&[0xFF, 0xF1, 0x50], None), "aac");
        assert_eq!(container_name(b"unknown!", Some("MOV")), "mov");
        assert_eq!(container_name(b"", None), "");
    }

    #[test]
    fn native_track_labels_take_precedence_over_container_fallbacks() {
        let track = |index, title: Option<&str>, language: Option<&str>| TrackInfo {
            index,
            channels: 1,
            sample_rate: 48_000,
            language: language.map(str::to_owned),
            title: title.map(str::to_owned),
        };
        // AVFoundation's view: a stored title on track 0, nothing on track 1.
        let avfoundation = || vec![track(0, Some("mic"), None), track(1, None, Some("por"))];
        let mut infos = avfoundation();
        merge_track_metadata(
            &mut infos,
            vec![
                // Symphonia's handler-name fallback never replaces a stored title.
                track(0, Some("Core Media Audio"), Some("eng")),
                track(1, Some("system"), Some("spa")),
            ],
        );
        assert_eq!(
            infos,
            [
                track(0, Some("mic"), Some("eng")),
                track(1, Some("system"), Some("por")),
            ]
        );
        // A container listing other tracks moves nothing.
        let mut infos = avfoundation();
        merge_track_metadata(&mut infos, vec![track(0, Some("other"), Some("eng"))]);
        assert_eq!(infos, avfoundation());
    }

    #[test]
    fn creation_dates_keep_their_offset_and_never_guess_a_zone() {
        let date = parse_zoned_date("2026-03-04T09:10:11-0300").unwrap();
        assert_eq!(date.offset().local_minus_utc(), -3 * 3600);
        assert_eq!(date.to_rfc3339(), "2026-03-04T09:10:11-03:00");
        assert!(parse_zoned_date("2026-03-04T09:10:11Z").is_some());
        assert_eq!(parse_zoned_date("2026-03-04 09:10:11"), None);
        assert_eq!(
            utc_date(1.5).unwrap().to_rfc3339(),
            "1970-01-01T00:00:01.500+00:00"
        );
        assert_eq!(utc_date(f64::NAN), None);
    }

    #[test]
    fn decodes_synthetic_stereo_wav_at_its_own_rate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stereo.wav");
        let rate = 22_050;
        // Left carries a ramp, right is silent: mono must be half the left.
        let left: Vec<i16> = (0..rate as i32).map(|n| ((n % 2_000) * 8) as i16).collect();
        let interleaved: Vec<i16> = left.iter().flat_map(|&l| [l, 0]).collect();
        write_wav(&path, rate, 2, &interleaved);

        let probe = MacMediaDecoder.probe(&path).unwrap();
        assert_eq!(probe.container, "wav");
        assert_eq!((probe.sample_rate, probe.channels), (rate, 2));
        assert_eq!(probe.audio_tracks, 1);
        assert_eq!(probe.tracks[0].index, 0);
        assert!((probe.duration_s.unwrap() - 1.0).abs() < 1e-3);

        let mut sink = CollectSink::default();
        let info = MacMediaDecoder.decode(&path, &mut sink).unwrap();
        assert_eq!(sink.format_calls, 1);
        assert_eq!(sink.sample_rate, Some(rate));
        assert_eq!(info.sample_rate, rate);
        assert_eq!(sink.samples.len(), left.len());
        for (mono, left) in sink.samples.iter().zip(&left) {
            assert!((mono - f32::from(*left) / 32_768.0 / 2.0).abs() < 1e-4);
        }
    }

    /// AAC priming and tail padding are trimmed or, at worst, retained;
    /// never fewer frames than the source.
    const MAX_AAC_PADDING: usize = 2112 + 1023;

    fn encode_tones(path: &Path, rate: u32, tracks: &[(&str, f32, usize)]) {
        let mut sources: Vec<SliceSource> = tracks
            .iter()
            .map(|&(_, frequency, length)| SliceSource::new(tone(frequency, rate, length)))
            .collect();
        let mut encoder_tracks: Vec<EncoderTrack<'_>> = tracks
            .iter()
            .zip(sources.iter_mut())
            .map(|(&(title, _, _), source)| EncoderTrack { title, source })
            .collect();
        crate::encoder::MacAudioEncoder
            .encode(path, &EncodeSettings::speech(rate), &mut encoder_tracks)
            .unwrap();
    }

    #[test]
    fn decodes_a_single_track_aac_m4a_natively() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("one-track.m4a");
        let rate = 48_000;
        encode_tones(&path, rate, &[("mic", 440.0, 48_000)]);
        let mut sink = CollectSink::default();
        let info = MacMediaDecoder.decode(&path, &mut sink).unwrap();
        assert_eq!((info.sample_rate, info.audio_tracks), (rate, 1));
        assert_eq!(info.tracks[0].title.as_deref(), Some("mic"));
        assert!(info.duration_s.is_some_and(|d| (d - 1.0).abs() < 0.05));
        assert!(
            (48_000..=48_000 + MAX_AAC_PADDING).contains(&sink.samples.len()),
            "{} frames for 48000",
            sink.samples.len()
        );
        assert!(
            energy(&sink.samples, 440.0, f64::from(rate))
                > 100.0 * energy(&sink.samples, 880.0, f64::from(rate))
        );
    }

    #[test]
    fn multi_track_m4a_keeps_native_labels_and_never_loses_a_longer_track() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("two-track.m4a");
        let rate = 48_000;
        encode_tones(
            &path,
            rate,
            &[("mic", 440.0, 48_000), ("system", 880.0, 72_000)],
        );

        let probe = MacMediaDecoder.probe(&path).unwrap();
        assert_eq!(probe.container, "mp4");
        assert_eq!(probe.audio_tracks, 2);
        assert_eq!(
            probe
                .tracks
                .iter()
                .map(|t| (t.index, t.sample_rate, t.channels))
                .collect::<Vec<_>>(),
            [(0, rate, 1), (1, rate, 1)]
        );
        // `MacAudioEncoder` stores each title as an ISO `udta/titl` box;
        // AVFoundation reads it back through its common title key.
        assert_eq!(
            probe
                .tracks
                .iter()
                .map(|t| t.title.as_deref())
                .collect::<Vec<_>>(),
            [Some("mic"), Some("system")]
        );
        // The file's single gapless length would cut the longer track short:
        // AVFoundation refuses before any sample and reports no duration it
        // cannot stand behind.
        assert_eq!(probe.duration_s, None);
        for index in 0..2 {
            let mut sink = CollectSink::default();
            let error = MacMediaDecoder
                .decode_track(&path, index, &mut sink)
                .unwrap_err();
            assert_eq!(error.code(), "unsupported");
            assert_eq!(sink.format_calls, 0);
        }
        let mut sink = CollectSink::default();
        let error = MacMediaDecoder
            .decode_track(&path, 2, &mut sink)
            .unwrap_err();
        assert_eq!(error.code(), "decode");
        assert_eq!(sink.format_calls, 0);

        // The platform chain still reads every track whole and unmixed.
        let chain = crate::decoder_chain();
        for (index, own, other, length) in [
            (0, 440.0, 880.0, 48_000usize),
            (1, 880.0, 440.0, 72_000usize),
        ] {
            let mut sink = CollectSink::default();
            let info = chain.decode_track(&path, index, &mut sink).unwrap();
            assert_eq!(info.sample_rate, rate);
            assert_eq!(info.tracks.len(), 2);
            assert!(
                (length..=length + MAX_AAC_PADDING).contains(&sink.samples.len()),
                "track {index}: {} frames for {length}",
                sink.samples.len()
            );
            let (own, other) = (
                energy(&sink.samples, own, f64::from(rate)),
                energy(&sink.samples, other, f64::from(rate)),
            );
            assert!(own > 100.0 * other, "track {index} carries the other track");
        }
    }

    #[test]
    fn sink_cancellation_stops_the_read_and_is_final_in_the_chain() {
        struct Cancel {
            calls: usize,
        }
        impl PcmSink for Cancel {
            fn format(&mut self, _: u32) -> Result<()> {
                Ok(())
            }
            fn samples(&mut self, _: &[f32]) -> Result<()> {
                self.calls += 1;
                Err(Error::Internal("cancelled".into()))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("long.wav");
        write_wav(&path, 16_000, 1, &vec![1_000; 16_000 * 30]);
        let chain = DecoderChain::new(vec![
            Box::new(MacMediaDecoder),
            Box::new(FakeDecoder::new("fallback", 8_000, vec![0.0; 10])),
        ]);
        let mut sink = Cancel { calls: 0 };
        let error = chain.decode(&path, &mut sink).unwrap_err();
        assert_eq!(error.code(), "internal");
        assert_eq!(
            sink.calls, 1,
            "no buffer is read after the sink refuses one"
        );
    }

    #[test]
    fn unreadable_input_fails_before_emission_so_the_chain_falls_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.m4a");
        std::fs::write(&path, b"not media, only synthetic bytes").unwrap();
        let mut sink = CollectSink::default();
        assert!(MacMediaDecoder.decode(&path, &mut sink).is_err());
        assert_eq!(sink.format_calls, 0);
        assert!(MacMediaDecoder.probe(&path).is_err());

        let chain = DecoderChain::new(vec![
            Box::new(MacMediaDecoder),
            Box::new(FakeDecoder::new("fallback", 8_000, vec![0.25; 10])),
        ]);
        let info = chain.decode(&path, &mut sink).unwrap();
        assert_eq!(info.sample_rate, 8_000);
        assert_eq!(sink.samples, [0.25; 10]);

        let missing = directory.path().join("missing.wav");
        assert_eq!(MacMediaDecoder.probe(&missing).unwrap_err().code(), "io");
    }
}
