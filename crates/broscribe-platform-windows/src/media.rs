use broscribe_platform::{Error, MediaDecoder, MediaInfo, PcmSink, Result, TrackInfo};
use std::path::Path;
use windows::{
    Win32::{
        Media::MediaFoundation::*,
        System::Com::StructuredStorage::{PropVariantClear, PropVariantToUInt64},
    },
    core::PCWSTR,
};

pub struct WinMediaDecoder;
struct Foundation;
impl Foundation {
    fn new() -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .map_err(|e| crate::error("starting Media Foundation", e))?;
        Ok(Self)
    }
}
impl Drop for Foundation {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}
struct Source {
    reader: IMFSourceReader,
    stream: u32,
    info: MediaInfo,
}
fn open(path: &Path, index: usize) -> Result<Source> {
    let path_name = crate::wide(std::fs::canonicalize(path).map_err(|e| Error::io_at(path, e))?);
    unsafe {
        let reader = MFCreateSourceReaderFromURL(PCWSTR(path_name.as_ptr()), None)
            .map_err(|e| crate::error("opening Media Foundation source", e))?;
        let mut audio = Vec::new();
        for stream in 0..u32::MAX {
            match reader.GetNativeMediaType(stream, 0) {
                Ok(mt) => {
                    if mt.GetGUID(&MF_MT_MAJOR_TYPE).ok() == Some(MFMediaType_Audio) {
                        audio.push((stream, mt));
                    }
                }
                Err(e) if e.code() == MF_E_INVALIDSTREAMNUMBER => break,
                Err(e) => return Err(crate::error("enumerating Media Foundation streams", e)),
            }
        }
        let (stream, native) = audio
            .get(index)
            .ok_or_else(|| Error::Decode(format!("audio track {index} does not exist")))?;
        let channels = native
            .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
            .map_err(|e| crate::error("audio channel count", e))?;
        let output =
            MFCreateMediaType().map_err(|e| crate::error("creating decoded audio type", e))?;
        output
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(|e| crate::error("setting audio major type", e))?;
        output
            .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)
            .map_err(|e| crate::error("setting float PCM type", e))?;
        reader
            .SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)
            .map_err(|e| crate::error("deselecting media streams", e))?;
        reader
            .SetStreamSelection(*stream, true)
            .map_err(|e| crate::error("selecting audio stream", e))?;
        reader
            .SetCurrentMediaType(*stream, None, &output)
            .map_err(|e| crate::error("negotiating float audio decoder", e))?;
        let output = reader
            .GetCurrentMediaType(*stream)
            .map_err(|e| crate::error("decoded media type", e))?;
        let sample_rate = output
            .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
            .map_err(|e| crate::error("decoded sample rate", e))?;
        let duration_s = reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .ok()
            .and_then(|mut v| {
                let duration = PropVariantToUInt64(&v)
                    .ok()
                    .map(|n| n as f64 / 10_000_000.0);
                let _ = PropVariantClear(&mut v);
                duration
            });
        let tracks = audio
            .iter()
            .enumerate()
            .map(|(index, (_, mt))| {
                Ok(TrackInfo {
                    index,
                    channels: u16::try_from(
                        mt.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                            .map_err(|e| crate::error("track channels", e))?,
                    )
                    .map_err(|_| Error::Decode("too many track channels".into()))?,
                    sample_rate: mt
                        .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                        .map_err(|e| crate::error("track rate", e))?,
                    language: None,
                    title: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let info = MediaInfo {
            sample_rate,
            channels: u16::try_from(channels)
                .map_err(|_| Error::Decode("too many audio channels".into()))?,
            duration_s,
            created_at: None,
            audio_tracks: audio.len() as u32,
            tracks,
            container: path
                .extension()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default(),
        };
        Ok(Source {
            reader,
            stream: *stream,
            info,
        })
    }
}
impl MediaDecoder for WinMediaDecoder {
    fn name(&self) -> &str {
        "media-foundation"
    }
    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        let _com = crate::Com::new()?;
        let _mf = Foundation::new()?;
        Ok(open(path, 0)?.info)
    }
    fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        self.decode_track(path, 0, sink)
    }
    fn decode_track(&self, path: &Path, index: usize, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        let _com = crate::Com::new()?;
        let _mf = Foundation::new()?;
        let source = open(path, index)?;
        unsafe {
            let mt = source
                .reader
                .GetCurrentMediaType(source.stream)
                .map_err(|e| crate::error("decoded format", e))?;
            let channels =
                mt.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                    .map_err(|e| crate::error("decoded channels", e))? as usize;
            if channels == 0 || source.info.sample_rate == 0 {
                return Err(Error::Decode("empty Media Foundation audio format".into()));
            }
            // Read once before committing the sink, allowing the chain to fall back on codec errors.
            let mut formatted = false;
            let mut mono = Vec::new();
            loop {
                let mut flags = 0;
                let mut sample = None;
                source
                    .reader
                    .ReadSample(
                        source.stream,
                        0,
                        None,
                        Some(&mut flags),
                        None,
                        Some(&mut sample),
                    )
                    .map_err(|e| crate::error("decoding Media Foundation audio", e))?;
                if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                    return Err(Error::Decode("audio format changed within source".into()));
                }
                if !formatted {
                    sink.format(source.info.sample_rate)?;
                    formatted = true;
                }
                if let Some(sample) = sample {
                    let buffer = sample
                        .ConvertToContiguousBuffer()
                        .map_err(|e| crate::error("decoded audio buffer", e))?;
                    let mut ptr = std::ptr::null_mut();
                    let mut len = 0;
                    buffer
                        .Lock(&mut ptr, None, Some(&mut len))
                        .map_err(|e| crate::error("locking decoded audio", e))?;
                    let result = (|| {
                        if !(len as usize).is_multiple_of(channels * 4) {
                            return Err(Error::Decode("misaligned float PCM buffer".into()));
                        }
                        let bytes = if len == 0 {
                            &[][..]
                        } else {
                            std::slice::from_raw_parts(ptr, len as usize)
                        };
                        mono.clear();
                        mono.extend(bytes.chunks_exact(channels * 4).map(|frame| {
                            frame
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .map(|s| f32::from_le_bytes(*s))
                                .sum::<f32>()
                                / channels as f32
                        }));
                        sink.samples(&mono)
                    })();
                    let unlock = buffer
                        .Unlock()
                        .map_err(|e| crate::error("unlocking decoded audio", e));
                    result?;
                    unlock?;
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
        }
        Ok(source.info)
    }
}
