//! Independent, track-addressable sources merged on one recording timeline.
use crate::analyze::{self, Analysis, AnalyzeOptions, Turn};
use crate::audio::TARGET_RATE;
use crate::manifest::{MAX_SOURCE_OFFSET_S, ManifestSource};
use crate::record::MeEvidence;
use broscribe_core::schema::{LanguageSpan, TrackRole};
use broscribe_core::{Error, MeetingId, Result, SpeakerId};
use broscribe_models::ModelSet;
use broscribe_platform::{DecoderChain, PcmSink};

/// Rate, length and energy of a decoded track, without holding its samples:
/// verifying an encoded track never needs a second copy of it.
#[derive(Default)]
struct Level {
    rate: u32,
    frames: usize,
    energy: f64,
}
impl PcmSink for Level {
    fn format(&mut self, rate: u32) -> Result<()> {
        if rate == 0 {
            return Err(Error::InvalidInput("decoded sample rate is zero".into()));
        }
        self.rate = rate;
        Ok(())
    }
    fn samples(&mut self, samples: &[f32]) -> Result<()> {
        self.frames += samples.len();
        // Sample by sample: the same sum as over the whole decoded track.
        for s in samples {
            self.energy += f64::from(*s).powi(2);
        }
        Ok(())
    }
}

/// `offset_s` in 16 kHz frames; `invalid_input` unless it is finite and
/// within [`MAX_SOURCE_OFFSET_S`], so no timeline arithmetic can overflow
/// and no offset pads a track with more silence than that limit.
pub(crate) fn offset_frames(offset_s: f64) -> Result<i64> {
    if !offset_s.is_finite() || offset_s.abs() > MAX_SOURCE_OFFSET_S {
        return Err(Error::InvalidInput(format!(
            "source offset {offset_s} s must be finite and within ±{MAX_SOURCE_OFFSET_S} s"
        )));
    }
    // Bounded above, so the rounded frame count is exact in i64.
    Ok((offset_s * f64::from(TARGET_RATE)).round() as i64)
}

/// Frames trimmed from the start of a track (negative offset) and the frame
/// at which the rest starts on the meeting timeline (positive offset).
fn placement(offset: i64, len: usize) -> (usize, usize) {
    let skip = usize::try_from(-offset).map_or(0, |skip| skip.min(len));
    let start = usize::try_from(offset).unwrap_or(0);
    (skip, start)
}

/// Decode precisely the selected audio track at model rate, with the same
/// container-duration check and ffmpeg fallback as whole-file decoding.
pub fn decode_source(decoders: &DecoderChain, source: &ManifestSource) -> Result<Vec<f32>> {
    crate::audio::decode_track_16k(
        decoders,
        &source.path,
        source.audio_track_index.unwrap_or(0),
    )
    .map(|d| d.samples)
}

pub(crate) fn resample_slice(samples: &[f32], rate: u32) -> Vec<f32> {
    let mut resampler =
        broscribe_capture::resample::Resampler::new(f64::from(rate), f64::from(TARGET_RATE));
    let mut out = vec![0.0; resampler.output_len(samples.len() as u64) as usize];
    let mut filled = 0;
    for chunk in samples.chunks(4096) {
        resampler.push(chunk);
        filled += resampler.pull(&mut out[filled..]);
    }
    resampler.finish();
    filled += resampler.pull(&mut out[filled..]);
    out.truncate(filled);
    out
}

/// The speech of a track known to hold one voice: its ASR regions and one
/// speaker-1 turn per region.
fn one_voice_turns(
    samples: &[f32],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
) -> Result<(Vec<analyze::AsrRegion>, Vec<Turn>)> {
    let regions = analyze::transcribe_speech(samples, models, languages)?;
    let id = SpeakerId::new(1)?;
    let turns = regions
        .iter()
        .map(|((a, b), _, _)| Turn {
            start: *a,
            end: *b,
            speaker: id,
        })
        .collect();
    Ok((regions, turns))
}

/// The transcript of a track known to hold one voice, every segment speaker
/// 1, with no speaker record and no embedding: for a caller that builds the
/// track's speakers itself from its own embeddings (the non-room mic of
/// [`crate::record::analyze_tracks`]).
pub(crate) fn one_voice_transcript(
    samples: &[f32],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
) -> Result<Analysis> {
    let (regions, turns) = one_voice_turns(samples, models, languages)?;
    Ok(Analysis {
        segments: analyze::segments_from(regions, &turns),
        speakers: Vec::new(),
        diar_segments: turns,
        duration_s: samples.len() as f64 / f64::from(TARGET_RATE),
    })
}

/// ASR and centroid extraction without diarization for a known single-person track.
/// Multiple incompatible voice embeddings in clean VAD regions trigger normal
/// diarization instead; a manifest hint never forces different voices together.
pub(crate) fn single_person(
    samples: &[f32],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
) -> Result<Analysis> {
    let (regions, turns) = one_voice_turns(samples, models, languages)?;
    let ranges: Vec<_> = turns
        .iter()
        .filter(|t| t.end - t.start >= analyze::MIN_CLEAN_S)
        .flat_map(|t| {
            let count = ((t.end - t.start) / 3.0).ceil() as usize;
            let step = (t.end - t.start) / count as f64;
            (0..count).map(move |i| (t.start + i as f64 * step, t.start + (i + 1) as f64 * step))
        })
        .collect();
    let vectors = analyze::embed_ranges(samples, &ranges, models.embedder.as_ref())?;
    // A deliberately conservative conflict gate: uncertain evidence preserves
    // the participant mapping rather than inventing additional speakers.
    if vectors
        .iter()
        .enumerate()
        .any(|(i, (_, a))| vectors[i + 1..].iter().any(|(_, b)| cosine(a, b) < 0.5))
    {
        let turns = analyze::label_turns(&models.diarizer.diarize(samples)?);
        let speakers = analyze::build_speakers(samples, &turns, models.embedder.as_ref())?;
        return Ok(Analysis {
            segments: analyze::segments_from(regions, &turns),
            speakers,
            diar_segments: turns,
            duration_s: samples.len() as f64 / f64::from(TARGET_RATE),
        });
    }
    let mut speaker = broscribe_core::schema::Speaker::new(SpeakerId::new(1)?, "t0");
    speaker.talk_s = turns.iter().map(|t| t.end - t.start).sum();
    let values: Vec<_> = vectors.iter().map(|(_, v)| v).collect();
    speaker.centroid = broscribe_core::identity::mean_normalized(&values).map(|v| {
        let mut centroid = broscribe_core::schema::Centroid::new(
            models.embedder.model_id(),
            &v,
            vectors.len() as u32,
        );
        centroid.ranges = vectors
            .iter()
            .map(|((a, b), _)| [*a as f32, *b as f32])
            .collect();
        centroid
    });
    let speakers = if turns.is_empty() {
        Vec::new()
    } else {
        vec![speaker]
    };
    Ok(Analysis {
        segments: analyze::segments_from(regions, &turns),
        speakers,
        diar_segments: turns,
        duration_s: samples.len() as f64 / f64::from(TARGET_RATE),
    })
}

pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return -1.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = (a.iter().map(|x| x * x).sum::<f32>() * b.iter().map(|x| x * x).sum::<f32>()).sqrt();
    if norm > 0.0 { dot / norm } else { -1.0 }
}

/// Merge without mixing PCM or sharing track-local speaker IDs. Negative
/// offsets trim content before meeting time zero, including word timestamps.
pub(crate) fn append(
    out: &mut Analysis,
    mut track: Analysis,
    track_id: &str,
    offset: f64,
) -> Result<()> {
    let base = out
        .speakers
        .iter()
        .map(|s| s.id.number())
        .max()
        .unwrap_or(0);
    let map = |id: SpeakerId| SpeakerId::new(base + id.number());
    for speaker in &mut track.speakers {
        speaker.id = map(speaker.id)?;
        speaker.track = track_id.into();
        if let Some(centroid) = &mut speaker.centroid {
            for range in &mut centroid.ranges {
                range[0] = (range[0] + offset as f32).max(0.0);
                range[1] += offset as f32;
            }
            centroid.ranges.retain(|r| r[1] > r[0]);
        }
    }
    for mut segment in track.segments {
        segment.start += offset;
        segment.end += offset;
        if segment.end <= 0.0 {
            continue;
        }
        segment.start = segment.start.max(0.0);
        segment.track = track_id.into();
        segment.speaker = segment.speaker.map(map).transpose()?;
        for [start, end] in &mut segment.speech_overlap {
            *start = (*start + offset).max(0.0);
            *end += offset;
        }
        segment.speech_overlap.retain(|[start, end]| end > start);
        for word in &mut segment.words {
            word.start = (word.start + offset).max(0.0);
            word.end += offset;
        }
        segment.words.retain(|w| w.end > 0.0);
        out.segments.push(segment);
    }
    for mut turn in track.diar_segments {
        turn.start = (turn.start + offset).max(0.0);
        turn.end += offset;
        if turn.end > turn.start {
            turn.speaker = map(turn.speaker)?;
            out.diar_segments.push(turn);
        }
    }
    out.speakers.extend(track.speakers);
    out.duration_s = out.duration_s.max((track.duration_s + offset).max(0.0));
    sort(out);
    Ok(())
}
pub(crate) fn empty() -> Analysis {
    Analysis {
        segments: Vec::new(),
        speakers: Vec::new(),
        diar_segments: Vec::new(),
        duration_s: 0.0,
    }
}
pub(crate) fn sort(out: &mut Analysis) {
    out.segments
        .sort_by(|a, b| a.start.total_cmp(&b.start).then(a.track.cmp(&b.track)));
    for (id, segment) in out.segments.iter_mut().enumerate() {
        segment.id = id as u32;
    }
    out.diar_segments
        .sort_by(|a, b| a.start.total_cmp(&b.start));
}

/// Indices of the microphone and system tracks when `roles` holds exactly
/// one of each: only then are they analyzed as a pair (echo cancellation,
/// segment-level Me verification). Any other mix is analyzed per track.
pub(crate) fn mic_system_pair(
    roles: impl IntoIterator<Item = TrackRole>,
) -> Option<(usize, usize)> {
    let (mut mic, mut system) = (Vec::new(), Vec::new());
    for (i, role) in roles.into_iter().enumerate() {
        match role {
            TrackRole::Mic => mic.push(i),
            TrackRole::System => system.push(i),
            _ => {}
        }
    }
    match (mic.as_slice(), system.as_slice()) {
        (&[mic], &[system]) => Some((mic, system)),
        _ => None,
    }
}

/// Me's evidence as stored now, which a mic + system pair of `sources` is
/// analyzed against (Me's confirmed prints derived from a `held_out` meeting
/// left out); `None` without such a pair. A publisher fences on it
/// ([`MeEvidence::still_current`]) so a forget during the analysis never
/// publishes Me naming from the forgotten voice.
pub(crate) fn me_evidence(
    env: &crate::Env<'_>,
    sources: &[ManifestSource],
    model: &str,
    held_out: &[MeetingId],
) -> Result<Option<MeEvidence>> {
    mic_system_pair(sources.iter().map(|s| s.role))
        .map(|_| MeEvidence::load(env.lib, model, held_out))
        .transpose()
}

/// Analyze N independent sources into one meeting. Hints are enrolled by the
/// caller's normal batch identity phase, not trusted as confirmed assignments.
/// A mic + system pair is analyzed against `me` ([`me_evidence`] of
/// `sources`). Also returns each source's decoded 16 kHz audio.
pub(crate) fn analyze_sources_with_audio(
    env: &crate::Env<'_>,
    sources: &[ManifestSource],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
    me: Option<&MeEvidence>,
) -> Result<(Analysis, Vec<Vec<f32>>)> {
    analyze_sources_impl(env, sources, models, languages, None, None, me)
}

/// Test entry point: [`analyze_sources_with_evidence`] with an explicit
/// setup and echo handling for a mic + system pair (see `analyze_decoded`),
/// against Me's evidence as stored now.
#[cfg(test)]
pub(crate) fn analyze_sources_with_setup(
    env: &crate::Env<'_>,
    sources: &[ManifestSource],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
    setup: Option<broscribe_dsp::Setup>,
    handling: Option<broscribe_dsp::EchoHandling>,
) -> Result<Analysis> {
    let me = me_evidence(env, sources, models.embedder.model_id(), &[])?;
    analyze_sources_with_evidence(
        env,
        sources,
        models,
        languages,
        setup,
        handling,
        me.as_ref(),
    )
}

/// Analyze N independent sources into one meeting with an explicit setup and
/// echo handling for a mic + system pair (see `analyze_decoded`), against
/// `me` ([`me_evidence`] of `sources`, taken before the analysis), so the
/// publisher can fence its commit on [`MeEvidence::still_current`].
pub(crate) fn analyze_sources_with_evidence(
    env: &crate::Env<'_>,
    sources: &[ManifestSource],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
    setup: Option<broscribe_dsp::Setup>,
    handling: Option<broscribe_dsp::EchoHandling>,
    me: Option<&MeEvidence>,
) -> Result<Analysis> {
    analyze_sources_impl(env, sources, models, languages, setup, handling, me).map(|(a, _)| a)
}

fn analyze_sources_impl(
    env: &crate::Env<'_>,
    sources: &[ManifestSource],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
    setup: Option<broscribe_dsp::Setup>,
    handling: Option<broscribe_dsp::EchoHandling>,
    me: Option<&MeEvidence>,
) -> Result<(Analysis, Vec<Vec<f32>>)> {
    if sources.is_empty() {
        return Err(Error::InvalidInput(
            "multi-track import has no sources".into(),
        ));
    }
    // Reject unusable offsets before decoding anything.
    for source in sources {
        offset_frames(source.offset_s)?;
    }
    let audio = sources
        .iter()
        .map(|s| decode_source(env.decoders, s))
        .collect::<Result<Vec<_>>>()?;
    let views: Vec<&[f32]> = audio.iter().map(Vec::as_slice).collect();
    let out = analyze_decoded_with(env, sources, &views, models, languages, setup, handling, me)?;
    Ok((out, audio))
}

/// The role-aware analysis of [`analyze_sources_with_evidence`] on already
/// decoded 16 kHz audio: `audio[i]` is `sources[i]`, whose path is not read.
/// A mic + system pair goes through [`crate::record::analyze_tracks`]
/// (ignoring Me samples derived from a `held_out` meeting), participant
/// tracks through the single-person path, others through plain analysis.
/// Track labels are `t<i>`.
#[allow(clippy::too_many_arguments)] // Mirrors `analyze_tracks`' explicit inputs.
pub(crate) fn analyze_decoded(
    env: &crate::Env<'_>,
    sources: &[ManifestSource],
    audio: &[&[f32]],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
    setup: Option<broscribe_dsp::Setup>,
    handling: Option<broscribe_dsp::EchoHandling>,
    held_out: &[MeetingId],
) -> Result<Analysis> {
    let me = me_evidence(env, sources, models.embedder.model_id(), held_out)?;
    analyze_decoded_with(
        env,
        sources,
        audio,
        models,
        languages,
        setup,
        handling,
        me.as_ref(),
    )
}

/// [`analyze_decoded`] against Me's evidence `me` ([`me_evidence`]).
#[allow(clippy::too_many_arguments)] // Mirrors `analyze_tracks`' explicit inputs.
fn analyze_decoded_with(
    env: &crate::Env<'_>,
    sources: &[ManifestSource],
    audio: &[&[f32]],
    models: &mut ModelSet,
    languages: &[LanguageSpan],
    setup: Option<broscribe_dsp::Setup>,
    handling: Option<broscribe_dsp::EchoHandling>,
    me: Option<&MeEvidence>,
) -> Result<Analysis> {
    if audio.len() != sources.len() {
        return Err(Error::InvalidInput(
            "source/audio track count differs".into(),
        ));
    }
    let offsets = sources
        .iter()
        .map(|s| offset_frames(s.offset_s))
        .collect::<Result<Vec<_>>>()?;
    let mut out = empty();
    let duration_s = sources
        .iter()
        .zip(audio)
        .map(|(s, a)| (a.len() as f64 / f64::from(TARGET_RATE) + s.offset_s).max(0.0))
        .fold(0.0, f64::max);
    let languages: Vec<_> = languages
        .iter()
        .map(|l| LanguageSpan {
            lang: l.lang.clone(),
            from_s: l.from_s,
            to_s: l.to_s.min(duration_s),
        })
        .collect();
    let pair = mic_system_pair(sources.iter().map(|s| s.role));
    if let Some((mic, system)) = pair {
        // Integer placement: the padded timeline always holds every
        // visible frame, whatever the rounding of either offset.
        let span = |i: usize| {
            let (skip, start) = placement(offsets[i], audio[i].len());
            (skip, start, start + (audio[i].len() - skip))
        };
        let frame_count = span(mic).2.max(span(system).2);
        let align = |i: usize| {
            let (skip, start, end) = span(i);
            let visible = &audio[i][skip..];
            if start == 0 && visible.len() == frame_count {
                return std::borrow::Cow::Borrowed(visible);
            }
            let mut samples = vec![0.0; frame_count];
            samples[start..end].copy_from_slice(visible);
            std::borrow::Cow::Owned(samples)
        };
        let me = me.ok_or_else(|| {
            Error::Internal("a mic + system pair is analyzed against Me's evidence".into())
        })?;
        let (mut analysis, _) = crate::record::analyze_tracks_with(
            env,
            &align(mic),
            &align(system),
            TARGET_RATE,
            setup.unwrap_or_else(|| broscribe_dsp::SetupRules::builtin().fallback),
            handling,
            models,
            &languages,
            me,
        )?;
        // Padding establishes a common DSP timeline, not extra source audio.
        // Constrain even ASR hallucinations in padding to the original support.
        for (track, i) in [("mic", mic), ("system", system)] {
            let from = sources[i].offset_s.max(0.0);
            let to =
                (sources[i].offset_s + audio[i].len() as f64 / f64::from(TARGET_RATE)).max(0.0);
            for segment in analysis.segments.iter_mut().filter(|s| s.track == track) {
                segment.start = segment.start.max(from);
                segment.end = segment.end.min(to);
                for [start, end] in &mut segment.speech_overlap {
                    *start = start.max(from);
                    *end = end.min(to);
                }
                segment.speech_overlap.retain(|[start, end]| end > start);
                let original_words = segment.words.len();
                for word in &mut segment.words {
                    word.start = word.start.max(from);
                    word.end = word.end.min(to);
                }
                segment.words.retain(|w| w.end > w.start);
                if segment.words.len() != original_words {
                    segment.text.clear();
                    for (i, word) in segment.words.iter().enumerate() {
                        if i > 0 {
                            segment.text.push(' ');
                        }
                        segment.text.push_str(&word.text);
                    }
                    if segment.words.is_empty() {
                        segment.end = segment.start;
                    }
                }
            }
            let speakers = &analysis.speakers;
            for turn in analysis.diar_segments.iter_mut().filter(|t| {
                speakers
                    .iter()
                    .any(|s| s.id == t.speaker && s.track == track)
            }) {
                turn.start = turn.start.max(from);
                turn.end = turn.end.min(to);
            }
            for speaker in analysis.speakers.iter_mut().filter(|s| s.track == track) {
                if let Some(centroid) = &mut speaker.centroid {
                    for range in &mut centroid.ranges {
                        range[0] = range[0].max(from as f32);
                        range[1] = range[1].min(to as f32);
                    }
                    centroid.ranges.retain(|r| r[1] > r[0]);
                }
            }
        }
        analysis.segments.retain(|s| s.end > s.start);
        analysis.diar_segments.retain(|t| t.end > t.start);
        for speaker in &mut analysis.speakers {
            let diar_talk: f64 = analysis
                .diar_segments
                .iter()
                .filter(|t| t.speaker == speaker.id)
                .map(|t| t.end - t.start)
                .sum();
            let segment_talk: f64 = analysis
                .segments
                .iter()
                .filter(|s| s.speaker == Some(speaker.id))
                .map(|s| s.end - s.start)
                .sum();
            speaker.talk_s = diar_talk.max(segment_talk);
        }
        analysis.speakers.retain(|s| s.talk_s > 0.0);
        for speaker in &mut analysis.speakers {
            speaker.track = format!("t{}", if speaker.track == "mic" { mic } else { system });
        }
        for segment in &mut analysis.segments {
            segment.track = format!("t{}", if segment.track == "mic" { mic } else { system });
        }
        out = analysis;
    }
    for (i, source) in sources.iter().enumerate() {
        if pair.is_some_and(|(mic, system)| i == mic || i == system) {
            continue;
        }
        let samples = audio[i];
        let offset = source.offset_s.max(0.0);
        let (skip, _) = placement(offsets[i], samples.len());
        let visible = &samples[skip..];
        let local_languages: Vec<_> = languages
            .iter()
            .map(|l| LanguageSpan {
                lang: l.lang.clone(),
                from_s: l.from_s - offset,
                to_s: l.to_s - offset,
            })
            .collect();
        let analysis = if source.role == TrackRole::Participant {
            single_person(visible, models, &local_languages)?
        } else {
            analyze::analyze_with_languages(
                visible,
                models,
                AnalyzeOptions::default(),
                &local_languages,
            )?
        };
        append(&mut out, analysis, &format!("t{i}"), offset)?;
    }
    Ok(out)
}

/// Common-length track stream with source offset applied without PCM copies.
struct OffsetSource<'a> {
    samples: &'a [f32],
    offset: i64,
    frames: usize,
    pos: usize,
}
impl broscribe_platform::PcmSource for OffsetSource<'_> {
    fn read(&mut self, out: &mut [f32]) -> Result<usize> {
        let n = out.len().min(self.frames - self.pos);
        out[..n].fill(0.0);
        let first = (self.pos as i64 - self.offset).max(0) as usize;
        let end = ((self.pos + n) as i64 - self.offset).max(0) as usize;
        let end = end.min(self.samples.len());
        if first < end {
            let target = (first as i64 + self.offset - self.pos as i64) as usize;
            out[target..target + end - first].copy_from_slice(&self.samples[first..end]);
        }
        self.pos += n;
        Ok(n)
    }
}

/// Encode independent aligned tracks and verify every decoded track.
pub(crate) fn encode_sources(
    decoders: &DecoderChain,
    sources: &[ManifestSource],
    audio: &[Vec<f32>],
    encoder: &dyn broscribe_platform::AudioEncoder,
    out: &std::path::Path,
) -> Result<()> {
    if sources.len() != audio.len() || sources.is_empty() {
        return Err(Error::InvalidInput(
            "source/audio track count differs".into(),
        ));
    }
    let offsets = sources
        .iter()
        .map(|s| offset_frames(s.offset_s))
        .collect::<Result<Vec<_>>>()?;
    let frames = offsets
        .iter()
        .zip(audio)
        .map(|(&offset, a)| {
            let (skip, start) = placement(offset, a.len());
            start + (a.len() - skip)
        })
        .max()
        .unwrap_or(0);
    let mut streams: Vec<_> = offsets
        .iter()
        .zip(audio)
        .map(|(&offset, a)| OffsetSource {
            samples: a,
            offset,
            frames,
            pos: 0,
        })
        .collect();
    let titles: Vec<_> = (0..sources.len()).map(|i| format!("t{i}")).collect();
    let mut tracks: Vec<_> = streams
        .iter_mut()
        .zip(&titles)
        .map(|(stream, title)| broscribe_platform::EncoderTrack {
            title,
            source: stream as &mut dyn broscribe_platform::PcmSource,
        })
        .collect();
    let encoded = encoder.encode(
        out,
        &broscribe_platform::EncodeSettings::speech(TARGET_RATE),
        &mut tracks,
    )?;
    if encoded.len() != sources.len() || encoded.iter().any(|t| t.frames != frames as u64) {
        return Err(Error::Decode(
            "encoder did not consume every aligned track".into(),
        ));
    }
    for (i, (&offset, samples)) in offsets.iter().zip(audio).enumerate() {
        let mut decoded = Level::default();
        decoders.decode_track(out, i, &mut decoded)?;
        if decoded.rate != TARGET_RATE
            || decoded.frames.abs_diff(frames) > TARGET_RATE as usize / 10
        {
            return Err(Error::Decode(format!(
                "encoded track {i} failed rate/length verification"
            )));
        }
        let (skip, _) = placement(offset, samples.len());
        let expected = samples[skip..]
            .iter()
            .map(|s| f64::from(*s).powi(2))
            .sum::<f64>()
            / frames.max(1) as f64;
        let actual = decoded.energy / decoded.frames.max(1) as f64;
        if (expected > 1e-9
            && (actual <= 1e-12 || (10.0 * (actual / expected).log10()).abs() > 3.0))
            || (expected <= 1e-12 && actual > 1e-6)
        {
            return Err(Error::Decode(format!(
                "encoded track {i} failed level verification"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, conversation, models, write_wav};
    #[test]
    fn multitrack_participant_offsets_merge_without_speaker_collision() {
        let fixture = Fixture::new();
        let a = fixture.input.join("a.wav");
        let b = fixture.input.join("b.wav");
        write_wav(&a, TARGET_RATE, &conversation(&[(Some(0), 2.0)]));
        write_wav(&b, TARGET_RATE, &conversation(&[(Some(1), 2.0)]));
        let sources = vec![
            ManifestSource {
                path: a,
                audio_track_index: None,
                role: TrackRole::Participant,
                person_hint: None,
                offset_s: 0.0,
            },
            ManifestSource {
                path: b,
                audio_track_index: None,
                role: TrackRole::Participant,
                person_hint: None,
                offset_s: 3.0,
            },
        ];
        let result =
            analyze_sources_with_setup(&fixture.env(), &sources, &mut models(), &[], None, None)
                .unwrap();
        assert_eq!(result.speakers.len(), 2);
        assert!(
            result
                .segments
                .iter()
                .any(|s| s.track == "t0" && s.start < 1.0)
        );
        assert!(
            result
                .segments
                .iter()
                .any(|s| s.track == "t1" && s.start >= 3.0)
        );
        assert_ne!(result.speakers[0].id, result.speakers[1].id);
        assert_eq!(result.duration_s, 5.0);
    }
}

#[cfg(test)]
mod import_tests {
    use crate::{
        batch::{BatchOptions, import_manifest},
        manifest,
        testutil::{Fixture, models},
    };
    use broscribe_core::schema::{Assignment, Handling};
    use broscribe_platform::encoder::{FakeEncodedDecoder, FakeEncoder};
    use std::sync::Arc;

    #[test]
    fn multitrack_manifest_imports_one_aligned_meeting_for_copy_and_reference() {
        for handling in [Handling::Copy, Handling::Reference] {
            let mut fixture = Fixture::new();
            fixture.chain.0.push(Box::new(FakeEncodedDecoder));
            fixture.config.identity.insert(
                fixture.config.profile.speaker_embedding.clone(),
                broscribe_core::config::Thresholds {
                    auto_name: Some(0.8),
                    suggest: Some(0.6),
                    margin: Some(0.1),
                    merge: Some(0.95),
                    auto_merge: Some(0.95),
                },
            );
            let a = fixture.wav("participant-a.wav", &[(Some(0), 4.0)]);
            let b = fixture.wav("participant-b.wav", &[(Some(1), 4.0)]);
            let manifest=manifest::parse(&serde_json::json!({
                "format":manifest::FORMAT,"version":2,"meetings":[{
                    "source_id":"synthetic-multitrack","tracks":"per-participant","sources":[
                        {"path":a,"role":"participant","person_hint":{"name":"Alex Example"},"offset_s":0.0},
                        {"path":b,"role":"participant","person_hint":{"name":"Robin Example"},"offset_s":3.0}
                    ]
                }]
            }).to_string()).unwrap();
            assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);
            let encoder = Arc::new(FakeEncoder::default());
            let options = BatchOptions {
                handling: Some(handling),
                encoder: encoder.clone(),
            };
            let result =
                import_manifest(&fixture.env(), &mut models(), &manifest, &options).unwrap();
            assert!(result.failed.is_empty(), "{:?}", result.failed);
            assert_eq!(result.resolved, 1);
            let dirs = fixture.lib.list_meeting_dirs().unwrap();
            assert_eq!(dirs.len(), 1);
            let meeting = fixture.lib.load_meeting_at(&dirs[0]).unwrap();
            assert_eq!(meeting.duration_s, 7.0);
            assert_eq!(meeting.tracks.len(), 2);
            assert!(
                meeting
                    .segments
                    .iter()
                    .any(|s| s.track == "t0" && s.start < 0.1)
            );
            assert!(
                meeting
                    .segments
                    .iter()
                    .any(|s| s.track == "t1" && s.start >= 3.0)
            );
            assert!(
                meeting
                    .speakers
                    .iter()
                    .all(|s| s.assignment == Assignment::Auto)
            );
            let sources = crate::audio::meeting_tracks(&meeting);
            assert_eq!(sources.len(), 2);
            if handling == Handling::Copy {
                assert!(meeting.recording.as_ref().unwrap().sources.is_empty());
                assert_eq!(sources[1].track_index, 1);
                assert_eq!(sources[1].offset_s, 0.0);
                let second = crate::audio::decode_meeting_track(
                    &fixture.lib,
                    &fixture.chain,
                    &meeting,
                    &sources[1],
                )
                .unwrap();
                assert!(second[..48000].iter().all(|s| *s == 0.0));
                assert!(second[48000..].iter().any(|s| s.abs() > 0.01));
            } else {
                assert_eq!(sources[1].offset_s, 3.0);
                assert!(encoder.calls.lock().unwrap().is_empty());
            }
            let second_speaker = meeting.speakers.iter().find(|s| s.track == "t1").unwrap();
            let confirmation = crate::confirm::confirmation_samples(
                &fixture.lib,
                &fixture.chain,
                models().embedder.as_ref(),
                &meeting.id,
                second_speaker.id,
            )
            .unwrap();
            assert!(!confirmation.is_empty());
            assert!(confirmation.iter().all(|s| s.start >= 3.0));
            let mixed = crate::audio::playback_mix(&fixture.lib, &fixture.chain, &meeting).unwrap();
            assert_eq!(mixed.len(), 112000);
            let rerun =
                import_manifest(&fixture.env(), &mut models(), &manifest, &options).unwrap();
            assert!(rerun.failed.is_empty(), "{:?}", rerun.failed);
            assert_eq!(fixture.lib.list_meeting_dirs().unwrap().len(), 1);
        }
    }
}

#[cfg(test)]
mod participant_tests {
    use super::*;
    use crate::testutil::{conversation, models};
    struct RejectDiarization;
    impl broscribe_models::Diarizer for RejectDiarization {
        fn diarize(&self, _: &[f32]) -> Result<Vec<broscribe_models::DiarSegment>> {
            Err(Error::Internal(
                "single-person track must not invoke diarization".into(),
            ))
        }
    }
    #[test]
    fn multitrack_participant_skips_diarization_unless_windows_contain_distinct_voices() {
        let mut single = models();
        single.diarizer = Box::new(RejectDiarization);
        let analysis = single_person(&conversation(&[(Some(0), 6.0)]), &mut single, &[]).unwrap();
        assert_eq!(analysis.speakers.len(), 1);
        let analysis = single_person(
            &conversation(&[(Some(0), 4.0), (Some(1), 4.0)]),
            &mut models(),
            &[],
        )
        .unwrap();
        assert_eq!(analysis.speakers.len(), 2);
    }
    #[test]
    fn multitrack_negative_offset_discards_pre_meeting_speech() {
        let fixture = crate::testutil::Fixture::new();
        let path = fixture.wav("trimmed.wav", &[(Some(0), 4.0), (Some(1), 4.0)]);
        let source = ManifestSource {
            path,
            audio_track_index: None,
            role: TrackRole::Participant,
            person_hint: None,
            offset_s: -4.0,
        };
        let result =
            analyze_sources_with_setup(&fixture.env(), &[source], &mut models(), &[], None, None)
                .unwrap();
        assert_eq!(result.duration_s, 4.0);
        assert_eq!(result.speakers.len(), 1);
        assert!(
            result
                .segments
                .iter()
                .all(|s| s.start >= 0.0 && s.end <= 4.0)
        );
    }

    #[test]
    fn multitrack_huge_finite_offsets_are_invalid_input_without_allocation() {
        let fixture = crate::testutil::Fixture::new();
        let huge = [
            1e300,
            -1e300,
            f64::MAX,
            -f64::MAX,
            crate::manifest::MAX_SOURCE_OFFSET_S + 1.0,
            f64::NAN,
        ];
        let source = |role: TrackRole, offset_s: f64| ManifestSource {
            // Never read: the offset is rejected before decoding.
            path: fixture.input.join("missing.wav"),
            audio_track_index: None,
            role,
            person_hint: None,
            offset_s,
        };
        let short = conversation(&[(Some(0), 1.0)]);
        for offset in huge {
            let participant = [source(TrackRole::Participant, offset)];
            let err = analyze_sources_with_setup(
                &fixture.env(),
                &participant,
                &mut models(),
                &[],
                None,
                None,
            )
            .unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{offset}: {err}");
            // The mic + system pair once padded a timeline of this length.
            let pair = [
                source(TrackRole::Mic, 0.0),
                source(TrackRole::System, offset),
            ];
            let err = analyze_decoded(
                &fixture.env(),
                &pair,
                &[short.as_slice(), short.as_slice()],
                &mut models(),
                &[],
                None,
                None,
                &[],
            )
            .unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{offset}: {err}");
            let encoder = broscribe_platform::encoder::FakeEncoder::default();
            let err = encode_sources(
                &fixture.chain,
                &pair,
                &[short.clone(), short.clone()],
                &encoder,
                &fixture.input.join("never.m4a"),
            )
            .unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{offset}: {err}");
            assert!(encoder.calls.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn multitrack_offsets_within_the_limit_keep_exact_frame_alignment() {
        let max = crate::manifest::MAX_SOURCE_OFFSET_S;
        assert_eq!(
            offset_frames(max).unwrap(),
            (max * f64::from(TARGET_RATE)) as i64
        );
        assert_eq!(
            offset_frames(-max).unwrap(),
            -((max * f64::from(TARGET_RATE)) as i64)
        );
        assert_eq!(offset_frames(0.5).unwrap(), 8_000);
        assert_eq!(offset_frames(-0.25).unwrap(), -4_000);
        // (skip, start): trimmed before time zero, or placed after it.
        assert_eq!(placement(0, 3), (0, 0));
        assert_eq!(placement(-2, 3), (2, 0));
        assert_eq!(placement(-5, 3), (3, 0));
        assert_eq!(placement(7, 3), (0, 7));
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::testutil::{Fixture, conversation, models};

    #[test]
    fn mic_system_sources_reuse_recording_analysis_with_offsets_and_track_mapping() {
        let fixture = Fixture::new();
        let mic = fixture.wav("mic.wav", &[(Some(0), 3.0)]);
        let system = fixture.wav("system.wav", &[(Some(1), 4.0)]);
        let sources = [
            ManifestSource {
                path: system,
                audio_track_index: None,
                role: TrackRole::System,
                person_hint: None,
                offset_s: 0.0,
            },
            ManifestSource {
                path: mic,
                audio_track_index: None,
                role: TrackRole::Mic,
                person_hint: None,
                offset_s: 1.0,
            },
        ];
        let mut mic = vec![0.0; 16000];
        mic.extend(conversation(&[(Some(0), 3.0)]));
        // Decode the quantized synthetic PCM, exactly as the import does.
        mic[16000..].copy_from_slice(&decode_source(&fixture.chain, &sources[1]).unwrap());
        let system = decode_source(&fixture.chain, &sources[0]).unwrap();
        for setup in [
            broscribe_dsp::Setup::Headphones,
            broscribe_dsp::Setup::SpeakersMic,
            broscribe_dsp::Setup::HybridRoom,
        ] {
            let (mut expected, _) = crate::record::analyze_tracks(
                &fixture.env(),
                &mic,
                &system,
                TARGET_RATE,
                setup,
                None,
                &mut models(),
                &[],
                &[],
            )
            .unwrap();
            for speaker in &mut expected.speakers {
                speaker.track = if speaker.track == "mic" { "t1" } else { "t0" }.into();
            }
            for segment in &mut expected.segments {
                segment.track = if segment.track == "mic" { "t1" } else { "t0" }.into();
            }
            let result = analyze_sources_with_setup(
                &fixture.env(),
                &sources,
                &mut models(),
                &[],
                Some(setup),
                None,
            )
            .unwrap();
            assert_eq!(result.speakers.len(), expected.speakers.len());
            for (actual, expected) in result.speakers.iter().zip(&expected.speakers) {
                assert_eq!(
                    (
                        actual.id,
                        &actual.track,
                        actual.assignment,
                        &actual.person_id
                    ),
                    (
                        expected.id,
                        &expected.track,
                        expected.assignment,
                        &expected.person_id
                    )
                );
                assert_eq!(
                    actual.centroid.as_ref().map(|c| &c.vector_b64),
                    expected.centroid.as_ref().map(|c| &c.vector_b64)
                );
            }
            assert!(
                result
                    .segments
                    .iter()
                    .filter(|s| s.track == "t1")
                    .all(|s| s.start >= 1.0
                        && s.end <= 4.0
                        && s.words.iter().all(|w| w.start >= 1.0 && w.end <= 4.0))
            );
            // Independent mic speech survives the production analysis of the
            // padded pair itself, then the routing onto track t1.
            assert!(
                expected.segments.iter().any(|s| s.track == "t1"),
                "{setup:?}: analyze_tracks lost the independent mic speech: {:?}",
                expected.segments
            );
            assert!(
                result.segments.iter().any(|s| s.track == "t1"),
                "{setup:?}: routing lost the mic speech: {:?}",
                result.segments
            );
        }
    }

    #[test]
    fn v1_per_participant_container_import_expands_all_audio_tracks() {
        use crate::batch::{BatchOptions, import_manifest};
        let mut fixture = Fixture::new();
        fixture
            .chain
            .0
            .insert(0, Box::new(broscribe_platform::encoder::FakeEncodedDecoder));
        let file = fixture.input.join("participants.m4a");
        std::fs::write(&file, serde_json::to_vec(&serde_json::json!({
            "sample_rate":16000,
            "tracks":[{"samples":conversation(&[(Some(0), 3.0)])},{"samples":conversation(&[(Some(1), 4.0)])}]
        })).unwrap()).unwrap();
        let manifest = crate::manifest::parse(
            &serde_json::json!({
                "format":crate::manifest::FORMAT,"version":1,
                "meetings":[{"audio_path":file,"tracks":"per-participant"}]
            })
            .to_string(),
        )
        .unwrap();
        assert!(manifest.issues.is_empty());
        let summary = import_manifest(
            &fixture.env(),
            &mut models(),
            &manifest,
            &BatchOptions {
                handling: Some(broscribe_core::schema::Handling::Reference),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(summary.failed.is_empty(), "{:?}", summary.failed);
        let meeting = fixture
            .lib
            .load_meeting_at(&fixture.lib.list_meeting_dirs().unwrap()[0])
            .unwrap();
        assert_eq!(meeting.tracks.len(), 2);
        assert!(
            meeting
                .tracks
                .iter()
                .all(|t| t.role == TrackRole::Participant)
        );
        assert!(meeting.segments.iter().any(|s| s.track == "t0"));
        assert!(meeting.segments.iter().any(|s| s.track == "t1"));
        assert_eq!(meeting.recording.unwrap().sources[1].track_index, 1);
    }
}
