//! Offline, pull-based AAC encoding. Each source gets its own enabled track;
//! no AVAssetWriterInputGroup (alternate group) is ever created. Sources are
//! pulled only while their input is ready, and autoreleased objects are drained
//! per batch, so neither PCM nor Objective-C objects grow with recording length.

use std::path::Path;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use broscribe_platform::{
    AudioEncoder, EncodeSettings, EncodedTrack, EncoderTrack, Error, PcmSource, Result,
};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeAppleM4A, AVMediaTypeAudio,
    AVMetadataCommonIdentifierTitle, AVMetadataItem, AVMutableMetadataItem,
};
use objc2_avf_audio::{AVEncoderBitRateKey, AVFormatIDKey, AVNumberOfChannelsKey, AVSampleRateKey};
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMAudioFormatDescriptionCreate, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMSampleTimingInfo, CMTime, kCMTimeInvalid, kCMTimeZero,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};

const BATCH_FRAMES: usize = 4096;

#[derive(Default)]
pub struct MacAudioEncoder;

impl AudioEncoder for MacAudioEncoder {
    fn name(&self) -> &str {
        "avassetwriter"
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
        if tracks.is_empty()
            || settings.sample_rate == 0
            || settings.sample_rate > i32::MAX as u32
            || settings.bitrate_per_track == 0
        {
            return Err(Error::InvalidInput(
                "AAC encoding requires tracks, a positive sample rate and bitrate".into(),
            ));
        }
        match std::fs::symlink_metadata(out) {
            Ok(_) => return Err(Error::AlreadyExists(out.display().to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(Error::io_at(out, error)),
        }
        autoreleasepool(|_| encode(out, settings, tracks))
    }
}

struct Output<'a> {
    writer: Retained<AVAssetWriter>,
    path: &'a Path,
    complete: bool,
}

impl Drop for Output<'_> {
    fn drop(&mut self) {
        if !self.complete {
            // SAFETY: no append or finish call remains in flight on error.
            unsafe {
                if self.writer.status() == AVAssetWriterStatus::Writing {
                    self.writer.cancelWriting();
                }
            }
            let _ = std::fs::remove_file(self.path);
        }
    }
}

fn encode(
    out: &Path,
    settings: &EncodeSettings,
    tracks: &mut [EncoderTrack<'_>],
) -> Result<Vec<EncodedTrack>> {
    let url = NSURL::from_file_path(out)
        .ok_or_else(|| Error::InvalidInput("invalid AAC output path".into()))?;
    // SAFETY: framework constants and a file URL; no callback lifetime involved.
    let writer = unsafe {
        AVAssetWriter::assetWriterWithURL_fileType_error(
            &url,
            AVFileTypeAppleM4A
                .ok_or_else(|| Error::Unsupported("AVFileTypeAppleM4A unavailable".into()))?,
        )
    }
    .map_err(|error| Error::Decode(format!("AVAssetWriter: {error}")))?;
    let mut output = Output {
        writer,
        path: out,
        complete: false,
    };
    let writer = &output.writer;
    let format = pcm_format(settings.sample_rate)?;
    let output_settings = aac_settings(settings)?;
    // SAFETY: AVMediaTypeAudio is a framework-owned constant.
    let media_type = unsafe { AVMediaTypeAudio }
        .ok_or_else(|| Error::Unsupported("AVMediaTypeAudio unavailable".into()))?;
    // SAFETY: dictionary has the documented AAC numeric keys and values.
    if !unsafe { writer.canApplyOutputSettings_forMediaType(Some(&output_settings), media_type) } {
        return Err(Error::Unsupported(
            "AVAssetWriter cannot encode the requested AAC settings".into(),
        ));
    }
    let mut inputs = Vec::with_capacity(tracks.len());
    for track in tracks.iter() {
        // SAFETY: validated audio settings and a matching mono PCM format hint.
        let input = unsafe {
            AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings_sourceFormatHint(
                media_type,
                Some(&output_settings),
                Some(&format),
            )
        };
        let title = NSString::from_str(track.title);
        // SAFETY: metadata setters copy/retain their arguments; NSString is NSCopying.
        unsafe {
            let item = AVMutableMetadataItem::new();
            item.setIdentifier(AVMetadataCommonIdentifierTitle);
            item.setValue(Some(title.as_ref()));
            let items = NSArray::<AVMetadataItem>::from_slice(&[item.as_ref()]);
            input.setMetadata(&items);
            input.setMarksOutputTrackAsEnabled(true);
            input.setExpectsMediaDataInRealTime(false);
            if !writer.canAddInput(&input) {
                return Err(Error::Unsupported(
                    "AVAssetWriter cannot add another AAC track".into(),
                ));
            }
            writer.addInput(&input);
        }
        inputs.push(input);
    }
    // SAFETY: inputs are completely configured before starting the writer.
    unsafe {
        if !writer.startWriting() {
            return Err(writer_error(writer, "start writing"));
        }
        writer.startSessionAtSourceTime(kCMTimeZero);
    }
    let mut frames = vec![0u64; tracks.len()];
    let mut finished = vec![false; tracks.len()];
    let mut remaining = tracks.len();
    while remaining != 0 {
        let mut progressed = false;
        // A small round-robin batch keeps track timestamps interleaved and lets
        // the writer apply backpressure to every input independently.
        autoreleasepool(|_| -> Result<()> {
            for (index, input) in inputs.iter().enumerate() {
                // SAFETY: writer/input stay alive throughout the synchronous loop.
                if finished[index] || !unsafe { input.isReadyForMoreMediaData() } {
                    continue;
                }
                let batch = read_batch(
                    tracks[index].source,
                    &format,
                    settings.sample_rate,
                    frames[index],
                )?;
                match batch {
                    Some((buffer, count)) => {
                        // SAFETY: buffer owns its PCM data and has monotonic timestamps.
                        if !unsafe { input.appendSampleBuffer(&buffer) } {
                            return Err(writer_error(writer, "append PCM"));
                        }
                        frames[index] = frames[index]
                            .checked_add(count as u64)
                            .filter(|frames| *frames <= i64::MAX as u64)
                            .ok_or_else(|| {
                                Error::InvalidInput(
                                    "AAC track duration exceeds CMTime range".into(),
                                )
                            })?;
                    }
                    None => {
                        if frames[index] == 0 {
                            return Err(Error::InvalidInput(format!(
                                "AAC track {} contains no samples",
                                tracks[index].title
                            )));
                        }
                        // SAFETY: no further buffers will be appended to this input.
                        unsafe {
                            input.markAsFinished();
                        }
                        finished[index] = true;
                        remaining -= 1;
                    }
                }
                progressed = true;
            }
            Ok(())
        })?;
        // SAFETY: status is thread-safe and does not alter the writer.
        if unsafe { writer.status() } != AVAssetWriterStatus::Writing {
            return Err(writer_error(writer, "writing PCM"));
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let completion = RcBlock::new(move || {
        let _ = sender.send(());
    });
    // SAFETY: every input is finished and append calls have returned. The API
    // copies the block, and the receiver remains alive until completion.
    unsafe {
        writer.finishWritingWithCompletionHandler(&completion);
    }
    receiver
        .recv()
        .map_err(|_| Error::Internal("AVAssetWriter completion disconnected".into()))?;
    // SAFETY: completion establishes that writing has finished.
    if unsafe { writer.status() } != AVAssetWriterStatus::Completed {
        return Err(writer_error(writer, "finish writing"));
    }
    // The final journal must remain available until this durable file has also
    // passed the caller's independent per-track decode verification.
    std::fs::File::open(out)
        .and_then(|file| file.sync_all())
        .map_err(|error| Error::io_at(out, error))?;
    output.complete = true;
    Ok(tracks
        .iter()
        .zip(frames)
        .map(|(track, frames)| EncodedTrack {
            title: track.title.to_owned(),
            frames,
        })
        .collect())
}

fn aac_settings(settings: &EncodeSettings) -> Result<Retained<NSDictionary<NSString, AnyObject>>> {
    // SAFETY: these framework constants are available on supported macOS versions.
    let keys = unsafe {
        [
            AVFormatIDKey,
            AVSampleRateKey,
            AVNumberOfChannelsKey,
            AVEncoderBitRateKey,
        ]
    };
    let keys = keys
        .map(|key| key.ok_or_else(|| Error::Unsupported("AAC output settings unavailable".into())));
    let [format, rate, channels, bitrate] = keys;
    let keys = [format?, rate?, channels?, bitrate?];
    let values = [
        NSNumber::new_u32(kAudioFormatMPEG4AAC),
        NSNumber::new_u32(settings.sample_rate),
        NSNumber::new_u32(1),
        // AVAssetWriter refuses AAC bit rates above what the encoder accepts
        // at this sample rate (48 kbit/s at 16 kHz): clamp, never fail.
        NSNumber::new_u32(settings.bitrate_per_track.min(
            broscribe_platform::encoder::aac_lc_mono_max_bitrate(settings.sample_rate),
        )),
    ];
    Ok(NSDictionary::from_slices(
        &keys,
        &[
            values[0].as_ref(),
            values[1].as_ref(),
            values[2].as_ref(),
            values[3].as_ref(),
        ],
    ))
}

fn pcm_format(rate: u32) -> Result<CFRetained<CMFormatDescription>> {
    let mut description = AudioStreamBasicDescription {
        mSampleRate: rate as f64,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: 1,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    let mut format = std::ptr::null();
    // SAFETY: ASBD is initialized; mono PCM needs no layout or magic cookie.
    let status = unsafe {
        CMAudioFormatDescriptionCreate(
            None,
            NonNull::from(&mut description),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            None,
            NonNull::from(&mut format),
        )
    };
    check_status(status, "create PCM format")?;
    // SAFETY: successful Create transfers a +1 reference.
    Ok(unsafe {
        CFRetained::from_raw(
            NonNull::new(format.cast_mut())
                .ok_or_else(|| Error::Internal("Core Media returned no PCM format".into()))?,
        )
    })
}

fn read_batch(
    source: &mut dyn PcmSource,
    format: &CMFormatDescription,
    rate: u32,
    start: u64,
) -> Result<Option<(CFRetained<CMSampleBuffer>, usize)>> {
    let bytes = BATCH_FRAMES * std::mem::size_of::<f32>();
    let mut block = std::ptr::null_mut();
    // SAFETY: Core Media allocates/owns the memory block; out pointer is valid.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            std::ptr::null_mut(),
            bytes,
            None,
            std::ptr::null(),
            0,
            bytes,
            0,
            NonNull::from(&mut block),
        )
    };
    check_status(status, "allocate PCM batch")?;
    // SAFETY: successful Create transfers a +1 reference.
    let block = unsafe {
        CFRetained::from_raw(
            NonNull::new(block)
                .ok_or_else(|| Error::Internal("Core Media returned no PCM block".into()))?,
        )
    };
    // Initialize before exposing a Rust f32 slice. Core Media allocates a single
    // suitably aligned block, and only this function can access it until append.
    // SAFETY: the owned block has `bytes` writable bytes.
    check_status(
        unsafe { CMBlockBuffer::fill_data_bytes(0, &block, 0, bytes) },
        "initialize PCM batch",
    )?;
    let mut data = std::ptr::null_mut();
    let mut contiguous = 0;
    // SAFETY: live block and valid out pointers; requesting offset zero.
    check_status(
        unsafe { block.data_pointer(0, &mut contiguous, std::ptr::null_mut(), &mut data) },
        "access PCM batch",
    )?;
    if contiguous < bytes
        || data.is_null()
        || !(data as usize).is_multiple_of(std::mem::align_of::<f32>())
    {
        return Err(Error::Internal(
            "Core Media returned invalid PCM memory".into(),
        ));
    }
    // SAFETY: aligned, initialized, exclusive contiguous storage for this batch.
    let samples = unsafe { std::slice::from_raw_parts_mut(data.cast::<f32>(), BATCH_FRAMES) };
    let count = source.read(samples)?;
    if count > BATCH_FRAMES {
        return Err(Error::InvalidInput(
            "PCM source exceeded supplied batch".into(),
        ));
    }
    if count == 0 {
        return Ok(None);
    }
    if samples[..count].iter().any(|sample| !sample.is_finite()) {
        return Err(Error::InvalidInput(
            "AAC source contains non-finite PCM".into(),
        ));
    }
    // Audio sample-buffer data must match the number of frames, even when a
    // pull source returns a short batch. A subreference retains the allocation
    // without copying PCM or exposing its unused tail to the AAC encoder.
    let block = if count < BATCH_FRAMES {
        let mut exact = std::ptr::null_mut();
        // SAFETY: count is positive and within this live block's capacity.
        let status = unsafe {
            CMBlockBuffer::create_with_buffer_reference(
                None,
                &block,
                0,
                count * std::mem::size_of::<f32>(),
                0,
                NonNull::from(&mut exact),
            )
        };
        check_status(status, "size partial PCM batch")?;
        // SAFETY: successful Create transfers a +1 reference.
        unsafe {
            CFRetained::from_raw(NonNull::new(exact).ok_or_else(|| {
                Error::Internal("Core Media returned no partial PCM block".into())
            })?)
        }
    } else {
        block
    };
    // SAFETY: the rate is positive and fits CMTimeScale; start fits CMTimeValue.
    let timing = unsafe {
        CMSampleTimingInfo {
            duration: CMTime::new(1, rate as i32),
            presentationTimeStamp: CMTime::new(start as i64, rate as i32),
            decodeTimeStamp: kCMTimeInvalid,
        }
    };
    let sample_size = std::mem::size_of::<f32>();
    let mut buffer = std::ptr::null_mut();
    // SAFETY: fixed-size PCM samples use one shared timing entry and one shared
    // sample size. Only count frames are referenced; unused capacity is ignored.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            count as isize,
            1,
            &timing,
            1,
            &sample_size,
            NonNull::from(&mut buffer),
        )
    };
    check_status(status, "create PCM sample buffer")?;
    // SAFETY: sample buffer retains its data block and transfers a +1 reference.
    let buffer = unsafe {
        CFRetained::from_raw(
            NonNull::new(buffer)
                .ok_or_else(|| Error::Internal("Core Media returned no sample buffer".into()))?,
        )
    };
    Ok(Some((buffer, count)))
}

fn check_status(status: i32, operation: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Decode(format!(
            "Core Media {operation}: OSStatus {status}"
        )))
    }
}

fn writer_error(writer: &AVAssetWriter, operation: &str) -> Error {
    // SAFETY: status/error are thread-safe queries.
    let detail = unsafe { writer.error() }
        .map(|error| error.to_string())
        .unwrap_or_else(|| format!("status {:?}", unsafe { writer.status() }));
    Error::Decode(format!("AVAssetWriter {operation}: {detail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::encoder::SliceSource;
    use broscribe_platform::symphonia_decoder::SymphoniaDecoder;
    use broscribe_platform::{MediaDecoder, PcmSink};

    struct Tone {
        at: usize,
        length: usize,
        frequency: f32,
        largest_pull: usize,
    }
    impl PcmSource for Tone {
        fn read(&mut self, out: &mut [f32]) -> Result<usize> {
            self.largest_pull = self.largest_pull.max(out.len());
            // Deliberately short reads also exercise final/partial buffers.
            let count = out.len().min(997).min(self.length - self.at);
            for (index, sample) in out[..count].iter_mut().enumerate() {
                *sample = 0.2
                    * (std::f32::consts::TAU * self.frequency * (self.at + index) as f32
                        / 48_000.0)
                        .sin();
            }
            self.at += count;
            Ok(count)
        }
    }
    #[derive(Default)]
    struct Decoded(Vec<f32>);
    impl PcmSink for Decoded {
        fn format(&mut self, rate: u32) -> Result<()> {
            assert_eq!(rate, 48_000);
            Ok(())
        }
        fn samples(&mut self, samples: &[f32]) -> Result<()> {
            self.0.extend_from_slice(samples);
            Ok(())
        }
    }
    fn energy(samples: &[f32], frequency: f64) -> f64 {
        let (mut real, mut imaginary) = (0.0, 0.0);
        for (index, sample) in samples.iter().enumerate() {
            let phase = std::f64::consts::TAU * frequency * index as f64 / 48_000.0;
            real += *sample as f64 * phase.cos();
            imaginary += *sample as f64 * phase.sin();
        }
        real * real + imaginary * imaginary
    }

    #[test]
    fn synthetic_two_track_aac_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.m4a");
        let mut mic = Tone {
            at: 0,
            length: 48_000,
            frequency: 440.0,
            largest_pull: 0,
        };
        let mut system = Tone {
            at: 0,
            length: 72_000,
            frequency: 880.0,
            largest_pull: 0,
        };
        let result = MacAudioEncoder
            .encode(
                &path,
                &EncodeSettings::speech(48_000),
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
        assert_eq!(
            result.iter().map(|track| track.frames).collect::<Vec<_>>(),
            [48_000, 72_000]
        );
        assert!(mic.largest_pull <= BATCH_FRAMES && system.largest_pull <= BATCH_FRAMES);
        let decoder = SymphoniaDecoder;
        assert_eq!(decoder.probe(&path).unwrap().audio_tracks, 2);
        for (index, own, other, length) in [
            (0, 440.0, 880.0, 48_000usize),
            (1, 880.0, 440.0, 72_000usize),
        ] {
            let mut decoded = Decoded::default();
            decoder.decode_track(&path, index, &mut decoded).unwrap();
            // Symphonia exposes raw AAC-LC frames rather than applying the
            // container's priming/trailing trims. Apple's AAC encoder primes
            // with 2112 frames and pads the tail to a 1024-frame packet. Do not
            // mistake that codec padding for extra source PCM or permit loss.
            const MAX_AAC_PADDING: usize = 2112 + 1023;
            assert!(
                (length..=length + MAX_AAC_PADDING).contains(&decoded.0.len()),
                "track {index}: decoded {} frames for {length} source frames",
                decoded.0.len(),
            );
            assert!(energy(&decoded.0, own) > 100.0 * energy(&decoded.0, other));
        }
    }

    #[test]
    fn aac_encodes_at_bluetooth_call_rates() {
        let directory = tempfile::tempdir().unwrap();
        for rate in [16_000u32, 24_000] {
            let tone: Vec<f32> = (0..rate)
                .map(|n| 0.2 * (std::f32::consts::TAU * 440.0 * n as f32 / rate as f32).sin())
                .collect();
            // Both the speech preset and a request above the encoder's limit.
            for (name, settings) in [
                ("speech", EncodeSettings::speech(rate)),
                (
                    "high",
                    EncodeSettings {
                        sample_rate: rate,
                        bitrate_per_track: 128_000,
                    },
                ),
            ] {
                let path = directory.path().join(format!("{name}-{rate}.m4a"));
                let mut source = SliceSource::new(tone.clone());
                let written = MacAudioEncoder
                    .encode(
                        &path,
                        &settings,
                        &mut [EncoderTrack {
                            title: "mic",
                            source: &mut source,
                        }],
                    )
                    .unwrap_or_else(|e| panic!("{name} at {rate} Hz: {e}"));
                assert_eq!(written[0].frames, u64::from(rate));
                assert_eq!(SymphoniaDecoder.probe(&path).unwrap().audio_tracks, 1);
            }
        }
    }

    #[test]
    fn source_error_removes_partial_output_and_existing_file_survives() {
        struct Broken;
        impl PcmSource for Broken {
            fn read(&mut self, _: &mut [f32]) -> Result<usize> {
                Err(Error::InvalidInput("synthetic source failure".into()))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("failed.m4a");
        let mut source = Broken;
        assert!(
            MacAudioEncoder
                .encode(
                    &path,
                    &EncodeSettings::speech(48_000),
                    &mut [EncoderTrack {
                        title: "mic",
                        source: &mut source
                    }]
                )
                .is_err()
        );
        assert!(!path.exists());
        std::fs::write(&path, b"preserve synthetic data").unwrap();
        let mut source = SliceSource::new(vec![0.0; 1024]);
        assert!(matches!(
            MacAudioEncoder.encode(
                &path,
                &EncodeSettings::speech(48_000),
                &mut [EncoderTrack {
                    title: "mic",
                    source: &mut source
                }]
            ),
            Err(Error::AlreadyExists(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve synthetic data");
    }
}
