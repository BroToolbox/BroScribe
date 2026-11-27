//! Lossless two-track recording jobs. Inference and encoding run outside the
//! library lock; only a verified, committed final recording releases its journal.
use crate::{
    Env,
    analyze::{self, Analysis, AnalyzeOptions},
    audio::{TARGET_RATE, file_sha256},
    multitrack,
};
use broscribe_capture::{
    finalize,
    journal::{self, Cutoff, SessionState, Track as CaptureTrack},
    timeline::{Timeline, TimelineOptions},
};
use broscribe_core::queue::{JobState, LeaseHandle};
use broscribe_core::schema::{
    self, Assignment, Bookmark, Handling, LanguageSpan, Meeting, Note, ProcessingStatus, Source,
    Track, TrackRole,
};
use broscribe_core::{Error, Library, MeetingId, Result, SpeakerId};
use broscribe_core::{index::Index, ops};
use broscribe_dsp::{
    DuplicateConfig, EchoConfig, EchoHandling, EchoReport, MicDecision, MicSegment, Setup,
    SystemSegment,
};
use broscribe_models::ModelSet;
use broscribe_platform::{AudioEncoder, CalendarAuthority};

/// The daemon/worker boundary uses the canonical, ML-independent IPC schema.
pub use broscribe_ipc::RecordJobInput as RecordInput;

/// The Me evidence one analysis uses: the self person and their confirmed
/// voiceprints of the embedding model (minus samples of `held_out`
/// meetings). It is snapshot before the analysis, and a publication checks
/// under the library lock that nothing it relied on was revoked since
/// ([`Self::still_current`]): a voice or person forgotten while inference or
/// encoding ran never comes back through the Me naming, the mic centroid, the
/// weak samples that naming teaches, or mic speech dropped as echo of a voice
/// verified against it. Such a pass is analysed again instead.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MeEvidence {
    model: String,
    held_out: Vec<MeetingId>,
    person: Option<broscribe_core::PersonId>,
    prints: Vec<schema::Voiceprint>,
}

impl MeEvidence {
    /// The self person and their confirmed prints of `model` as stored now.
    pub(crate) fn load(
        lib: &broscribe_core::Library,
        model: &str,
        held_out: &[MeetingId],
    ) -> Result<Self> {
        let person = lib
            .list_people()?
            .into_iter()
            .find(|p| p.is_self)
            .map(|p| p.id);
        let prints = match &person {
            Some(id) => lib
                .load_voiceprints(id)?
                .embeddings
                .into_iter()
                .filter(|v| v.model == model && v.strength == schema::Strength::Confirmed)
                .filter(|v| {
                    v.source
                        .meeting_id
                        .as_ref()
                        .is_none_or(|id| !held_out.contains(id))
                })
                .collect(),
            None => Vec::new(),
        };
        Ok(Self {
            model: model.to_owned(),
            held_out: held_out.to_vec(),
            person,
            prints,
        })
    }

    /// Whether the self person is still the same one and every print this
    /// evidence holds is still stored, unchanged. Prints added since only
    /// add to what was known, so they keep the evidence current.
    pub(crate) fn still_current(&self, lib: &broscribe_core::Library) -> Result<bool> {
        let now = Self::load(lib, &self.model, &self.held_out)?;
        Ok(now.person == self.person && self.prints.iter().all(|p| now.prints.contains(p)))
    }
}

/// Analyze already aligned common-rate tracks. No disk mutation or library lock.
/// Me's confirmed voiceprints derived from a `held_out` meeting are ignored,
/// so a benchmark never verifies Me with samples of the audio it evaluates;
/// production passes `&[]`.
#[allow(clippy::too_many_arguments)] // Explicit track inputs keep this public analysis boundary allocation-free.
pub fn analyze_tracks(
    env: &Env<'_>,
    mic: &[f32],
    system: &[f32],
    rate: u32,
    setup: Setup,
    handling: Option<EchoHandling>,
    models: &mut ModelSet,
    languages: &[LanguageSpan],
    held_out: &[MeetingId],
) -> Result<(Analysis, EchoReport)> {
    let me = MeEvidence::load(env.lib, models.embedder.model_id(), held_out)?;
    analyze_tracks_with(
        env, mic, system, rate, setup, handling, models, languages, &me,
    )
}

/// [`analyze_tracks`] with Me evidence the caller snapshot before the
/// analysis, so its publication can check it ([`MeEvidence::still_current`]).
#[allow(clippy::too_many_arguments)] // Mirrors `analyze_tracks`' explicit inputs.
pub(crate) fn analyze_tracks_with(
    env: &Env<'_>,
    mic: &[f32],
    system: &[f32],
    rate: u32,
    setup: Setup,
    handling: Option<EchoHandling>,
    models: &mut ModelSet,
    languages: &[LanguageSpan],
    me: &MeEvidence,
) -> Result<(Analysis, EchoReport)> {
    if me.model != models.embedder.model_id() {
        return Err(Error::Internal(format!(
            "Me evidence of {} used with {}",
            me.model,
            models.embedder.model_id()
        )));
    }
    let echo = broscribe_dsp::process_echo(
        mic,
        system,
        rate,
        handling.unwrap_or_else(|| EchoHandling::for_setup(setup)),
        &EchoConfig::default(),
    )
    .map_err(|e| Error::Inference(e.to_string()))?;
    // An untouched mic at the model rate stays borrowed: no whole-track copy.
    let mic: std::borrow::Cow<'_, [f32]> = if rate == TARGET_RATE {
        echo.mic
    } else {
        std::borrow::Cow::Owned(multitrack::resample_slice(&echo.mic, rate))
    };
    let system = if rate == TARGET_RATE {
        std::borrow::Cow::Borrowed(system)
    } else {
        std::borrow::Cow::Owned(multitrack::resample_slice(system, rate))
    };
    // A non-room mic is one voice; its speakers are built below from the
    // scoring embeddings, so its transcription embeds nothing.
    let mut mic_analysis = if setup.is_room() {
        analyze::analyze_with_languages(&mic, models, AnalyzeOptions::default(), languages)?
    } else {
        multitrack::one_voice_transcript(&mic, models, languages)?
    };
    let system_analysis =
        analyze::analyze_with_languages(&system, models, AnalyzeOptions::default(), languages)?;
    let model = models.embedder.model_id();
    let me_vectors = me
        .prints
        .iter()
        .map(schema::Voiceprint::vector)
        .collect::<Result<Vec<_>>>()?;
    let threshold = env.thresholds(model).and_then(|t| t.auto_name);
    // No calibration means no destructive voice decisions and no inferred Me.
    let enrolled = !me_vectors.is_empty() && threshold.is_some();
    let remote_vectors: std::collections::BTreeMap<SpeakerId, Vec<f32>> = system_analysis
        .speakers
        .iter()
        .filter_map(|s| s.centroid.as_ref().map(|c| (s.id, c)))
        .map(|(id, c)| Ok((id, c.vector()?)))
        .collect::<Result<_>>()?;
    // Remote speech on the mic's timeline, searched in time order.
    let remote_spans = IntervalIndex::new(
        system_analysis
            .segments
            .iter()
            .filter_map(|r| {
                let speaker = r.speaker?;
                let start = r.start + echo.report.alignment.lag_s_at(r.start);
                let end = r.end + echo.report.alignment.lag_s_at(r.end);
                Some((start, end, speaker))
            })
            .collect(),
    );
    let mut scored = Vec::with_capacity(mic_analysis.segments.len());
    // Each scored embedding with the exact clip it embedded, for reuse.
    let mut embedded = Vec::with_capacity(mic_analysis.segments.len());
    for segment in &mic_analysis.segments {
        let (_, clip) = embedded_clip(mic.len(), segment.start, segment.end);
        let embedding = if segment.speech_overlap.is_empty()
            && segment.end - segment.start >= analyze::MIN_CLEAN_S
        {
            models.embedder.embed(&mic[clip.clone()])?
        } else {
            None
        };
        let me_score = embedding.as_ref().and_then(|v| {
            me_vectors
                .iter()
                .map(|m| multitrack::cosine(v, m))
                .reduce(f32::max)
        });
        let remote_score = embedding.as_ref().map(|v| {
            let near: std::collections::BTreeSet<SpeakerId> = remote_spans
                .overlapping(segment.start - 0.5, segment.end + 0.5)
                .map(|&(_, _, id)| id)
                .collect();
            near.iter()
                .filter_map(|id| remote_vectors.get(id))
                .map(|remote| multitrack::cosine(v, remote))
                .fold(-1.0, f32::max)
        });
        scored.push(MicSegment {
            start_s: segment.start,
            end_s: segment.end,
            text: segment.text.clone(),
            me_score,
            best_remote_score: remote_score,
        });
        embedded.push(Some(ScoredEmbedding {
            clip,
            vector: embedding,
        }));
    }
    let remote: Vec<_> = system_analysis
        .segments
        .iter()
        .map(|s| SystemSegment {
            start_s: s.start + echo.report.alignment.lag_s_at(s.start),
            end_s: s.end + echo.report.alignment.lag_s_at(s.end),
            text: s.text.clone(),
        })
        .collect();
    let decisions = broscribe_dsp::classify_mic_segments(
        &scored,
        &remote,
        enrolled,
        setup,
        &DuplicateConfig::new(threshold.unwrap_or(1.0)),
    );
    // Separate verified Me and unknown mic speech even when the non-room mic
    // was transcribed as one person. Unknown speech remains in the review queue.
    let mut speakers = Vec::new();
    let mut kept = Vec::new();
    let mut kept_embeddings = Vec::new();
    for (((mut segment, score), decision), cached) in mic_analysis
        .segments
        .into_iter()
        .zip(scored)
        .zip(decisions)
        .zip(embedded)
    {
        if decision == MicDecision::DropEcho {
            continue;
        }
        if !setup.is_room() {
            let is_me = enrolled
                && decision == MicDecision::Keep
                && (segment.end - segment.start < analyze::MIN_CLEAN_S
                    || score
                        .me_score
                        .is_some_and(|v| v >= threshold.unwrap_or(1.0)));
            let id = SpeakerId::new(if is_me { 1 } else { 2 })?;
            segment.speaker = Some(id);
            if !speakers.iter().any(|s: &schema::Speaker| s.id == id) {
                let mut speaker = schema::Speaker::new(id, "mic");
                if is_me {
                    speaker.person_id.clone_from(&me.person);
                    speaker.assignment = Assignment::Auto;
                    speaker.score = score.me_score;
                }
                speakers.push(speaker);
            }
            if let Some(s) = speakers.iter_mut().find(|s| s.id == id) {
                s.talk_s += segment.end - segment.start;
            }
        }
        kept.push(segment);
        kept_embeddings.push(cached);
    }
    for speaker in &mut speakers {
        let mut ranges = Vec::new();
        for (segment, cached) in kept.iter().zip(kept_embeddings.iter_mut()) {
            if segment.speaker == Some(speaker.id)
                && segment.end - segment.start >= analyze::MIN_CLEAN_S
            {
                ranges.push(((segment.start, segment.end), cached.take()));
            }
        }
        speaker.centroid = mic_centroid(&mic, ranges, models.embedder.as_ref())?;
    }
    mic_analysis.segments = kept;
    if !setup.is_room() {
        mic_analysis.speakers = speakers;
        mic_analysis.diar_segments = mic_analysis
            .segments
            .iter()
            .filter_map(|segment| {
                segment.speaker.map(|speaker| analyze::Turn {
                    start: segment.start,
                    end: segment.end,
                    speaker,
                })
            })
            .collect();
    }
    let mut merged = multitrack::empty();
    multitrack::append(&mut merged, mic_analysis, "mic", 0.0)?;
    multitrack::append(&mut merged, system_analysis, "system", 0.0)?;
    Ok((merged, echo.report))
}

/// The embedding a mic segment was scored with and the exact sample range
/// ([`embedded_clip`]) it embedded.
struct ScoredEmbedding {
    clip: std::ops::Range<usize>,
    vector: Option<Vec<f32>>,
}

/// The range `analyze::embed_ranges` (and so [`analyze::centroid_of`])
/// embeds for `start_s..end_s` of a track of `len` samples: the times
/// clamped to the track, then truncated to samples. Mic scoring embeds
/// exactly this clip, so the centroid can reuse its embedding.
fn embedded_clip(
    len: usize,
    start_s: f64,
    end_s: f64,
) -> (crate::ranges::Range, std::ops::Range<usize>) {
    let rate = f64::from(TARGET_RATE);
    let duration = len as f64 / rate;
    let a = start_s.max(0.0).min(duration);
    let b = end_s.max(a).min(duration);
    let first = ((a * rate) as usize).min(len);
    let last = ((b * rate) as usize).min(len);
    ((a, b), first..last)
}

/// [`analyze::centroid_of`] over mic `ranges`, reusing a range's scoring
/// embedding only when it embedded exactly the clip the centroid embeds, in
/// one piece (the embedder's input limit does not split it); every other
/// range is embedded as `centroid_of` would.
fn mic_centroid(
    mic: &[f32],
    ranges: Vec<(crate::ranges::Range, Option<ScoredEmbedding>)>,
    embedder: &dyn broscribe_models::SpeakerEmbedder,
) -> Result<Option<schema::Centroid>> {
    let limit = embedder.max_input_samples().unwrap_or(usize::MAX);
    let mut parts = Vec::with_capacity(ranges.len());
    for ((start, end), cached) in ranges {
        let ((a, b), clip) = embedded_clip(mic.len(), start, end);
        if clip.is_empty() {
            continue;
        }
        let fits = clip.len() <= limit;
        match cached.filter(|c| c.clip == clip && fits) {
            Some(ScoredEmbedding {
                vector: Some(vector),
                ..
            }) => {
                if vector.len() != embedder.dim() {
                    return Err(Error::Inference(format!(
                        "{} returned {} values, expected {}",
                        embedder.model_id(),
                        vector.len(),
                        embedder.dim()
                    )));
                }
                parts.push(((a, b), vector));
            }
            // The same clip embedded to nothing: so would it again.
            Some(ScoredEmbedding { vector: None, .. }) => {}
            None => parts.extend(analyze::embed_ranges(mic, &[(start, end)], embedder)?),
        }
    }
    let (ranges, vectors): (Vec<_>, Vec<_>) = parts.into_iter().unzip();
    Ok(
        broscribe_core::identity::mean_normalized(&vectors).map(|c| {
            let mut centroid = schema::Centroid::new(
                embedder.model_id(),
                &c,
                u32::try_from(vectors.len()).unwrap_or(u32::MAX),
            );
            centroid.ranges = ranges
                .into_iter()
                .map(|(a, b)| [a as f32, b as f32])
                .collect();
            centroid
        }),
    )
}

/// Intervals `(start, end, value)` sorted by start, with the running
/// maximum end, so the ones overlapping a range are found by binary search
/// instead of a scan per query.
struct IntervalIndex<T> {
    items: Vec<(f64, f64, T)>,
    max_end: Vec<f64>,
}

impl<T> IntervalIndex<T> {
    fn new(mut items: Vec<(f64, f64, T)>) -> Self {
        items.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut running = f64::NEG_INFINITY;
        let max_end = items
            .iter()
            .map(|(_, end, _)| {
                running = running.max(*end);
                running
            })
            .collect();
        Self { items, max_end }
    }

    /// Items with `start <= to` and `end >= from`.
    fn overlapping(&self, from: f64, to: f64) -> impl Iterator<Item = &(f64, f64, T)> {
        let lo = self.max_end.partition_point(|&end| end < from);
        let hi = self.items.partition_point(|(start, _, _)| *start <= to);
        self.items[lo..hi.max(lo)]
            .iter()
            .filter(move |(_, end, _)| *end >= from)
    }
}

/// Execute a claimed recording job, including publication and final cleanup.
/// `calendar_authority` is asked for the live system-calendar opt-in only when
/// the session's calendar snapshot holds native events.
pub fn run(
    env: &Env<'_>,
    models: &mut ModelSet,
    lease: &LeaseHandle,
    input: &RecordInput,
    provisional: bool,
    encoder: &dyn AudioEncoder,
    calendar_authority: &dyn CalendarAuthority,
) -> Result<MeetingId> {
    if provisional != input.cutoff.is_some() {
        return Err(Error::InvalidInput(
            "provisional recording requires cutoff; final recording forbids cutoff".into(),
        ));
    }
    let source_id = format!("recording:{}", input.session_id);
    let manifest = load_record_journal(env.lib, input);
    let started = manifest.as_ref().ok().map(|m| m.started_at);
    let existing = session_meeting(env.lib, &source_id, started)?;
    if let Some(meeting) = existing
        .as_ref()
        .filter(|m| m.processing.status == ProcessingStatus::Done)
    {
        if !provisional && input.journal_dir.exists() {
            manifest?;
            remove_record_journal(env.lib, input)?;
        }
        lease.commit(env.lib, JobState::Done, |tx, _| {
            if !provisional {
                broscribe_core::live_identity::remove_sidecar(tx.root(), &input.session_id)?;
            }
            Ok(())
        })?;
        return Ok(meeting.id.clone());
    }
    // A discarded session (or one whose journal is gone) publishes nothing;
    // checked again under the lock of the commit below.
    broscribe_core::lifecycle::check_publication(&input.journal_dir, &input.session_id)?;
    let manifest = manifest?;
    if !provisional && manifest.state != SessionState::Stopped {
        return Err(Error::InvalidInput(
            "final recording journal is still active".into(),
        ));
    }
    let cutoff = input.cutoff.as_ref().map(|c| Cutoff {
        mic: c.chunks.get(&broscribe_ipc::Track::Mic).copied(),
        system: c.chunks.get(&broscribe_ipc::Track::System).copied(),
    });
    let timeline = Timeline::plan(
        &input.journal_dir,
        &manifest,
        &TimelineOptions {
            cutoff,
            ..Default::default()
        },
    )?;
    if timeline.frames == 0 {
        let id = existing
            .as_ref()
            .map(|m| m.id.clone())
            .unwrap_or(env.lib.new_meeting_id()?);
        lease.commit(env.lib, JobState::Done, |_, _| Ok(()))?;
        if !provisional {
            remove_record_journal(env.lib, input)?;
        }
        return Ok(id);
    }
    let duration = timeline.duration_s();
    let mut languages = Vec::new();
    let mut changes = manifest.languages.clone();
    changes.sort_by_key(|l| l.at_ns);
    let mut from = 0.0;
    let mut language = manifest
        .meta
        .language
        .clone()
        .or_else(|| env.config.default_language.clone());
    for change in changes {
        let at = timeline.time_map.recording_s(change.at_ns).min(duration);
        if at > from
            && let Some(lang) = language.take()
        {
            languages.push(LanguageSpan {
                lang,
                from_s: from,
                to_s: at,
            });
        }
        from = at;
        language = Some(change.language);
    }
    if let Some(lang) = language
        && duration > from
    {
        languages.push(LanguageSpan {
            lang,
            from_s: from,
            to_s: duration,
        });
    }
    // Alignment, echo handling, ASR and embeddings read a 16 kHz timeline;
    // the device-rate one is only streamed to the encoder, never held whole.
    let analysis_timeline = plan_analysis_timeline(&input.journal_dir, &manifest, cutoff)?;
    // The recording's wall-clock interval: its stop, or (still recording)
    // the audio this pass covers.
    let recording_end = manifest.stopped_at.unwrap_or_else(|| {
        manifest.started_at + chrono::TimeDelta::milliseconds((duration * 1000.0).round() as i64)
    });
    let calendar = cached_calendar(
        &input.journal_dir,
        manifest.started_at,
        recording_end.max(manifest.started_at),
        manifest.meta.app.as_deref(),
        calendar_authority,
    )?;
    let mut hotwords = env.config.hotwords.clone();
    if let Some(event) = calendar.as_ref().and_then(|events| events.first()) {
        hotwords.extend(event.attendees.iter().filter_map(|a| a.name.clone()));
    }
    if models.asr.accepts_hotwords() {
        models.asr.set_hotwords(&hotwords)?;
    }
    let id = match &existing {
        Some(m) => m.id.clone(),
        None => job_meeting_id(env, lease)?,
    };
    // Encoded audio belongs to this attempt (its lease token): a holder that
    // lost the job never overwrites or removes another attempt's file.
    let encoded = crate::import::staged_audio_path(lease, encoder.extension());
    let pass = Pass {
        input,
        provisional,
        manifest: &manifest,
        timeline: &timeline,
        analysis_timeline: &analysis_timeline,
        languages: &languages,
        duration,
        calendar: calendar.as_deref(),
        id: &id,
        source_id: &source_id,
        encoded: &encoded,
    };
    // Encoded and verified once; a pass analysed again reuses it.
    let mut recording = None;
    let mut passes = 0;
    let published = loop {
        passes += 1;
        let outcome = publish_pass(
            env,
            models,
            lease,
            encoder,
            calendar_authority,
            &pass,
            &mut recording,
        );
        let error = match outcome {
            Ok(Some(id)) => break id,
            Ok(None) if passes < ME_EVIDENCE_PASSES => {
                tracing::info!("Me evidence changed during analysis; analysing again");
                continue;
            }
            Ok(None) => me_evidence_changed("recording analysis"),
            Err(error) => error,
        };
        // Never leave encoded meeting audio behind in the job folder: a
        // retry encodes again, and a discarded session must keep none.
        if !provisional {
            let _ = std::fs::remove_file(&encoded);
        }
        return Err(error);
    };
    if !provisional {
        remove_record_journal(env.lib, input)?;
    }
    lease.commit(env.lib, JobState::Done, |_, _| Ok(()))?;
    Ok(published)
}

fn load_record_journal(lib: &Library, input: &RecordInput) -> Result<journal::SessionManifest> {
    let manifest = journal::load_manifest(&input.journal_dir)?;
    if manifest.session_id != input.session_id {
        return Err(Error::InvalidInput(
            "recording session id differs from journal".into(),
        ));
    }
    let root = lib
        .root()
        .canonicalize()
        .map_err(|e| Error::io_at(lib.root(), e))?;
    journal::check_library(&manifest, &root)?;
    Ok(manifest)
}

fn remove_record_journal(lib: &Library, input: &RecordInput) -> Result<()> {
    load_record_journal(lib, input)?;
    journal::remove(&input.journal_dir)
}

/// Analysis passes of one job before it gives up because the Me evidence
/// each one used ([`MeEvidence`]) was revoked before it could publish.
pub(crate) const ME_EVIDENCE_PASSES: usize = 3;

/// The error of a job that ran out of [`ME_EVIDENCE_PASSES`]: Me's voice or
/// identity kept changing during `what`. Retryable; nothing was published.
pub(crate) fn me_evidence_changed(what: &str) -> Error {
    Error::Service {
        code: "me_evidence_changed",
        message: format!("Me's voice or identity kept changing during {what}; retry"),
    }
}

/// What every analysis pass of one record job shares.
#[derive(Clone, Copy)]
struct Pass<'a> {
    input: &'a RecordInput,
    provisional: bool,
    manifest: &'a journal::SessionManifest,
    /// The device-rate timeline the encoder streams.
    timeline: &'a Timeline,
    analysis_timeline: &'a Timeline,
    languages: &'a [LanguageSpan],
    duration: f64,
    calendar: Option<&'a [broscribe_integrations::CalendarEvent]>,
    id: &'a MeetingId,
    source_id: &'a str,
    /// This attempt's encoded audio ([`crate::import::staged_audio_path`]).
    encoded: &'a std::path::Path,
}

/// One analysis and publication of a record job, with the Me evidence
/// snapshot before its analysis ([`MeEvidence`]). `None`: that evidence was
/// revoked before publication, which published nothing; analyse again.
/// `recording` is the final pass' verified encoding, made by the first pass
/// that gets that far and reused by later ones.
fn publish_pass(
    env: &Env<'_>,
    models: &mut ModelSet,
    lease: &LeaseHandle,
    encoder: &dyn AudioEncoder,
    calendar_authority: &dyn CalendarAuthority,
    pass: &Pass<'_>,
    recording: &mut Option<schema::Recording>,
) -> Result<Option<MeetingId>> {
    let Pass {
        input,
        provisional,
        manifest,
        timeline,
        analysis_timeline,
        languages,
        duration,
        calendar,
        id,
        source_id,
        encoded,
    } = *pass;
    let model = models.embedder.model_id().to_owned();
    let me = MeEvidence::load(env.lib, &model, &[])?;
    let mic = analysis_timeline.read_track(CaptureTrack::Mic)?;
    let system = analysis_timeline.read_track(CaptureTrack::System)?;
    let setup = crate::setup_final::resolve_final_setup(
        &manifest.meta,
        &system,
        analysis_timeline.sample_rate,
    )?;
    let (analysis, echo) = analyze_tracks_with(
        env,
        &mic,
        &system,
        analysis_timeline.sample_rate,
        setup,
        session_echo_handling(&manifest.meta),
        models,
        languages,
        &me,
    )?;
    let mut meeting = Meeting::new(
        id.clone(),
        Source::Recording,
        manifest.started_at.fixed_offset(),
    );
    meeting.source_id = Some(source_id.to_owned());
    meeting.title = manifest.meta.title.clone();
    // The start title has no edit timestamp, like a legacy User title.
    // A later explicit edit wins; an earlier pass's legacy rename is kept.
    // Detected window titles remain replaceable by the calendar.
    if meeting.title.is_some() && !manifest.meta.title_detected {
        meeting.title_source = Some(schema::TitleSource::User);
    } else {
        meeting.title_source = Some(schema::TitleSource::File);
    }
    meeting.app = manifest.meta.app.clone();
    meeting.setup = Some(setup.to_string());
    for tag in &manifest.meta.tags {
        meeting.add_tag(tag);
    }
    meeting.languages = languages.to_vec();
    meeting.duration_s = duration;
    meeting.bookmarks = manifest
        .bookmarks
        .iter()
        .filter_map(|b| {
            let at_s = timeline.time_map.recording_s(b.at_ns);
            (at_s <= duration).then(|| Bookmark {
                at_s,
                label: b.label.clone(),
            })
        })
        .collect();
    meeting.notes = manifest
        .notes
        .iter()
        .filter_map(|n| {
            let at_s = timeline.time_map.recording_s(n.at_ns);
            (at_s <= duration).then(|| Note {
                at_s,
                text: n.text.clone(),
            })
        })
        .collect();
    meeting.tracks = vec![
        Track {
            id: "mic".into(),
            role: TrackRole::Mic,
        },
        Track {
            id: "system".into(),
            role: TrackRole::System,
        },
    ];
    meeting.segments = analysis.segments;
    meeting.speakers = analysis.speakers;
    let (roster, caption_names) = reader_evidence(&input.journal_dir, &meeting, setup)?;
    // Caption samples come from the agreed clean speech, embedded here,
    // outside the library lock, from the tracks the recording stores.
    let caption_embeddings = crate::caption_replay::embed_caption_ranges(
        &meeting,
        &caption_names,
        models.embedder.as_ref(),
        |track| {
            Ok(match track {
                "mic" => Some((std::borrow::Cow::Borrowed(mic.as_slice()), 0.0)),
                "system" => Some((std::borrow::Cow::Borrowed(system.as_slice()), 0.0)),
                _ => None,
            })
        },
    )?;
    drop((mic, system));
    let known_people = env.lib.list_people()?;
    let removed: Vec<_> = manifest
        .participants
        .iter()
        .filter(|p| p.removed)
        .map(|p| p.person_ref.as_str())
        .collect();
    for participant in &manifest.participants {
        if participant.removed {
            continue;
        }
        let person_id = broscribe_core::PersonId::parse(&participant.person_ref)?;
        let known = known_people.iter().find(|p| p.id == person_id);
        meeting.participants.push(schema::Participant {
            name: participant
                .name
                .clone()
                .or_else(|| known.map(|p| p.name.clone())),
            email: participant
                .email
                .clone()
                .or_else(|| known.and_then(|p| p.emails.first().cloned())),
            person_id: Some(person_id),
            is_self: known.is_some_and(|p| p.is_self),
            evidence: vec!["user".into()],
        });
    }
    for member in &roster {
        let matched = crate::manifest::match_person(
            &known_people,
            Some(&member.name),
            member.email.as_deref(),
        );
        if matched.is_some_and(|p| removed.contains(&p.id.as_str())) {
            continue;
        }
        if meeting.participants.iter().any(|p| {
            p.person_id
                .as_ref()
                .is_some_and(|id| matched.is_some_and(|known| known.id == *id))
        }) {
            continue;
        }
        meeting.participants.push(schema::Participant {
            name: Some(member.name.clone()),
            email: member.email.clone(),
            person_id: matched.map(|p| p.id.clone()),
            is_self: member.is_self,
            evidence: vec!["roster".into()],
        });
    }
    meeting.models = crate::import::models_used(env, &model, true);
    meeting.processing.status = if provisional {
        ProcessingStatus::Provisional
    } else {
        ProcessingStatus::Done
    };
    meeting.processing.echo_report = Some(
        serde_json::to_value(echo)
            .map_err(|e| Error::Internal(format!("serialize echo report: {e}")))?,
    );
    if !provisional {
        if recording.is_none() {
            *recording = Some(encode_final(env, timeline, encoded, encoder)?);
        }
        meeting.recording.clone_from(recording);
    }
    // Inference may have used attendee hotwords: if consent changed while it
    // ran, retry without that prior instead of publishing derived metadata.
    // Query the helper outside the library lock, immediately before commit.
    if let Some(events) = calendar {
        check_calendar_authorities(events, calendar_authority)?;
        events[0].apply_to(&mut meeting);
        link_calendar_attendees(&mut meeting, &known_people, &removed);
    }
    let thresholds = env.thresholds(&model).copied();
    lease.commit(env.lib, JobState::Running, |tx, job_dir| {
        // The discard fence, under the same lock a discard checks: a session
        // discarded meanwhile (or whose journal is gone) publishes nothing.
        broscribe_core::lifecycle::check_publication(&input.journal_dir, &input.session_id)?;
        let current_journal = load_record_journal(tx.library(), input)?;
        // The Me fence, before anything is written: a voice or person
        // forgotten since the analysis snapshot means its Me naming, mic
        // centroid and echo decisions are stale. Publish nothing; analyse again.
        if !me.still_current(tx.library())? {
            return Ok(None);
        }
        // Reconcile under the lock: concurrent process-now inference must
        // neither regress a final transcript nor create another meeting.
        if let Some(current) = session_meeting(tx.library(), source_id, Some(manifest.started_at))?
        {
            if current.processing.status == ProcessingStatus::Done
                || (provisional && current.duration_s > meeting.duration_s)
            {
                return Ok(Some(current.id));
            }
            carry_review(tx, &current, &mut meeting)?;
        }
        // A merge can retire a profile before or during this pass. Keep
        // explicit attendance linked to the surviving profile when possible.
        for participant in &mut meeting.participants {
            let Some(id) = participant
                .person_id
                .as_ref()
                .filter(|id| !tx.library().person_dir(id).is_dir())
            else {
                continue;
            };
            if let Some(person) = ops::resolve_merged_person(tx.library(), id)? {
                participant.name.get_or_insert(person.name);
                participant.is_self = person.is_self;
                participant.person_id = Some(person.id);
            } else {
                participant.is_self = false;
                participant.person_id = None;
            }
        }
        // The user's latest title for the session, read under the lock that
        // every title edit takes: the newest explicit edit wins.
        if let Some(title) = broscribe_core::lifecycle::read_session_title(&input.journal_dir)? {
            meeting.offer_user_title(&title.title, title.set_at);
        }
        // Tag edits reach the journal before the library: reconcile this
        // job's tags with the journal as it is now, not as the job read it
        // at start, so a tag removed while inference ran stays removed (in
        // `tags_removed`, which later passes keep). Only tags of the
        // session are touched; tags set on the meeting itself stay.
        let fold = |t: &str| t.trim().to_lowercase();
        for tag in &manifest.meta.tags {
            if !current_journal
                .meta
                .tags
                .iter()
                .any(|t| fold(t) == fold(tag))
            {
                meeting.remove_tag(tag);
            }
        }
        for tag in &current_journal.meta.tags {
            if !manifest.meta.tags.iter().any(|t| fold(t) == fold(tag)) {
                meeting.add_tag(tag);
            }
        }
        crate::live_identity::apply_session_decisions(
            tx,
            &mut meeting,
            &input.session_id,
            analysis_timeline,
        )?;
        // The meeting folder is created and the audio moved into it only
        // right before `meeting.json` is saved (below).
        // Resolve remote and room speakers through the regular identity resolver.
        // Non-room mic labels are segment-verified above and must not be replaced
        // by a track-wide centroid or inferred name for Unknown (mic).
        let mic_speakers: Vec<_> = if setup.is_room() {
            Vec::new()
        } else {
            meeting
                .speakers
                .iter()
                .filter(|s| s.track == "mic")
                .cloned()
                .collect()
        };
        let before: Vec<_> = meeting
            .speakers
            .iter()
            .map(|s| (s.id, ops::SpeakerState::of(s)))
            .collect();
        let people = ops::load_person_samples(tx.library(), &model)?;
        let roster_people = meeting.roster_person_ids();
        ops::resolve_meeting(
            &mut meeting,
            &model,
            &people,
            thresholds.as_ref(),
            Some(&roster_people),
        )?;
        for speaker in mic_speakers {
            if let Some(s) = meeting.speaker_mut(speaker.id) {
                *s = speaker;
            }
        }
        let mut index = Index::open(tx.root())?;
        let caption_names = crate::caption_replay::drop_forgotten(
            tx.library(),
            &tx.library().list_people()?,
            &meeting,
            &roster,
            &caption_names,
        )?;
        apply_caption_names(
            tx,
            &mut index,
            &mut meeting,
            None,
            &roster,
            &caption_names,
            &caption_embeddings,
        )?;
        ops::guard_automatic_names(&mut meeting);
        // The final pass groups over-split voices once names are known
        // (caption-proven speakers count as confirmed), before weak samples
        // are synced, so a grouped member teaches no weak sample.
        if !provisional {
            ops::reconcile_live_speakers(tx.library(), &mut meeting, &env.config.live)?;
            let proven = ops::caption_proven(tx.library(), &meeting)?;
            ops::apply_auto_groups(&mut meeting, &model, thresholds.as_ref(), &proven)?;
            // A kept speaker never stays named as someone a member rejected.
            ops::resolve_rejected_kept(
                &mut meeting,
                &model,
                &people,
                thresholds.as_ref(),
                Some(&roster_people),
            )?;
        }
        if !provisional {
            ops::guard_automatic_names(&mut meeting);
            ops::reconcile_live_speakers(tx.library(), &mut meeting, &env.config.live)?;
        }
        ops::refresh_identity_conflict_notes(tx.library(), &mut meeting)?;
        let changes = crate::caption_replay::speaker_changes(&meeting, before);
        ops::sync_weak_samples(tx, &meeting, &changes)?;
        let dir = if tx.library().meeting_exists(&meeting.id)? {
            tx.library().meeting_dir(&meeting.id)?
        } else {
            unpublished_dir(tx, &meeting)?
        };
        if !provisional {
            std::fs::rename(encoded, dir.join(format!("audio.{}", encoder.extension())))
                .map_err(|e| Error::io_at(encoded, e))?;
            // This attempt's audio is in place; what crashed or superseded
            // attempts left goes now, while this holder owns the job.
            remove_stale_stages(job_dir)?;
            for name in ["roster.json", "captions.jsonl"] {
                let source = input.journal_dir.join(name);
                if source.exists() {
                    std::fs::copy(&source, dir.join(name)).map_err(|e| Error::io_at(&source, e))?;
                }
            }
            crate::batch::failpoint(crate::batch::FailPoint::RecordAfterRename)?;
            ops::sync_live_promotions(tx, None, &meeting, &model)?;
        }
        tx.save_meeting(&meeting)?;
        // The meeting holds the decisions now; the sidecar goes with the
        // final commit (a provisional pass leaves it for the final one).
        if !provisional {
            broscribe_core::live_identity::remove_sidecar(tx.root(), &input.session_id)?;
        }
        index.upsert_meeting(tx, &meeting)?;
        Ok(Some(meeting.id.clone()))
    })
}

/// The recording meeting of session `source_id`. An unreadable meeting file
/// of another meeting is skipped; one whose folder starts with the session's
/// start minute (`started`) may be this session's own meeting, so the lookup
/// fails rather than publishing a second meeting for the session.
fn session_meeting(
    lib: &broscribe_core::Library,
    source_id: &str,
    started: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<Option<Meeting>> {
    let prefix = started.map(|t| t.format("%Y-%m-%d-%H%M").to_string());
    crate::finish::find_meeting(
        lib,
        |m| m.source == Source::Recording && m.source_id.as_deref() == Some(source_id),
        |dir| {
            Ok(prefix.as_deref().is_some_and(|prefix| {
                dir.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
            }))
        },
    )
}

/// The folder of unpublished `meeting` (its ID persisted by the job): one an
/// attempt that failed before saving `meeting.json` left is found by the
/// stable ID and start minute, also after a title change, and renamed to the
/// current title's name (which [`broscribe_core::WriteTx::save_meeting`]
/// expects); otherwise a new folder. Several such folders are refused.
fn unpublished_dir(
    tx: &mut broscribe_core::WriteTx<'_>,
    meeting: &Meeting,
) -> Result<std::path::PathBuf> {
    let started = meeting.started_at.format("%Y-%m-%d-%H%M").to_string();
    let mut left = broscribe_core::lifecycle::unpublished_meeting_dirs(
        tx.root(),
        &meeting.id,
        Some(&started),
    )?;
    match left.len() {
        0 => tx.create_meeting_dir(meeting),
        1 => {
            let left = left.remove(0);
            let named = tx.root().join(broscribe_core::paths::meeting_rel_dir(
                &meeting.started_at,
                meeting.title.as_deref(),
                &meeting.id,
            ));
            if left != named {
                std::fs::rename(&left, &named).map_err(|e| Error::io_at(&left, e))?;
                let parent = named
                    .parent()
                    .ok_or_else(|| Error::Internal("the meeting folder has no parent".into()))?;
                broscribe_core::fsutil::sync_dir(parent)?;
            }
            Ok(named)
        }
        _ => Err(Error::InvalidInput(format!(
            "meeting {} has several unpublished folders; keep one and remove the others",
            meeting.id
        ))),
    }
}

/// Encodes the final recording to this attempt's `encoded` path and verifies
/// it. A file that fails encoding or verification is removed, never left for
/// a retry or a publication.
fn encode_final(
    env: &Env<'_>,
    timeline: &Timeline,
    encoded: &std::path::Path,
    encoder: &dyn AudioEncoder,
) -> Result<schema::Recording> {
    match std::fs::remove_file(encoded) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io_at(encoded, e)),
    }
    let made = (|| -> Result<(String, u64)> {
        let report = finalize::encode(timeline, encoded, encoder)?;
        finalize::verify(env.decoders, &report)?;
        file_sha256(encoded)
    })();
    match made {
        Ok((sha, bytes)) => Ok(crate::import::recording(
            encoded,
            &sha,
            bytes,
            Handling::Copy,
            encoder.extension(),
        )),
        Err(error) => {
            let _ = std::fs::remove_file(encoded);
            Err(error)
        }
    }
}

/// Removes the encoded audio (`audio.*`) other attempts of this record job
/// left in its folder: holders that crashed, lost the lease, or ran an older
/// build. Only the lease holder calls it, under the library lock, after its
/// own file went into the meeting folder.
fn remove_stale_stages(job_dir: &std::path::Path) -> Result<()> {
    let entries = match std::fs::read_dir(job_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io_at(job_dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(job_dir, e))?;
        let path = entry.path();
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("audio."))
            && path.is_file()
        {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io_at(&path, e)),
            }
        }
    }
    Ok(())
}

/// Carries the review of the meeting an earlier pass published into
/// `meeting`, and moves the samples learned on that pass to the matching new
/// speakers (unmatched or ambiguous ones fall back to range attribution).
fn carry_review(
    tx: &mut broscribe_core::WriteTx<'_>,
    earlier: &Meeting,
    meeting: &mut Meeting,
) -> Result<()> {
    let carried = preserve_review(earlier, meeting);
    meeting.id.clone_from(&earlier.id);
    meeting
        .speaker_namespace
        .clone_from(&earlier.speaker_namespace);
    let published: Vec<_> = earlier.speakers.iter().map(|s| s.id).collect();
    ops::migrate_sample_speakers(tx, meeting, &published, |old| {
        let mut new = carried.iter().filter(|(o, _)| *o == old).map(|l| l.1);
        new.next().filter(|_| new.next().is_none())
    })?;
    Ok(())
}

/// Carries the user's review from the meeting an earlier pass published,
/// and the participants its captions linked (so a rename or forget since
/// that pass is honoured). Returns the (earlier, new) speaker pairs it matched.
fn preserve_review(old: &Meeting, new: &mut Meeting) -> Vec<(SpeakerId, SpeakerId)> {
    let mut carried = Vec::new();
    // A calendar title is recomputed from the currently authorized snapshot.
    // Only independent metadata can survive a revoked provisional prior.
    if old.title_source != Some(schema::TitleSource::Calendar) {
        new.keep_stronger_title(old);
    }
    // Each new speaker's overlap with each earlier speaker, found through an
    // ordered index of the earlier segments instead of a scan per span.
    let earlier = IntervalIndex::new(
        old.segments
            .iter()
            .filter_map(|s| s.speaker.map(|id| (s.start, s.end, id)))
            .collect(),
    );
    let mut overlaps: std::collections::BTreeMap<(SpeakerId, SpeakerId), f64> =
        std::collections::BTreeMap::new();
    for span in &new.segments {
        let Some(id) = span.speaker else {
            continue;
        };
        for &(start, end, prior) in earlier.overlapping(span.start, span.end) {
            *overlaps.entry((id, prior)).or_default() +=
                (end.min(span.end) - start.max(span.start)).max(0.0);
        }
    }
    let mut prior_talk: std::collections::BTreeMap<SpeakerId, f64> =
        std::collections::BTreeMap::new();
    for s in &old.segments {
        if let Some(id) = s.speaker {
            *prior_talk.entry(id).or_default() += s.end - s.start;
        }
    }
    for speaker in &mut new.speakers {
        let best = old
            .speakers
            .iter()
            .filter(|s| s.track == speaker.track)
            .map(|prior| {
                let overlap = overlaps
                    .get(&(speaker.id, prior.id))
                    .copied()
                    .unwrap_or(0.0);
                (prior, overlap)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((prior, overlap)) = best {
            let prior_talk = prior_talk.get(&prior.id).copied().unwrap_or(0.0);
            if overlap > 0.0 && overlap >= prior_talk * 0.8 {
                carried.push((prior.id, speaker.id));
                speaker.rejected_person_ids = prior.rejected_person_ids.clone();
                speaker.live_speaker_override = prior.live_speaker_override;
                if prior.assignment == Assignment::Confirmed {
                    speaker.assignment = prior.assignment;
                    speaker.person_id = prior.person_id.clone();
                    speaker.score = prior.score;
                    speaker.suggestion = None;
                }
            }
        }
    }
    let added: Vec<_> = old
        .tags
        .iter()
        .filter(|tag| !new.tags.contains(tag))
        .cloned()
        .collect();
    new.tags.extend(added);
    new.tags_removed = old.tags_removed.clone();
    new.live_attributions.clone_from(&old.live_attributions);
    ops::carry_live_speaker_overrides(old, new);
    carry_caption_participants(old, new);
    carried
}

fn carry_caption_participants(old: &Meeting, new: &mut Meeting) {
    use broscribe_readers::captions::folded_name;
    let has_captions = |p: &schema::Participant| p.evidence.iter().any(|e| e == "captions");
    for prior in old.participants.iter().filter(|p| has_captions(p)) {
        let Some(name) = prior.name.as_deref().map(folded_name) else {
            continue;
        };
        let same_name =
            |p: &schema::Participant| p.name.as_deref().is_some_and(|n| folded_name(n) == name);
        if new
            .participants
            .iter()
            .any(|p| has_captions(p) && same_name(p))
        {
            continue;
        }
        // A forgotten link (no person) stays separate: it must not mark an
        // unlinked roster entry of the same name.
        let linked = new
            .participants
            .iter_mut()
            .find(|p| prior.person_id.is_some() && p.person_id == prior.person_id && same_name(p));
        match linked {
            Some(p) => p.evidence.push("captions".into()),
            None => new.participants.push(schema::Participant {
                name: prior.name.clone(),
                person_id: prior.person_id.clone(),
                evidence: vec!["captions".into()],
                ..schema::Participant::default()
            }),
        }
    }
}

pub(crate) fn reader_evidence(
    dir: &std::path::Path,
    meeting: &Meeting,
    setup: Setup,
) -> Result<(
    Vec<broscribe_readers::RosterMember>,
    Vec<broscribe_readers::alignment::CaptionNaming>,
)> {
    use broscribe_readers::{
        ReaderEvent, RosterEvent,
        alignment::{AsrSpan, CaptionAligner, Setup as ReaderSetup, Track as ReaderTrack},
    };
    // The roster is the additive union of every snapshot, stamped with the
    // last one. It is a prior, not timed evidence: a process-now pass keeps it
    // even when the roster changed after the cutoff.
    let roster_path = dir.join("roster.json");
    let roster = if roster_path.exists() {
        match broscribe_core::fsutil::read_json::<RosterEvent>(&roster_path) {
            Ok(event) => event.members,
            Err(error) => {
                tracing::warn!(%error, "skipping invalid roster journal");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let captions = dir.join("captions.jsonl");
    let mut aligner = CaptionAligner::default();
    if captions.exists() {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(
            std::fs::File::open(&captions).map_err(|e| Error::io_at(&captions, e))?,
        );
        for line in reader.lines() {
            let line = line.map_err(|e| Error::io_at(&captions, e))?;
            if line.trim().is_empty() {
                continue;
            }
            let event: ReaderEvent = match serde_json::from_str(&line) {
                Ok(event) => event,
                Err(error) => {
                    tracing::warn!(%error, "skipping invalid caption journal line");
                    continue;
                }
            };
            if let ReaderEvent::Caption(caption) = event
                && caption.observed_s <= meeting.duration_s
            {
                aligner.ingest(caption);
            }
        }
    }
    let spans: Vec<_> = meeting
        .segments
        .iter()
        .filter_map(|s| {
            s.speaker.map(|speaker_id| AsrSpan {
                id: format!("{}:{}", s.track, s.id),
                speaker_id,
                track: if s.track == "mic" {
                    ReaderTrack::Mic
                } else {
                    ReaderTrack::System
                },
                start_s: s.start,
                end_s: s.end,
                text: s.text.clone(),
            })
        })
        .collect();
    let setup = match setup {
        Setup::Headphones | Setup::BluetoothCall => ReaderSetup::Headphones,
        Setup::HybridRoom => ReaderSetup::HybridRoom,
        Setup::InPerson => ReaderSetup::InPerson,
        _ => ReaderSetup::Speakerphone,
    };
    Ok((roster, aligner.names(&spans, setup)))
}

/// The meeting's own participant an earlier caption naming of `naming`'s
/// name linked (its `person_id` is cleared when that person was forgotten).
/// Self namings resolve through the self person instead.
pub(crate) fn caption_participant<'a>(
    meeting: &'a Meeting,
    naming: &broscribe_readers::alignment::CaptionNaming,
) -> Option<&'a schema::Participant> {
    use broscribe_readers::captions::folded_name;
    if naming.is_self {
        return None;
    }
    let name = folded_name(&naming.name);
    meeting.participants.iter().find(|p| {
        p.evidence.iter().any(|e| e == "captions")
            && p.name.as_deref().is_some_and(|n| folded_name(n) == name)
    })
}

/// Records that this meeting's captions named `person` under `naming`'s name,
/// so replay keeps that person after a rename and notices a forget. A
/// participant already linked to `person` under the same name gains the
/// `captions` evidence; otherwise a caption participant is added.
fn link_caption_participant(
    meeting: &mut Meeting,
    person: &schema::Person,
    naming: &broscribe_readers::alignment::CaptionNaming,
) {
    use broscribe_readers::captions::{folded_name, normalize_name};
    if naming.is_self || caption_participant(meeting, naming).is_some() {
        return;
    }
    let name = folded_name(&naming.name);
    let same = meeting.participants.iter_mut().find(|p| {
        p.person_id.as_ref() == Some(&person.id)
            && p.name.as_deref().is_some_and(|n| folded_name(n) == name)
    });
    match same {
        Some(p) => p.evidence.push("captions".into()),
        None => meeting.participants.push(schema::Participant {
            name: Some(normalize_name(&naming.name)),
            person_id: Some(person.id.clone()),
            evidence: vec!["captions".into()],
            ..schema::Participant::default()
        }),
    }
}

/// Names speakers from caption namings. A caption sample is added for a
/// naming only from `embeddings` of its agreed clean ranges (computed outside
/// the library lock); without them the naming adds no sample. `names` were
/// aligned against `view` when given (the meeting with speaker groups
/// formed in this same publish folded, so a voice over-split into several
/// clusters is named once, with all its ranges), else against `meeting`.
/// Grouped members of `meeting` are frozen: a naming of one is skipped.
pub(crate) fn apply_caption_names(
    tx: &mut broscribe_core::WriteTx<'_>,
    index: &mut Index,
    meeting: &mut Meeting,
    view: Option<&Meeting>,
    roster: &[broscribe_readers::RosterMember],
    names: &[broscribe_readers::alignment::CaptionNaming],
    embeddings: &broscribe_readers::alignment::RangeEmbeddings,
) -> Result<()> {
    let mut people = tx.library().list_people()?;
    let mut writes = CaptionWrites::default();
    plan_caption_names(
        tx.library(),
        &mut people,
        meeting,
        view,
        roster,
        names,
        embeddings,
        &mut writes,
    )?;
    write_caption_names(tx, index, &people, &writes)
}

/// What caption naming writes besides the meeting: the people it creates
/// and the caption samples it adds (with their person), in naming order.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CaptionWrites {
    pub created: Vec<schema::Person>,
    pub prints: Vec<(broscribe_core::PersonId, schema::Voiceprint)>,
}

/// [`apply_caption_names`] without writing: names `meeting`'s speakers in
/// memory and adds to `writes` what [`write_caption_names`] must store.
/// `people` is the library's people, extended with those created here, so
/// later namings (of this or another meeting) find them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_caption_names(
    lib: &broscribe_core::Library,
    people: &mut Vec<schema::Person>,
    meeting: &mut Meeting,
    view: Option<&Meeting>,
    roster: &[broscribe_readers::RosterMember],
    names: &[broscribe_readers::alignment::CaptionNaming],
    embeddings: &broscribe_readers::alignment::RangeEmbeddings,
    writes: &mut CaptionWrites,
) -> Result<()> {
    use broscribe_readers::alignment;
    let user = ops::UserSpeakerEvidence::of(meeting);
    let folded_user = view.map(ops::UserSpeakerEvidence::of);
    for naming in names {
        // Do not create name-only people for confirmed, absent or grouped
        // (frozen) speakers.
        if meeting
            .speaker(naming.speaker_id)
            .is_none_or(|s| s.assignment == Assignment::Confirmed)
            || ops::is_grouped_member(meeting, naming.speaker_id)
        {
            continue;
        }
        // A person this meeting's captions already named keeps the naming
        // even after the user renamed them.
        let linked = caption_participant(meeting, naming)
            .and_then(|p| p.person_id.as_ref())
            .and_then(|id| people.iter().find(|p| p.id == *id));
        let matched = linked
            .or_else(|| alignment::match_person(naming, people, roster))
            .cloned();
        let (person, created) = if let Some(person) = matched {
            (person, None)
        } else {
            use broscribe_readers::captions::folded_name;
            let name = folded_name(&naming.name);
            // None may mean an ambiguous existing name, not an unknown person.
            if naming.is_self
                || people.iter().any(|p| {
                    folded_name(&p.name) == name || p.aliases.iter().any(|a| folded_name(a) == name)
                })
            {
                continue;
            }
            let mut id = lib.new_person_id();
            while people.iter().any(|p| p.id == id) {
                id = lib.new_person_id();
            }
            let mut created = alignment::new_caption_person(id, &naming.name);
            created.person.needs_review = created.needs_review;
            (created.person.clone(), Some(created))
        };
        let conflict = user
            .conflict(meeting, naming.speaker_id, Some(&person.id))
            .or_else(|| {
                folded_user.as_ref().zip(view).and_then(|(user, view)| {
                    user.conflict(view, naming.speaker_id, Some(&person.id))
                })
            });
        if let Some(mut conflict) = conflict {
            if created.is_some() {
                conflict.candidate = None;
                conflict.unlinked_caption = Some(person.name.clone());
            }
            let speaker = meeting
                .speaker_mut(naming.speaker_id)
                .expect("checked above");
            speaker.identity_conflict = Some(conflict);
            // A voice or caption name already on the speaker is also held
            // for review when the user's lines conflict with this naming.
            speaker.assignment = Assignment::None;
            speaker.person_id = None;
            speaker.suggestion = None;
            speaker.live_suggestion_lines = None;
            speaker.score = None;
            continue;
        }
        if user.has_live_line(naming.speaker_id)
            || folded_user
                .as_ref()
                .is_some_and(|user| user.has_live_line(naming.speaker_id))
        {
            continue;
        }
        let (meeting_id, namespace) = (meeting.id.clone(), meeting.speaker_namespace.clone());
        let Some(speaker) = meeting.speaker_mut(naming.speaker_id) else {
            continue;
        };
        if !alignment::apply_naming(speaker, &person, naming) {
            continue;
        }
        // The sample stands for every range the naming was aligned on.
        let mut sampled = speaker.clone();
        if let Some(c) = view
            .and_then(|v| v.speaker(naming.speaker_id))
            .and_then(|s| s.centroid.clone())
        {
            sampled.centroid = Some(c);
        }
        let print = alignment::caption_voiceprint(
            &sampled,
            &person.id,
            &meeting_id,
            &namespace,
            naming,
            embeddings,
        );
        if let Some(created) = created {
            meeting.participants.push(created.participant);
            writes.created.push(created.person.clone());
            people.push(created.person);
        } else {
            link_caption_participant(meeting, &person, naming);
        }
        writes.prints.extend(print.map(|p| (person.id.clone(), p)));
    }
    Ok(())
}

/// Stores what [`plan_caption_names`] planned: the created people, then each
/// caption sample (promoted against everyone else's samples of its model,
/// `people` being every person known after the plan).
pub(crate) fn write_caption_names(
    tx: &mut broscribe_core::WriteTx<'_>,
    index: &mut Index,
    people: &[schema::Person],
    writes: &CaptionWrites,
) -> Result<()> {
    use broscribe_readers::alignment;
    for person in &writes.created {
        tx.save_person(person)?;
        index.upsert_person(tx, person)?;
    }
    for (person, print) in &writes.prints {
        let model = print.model.clone();
        let mut prints = tx.library().load_voiceprints(person)?;
        prints.upsert(vec![print.clone()]);
        let mut between = Vec::new();
        for other in people.iter().filter(|p| p.id != *person) {
            for other_print in tx.library().load_voiceprints(&other.id)?.embeddings {
                if other_print.model == model {
                    between.push(other_print.vector()?);
                }
            }
        }
        alignment::promote_captions(&mut prints.embeddings, &model, &between, false)?;
        tx.save_voiceprints(&prints)?;
    }
    Ok(())
}

/// The timeline inference reads: the journal up to `cutoff`, resampled to
/// [`TARGET_RATE`], so a long recording is never held at the device rate.
fn plan_analysis_timeline(
    dir: &std::path::Path,
    manifest: &journal::SessionManifest,
    cutoff: Option<Cutoff>,
) -> Result<Timeline> {
    Timeline::plan(
        dir,
        manifest,
        &TimelineOptions {
            sample_rate: Some(TARGET_RATE),
            cutoff,
            ..Default::default()
        },
    )
}

/// File in a record job's folder holding the meeting ID the job publishes.
use broscribe_core::lifecycle::RECORD_MEETING_ID_FILE as MEETING_ID_FILE;

/// The ID of the meeting a job publishes when no meeting of its session
/// exists yet: chosen once and kept in the job's folder before anything moves
/// into the library, so a retry after a failed publication reuses it (and the
/// folder that attempt may have left) instead of leaving an orphan folder.
fn job_meeting_id(env: &Env<'_>, lease: &LeaseHandle) -> Result<MeetingId> {
    let job_dir = lease.job_dir();
    let path = job_dir.join(MEETING_ID_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => return MeetingId::parse(text.trim()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io_at(&path, e)),
    }
    let id = env.lib.new_meeting_id()?;
    std::fs::create_dir_all(&job_dir).map_err(|e| Error::io_at(&job_dir, e))?;
    broscribe_core::fsutil::write_atomic(&path, id.as_str().as_bytes())?;
    Ok(id)
}

/// The calendar meeting(s) of the recording interval `start..end` from the
/// session's durable snapshot, after dropping every event its provider's live
/// authority no longer covers ([`fence_native_events`]).
fn cached_calendar(
    journal: &std::path::Path,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    app: Option<&str>,
    authority: &dyn CalendarAuthority,
) -> Result<Option<Vec<broscribe_integrations::CalendarEvent>>> {
    let path = journal.join("calendar.json");
    if !path.exists() {
        return Ok(None);
    }
    // Like bad roster and caption lines, an unreadable cache only loses the
    // calendar prior; it never fails finalization.
    let mut cache: broscribe_integrations::CalendarCache =
        match broscribe_core::fsutil::read_json(&path) {
            Ok(cache) => cache,
            Err(error) => {
                tracing::warn!(%error, "skipping unreadable calendar journal");
                return Ok(None);
            }
        };
    fence_native_events(&mut cache, authority);
    match cache.match_recording(start, end, None, app) {
        Ok(events) => Ok(events.map(|events| events.into_iter().cloned().collect())),
        Err(error) if error.code() == "calendar_match_ambiguous" => {
            tracing::warn!(
                code = error.code(),
                "calendar match ambiguous; using no calendar prior"
            );
            Ok(None)
        }
        Err(error) => Err(Error::Service {
            code: error.code(),
            message: "calendar matching failed".into(),
        }),
    }
}

/// Why a native event stamped with `stamped` is no longer covered by its
/// provider's `live` generation; `None` while it is.
fn generation_mismatch(
    live: &broscribe_platform::Result<Option<u64>>,
    stamped: Option<u64>,
) -> Option<&'static str> {
    match (live, stamped) {
        (Ok(Some(live)), Some(generation)) if *live == generation => None,
        (_, None) => Some("calendar_generation_missing"),
        (Err(error), _) => Some(error.code()),
        (Ok(None), _) => Some("calendar_inactive"),
        (Ok(Some(_)), _) => Some("calendar_generation_changed"),
    }
}

/// Why a Google event stamped with `stamped` is no longer covered by Google's
/// `live` authority; `None` only while both its account and its calendar
/// selection generations are the live ones. A legacy event carrying only a
/// scalar `account_generation` has no pair and never counts.
fn google_mismatch(
    live: &broscribe_platform::Result<Option<broscribe_core::calendar::GoogleCalendarStamp>>,
    stamped: Option<broscribe_core::calendar::GoogleCalendarStamp>,
) -> Option<&'static str> {
    match (live, stamped) {
        (Ok(Some(live)), Some(stamp)) if *live == stamp => None,
        (_, None) => Some("calendar_generation_missing"),
        (Err(error), _) => Some(error.code()),
        (Ok(None), _) => Some("calendar_inactive"),
        (Ok(Some(live)), Some(stamp)) if live.generation != stamp.generation => {
            Some("calendar_generation_changed")
        }
        (Ok(Some(_)), Some(_)) => Some("calendar_selection_changed"),
    }
}

fn check_calendar_authorities(
    events: &[broscribe_integrations::CalendarEvent],
    authority: &dyn CalendarAuthority,
) -> Result<()> {
    let mut native = std::collections::BTreeMap::new();
    let mut remote = std::collections::BTreeMap::new();
    let mut google = None;
    for event in events {
        let valid = if let Some(provider) = event.integration.calendar_provider() {
            let live = native
                .entry(provider)
                .or_insert_with(|| authority.active_generation(provider));
            generation_mismatch(live, event.account_generation).is_none()
        } else if event.integration == broscribe_integrations::IntegrationId::Google {
            // Google priors were read under one account and one calendar
            // selection generation; a scalar-stamped or unstamped (older) Google
            // event never counts.
            let live = google.get_or_insert_with(|| authority.google_authority());
            google_mismatch(live, event.google).is_none()
        } else {
            event.remote.as_ref().is_some_and(|stamp| {
                let live = remote
                    .entry(stamp.account.clone())
                    .or_insert_with(|| authority.remote_authority(&stamp.account));
                remote_stamp_matches(
                    event.integration,
                    stamp,
                    live.as_ref().ok().and_then(|live| live.as_ref()),
                )
            })
        };
        if !valid {
            return Err(Error::Service {
                code: "calendar_authority_changed",
                message:
                    "calendar access changed during recording analysis; retry without the old prior"
                        .into(),
            });
        }
    }
    Ok(())
}

fn remote_stamp_matches(
    integration: broscribe_integrations::IntegrationId,
    stamp: &broscribe_core::calendar::RemoteCalendarStamp,
    live: Option<&broscribe_core::calendar::AccountAuthority>,
) -> bool {
    let integration_matches = matches!(
        (integration, stamp.account.integration),
        (
            broscribe_integrations::IntegrationId::MicrosoftGraph,
            broscribe_core::calendar::RemoteIntegration::MicrosoftGraph
        )
    );
    integration_matches
        && live.is_some_and(|live| {
            live.account == stamp.account
                && live.generation == stamp.generation
                && live.selection_generation == stamp.selection_generation
                && live.selected_calendars.contains(&stamp.target)
        })
}

/// Native and Google events in a durable session snapshot were consented
/// under their provider's authority: the EventKit opt-in generation, or the
/// Google account AND calendar-selection generations. Revoking access,
/// disconnecting, signing in again or reselecting calendars since then moves
/// it. So every consumption, also after a worker restart, asks the helper for
/// each present provider's live authority now (at most once per provider) and
/// keeps only that provider's events stamped with exactly it. Without an
/// answer (helper down, error, not active) or a stamp, that provider's events
/// are dropped; other providers' events never are. A snapshot without such
/// events asks nothing.
fn fence_native_events(
    cache: &mut broscribe_integrations::CalendarCache,
    authority: &dyn CalendarAuthority,
) {
    use std::collections::BTreeMap;
    let mut live = BTreeMap::new();
    let mut remote = BTreeMap::new();
    let mut google = None;
    for event in &cache.events {
        if let Some(provider) = event.integration.calendar_provider() {
            live.entry(provider)
                .or_insert_with(|| authority.active_generation(provider));
        } else if event.integration == broscribe_integrations::IntegrationId::Google {
            google.get_or_insert_with(|| authority.google_authority());
        } else if let Some(stamp) = &event.remote {
            remote
                .entry(stamp.account.clone())
                .or_insert_with(|| authority.remote_authority(&stamp.account));
        }
    }
    // Microsoft snapshots without a stamp must be excluded even when no authority
    // can be queried; never guess an account from a provider slot.
    let mut excluded: BTreeMap<(&'static str, &'static str), usize> = BTreeMap::new();
    cache.events.retain(|e| {
        let code = if let Some(provider) = e.integration.calendar_provider() {
            generation_mismatch(&live[&provider], e.account_generation)
        } else if e.integration == broscribe_integrations::IntegrationId::Google {
            google
                .as_ref()
                .and_then(|live| google_mismatch(live, e.google))
        } else {
            let valid = e.remote.as_ref().is_some_and(|stamp| {
                remote_stamp_matches(
                    e.integration,
                    stamp,
                    remote
                        .get(&stamp.account)
                        .and_then(|result| result.as_ref().ok())
                        .and_then(|live| live.as_ref()),
                )
            });
            (!valid).then_some("calendar_provenance_unverified")
        };
        let Some(code) = code else {
            return true;
        };
        *excluded.entry((e.integration.as_str(), code)).or_default() += 1;
        false
    });
    for ((integration, code), excluded) in excluded {
        tracing::warn!(
            integration,
            code,
            excluded,
            "excluding calendar events no longer covered by that provider's live authority"
        );
    }
}

/// Joins calendar-only attendees to known people (exact e-mail, then name, as
/// [`crate::manifest::match_person`]) so they reach the resolver's roster prior.
/// A person the user removed from the session is never re-added by the calendar,
/// and an attendee already present under another identity merges into it.
fn link_calendar_attendees(meeting: &mut Meeting, people: &[schema::Person], removed: &[&str]) {
    let mut i = 0;
    while i < meeting.participants.len() {
        let attendee = &meeting.participants[i];
        if attendee.person_id.is_some() || !attendee.evidence.iter().any(|e| e == "calendar") {
            i += 1;
            continue;
        }
        let Some(known) = crate::manifest::match_person(
            people,
            attendee.name.as_deref(),
            attendee.email.as_deref(),
        ) else {
            i += 1;
            continue;
        };
        if removed.contains(&known.id.as_str()) {
            if attendee.evidence.iter().all(|e| e == "calendar") {
                meeting.participants.remove(i);
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(at) = meeting
            .participants
            .iter()
            .position(|p| p.person_id.as_ref() == Some(&known.id))
        {
            let merged = meeting.participants.remove(i);
            let target = &mut meeting.participants[if at > i { at - 1 } else { at }];
            for evidence in merged.evidence {
                if !target.evidence.contains(&evidence) {
                    target.evidence.push(evidence);
                }
            }
            continue;
        }
        let attendee = &mut meeting.participants[i];
        attendee.person_id = Some(known.id.clone());
        attendee.is_self |= known.is_self;
        i += 1;
    }
}

/// The audio check's echo handling saved with the session; an unknown name
/// (written by a newer version) falls back to the setup default.
fn session_echo_handling(meta: &journal::SessionMeta) -> Option<EchoHandling> {
    let name = meta.echo_handling.as_deref()?;
    match serde_json::from_value(serde_json::Value::String(name.to_owned())) {
        Ok(handling) => Some(handling),
        Err(_) => {
            tracing::warn!(
                handling = name,
                "unknown echo handling; using the setup default"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn record_empty_timeline_finishes_without_creating_meeting() {
        let fixture = Fixture::new();
        let writer = writer(&fixture);
        let dir = writer.dir().to_owned();
        writer.finish(0, "user").unwrap();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir.clone(),
            cutoff: None,
        };
        let lease = claim(&fixture, &input, false);
        let encoder = FakeEncoder::default();
        run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
        assert!(!dir.exists());
        assert!(encoder.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn record_short_keep_mic_segment_is_me_only_when_enrolled_non_room() {
        let mut fixture = Fixture::new();
        enroll_me(&mut fixture);
        let near = speech(0.5, -8.0, 2);
        let (analysis, _) = analyze_tracks(
            &fixture.env(),
            &near,
            &vec![0.0; near.len()],
            16000,
            Setup::Headphones,
            None,
            &mut models(),
            &[],
            &[],
        )
        .unwrap();
        let mic = analysis.speakers.iter().find(|s| s.track == "mic").unwrap();
        assert!(mic.person_id.is_some());
        assert_eq!(mic.assignment, Assignment::Auto);
    }

    #[test]
    fn record_invalid_reader_evidence_is_skipped() {
        let fixture = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("roster.json"), b"{incomplete").unwrap();
        std::fs::write(
            dir.path().join("captions.jsonl"),
            b"{incomplete\n\n{\"other\":true}\n",
        )
        .unwrap();
        let meeting = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            Source::Recording,
            schema::now(),
        );
        let (roster, names) = reader_evidence(dir.path(), &meeting, Setup::Headphones).unwrap();
        assert!(roster.is_empty() && names.is_empty());
    }

    fn caption_name(speaker_id: SpeakerId) -> broscribe_readers::alignment::CaptionNaming {
        broscribe_readers::alignment::CaptionNaming {
            speaker_id,
            name: "Alex Example".into(),
            is_self: false,
            distinct_spans: 1,
            purity: 1.0,
            start_s: 0.0,
            end_s: 3.0,
            ranges: vec![[0.0, 3.0]],
        }
    }

    #[test]
    fn record_caption_ambiguous_name_and_confirmed_speaker_create_no_people() {
        let fixture = Fixture::new();
        let speaker_id = SpeakerId::new(1).unwrap();
        let mut meeting = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            Source::Recording,
            schema::now(),
        );
        let mut speaker = schema::Speaker::new(speaker_id, "system");
        speaker.assignment = Assignment::Confirmed;
        meeting.speakers.push(speaker);
        fixture
            .lib
            .write(|tx| {
                let mut index = Index::open(tx.root())?;
                apply_caption_names(
                    tx,
                    &mut index,
                    &mut meeting,
                    None,
                    &[],
                    &[caption_name(speaker_id)],
                    &broscribe_readers::alignment::RangeEmbeddings::default(),
                )
            })
            .unwrap();
        assert!(fixture.lib.list_people().unwrap().is_empty());
        meeting.speakers[0].assignment = Assignment::None;
        fixture
            .lib
            .write(|tx| {
                for _ in 0..2 {
                    tx.save_person(&Person::new(tx.library().new_person_id(), "Alex Example"))?;
                }
                let mut index = Index::open(tx.root())?;
                apply_caption_names(
                    tx,
                    &mut index,
                    &mut meeting,
                    None,
                    &[],
                    &[caption_name(speaker_id)],
                    &broscribe_readers::alignment::RangeEmbeddings::default(),
                )
            })
            .unwrap();
        assert_eq!(fixture.lib.list_people().unwrap().len(), 2);
        assert!(meeting.speakers[0].person_id.is_none());
    }

    #[test]
    fn record_caption_second_meeting_promotes_samples_under_transaction() {
        let fixture = Fixture::new();
        let person = Person::new(fixture.lib.new_person_id(), "Alex Example");
        let other = Person::new(fixture.lib.new_person_id(), "Casey Example");
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&person)?;
                tx.save_person(&other)?;
                let mut prints = Voiceprints::empty(other.id.clone());
                prints.embeddings.push(Voiceprint {
                    id: "synthetic-other".into(),
                    model: "synthetic-level".into(),
                    dim: 2,
                    vector_b64: schema::encode_vector(&[0.0, 1.0]),
                    source: VoiceprintSource {
                        kind: VoiceprintKind::Enrollment,
                        meeting_id: None,
                        sha256: None,
                        start: 0.0,
                        end: 3.0,
                        ranges: Vec::new(),
                        speaker_id: None,
                        speaker_namespace: String::new(),
                    },
                    device: None,
                    strength: Strength::Confirmed,
                    created_at: schema::now(),
                });
                tx.save_voiceprints(&prints)
            })
            .unwrap();
        let speaker_id = SpeakerId::new(1).unwrap();
        for _ in 0..2 {
            let mut embeddings =
                broscribe_readers::alignment::RangeEmbeddings::new("synthetic-level");
            embeddings.insert("system", [0.0, 3.0], vec![1.0, 0.0]);
            let mut meeting = Meeting::new(
                fixture.lib.new_meeting_id().unwrap(),
                Source::Recording,
                schema::now(),
            );
            let mut speaker = schema::Speaker::new(speaker_id, "system");
            speaker.centroid = Some(schema::Centroid::new("synthetic-level", &[1.0, 0.0], 1));
            meeting.speakers.push(speaker);
            fixture
                .lib
                .write(|tx| {
                    let mut index = Index::open(tx.root())?;
                    apply_caption_names(
                        tx,
                        &mut index,
                        &mut meeting,
                        None,
                        &[],
                        &[caption_name(speaker_id)],
                        &embeddings,
                    )
                })
                .unwrap();
            fixture
                .lib
                .write(|tx| {
                    tx.create_meeting_dir(&meeting)?;
                    tx.save_meeting(&meeting)
                })
                .unwrap();
        }
        let prints = fixture.lib.load_voiceprints(&person.id).unwrap();
        assert_eq!(prints.embeddings.len(), 2);
        assert!(
            prints
                .embeddings
                .iter()
                .all(|p| p.strength == Strength::Confirmed)
        );
    }

    #[test]
    fn record_cached_calendar_sets_prior_and_hotwords_before_asr() {
        use std::sync::{Arc, Mutex};
        struct Words(Arc<Mutex<Vec<String>>>);
        impl broscribe_models::Asr for Words {
            fn transcribe(&self, _: &[f32]) -> Result<broscribe_models::AsrOutput> {
                assert!(self.0.lock().unwrap().iter().any(|w| w == "Casey Example"));
                Ok(broscribe_models::AsrOutput {
                    text: "synthetic words".into(),
                    tokens: Vec::new(),
                })
            }
            fn accepts_hotwords(&self) -> bool {
                true
            }
            fn set_hotwords(&mut self, words: &[String]) -> Result<()> {
                *self.0.lock().unwrap() = words.to_vec();
                Ok(())
            }
        }
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &speech(2.0, -18.0, 3)).unwrap();
        }
        let dir = writer.dir().to_owned();
        let at = writer.manifest().started_at;
        writer.finish(2_000_000_000, "user").unwrap();
        let event = broscribe_integrations::CalendarEvent {
            id: "synthetic-event".into(),
            integration: broscribe_integrations::IntegrationId::Google,
            title: Some("Synthetic planning".into()),
            start: at,
            end: at + chrono::Duration::hours(1),
            join_links: Vec::new(),
            app: None,
            account_generation: None,
            google: Some(GOOGLE_AUTHORITY),
            remote: None,
            ical_uid: None,
            occurrence_id: None,
            attendees: vec![broscribe_integrations::Attendee {
                name: Some("Casey Example".into()),
                email: Some("casey@example.invalid".into()),
                is_self: false,
            }],
        };
        broscribe_core::fsutil::write_json_atomic(
            &dir.join("calendar.json"),
            &broscribe_integrations::CalendarCache {
                events: vec![event],
            },
        )
        .unwrap();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir,
            cutoff: None,
        };
        let lease = claim(&fixture, &input, false);
        let mut models = models();
        models.asr = Box::new(Words(Arc::new(Mutex::new(Vec::new()))));
        let id = run(
            &fixture.env(),
            &mut models,
            &lease,
            &input,
            false,
            &FakeEncoder::default(),
            &NoQuery,
        )
        .unwrap();
        let meeting = fixture.lib.load_meeting(&id).unwrap();
        assert_eq!(meeting.title.as_deref(), Some("Synthetic planning"));
        assert_eq!(meeting.title_source, Some(schema::TitleSource::Calendar));
        assert!(
            meeting
                .participants
                .iter()
                .any(|p| p.email.as_deref() == Some("casey@example.invalid")
                    && p.evidence.iter().any(|e| e == "calendar"))
        );
        assert!(meeting.hotwords.iter().any(|w| w == "Casey Example"));
    }

    #[test]
    fn record_finalization_preserves_user_title_over_calendar_prior() {
        let fixture = Fixture::new();
        let mut old = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            Source::Recording,
            schema::now(),
        );
        old.title = Some("User title".into());
        old.title_source = Some(schema::TitleSource::User);
        let mut final_meeting = old.clone();
        final_meeting.title = Some("Calendar title".into());
        final_meeting.title_source = Some(schema::TitleSource::Calendar);
        preserve_review(&old, &mut final_meeting);
        assert_eq!(final_meeting.title, old.title);
        assert_eq!(final_meeting.title_source, old.title_source);
    }

    #[test]
    fn record_finalization_retry_after_migration_keeps_renumbered_samples() {
        let fixture = Fixture::new();
        let sid = |n: u32| SpeakerId::new(n).unwrap();
        let seg = |id: u32, speaker: u32, start: f64| schema::Segment {
            speech_overlap: Vec::new(),
            id,
            track: "t0".into(),
            speaker: Some(sid(speaker)),
            start,
            end: start + 4.0,
            text: "synthetic words".into(),
            words: Vec::new(),
        };
        let mut earlier = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            Source::Recording,
            schema::now(),
        );
        earlier.speakers = vec![
            schema::Speaker::new(sid(1), "t0"),
            schema::Speaker::new(sid(2), "t0"),
        ];
        earlier.segments = vec![seg(0, 1, 0.0), seg(1, 2, 5.0)];
        // The final pass labels the two voices the other way round.
        let mut final_meeting = earlier.clone();
        final_meeting.segments = vec![seg(0, 2, 0.0), seg(1, 1, 5.0)];
        let ana = Person::new(fixture.lib.new_person_id(), "Ana Invented");
        let print = |speaker: u32, start: f64| Voiceprint {
            id: format!("synthetic-{speaker}"),
            model: "test-embedder".into(),
            dim: 2,
            vector_b64: schema::encode_vector(&[1.0, 0.0]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Confirmation,
                meeting_id: Some(earlier.id.clone()),
                sha256: None,
                start,
                end: start + 4.0,
                ranges: Vec::new(),
                speaker_id: Some(sid(speaker)),
                speaker_namespace: earlier.speaker_namespace.clone(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        };
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&ana)?;
                tx.create_meeting_dir(&earlier)?;
                tx.save_meeting(&earlier)?;
                // Speaker 7 no longer exists: its sample falls back to ranges.
                tx.upsert_voiceprints(&ana.id, vec![print(1, 0.0), print(2, 5.0), print(7, 9.0)])
            })
            .unwrap();
        let failed: Result<()> = fixture.lib.write(|tx| {
            carry_review(tx, &earlier, &mut final_meeting)?;
            Err(Error::Internal(
                "synthetic failure before meeting publication".into(),
            ))
        });
        assert!(failed.is_err());
        let namespace = final_meeting.speaker_namespace.clone();
        assert!(!namespace.is_empty());
        let once = fixture.lib.load_voiceprints(&ana.id).unwrap();
        // A new worker sees the old meeting, not the candidate whose write failed.
        let published = fixture.lib.load_meeting(&earlier.id).unwrap();
        assert_eq!(published, earlier);
        let mut retry = published.clone();
        retry.segments = vec![seg(0, 2, 0.0), seg(1, 1, 5.0)];
        fixture
            .lib
            .write(|tx| {
                carry_review(tx, &published, &mut retry)?;
                tx.save_meeting(&retry)
            })
            .unwrap();
        assert_eq!(retry.speaker_namespace, namespace);
        assert_eq!(fixture.lib.load_voiceprints(&ana.id).unwrap(), once);
        assert_eq!(
            fixture
                .lib
                .load_meeting(&earlier.id)
                .unwrap()
                .speaker_namespace,
            namespace
        );
        let moved: Vec<_> = fixture
            .lib
            .load_voiceprints(&ana.id)
            .unwrap()
            .embeddings
            .into_iter()
            .map(|v| {
                assert_eq!(v.source.speaker_namespace, namespace);
                (v.source.start, v.source.speaker_id.map(SpeakerId::number))
            })
            .collect();
        assert_eq!(moved, vec![(0.0, Some(2)), (5.0, Some(1)), (9.0, None)]);
    }

    /// Records two seconds of synthetic speech with `meta`, `attendees` cached
    /// for the session and `removed` people dropped by the user mid-recording.
    fn record_with_calendar(
        fixture: &mut Fixture,
        meta: SessionMeta,
        attendees: Vec<broscribe_integrations::Attendee>,
        removed: &[&broscribe_core::PersonId],
    ) -> Meeting {
        record_with_snapshot(fixture, meta, &[], removed, &NoQuery, |at| {
            vec![google_event(
                "Synthetic planning",
                at,
                Some(GOOGLE_AUTHORITY),
                attendees,
            )]
        })
    }

    /// Records two seconds of synthetic speech with `meta` and people the user
    /// `added` or `removed` mid-recording. `snapshot` (given the session start)
    /// gives the durable session calendar snapshot, which the final job then
    /// consumes under `authority`.
    fn record_with_snapshot(
        fixture: &mut Fixture,
        meta: SessionMeta,
        added: &[&broscribe_core::PersonId],
        removed: &[&broscribe_core::PersonId],
        authority: &dyn CalendarAuthority,
        snapshot: impl FnOnce(
            chrono::DateTime<chrono::Utc>,
        ) -> Vec<broscribe_integrations::CalendarEvent>,
    ) -> Meeting {
        record_with_snapshot_models(
            fixture,
            meta,
            added,
            removed,
            authority,
            &mut models(),
            snapshot,
        )
    }

    /// [`record_with_snapshot`] analysing with `analysis` models.
    fn record_with_snapshot_models(
        fixture: &mut Fixture,
        meta: SessionMeta,
        added: &[&broscribe_core::PersonId],
        removed: &[&broscribe_core::PersonId],
        authority: &dyn CalendarAuthority,
        analysis: &mut ModelSet,
        snapshot: impl FnOnce(
            chrono::DateTime<chrono::Utc>,
        ) -> Vec<broscribe_integrations::CalendarEvent>,
    ) -> Meeting {
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(fixture);
        writer.set_meta(meta).unwrap();
        for id in added {
            writer
                .set_participant(id.as_str(), "Synthetic Example", true)
                .unwrap();
        }
        for id in removed {
            writer
                .set_participant(id.as_str(), "Synthetic Example", true)
                .unwrap();
            writer
                .set_participant(id.as_str(), "Synthetic Example", false)
                .unwrap();
        }
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &speech(2.0, -18.0, 3)).unwrap();
        }
        let dir = writer.dir().to_owned();
        let at = writer.manifest().started_at;
        writer.finish(2_000_000_000, "user").unwrap();
        broscribe_core::fsutil::write_json_atomic(
            &dir.join("calendar.json"),
            &broscribe_integrations::CalendarCache {
                events: snapshot(at),
            },
        )
        .unwrap();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir,
            cutoff: None,
        };
        let lease = claim(fixture, &input, false);
        let id = run(
            &fixture.env(),
            analysis,
            &lease,
            &input,
            false,
            &FakeEncoder::default(),
            authority,
        )
        .unwrap();
        fixture.lib.load_meeting(&id).unwrap()
    }

    fn calendar_event(
        integration: broscribe_integrations::IntegrationId,
        title: &str,
        start: chrono::DateTime<chrono::Utc>,
        account_generation: Option<u64>,
        attendees: Vec<broscribe_integrations::Attendee>,
    ) -> broscribe_integrations::CalendarEvent {
        broscribe_integrations::CalendarEvent {
            id: format!("synthetic {title}"),
            integration,
            title: Some(title.into()),
            start,
            end: start + chrono::Duration::hours(1),
            join_links: Vec::new(),
            app: None,
            attendees,
            account_generation,
            google: None,
            remote: None,
            ical_uid: None,
            occurrence_id: None,
        }
    }

    /// A Google event of the session snapshot stamped with `google`, the
    /// account and calendar-selection pair it was read under (no scalar stamp).
    fn google_event(
        title: &str,
        start: chrono::DateTime<chrono::Utc>,
        google: Option<broscribe_core::calendar::GoogleCalendarStamp>,
        attendees: Vec<broscribe_integrations::Attendee>,
    ) -> broscribe_integrations::CalendarEvent {
        broscribe_integrations::CalendarEvent {
            google,
            ..calendar_event(
                broscribe_integrations::IntegrationId::Google,
                title,
                start,
                None,
                attendees,
            )
        }
    }

    fn remote_account(handle: char) -> broscribe_core::calendar::AccountRef {
        broscribe_core::calendar::AccountRef {
            integration: broscribe_core::calendar::RemoteIntegration::MicrosoftGraph,
            account_id: broscribe_core::calendar::AccountId::parse(&handle.to_string().repeat(32))
                .unwrap(),
        }
    }

    fn remote_live(
        account: broscribe_core::calendar::AccountRef,
    ) -> broscribe_core::calendar::AccountAuthority {
        broscribe_core::calendar::AccountAuthority {
            account,
            generation: 7,
            selection_generation: 11,
            selected_calendars: vec![broscribe_core::calendar::CalendarTarget::Own {
                calendar_id: "same-calendar".into(),
            }],
        }
    }

    fn remote_event(
        account: broscribe_core::calendar::AccountRef,
        at: chrono::DateTime<chrono::Utc>,
    ) -> broscribe_integrations::CalendarEvent {
        let mut event = calendar_event(
            broscribe_integrations::IntegrationId::MicrosoftGraph,
            "Remote shared planning",
            at,
            None,
            vec![attendee("Remote Example", Some("remote@example.invalid"))],
        );
        event.ical_uid = Some("same-meeting-uid".into());
        event.occurrence_id = Some(at.to_rfc3339());
        event.remote = Some(broscribe_core::calendar::RemoteCalendarStamp {
            account,
            generation: 7,
            selection_generation: 11,
            target: broscribe_core::calendar::CalendarTarget::Own {
                calendar_id: "same-calendar".into(),
            },
        });
        event
    }

    #[test]
    fn record_remote_accounts_with_equal_generations_and_ids_never_exchange_authority() {
        for invalid in [
            "absent",
            "wrong-account",
            "reconnected",
            "reselected",
            "denied-target",
            "unstamped",
        ] {
            let mut fixture = Fixture::new();
            let people = fence_people(&fixture);
            let authority = FakeCalendarAuthority::default();
            authority
                .remote
                .lock()
                .unwrap()
                .insert(remote_account('b'), remote_live(remote_account('b')));
            let mut live_a = remote_live(remote_account('a'));
            match invalid {
                "wrong-account" => live_a.account = remote_account('b'),
                "reconnected" => live_a.generation += 1,
                "reselected" => live_a.selection_generation += 1,
                "denied-target" => live_a.selected_calendars.clear(),
                _ => {}
            }
            if invalid != "absent" {
                authority
                    .remote
                    .lock()
                    .unwrap()
                    .insert(remote_account('a'), live_a);
            }
            let meeting = record_fence(&mut fixture, &people, &authority, |at| {
                let mut a = remote_event(remote_account('a'), at);
                a.title = Some("Invalid account A".into());
                if invalid == "unstamped" {
                    a.remote = None;
                }
                vec![
                    a,
                    remote_event(remote_account('b'), at - chrono::Duration::minutes(5)),
                ]
            });
            assert_eq!(
                meeting.title.as_deref(),
                Some("Remote shared planning"),
                "{invalid}"
            );
            assert!(has_email(&meeting, "remote@example.invalid"));
            assert_user_choices_kept(&meeting, &people);
        }
    }

    #[test]
    fn record_duplicate_copies_are_order_invariant_but_conflicting_or_distinct_ties_have_no_hint() {
        for scenario in ["copies", "distinct", "conflicting"] {
            for reverse in [false, true] {
                let mut fixture = Fixture::new();
                let authority = FakeCalendarAuthority::default();
                for handle in ['a', 'b'] {
                    authority
                        .remote
                        .lock()
                        .unwrap()
                        .insert(remote_account(handle), remote_live(remote_account(handle)));
                }
                let meeting = record_with_snapshot(
                    &mut fixture,
                    meta(None, false),
                    &[],
                    &[],
                    &authority,
                    |at| {
                        let a = remote_event(remote_account('a'), at);
                        let mut b = remote_event(remote_account('b'), at);
                        match scenario {
                            "distinct" => b.ical_uid = Some("another-meeting".into()),
                            "conflicting" => b.title = Some("Conflicting title".into()),
                            _ => {}
                        }
                        let mut events = vec![a, b];
                        if reverse {
                            events.reverse();
                        }
                        events
                    },
                );
                if scenario == "copies" {
                    assert_eq!(meeting.title.as_deref(), Some("Remote shared planning"));
                    assert!(has_email(&meeting, "remote@example.invalid"));
                    assert!(meeting.hotwords.iter().any(|word| word == "Remote Example"));
                } else {
                    assert_ne!(meeting.title_source, Some(schema::TitleSource::Calendar));
                    assert!(!has_email(&meeting, "remote@example.invalid"));
                    assert!(!meeting.hotwords.iter().any(|word| word == "Remote Example"));
                }
            }
        }
    }

    fn attendee(name: &str, email: Option<&str>) -> broscribe_integrations::Attendee {
        broscribe_integrations::Attendee {
            name: Some(name.into()),
            email: email.map(Into::into),
            is_self: false,
        }
    }

    fn meta(title: Option<&str>, title_detected: bool) -> SessionMeta {
        SessionMeta {
            setup: Some("headphones".into()),
            language: Some("en".into()),
            title: title.map(Into::into),
            title_detected,
            ..Default::default()
        }
    }

    /// The live Google authority synthetic Google snapshots are stamped with:
    /// account generation and calendar-selection generation.
    const GOOGLE_AUTHORITY: broscribe_core::calendar::GoogleCalendarStamp =
        broscribe_core::calendar::GoogleCalendarStamp {
            generation: 7,
            selection_generation: 11,
        };

    /// Snapshots without native or Microsoft events never reach those
    /// authorities; Google answers [`GOOGLE_AUTHORITY`].
    struct NoQuery;
    impl CalendarAuthority for NoQuery {
        fn active_generation(&self, _: CalendarProvider) -> Result<Option<u64>> {
            panic!("a calendar snapshot without native events must not query the helper")
        }
        fn remote_authority(
            &self,
            _: &broscribe_core::calendar::AccountRef,
        ) -> Result<Option<broscribe_core::calendar::AccountAuthority>> {
            panic!("a Google snapshot must not query remote account authority")
        }
        fn google_authority(
            &self,
        ) -> Result<Option<broscribe_core::calendar::GoogleCalendarStamp>> {
            Ok(Some(GOOGLE_AUTHORITY))
        }
    }

    /// The helper is not running.
    struct HelperDown;
    impl CalendarAuthority for HelperDown {
        fn active_generation(&self, _: CalendarProvider) -> Result<Option<u64>> {
            Err(Error::DaemonUnavailable("synthetic helper down".into()))
        }
        fn remote_authority(
            &self,
            _: &broscribe_core::calendar::AccountRef,
        ) -> Result<Option<broscribe_core::calendar::AccountAuthority>> {
            Err(Error::DaemonUnavailable("synthetic helper down".into()))
        }
        fn google_authority(
            &self,
        ) -> Result<Option<broscribe_core::calendar::GoogleCalendarStamp>> {
            Err(Error::DaemonUnavailable("synthetic helper down".into()))
        }
    }

    /// Known people of the native-fence scenarios: Me, a person the user added
    /// and one the user removed during the recording.
    struct FencePeople {
        me: Person,
        casey: Person,
        sam: Person,
    }

    fn fence_people(fixture: &Fixture) -> FencePeople {
        let mut me = Person::new(fixture.lib.new_person_id(), "Me Example");
        me.is_self = true;
        me.emails = vec!["me@example.invalid".into()];
        let casey = Person::new(fixture.lib.new_person_id(), "Casey Example");
        let mut sam = Person::new(fixture.lib.new_person_id(), "Sam Example");
        sam.emails = vec!["sam@example.invalid".into()];
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&me)?;
                tx.save_person(&casey)?;
                tx.save_person(&sam)
            })
            .unwrap();
        FencePeople { me, casey, sam }
    }

    /// A remote event and, closer to the session start (so it wins when
    /// eligible), a native event stamped with `generation`.
    fn fence_snapshot(
        at: chrono::DateTime<chrono::Utc>,
        generation: Option<u64>,
    ) -> Vec<broscribe_integrations::CalendarEvent> {
        let mut me = attendee("Me Example", Some("me@example.invalid"));
        me.is_self = true;
        vec![
            google_event(
                "Remote planning",
                at - chrono::Duration::minutes(5),
                Some(GOOGLE_AUTHORITY),
                vec![attendee("Jordan Example", Some("jordan@example.invalid"))],
            ),
            calendar_event(
                broscribe_integrations::IntegrationId::SystemCalendar,
                "Native planning",
                at,
                generation,
                vec![
                    me,
                    attendee("Riley Example", Some("riley@example.invalid")),
                    attendee("Sam Example", Some("sam@example.invalid")),
                ],
            ),
        ]
    }

    fn record_fence(
        fixture: &mut Fixture,
        people: &FencePeople,
        authority: &dyn CalendarAuthority,
        snapshot: impl FnOnce(
            chrono::DateTime<chrono::Utc>,
        ) -> Vec<broscribe_integrations::CalendarEvent>,
    ) -> Meeting {
        record_with_snapshot(
            fixture,
            meta(None, false),
            &[&people.casey.id],
            &[&people.sam.id],
            authority,
            snapshot,
        )
    }

    /// The helper stamps the snapshot with its live generation 3; `change`
    /// then happens to the helper before the final job consumes the snapshot.
    fn record_after_helper_change(
        change: impl FnOnce(&FakeCalendarAuthority),
    ) -> (Meeting, FencePeople) {
        let mut fixture = Fixture::new();
        let people = fence_people(&fixture);
        let authority = FakeCalendarAuthority::default();
        *authority.generation.lock().unwrap() = Some(3);
        *authority.google_authority.lock().unwrap() = Some(GOOGLE_AUTHORITY);
        let meeting = record_fence(&mut fixture, &people, &authority, |at| {
            let snapshot = fence_snapshot(
                at,
                authority
                    .active_generation(CalendarProvider::EventKit)
                    .unwrap(),
            );
            change(&authority);
            snapshot
        });
        (meeting, people)
    }

    fn has_email(meeting: &Meeting, email: &str) -> bool {
        meeting
            .participants
            .iter()
            .any(|p| p.email.as_deref() == Some(email))
    }

    /// The user's additions and removals hold whichever calendar context applied.
    fn assert_user_choices_kept(meeting: &Meeting, people: &FencePeople) {
        let roster = meeting.roster_person_ids();
        assert!(meeting.participants.iter().any(|p| {
            p.person_id.as_ref() == Some(&people.casey.id) && p.evidence.iter().any(|e| e == "user")
        }));
        assert!(roster.contains(&people.casey.id));
        assert!(!roster.contains(&people.sam.id));
        assert!(!has_email(meeting, "sam@example.invalid"));
    }

    fn assert_remote_context_only(meeting: &Meeting, people: &FencePeople) {
        assert_eq!(meeting.title.as_deref(), Some("Remote planning"));
        assert_eq!(meeting.title_source, Some(schema::TitleSource::Calendar));
        assert!(meeting.participants.iter().any(|p| {
            p.email.as_deref() == Some("jordan@example.invalid")
                && p.evidence.iter().any(|e| e == "calendar")
        }));
        assert!(meeting.hotwords.iter().any(|w| w == "Jordan Example"));
        assert!(!has_email(meeting, "riley@example.invalid"));
        assert!(!has_email(meeting, "me@example.invalid"));
        assert!(!meeting.roster_person_ids().contains(&people.me.id));
        assert!(
            !meeting
                .hotwords
                .iter()
                .any(|w| w == "Riley Example" || w == "Me Example")
        );
        assert_user_choices_kept(meeting, people);
    }

    #[test]
    fn record_native_snapshot_under_the_live_generation_gives_title_roster_and_me_priors() {
        let (meeting, people) = record_after_helper_change(|_| {});
        assert_eq!(meeting.title.as_deref(), Some("Native planning"));
        assert_eq!(meeting.title_source, Some(schema::TitleSource::Calendar));
        let calendar = |p: &schema::Participant| p.evidence.iter().any(|e| e == "calendar");
        assert!(
            meeting
                .participants
                .iter()
                .any(|p| p.email.as_deref() == Some("riley@example.invalid") && calendar(p))
        );
        let me = meeting
            .participants
            .iter()
            .find(|p| p.person_id.as_ref() == Some(&people.me.id))
            .unwrap();
        assert!(me.is_self && calendar(me));
        assert!(meeting.roster_person_ids().contains(&people.me.id));
        assert!(meeting.hotwords.iter().any(|w| w == "Riley Example"));
        assert!(!has_email(&meeting, "jordan@example.invalid"));
        assert_user_choices_kept(&meeting, &people);
    }

    #[test]
    fn record_native_snapshot_revoked_disconnected_or_reconnected_before_use_keeps_remote_context()
    {
        // Revoking access and disconnecting leave no active generation;
        // reconnecting or reselecting calendars moves to a new one.
        for live in [None, Some(4)] {
            let (meeting, people) = record_after_helper_change(|authority| {
                *authority.generation.lock().unwrap() = live
            });
            assert_remote_context_only(&meeting, &people);
        }
    }

    #[test]
    fn record_snapshot_without_helper_answer_keeps_no_unverified_calendar_context() {
        // Without an answer neither the native nor the Google prior is
        // verified: both are dropped (fail closed); the user's choices stay.
        let unavailable: [&dyn CalendarAuthority; 2] = [&HelperDown, &UnsupportedCalendarAuthority];
        for authority in unavailable {
            let mut fixture = Fixture::new();
            let people = fence_people(&fixture);
            let meeting = record_fence(&mut fixture, &people, authority, |at| {
                fence_snapshot(at, Some(3))
            });
            assert_ne!(meeting.title_source, Some(schema::TitleSource::Calendar));
            assert!(!has_email(&meeting, "jordan@example.invalid"));
            assert!(!has_email(&meeting, "riley@example.invalid"));
            assert!(
                meeting
                    .participants
                    .iter()
                    .all(|p| !p.evidence.iter().any(|e| e == "calendar"))
            );
            assert_user_choices_kept(&meeting, &people);
        }
        // An unstamped native event is never accepted, even while opted in.
        let mut fixture = Fixture::new();
        let people = fence_people(&fixture);
        let authority = FakeCalendarAuthority::default();
        *authority.generation.lock().unwrap() = Some(3);
        *authority.google_authority.lock().unwrap() = Some(GOOGLE_AUTHORITY);
        let meeting = record_fence(&mut fixture, &people, &authority, |at| {
            fence_snapshot(at, None)
        });
        assert_remote_context_only(&meeting, &people);
    }

    /// Records the hotwords the recording's ASR is given before inference.
    struct HotwordAsr {
        inner: Box<dyn broscribe_models::Asr + Send>,
        words: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }
    impl broscribe_models::Asr for HotwordAsr {
        fn transcribe(&self, audio: &[f32]) -> Result<broscribe_models::AsrOutput> {
            self.inner.transcribe(audio)
        }
        fn accepts_hotwords(&self) -> bool {
            true
        }
        fn set_hotwords(&mut self, words: &[String]) -> Result<()> {
            self.words.lock().unwrap().extend_from_slice(words);
            Ok(())
        }
    }

    /// Google priors count only under the exact live pair: an unstamped event,
    /// a legacy event with only a scalar account generation (even the live
    /// one), one read under an older calendar selection or an older sign-in,
    /// one while Google is disconnected and one without a helper answer never
    /// reach inference (no title, attendee or hotword); the current pair does.
    /// Neither the native nor a Microsoft authority is asked to stand in.
    #[test]
    fn record_google_snapshot_needs_the_live_account_and_selection_pair_before_inference() {
        use broscribe_core::calendar::GoogleCalendarStamp;
        let previous_selection = GoogleCalendarStamp {
            selection_generation: GOOGLE_AUTHORITY.selection_generation - 1,
            ..GOOGLE_AUTHORITY
        };
        let previous_account = GoogleCalendarStamp {
            generation: GOOGLE_AUTHORITY.generation - 1,
            ..GOOGLE_AUTHORITY
        };
        for (case, live, accepted) in [
            ("current pair", Some(GOOGLE_AUTHORITY), true),
            ("unstamped", Some(GOOGLE_AUTHORITY), false),
            ("legacy scalar", Some(GOOGLE_AUTHORITY), false),
            ("previous selection", Some(GOOGLE_AUTHORITY), false),
            ("previous account", Some(GOOGLE_AUTHORITY), false),
            ("disconnected", None, false),
            ("helper down", Some(GOOGLE_AUTHORITY), false),
        ] {
            let mut fixture = Fixture::new();
            let authority = FakeCalendarAuthority::default();
            *authority.google_authority.lock().unwrap() = live;
            if case == "helper down" {
                *authority.google_error.lock().unwrap() = Some("helper_required");
            }
            let words = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut analysis = models();
            analysis.asr = Box::new(HotwordAsr {
                inner: analysis.asr,
                words: words.clone(),
            });
            let meeting = record_with_snapshot_models(
                &mut fixture,
                meta(None, false),
                &[],
                &[],
                &authority,
                &mut analysis,
                |at| {
                    let attendees = vec![attendee(
                        "Planning Example",
                        Some("planning@example.invalid"),
                    )];
                    let stamp = match case {
                        "unstamped" | "legacy scalar" => None,
                        "previous selection" => Some(previous_selection),
                        "previous account" => Some(previous_account),
                        _ => Some(GOOGLE_AUTHORITY),
                    };
                    let mut event = google_event("Google planning", at, stamp, attendees);
                    if case == "legacy scalar" {
                        // A pre-pair prior: the live account generation only.
                        event.account_generation = Some(GOOGLE_AUTHORITY.generation);
                    }
                    vec![event]
                },
            );
            assert!(
                authority.remote_queries.lock().unwrap().is_empty(),
                "{case}"
            );
            assert!(authority.queries.lock().unwrap().is_empty(), "{case}");
            assert_eq!(
                words
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|w| w == "Planning Example"),
                accepted,
                "{case}: hotwords given to ASR before inference"
            );
            assert_eq!(
                meeting.title.as_deref() == Some("Google planning"),
                accepted,
                "{case}"
            );
            assert_eq!(
                has_email(&meeting, "planning@example.invalid"),
                accepted,
                "{case}"
            );
            assert_eq!(
                meeting.hotwords.iter().any(|w| w == "Planning Example"),
                accepted,
                "{case}"
            );
        }
    }

    fn journal_root(fixture: &Fixture) -> std::path::PathBuf {
        fixture._tmp.path().join("journal")
    }

    fn provisional_input(
        dir: &std::path::Path,
        cutoff: broscribe_capture::journal::Cutoff,
    ) -> RecordInput {
        RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir.to_owned(),
            cutoff: Some(broscribe_ipc::ChunkCutoff {
                chunks: BTreeMap::from([
                    (broscribe_ipc::Track::Mic, cutoff.mic.unwrap()),
                    (broscribe_ipc::Track::System, cutoff.system.unwrap()),
                ]),
                recording_s: 2.0,
            }),
        }
    }

    /// Title edits on the provisional meeting (t1), the stopped session (t2)
    /// and none from the calendar: the latest explicit edit wins in the final
    /// meeting, and an edit after saving is routed to the meeting.
    #[test]
    fn record_latest_user_title_wins_across_provisional_session_and_final() {
        use broscribe_core::lifecycle::{self, SessionTitleOutcome};
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        writer
            .set_meta(meta(Some("Typed at start"), false))
            .unwrap();
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &speech(2.0, -18.0, 3)).unwrap();
        }
        let dir = writer.dir().to_owned();
        let input = provisional_input(&dir, writer.cutoff());
        let lease = claim(&fixture, &input, true);
        let encoder = FakeEncoder::default();
        let provisional = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            true,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        let t1 = lifecycle::set_meeting_title(
            &fixture.lib,
            &journal_root(&fixture),
            &provisional,
            "Provisional rename",
        )
        .unwrap();
        assert_eq!(t1.session_id.as_deref(), Some("synthetic-session"));
        writer.finish(2_000_000_000, "user").unwrap();
        // Stopped, final job not run yet: the session is still renamable.
        lifecycle::set_session_title(
            &fixture.lib,
            &journal_root(&fixture),
            "synthetic-session",
            "Renamed after stop",
            || Ok(()),
        )
        .unwrap();
        let input = RecordInput {
            cutoff: None,
            ..input
        };
        let lease = claim(&fixture, &input, false);
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        assert_eq!(
            id, provisional,
            "the provisional meeting became the final one"
        );
        let meeting = fixture.lib.load_meeting(&id).unwrap();
        assert_eq!(meeting.title.as_deref(), Some("Renamed after stop"));
        assert_eq!(meeting.title_source, Some(schema::TitleSource::User));
        assert!(!dir.exists());
        assert_eq!(
            lifecycle::set_session_title(
                &fixture.lib,
                &journal_root(&fixture),
                "synthetic-session",
                "Too late",
                || Ok(()),
            )
            .unwrap(),
            SessionTitleOutcome::AlreadyPublished { meeting_id: id }
        );
    }

    /// Encodes like [`FakeEncoder`], then the user discards the session
    /// while the final job is still working (before its commit).
    struct DiscardDuringEncode {
        inner: FakeEncoder,
        journal_root: std::path::PathBuf,
    }

    impl AudioEncoder for DiscardDuringEncode {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn extension(&self) -> &str {
            self.inner.extension()
        }
        fn encode(
            &self,
            out: &std::path::Path,
            settings: &broscribe_platform::encoder::EncodeSettings,
            tracks: &mut [broscribe_platform::encoder::EncoderTrack<'_>],
        ) -> broscribe_platform::Result<Vec<broscribe_platform::encoder::EncodedTrack>> {
            let encoded = self.inner.encode(out, settings, tracks)?;
            broscribe_core::lifecycle::write_discard_intent(
                &self.journal_root,
                "synthetic-session",
            )
            .unwrap();
            Ok(encoded)
        }
    }

    #[test]
    fn record_discarded_session_never_publishes_or_keeps_encoded_audio() {
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &speech(2.0, -18.0, 3)).unwrap();
        }
        let dir = writer.dir().to_owned();
        writer.finish(2_000_000_000, "user").unwrap();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir.clone(),
            cutoff: None,
        };
        // Discarded during inference/encoding: the commit refuses.
        let lease = claim(&fixture, &input, false);
        let encoder = DiscardDuringEncode {
            inner: FakeEncoder::default(),
            journal_root: journal_root(&fixture),
        };
        let err = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap_err();
        assert_eq!(err.code(), broscribe_core::lifecycle::RECORDING_DISCARDED);
        assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
        let staged: Vec<_> = std::fs::read_dir(lease.job_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("audio."))
            .collect();
        assert!(staged.is_empty(), "encoded audio left in the job folder");
        assert!(
            dir.exists(),
            "the discard cleanup, not the job, removes the journal"
        );
        // A retry (failed-retryable job) refuses before any work.
        let err = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &FakeEncoder::default(),
            &NoQuery,
        )
        .unwrap_err();
        assert_eq!(err.code(), broscribe_core::lifecycle::RECORDING_DISCARDED);
        assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
    }

    #[test]
    fn record_calendar_attendees_join_known_people_in_roster_except_removed() {
        let mut fixture = Fixture::new();
        let mut casey = Person::new(fixture.lib.new_person_id(), "Casey Example");
        casey.emails = vec!["casey@example.invalid".into()];
        let robin = Person::new(fixture.lib.new_person_id(), "Robin Example");
        let mut sam = Person::new(fixture.lib.new_person_id(), "Sam Example");
        sam.emails = vec!["sam@example.invalid".into()];
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&casey)?;
                tx.save_person(&robin)?;
                tx.save_person(&sam)
            })
            .unwrap();
        let meeting = record_with_calendar(
            &mut fixture,
            meta(None, false),
            vec![
                // Exact e-mail wins over a differently written display name.
                attendee("C. Example", Some("CASEY@example.invalid")),
                // A person without e-mails matches by name.
                attendee("Robin Example", Some("robin@example.invalid")),
                attendee("Sam Example", Some("sam@example.invalid")),
                attendee("Unknown Example", Some("unknown@example.invalid")),
            ],
            &[&sam.id],
        );
        let roster = meeting.roster_person_ids();
        assert!(roster.contains(&casey.id));
        assert!(roster.contains(&robin.id));
        assert!(!roster.contains(&sam.id));
        assert!(
            !meeting
                .participants
                .iter()
                .any(|p| p.email.as_deref() == Some("sam@example.invalid")),
            "the calendar never re-adds a person removed during the recording"
        );
        let unknown = meeting
            .participants
            .iter()
            .find(|p| p.email.as_deref() == Some("unknown@example.invalid"))
            .unwrap();
        assert!(unknown.person_id.is_none());
        assert_eq!(
            meeting
                .participants
                .iter()
                .filter(|p| p.person_id.as_ref() == Some(&casey.id))
                .count(),
            1
        );
    }

    #[test]
    fn record_detected_window_title_yields_to_calendar_but_typed_title_stays() {
        let mut detected = Fixture::new();
        let meeting = record_with_calendar(
            &mut detected,
            meta(Some("Meet - synthetic window"), true),
            Vec::new(),
            &[],
        );
        assert_eq!(meeting.title.as_deref(), Some("Synthetic planning"));
        assert_eq!(meeting.title_source, Some(schema::TitleSource::Calendar));
        let mut typed = Fixture::new();
        let meeting = record_with_calendar(
            &mut typed,
            meta(Some("Typed title"), false),
            Vec::new(),
            &[],
        );
        assert_eq!(meeting.title.as_deref(), Some("Typed title"));
        assert_eq!(meeting.title_source, Some(schema::TitleSource::User));
    }

    #[test]
    fn record_uses_the_sessions_echo_handling_over_the_setup_default() {
        let mut fixture = Fixture::new();
        let mut session = meta(None, false);
        session.echo_handling = Some("residue_gating".into());
        let meeting = record_with_calendar(&mut fixture, session, Vec::new(), &[]);
        let report = meeting.processing.echo_report.unwrap();
        assert_eq!(report["requested"], "residue_gating");
        let mut unknown = meta(None, false);
        unknown.echo_handling = Some("future_mode".into());
        assert_eq!(session_echo_handling(&unknown), None);
        assert_eq!(session_echo_handling(&meta(None, false)), None);
    }
    use super::*;
    use crate::testutil::Fixture;
    use broscribe_capture::journal::{JournalWriter, SessionMeta};
    use broscribe_core::{
        config::Thresholds,
        queue::{Queue, RECORD_FINAL_KIND, RECORD_PROVISIONAL_KIND},
        schema::{Person, Strength, Voiceprint, VoiceprintKind, VoiceprintSource, Voiceprints},
    };
    use broscribe_dsp::synth::{Voice, speech_like};
    use broscribe_models::fakes::{FakeAsr, FakeModelProvider};
    use broscribe_models::{ModelProvider, SpeakerEmbedder, SpeechSegment, Vad};
    use broscribe_platform::{
        CalendarProvider, DecoderChain, FakeCalendarAuthority, UnsupportedCalendarAuthority,
        encoder::{FakeEncodedDecoder, FakeEncoder},
    };
    use std::collections::{BTreeMap, BTreeSet};

    struct WholeVad;
    impl Vad for WholeVad {
        fn segments(&mut self, samples: &[f32]) -> Result<Vec<SpeechSegment>> {
            Ok(if samples.iter().any(|s| s.abs() > 1e-6) {
                vec![SpeechSegment {
                    start_s: 0.0,
                    end_s: samples.len() as f64 / 16000.0,
                }]
            } else {
                Vec::new()
            })
        }
    }
    // Deliberately simple fake voice identity, independent of ASR and speaker
    // routing: high- and low-level synthetic fixtures represent different voices.
    struct LevelEmbedder;
    impl SpeakerEmbedder for LevelEmbedder {
        fn model_id(&self) -> &str {
            "synthetic-level"
        }
        fn dim(&self) -> usize {
            2
        }
        fn embed(&self, samples: &[f32]) -> Result<Option<Vec<f32>>> {
            let rms =
                (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
            Ok((rms > 1e-6).then(|| {
                if rms > 0.04 {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                }
            }))
        }
    }
    fn models() -> ModelSet {
        let mut models = FakeModelProvider::default()
            .load(&broscribe_core::config::Profile::default())
            .unwrap();
        models.vad = Box::new(WholeVad);
        models.embedder = Box::new(LevelEmbedder);
        models.asr = Box::new(FakeAsr::new(&[
            "alpha", "beta", "gamma", "delta", "epsilon",
        ]));
        models
    }
    fn speech(seconds: f64, level: f32, seed: u64) -> Vec<f32> {
        speech_like(
            16000,
            seconds,
            Voice {
                seed,
                f0_hz: 130.0 + seed as f64 * 10.0,
                level_dbfs: level,
            },
        )
    }
    fn enroll_me(fixture: &mut Fixture) {
        fixture.config.identity.insert(
            "synthetic-level".into(),
            Thresholds {
                auto_name: Some(0.8),
                ..Default::default()
            },
        );
        let mut person = Person::new(fixture.lib.new_person_id(), "Synthetic Me");
        person.is_self = true;
        let mut prints = Voiceprints::empty(person.id.clone());
        prints.embeddings.push(Voiceprint {
            id: "synthetic-enrollment".into(),
            model: "synthetic-level".into(),
            dim: 2,
            vector_b64: schema::encode_vector(&[1.0, 0.0]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Enrollment,
                meeting_id: None,
                sha256: None,
                start: 0.0,
                end: 3.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        });
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&person)?;
                tx.save_voiceprints(&prints)
            })
            .unwrap();
    }
    fn claim(fixture: &Fixture, input: &RecordInput, provisional: bool) -> LeaseHandle {
        let queue = Queue::open(fixture.lib.root()).unwrap();
        let kind = if provisional {
            RECORD_PROVISIONAL_KIND
        } else {
            RECORD_FINAL_KIND
        };
        let key = if let Some(cutoff) = &input.cutoff {
            RecordInput::provisional_key(&input.session_id, cutoff)
        } else {
            input.session_id.clone()
        };
        let id = fixture
            .lib
            .write(|tx| queue.enqueue(tx, kind, &key, serde_json::to_value(input).unwrap()))
            .unwrap();
        queue
            .claim(&fixture.lib, &id, &fixture.probe)
            .unwrap()
            .unwrap()
    }
    fn writer(fixture: &Fixture) -> JournalWriter {
        let mut writer = JournalWriter::create(
            &fixture._tmp.path().join("journal"),
            "synthetic-session",
            SessionMeta {
                library_root: Some(fixture.lib.root().canonicalize().unwrap()),
                setup: Some("headphones".into()),
                language: Some("en".into()),
                ..Default::default()
            },
            2.0,
            std::process::id(),
        )
        .unwrap();
        for track in CaptureTrack::ALL {
            writer.begin_segment(track, 0, None, 16000, 0).unwrap();
        }
        writer
    }

    #[test]
    fn final_recording_retains_a_confirmed_participant_after_a_merge() {
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let source = Person::new(fixture.lib.new_person_id(), "Synthetic Source");
        let keep = Person::new(fixture.lib.new_person_id(), "Synthetic Target");
        let missing = Person::new(fixture.lib.new_person_id(), "Synthetic Unlinked");
        let model = "synthetic-level";
        ops::with_index(&fixture.lib, |tx, index| {
            for person in [&source, &keep] {
                tx.save_person(person)?;
                index.upsert_person(tx, person)?;
                let mut prints = Voiceprints::empty(person.id.clone());
                prints.embeddings.push(Voiceprint {
                    id: format!("synthetic-{}", person.id),
                    model: model.into(),
                    dim: 2,
                    vector_b64: schema::encode_vector(&[0.0, 1.0]),
                    source: VoiceprintSource {
                        kind: VoiceprintKind::Enrollment,
                        meeting_id: None,
                        sha256: None,
                        start: 0.0,
                        end: 2.0,
                        ranges: Vec::new(),
                        speaker_id: None,
                        speaker_namespace: String::new(),
                    },
                    device: None,
                    strength: Strength::Confirmed,
                    created_at: schema::now(),
                });
                tx.save_voiceprints(&prints)?;
            }
            Ok(())
        })
        .unwrap();
        let mut writer = writer(&fixture);
        for person in [&source, &missing] {
            writer
                .set_participant(person.id.as_str(), &person.name, true)
                .unwrap();
        }
        writer
            .append(CaptureTrack::System, 0, &speech(2.0, -18.0, 3))
            .unwrap();
        let input = final_input(writer.dir());
        writer.finish(2_000_000_000, "user").unwrap();

        let duplicate = ops::person_duplicates(&fixture.lib, model, None)
            .unwrap()
            .remove(0);
        let request = ops::MergePeople {
            keep: keep.id.clone(),
            source: source.id.clone(),
            revision: duplicate.revision,
        };
        ops::with_index(&fixture.lib, |tx, index| {
            ops::merge_people(tx, index, &request, model, None)
        })
        .unwrap();
        assert!(!fixture.lib.person_dir(&source.id).exists());

        let lease = claim(&fixture, &input, false);
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &FakeEncoder::default(),
            &NoQuery,
        )
        .unwrap();
        let meeting = fixture.lib.load_meeting(&id).unwrap();
        let participant = |name: &str| {
            meeting
                .participants
                .iter()
                .find(|p| p.name.as_deref() == Some(name))
                .unwrap()
        };
        assert_eq!(participant(&source.name).person_id.as_ref(), Some(&keep.id));
        assert_eq!(participant(&missing.name).person_id, None);
    }

    #[test]
    fn final_worker_refuses_a_foreign_or_unbound_journal() {
        let fixture = Fixture::new();
        let origin = Fixture::new();
        let mut writer = writer(&origin);
        writer
            .append(CaptureTrack::System, 0, &speech(2.0, -18.0, 3))
            .unwrap();
        let input = final_input(writer.dir());
        let mut manifest = writer.finish(2_000_000_000, "user").unwrap();
        let lease = claim(&fixture, &input, false);
        for code in [journal::LIBRARY_MISMATCH, journal::LIBRARY_UNKNOWN] {
            let before = std::fs::read(input.journal_dir.join(journal::MANIFEST_FILE)).unwrap();
            let error = run(
                &fixture.env(),
                &mut models(),
                &lease,
                &input,
                false,
                &FakeEncoder::default(),
                &NoQuery,
            )
            .unwrap_err();
            assert_eq!(error.code(), code);
            assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
            assert_eq!(
                std::fs::read(input.journal_dir.join(journal::MANIFEST_FILE)).unwrap(),
                before
            );
            manifest.meta.library_root = None;
            journal::write_manifest(&input.journal_dir, &manifest).unwrap();
        }
    }

    #[test]
    fn record_echo_duplicate_dropped_but_matching_user_speech_kept() {
        let mut fixture = Fixture::new();
        enroll_me(&mut fixture);
        let remote = speech(4.0, -35.0, 1);
        let near = speech(4.0, -8.0, 2);
        let (echo, _) = analyze_tracks(
            &fixture.env(),
            &remote,
            &remote,
            16000,
            Setup::SpeakersMic,
            Some(EchoHandling::None),
            &mut models(),
            &[],
            &[],
        )
        .unwrap();
        assert!(echo.segments.iter().all(|s| s.track == "system"));
        let (user, _) = analyze_tracks(
            &fixture.env(),
            &near,
            &remote,
            16000,
            Setup::SpeakersMic,
            Some(EchoHandling::None),
            &mut models(),
            &[],
            &[],
        )
        .unwrap();
        assert!(user.segments.iter().any(|s| s.track == "mic"));
        assert!(
            user.speakers
                .iter()
                .any(|s| s.track == "mic" && s.person_id.is_some())
        );
        fixture.config.identity.clear();
        let (unenrolled, _) = analyze_tracks(
            &fixture.env(),
            &remote,
            &remote,
            16000,
            Setup::SpeakersMic,
            Some(EchoHandling::None),
            &mut models(),
            &[],
            &[],
        )
        .unwrap();
        assert!(unenrolled.segments.iter().any(|s| s.track == "mic"));
    }
    /// PL28: a non-room mic segment whose bounds fall between samples is
    /// embedded once. Scoring embeds the very clip the centroid needs, so the
    /// centroid reuses it and equals `centroid_of` over the kept speech.
    #[test]
    fn record_mic_segment_between_samples_is_embedded_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        struct FractionalVad;
        impl Vad for FractionalVad {
            fn segments(&mut self, samples: &[f32]) -> Result<Vec<SpeechSegment>> {
                Ok(if samples.iter().any(|s| s.abs() > 1e-6) {
                    vec![SpeechSegment {
                        start_s: 1.00004,
                        end_s: 4.00004,
                    }]
                } else {
                    Vec::new()
                })
            }
        }
        // Text without token times: the segment is the speech region.
        struct UntimedAsr;
        impl broscribe_models::Asr for UntimedAsr {
            fn transcribe(&self, _: &[f32]) -> Result<broscribe_models::AsrOutput> {
                Ok(broscribe_models::AsrOutput {
                    text: "synthetic speech".into(),
                    tokens: Vec::new(),
                })
            }
            fn accepts_hotwords(&self) -> bool {
                false
            }
        }
        struct NoTurns;
        impl broscribe_models::Diarizer for NoTurns {
            fn diarize(&self, _: &[f32]) -> Result<Vec<broscribe_models::DiarSegment>> {
                Ok(Vec::new())
            }
        }
        struct Counting(Arc<AtomicUsize>);
        impl SpeakerEmbedder for Counting {
            fn model_id(&self) -> &str {
                "synthetic-level"
            }
            fn dim(&self) -> usize {
                2
            }
            fn embed(&self, samples: &[f32]) -> Result<Option<Vec<f32>>> {
                self.0.fetch_add(1, SeqCst);
                LevelEmbedder.embed(samples)
            }
        }
        let mut fixture = Fixture::new();
        enroll_me(&mut fixture);
        let mic = speech(5.0, -8.0, 2);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut models = models();
        models.vad = Box::new(FractionalVad);
        models.asr = Box::new(UntimedAsr);
        models.diarizer = Box::new(NoTurns);
        models.embedder = Box::new(Counting(calls.clone()));
        let (analysis, _) = analyze_tracks(
            &fixture.env(),
            &mic,
            &vec![0.0; mic.len()],
            16000,
            Setup::SpeakersMic,
            Some(EchoHandling::None),
            &mut models,
            &[],
            &[],
        )
        .unwrap();
        let kept: Vec<_> = analysis
            .segments
            .iter()
            .filter(|s| s.track == "mic")
            .map(|s| (s.start, s.end, s.text.as_str()))
            .collect();
        assert_eq!(kept, [(1.00004, 4.00004, "synthetic speech")]);
        assert_eq!(calls.load(SeqCst), 1, "one embedding of the kept segment");
        let me = analysis.speakers.iter().find(|s| s.track == "mic").unwrap();
        assert!(me.person_id.is_some(), "{me:?}");
        let expected = analyze::centroid_of(&mic, &[(1.00004, 4.00004)], &LevelEmbedder)
            .unwrap()
            .unwrap();
        assert_eq!(me.centroid.as_ref(), Some(&expected));
    }
    #[test]
    fn record_provisional_preserves_journal_then_final_includes_post_cutoff_audio_once() {
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        let audio = speech(2.0, -18.0, 3);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &audio).unwrap();
        }
        let dir = writer.dir().to_owned();
        let cutoff = writer.cutoff();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir.clone(),
            cutoff: Some(broscribe_ipc::ChunkCutoff {
                chunks: BTreeMap::from([
                    (broscribe_ipc::Track::Mic, cutoff.mic.unwrap()),
                    (broscribe_ipc::Track::System, cutoff.system.unwrap()),
                ]),
                recording_s: 2.0,
            }),
        };
        let encoder = FakeEncoder::default();
        let lease = claim(&fixture, &input, true);
        let provisional = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            true,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        let meeting = fixture.lib.load_meeting(&provisional).unwrap();
        assert_eq!(meeting.processing.status, ProcessingStatus::Provisional);
        assert_eq!(meeting.duration_s, 2.0);
        assert!(dir.exists());
        assert!(encoder.calls.lock().unwrap().is_empty());
        assert!(meeting.recording.is_none());
        let mut retention = fixture.config.clone();
        retention.retention = broscribe_core::config::Retention::DeleteImmediately {
            wait_until_named: false,
        };
        assert!(matches!(
            broscribe_core::retention::eligible(&meeting, &retention, schema::now(), false),
            broscribe_core::retention::Verdict::Keep(_)
        ));
        writer.pause(2_000_000_000).unwrap();
        writer.resume(3_000_000_000).unwrap();
        for track in CaptureTrack::ALL {
            writer.append(track, 3_000_000_000, &audio).unwrap();
        }
        writer
            .bookmark(3_500_000_000, Some("synthetic marker".into()))
            .unwrap();
        writer.note(3_500_000_000, "Synthetic note").unwrap();
        writer.finish(5_000_000_000, "user").unwrap();
        let input = RecordInput {
            cutoff: None,
            ..input
        };
        let lease = claim(&fixture, &input, false);
        let final_id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        assert_eq!(final_id, provisional);
        assert!(!dir.exists());
        let meeting = fixture.lib.load_meeting(&final_id).unwrap();
        assert_eq!(meeting.processing.status, ProcessingStatus::Done);
        assert_eq!(meeting.duration_s, 4.0);
        assert_eq!(meeting.bookmarks[0].at_s, 2.5);
        assert_eq!(meeting.notes[0].at_s, 2.5);
        assert!(meeting.processing.echo_report.is_some());
        let md = std::fs::read_to_string(
            fixture
                .lib
                .meeting_dir(&final_id)
                .unwrap()
                .join("meeting.md"),
        )
        .unwrap();
        assert!(md.contains("## Notes"));
        assert!(md.contains("Synthetic note"));
        assert_eq!(fixture.lib.list_meeting_dirs().unwrap().len(), 1);
    }
    #[test]
    fn record_failed_encoder_verification_never_deletes_journal() {
        let fixture = Fixture::new();
        let mut writer = writer(&fixture);
        let audio = speech(2.0, -18.0, 4);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &audio).unwrap();
        }
        let dir = writer.dir().to_owned();
        writer.finish(2_000_000_000, "user").unwrap();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir.clone(),
            cutoff: None,
        };
        let lease = claim(&fixture, &input, false);
        assert!(
            run(
                &fixture.env(),
                &mut models(),
                &lease,
                &input,
                false,
                &FakeEncoder::default(),
                &NoQuery,
            )
            .is_err()
        );
        // The audio that failed verification is this attempt's: it is gone.
        assert!(
            std::fs::read_dir(lease.job_dir())
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().starts_with("audio."))
        );
        lease.release(&fixture.lib).unwrap();
        assert!(dir.exists());
        assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
    }
    #[test]
    fn record_retry_after_failure_between_audio_move_and_save_reuses_the_meeting_id() {
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        let audio = speech(2.0, -18.0, 4);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &audio).unwrap();
        }
        let dir = writer.dir().to_owned();
        writer.finish(2_000_000_000, "user").unwrap();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir.clone(),
            cutoff: None,
        };
        let lease = claim(&fixture, &input, false);
        let encoder = FakeEncoder::default();
        crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::RecordAfterRename));
        let err = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap_err();
        assert!(err.to_string().contains("failpoint"), "{err}");
        assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
        assert!(dir.exists());
        let chosen = std::fs::read_to_string(lease.job_dir().join(MEETING_ID_FILE)).unwrap();
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        assert_eq!(id.as_str(), chosen.trim());
        // One meeting folder, holding the audio: none left orphaned.
        let mut folders = Vec::new();
        for year in std::fs::read_dir(fixture.lib.root()).unwrap() {
            let year = year.unwrap().path();
            let is_year = year
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.len() == 4 && n.bytes().all(|b| b.is_ascii_digit()));
            if !is_year {
                continue;
            }
            for month in std::fs::read_dir(&year).unwrap() {
                for folder in std::fs::read_dir(month.unwrap().path()).unwrap() {
                    folders.push(folder.unwrap().path());
                }
            }
        }
        assert_eq!(folders, vec![fixture.lib.meeting_dir(&id).unwrap()]);
        assert!(
            folders[0]
                .join(format!("audio.{}", encoder.extension()))
                .is_file()
        );
        assert!(!dir.exists());
    }
    #[test]
    fn record_analysis_reads_a_16_khz_timeline_and_echo_handling_works_on_it() {
        let fixture = Fixture::new();
        let mut spec = broscribe_dsp::synth::EchoSceneSpec::laptop(12.0);
        spec.rate = 48_000;
        let scene = broscribe_dsp::synth::echo_scene(&spec);
        let mut writer = JournalWriter::create(
            &fixture._tmp.path().join("journal"),
            "synthetic-session",
            SessionMeta::default(),
            2.0,
            std::process::id(),
        )
        .unwrap();
        for track in CaptureTrack::ALL {
            writer.begin_segment(track, 0, None, 48_000, 0).unwrap();
        }
        writer.append(CaptureTrack::Mic, 0, &scene.mic).unwrap();
        writer
            .append(CaptureTrack::System, 0, &scene.system)
            .unwrap();
        let dir = writer.dir().to_owned();
        let manifest = writer.finish(12_000_000_000, "user").unwrap();
        let device = Timeline::plan(&dir, &manifest, &TimelineOptions::default()).unwrap();
        assert_eq!(device.sample_rate, 48_000);
        let timeline = plan_analysis_timeline(&dir, &manifest, None).unwrap();
        assert_eq!(timeline.sample_rate, TARGET_RATE);
        let mic = timeline.read_track(CaptureTrack::Mic).unwrap();
        let system = timeline.read_track(CaptureTrack::System).unwrap();
        assert_eq!(mic.len(), system.len());
        assert!((mic.len() as f64 - 12.0 * f64::from(TARGET_RATE)).abs() <= 16.0);
        let echo = broscribe_dsp::process_echo(
            &mic,
            &system,
            timeline.sample_rate,
            EchoHandling::FullAec,
            &EchoConfig::default(),
        )
        .unwrap();
        let err_ms = (echo.report.alignment.lag_s_at(6.0) - spec.echo_delay_s).abs() * 1000.0;
        assert!(err_ms <= 1.0, "echo delay off by {err_ms:.3} ms");
        assert_ne!(echo.report.applied, EchoHandling::None, "{:?}", echo.report);
    }
    #[test]
    fn record_roster_updated_after_the_cutoff_still_yields_the_union() {
        let fixture = Fixture::new();
        let dir = tempfile::tempdir().unwrap();
        let member = |name: &str| broscribe_readers::RosterMember {
            name: name.into(),
            email: None,
            is_self: false,
        };
        // The union's last snapshot came after the provisional cutoff.
        let roster = broscribe_readers::RosterEvent {
            observed_s: 30.0,
            members: vec![member("Alex Example"), member("Casey Example")],
        };
        broscribe_core::fsutil::write_json_atomic(&dir.path().join("roster.json"), &roster)
            .unwrap();
        let mut meeting = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            Source::Recording,
            schema::now(),
        );
        meeting.duration_s = 10.0;
        let (members, _) = reader_evidence(dir.path(), &meeting, Setup::Headphones).unwrap();
        assert_eq!(members, roster.members);
    }
    #[test]
    fn record_unreadable_calendar_journal_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("calendar.json"), b"{incomplete").unwrap();
        let now = chrono::Utc::now();
        assert!(
            cached_calendar(dir.path(), now, now, None, &NoQuery)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn record_caption_journal_names_only_system_and_enrolls_weak_caption_voiceprint() {
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        let audio = speech(2.0, -18.0, 5);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &audio).unwrap();
        }
        let dir = writer.dir().to_owned();
        writer.finish(2_000_000_000, "user").unwrap();
        let roster = broscribe_readers::RosterEvent {
            observed_s: 1.0,
            members: vec![broscribe_readers::RosterMember {
                name: "Alex Example".into(),
                email: Some("alex@example.invalid".into()),
                is_self: false,
            }],
        };
        broscribe_core::fsutil::write_json_atomic(&dir.join("roster.json"), &roster).unwrap();
        let event = broscribe_readers::ReaderEvent::Caption(broscribe_readers::CaptionEvent {
            caption_id: "synthetic-caption".into(),
            revision: 1,
            speaker_name: "Alex Example".into(),
            is_self: false,
            text: "alpha beta gamma delta epsilon".into(),
            observed_s: 1.9,
            start_s: 0.0,
            end_s: 2.0,
        });
        std::fs::write(
            dir.join("captions.jsonl"),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir,
            cutoff: None,
        };
        let lease = claim(&fixture, &input, false);
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &FakeEncoder::default(),
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        let meeting = fixture.lib.load_meeting(&id).unwrap();
        assert!(
            meeting
                .speakers
                .iter()
                .filter(|s| s.track == "mic")
                .all(|s| s.person_id.is_none())
        );
        let person = fixture
            .lib
            .list_people()
            .unwrap()
            .into_iter()
            .find(|p| p.name == "Alex Example")
            .unwrap();
        assert!(person.needs_review);
        assert!(
            meeting
                .speakers
                .iter()
                .any(|s| s.track == "system" && s.person_id.as_ref() == Some(&person.id))
        );
        let prints = fixture.lib.load_voiceprints(&person.id).unwrap();
        assert!(
            prints
                .embeddings
                .iter()
                .any(|v| v.source.kind == VoiceprintKind::Captions && v.strength == Strength::Weak)
        );
        assert!(
            meeting
                .participants
                .iter()
                .any(|p| p.evidence.iter().any(|e| e == "roster"))
        );
    }

    /// A provisional pass, then `between`, then the final pass of a session
    /// whose journal captions (no roster) name the system voice "Alex
    /// Example"; returns the meeting both passes published.
    fn record_caption_passes(fixture: &mut Fixture, between: impl FnOnce(&Fixture)) -> MeetingId {
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(fixture);
        let audio = speech(2.0, -18.0, 6);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &audio).unwrap();
        }
        let dir = writer.dir().to_owned();
        let event = broscribe_readers::ReaderEvent::Caption(broscribe_readers::CaptionEvent {
            caption_id: "synthetic-caption".into(),
            revision: 1,
            speaker_name: "Alex Example".into(),
            is_self: false,
            text: "alpha beta gamma delta epsilon".into(),
            observed_s: 1.9,
            start_s: 0.0,
            end_s: 2.0,
        });
        std::fs::write(
            dir.join("captions.jsonl"),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        let cutoff = writer.cutoff();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir,
            cutoff: Some(broscribe_ipc::ChunkCutoff {
                chunks: BTreeMap::from([
                    (broscribe_ipc::Track::Mic, cutoff.mic.unwrap()),
                    (broscribe_ipc::Track::System, cutoff.system.unwrap()),
                ]),
                recording_s: 2.0,
            }),
        };
        let encoder = FakeEncoder::default();
        let lease = claim(fixture, &input, true);
        let provisional = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            true,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        between(fixture);
        writer.finish(2_000_000_000, "user").unwrap();
        let input = RecordInput {
            cutoff: None,
            ..input
        };
        let lease = claim(fixture, &input, false);
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        assert_eq!(id, provisional);
        id
    }

    fn caption_person(fixture: &Fixture) -> Person {
        let people = fixture.lib.list_people().unwrap();
        assert_eq!(people.len(), 1, "{people:?}");
        people.into_iter().next().unwrap()
    }

    #[test]
    fn record_final_pass_keeps_a_caption_person_renamed_after_the_provisional_pass() {
        let mut fixture = Fixture::new();
        let id = record_caption_passes(&mut fixture, |fixture| {
            let mut person = caption_person(fixture);
            assert_eq!(person.name, "Alex Example");
            person.name = "Alexandra Renamed".into();
            ops::with_index(&fixture.lib, |tx, index| {
                tx.save_person(&person)?;
                index.upsert_person(tx, &person)
            })
            .unwrap();
        });
        let person = caption_person(&fixture);
        assert_eq!(person.name, "Alexandra Renamed");
        let meeting = fixture.lib.load_meeting(&id).unwrap();
        assert!(
            meeting
                .speakers
                .iter()
                .any(|s| s.track == "system" && s.person_id.as_ref() == Some(&person.id))
        );
    }

    #[test]
    fn record_final_pass_never_recreates_a_person_forgotten_after_the_provisional_pass() {
        let mut fixture = Fixture::new();
        let id = record_caption_passes(&mut fixture, |fixture| {
            let person = caption_person(fixture);
            ops::with_index(&fixture.lib, |tx, index| {
                ops::forget_person(tx, index, person.id.as_str())
            })
            .unwrap();
        });
        let people = fixture.lib.list_people().unwrap();
        assert!(people.is_empty(), "{people:?}");
        let meeting = fixture.lib.load_meeting(&id).unwrap();
        assert!(meeting.speakers.iter().all(|s| s.person_id.is_none()));
    }

    #[test]
    fn record_final_pass_moves_samples_of_speakers_it_renumbers() {
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        let audio = speech(2.0, -18.0, 7);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &audio).unwrap();
        }
        let cutoff = writer.cutoff();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: writer.dir().to_owned(),
            cutoff: Some(broscribe_ipc::ChunkCutoff {
                chunks: BTreeMap::from([
                    (broscribe_ipc::Track::Mic, cutoff.mic.unwrap()),
                    (broscribe_ipc::Track::System, cutoff.system.unwrap()),
                ]),
                recording_s: 2.0,
            }),
        };
        let encoder = FakeEncoder::default();
        let lease = claim(&fixture, &input, true);
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            true,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();

        // The provisional pass numbered its speakers 10 higher than the
        // final pass will. The user confirmed its system voice as Ana, and
        // her confirmation sample names that speaker.
        let shift = |s: SpeakerId| SpeakerId::new(s.number() + 10).unwrap();
        let mut earlier = fixture.lib.load_meeting(&id).unwrap();
        for speaker in &mut earlier.speakers {
            speaker.id = shift(speaker.id);
        }
        for segment in &mut earlier.segments {
            segment.speaker = segment.speaker.map(shift);
        }
        let turn = earlier
            .segments
            .iter()
            .find(|s| s.track == "system" && s.speaker.is_some())
            .unwrap()
            .clone();
        let named = turn.speaker.unwrap();
        let ana = Person::new(fixture.lib.new_person_id(), "Ana Invented");
        let speaker = earlier.speaker_mut(named).unwrap();
        speaker.assignment = Assignment::Confirmed;
        speaker.person_id = Some(ana.id.clone());
        let vector = speaker.centroid.as_ref().unwrap().vector().unwrap();
        let print = Voiceprint {
            id: "synthetic-confirmation".into(),
            model: earlier.models.speaker_embedding.clone().unwrap(),
            dim: vector.len(),
            vector_b64: schema::encode_vector(&vector),
            source: VoiceprintSource {
                kind: VoiceprintKind::Confirmation,
                meeting_id: Some(id.clone()),
                sha256: None,
                start: turn.start,
                end: turn.end,
                ranges: Vec::new(),
                speaker_id: Some(named),
                speaker_namespace: earlier.speaker_namespace.clone(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        };
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&ana)?;
                tx.save_meeting(&earlier)?;
                tx.upsert_voiceprints(&ana.id, vec![print])
            })
            .unwrap();

        writer.finish(2_000_000_000, "user").unwrap();
        let input = RecordInput {
            cutoff: None,
            ..input
        };
        let lease = claim(&fixture, &input, false);
        let final_id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        assert_eq!(final_id, id);
        let meeting = fixture.lib.load_meeting(&id).unwrap();
        assert_eq!(meeting.processing.status, ProcessingStatus::Done);
        assert!(!meeting.speaker_namespace.is_empty());
        let confirmed: Vec<_> = meeting
            .speakers
            .iter()
            .filter(|s| s.assignment == Assignment::Confirmed)
            .collect();
        assert_eq!(confirmed.len(), 1, "{:?}", meeting.speakers);
        assert_eq!(confirmed[0].person_id.as_ref(), Some(&ana.id));
        assert_eq!(confirmed[0].track, "system");
        assert_ne!(confirmed[0].id, named);
        // The sample follows its speaker into the final pass's numbering.
        let sample = fixture.lib.load_voiceprints(&ana.id).unwrap().embeddings;
        let sample = sample
            .iter()
            .find(|v| v.id == "synthetic-confirmation")
            .unwrap();
        assert_eq!(sample.source.speaker_id, Some(confirmed[0].id));
        assert_eq!(sample.source.speaker_namespace, meeting.speaker_namespace);
    }

    /// An 8 s system voice the diarizer splits in two halves (one voice, two
    /// clusters), with `captions` `(name, start, end, text)`: the meetings a
    /// provisional pass and then the final pass published.
    fn record_split_voice(
        fixture: &mut Fixture,
        captions: &[(&str, f64, f64, &str)],
    ) -> (Meeting, Meeting) {
        fixture.config.identity.insert(
            "synthetic-level".into(),
            Thresholds {
                merge: Some(0.5),
                auto_merge: Some(0.9),
                ..Default::default()
            },
        );
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let split = || {
            let mut set = models();
            set.asr = Box::new(FakeAsr::new(&[
                "w0", "w1", "w2", "w3", "w4", "w5", "w6", "w7", "w8", "w9",
            ]));
            set.diarizer = Box::new(broscribe_models::fakes::ScheduleDiarizer {
                schedule: vec![
                    broscribe_models::DiarSegment {
                        start_s: 0.0,
                        end_s: 4.0,
                        speaker: 0,
                    },
                    broscribe_models::DiarSegment {
                        start_s: 4.0,
                        end_s: 8.0,
                        speaker: 1,
                    },
                ],
            });
            set
        };
        let mut writer = writer(fixture);
        let voice = speech(8.0, -18.0, 6);
        writer.append(CaptureTrack::System, 0, &voice).unwrap();
        writer
            .append(CaptureTrack::Mic, 0, &vec![0.0; voice.len()])
            .unwrap();
        let dir = writer.dir().to_owned();
        let lines: String = captions
            .iter()
            .enumerate()
            .map(|(i, (name, start, end, text))| {
                let event =
                    broscribe_readers::ReaderEvent::Caption(broscribe_readers::CaptionEvent {
                        caption_id: format!("synthetic-caption-{i}"),
                        revision: 1,
                        speaker_name: (*name).into(),
                        is_self: false,
                        text: (*text).into(),
                        observed_s: *end,
                        start_s: *start,
                        end_s: *end,
                    });
                format!("{}\n", serde_json::to_string(&event).unwrap())
            })
            .collect();
        std::fs::write(dir.join("captions.jsonl"), lines).unwrap();
        let cutoff = writer.cutoff();
        let input = RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir,
            cutoff: Some(broscribe_ipc::ChunkCutoff {
                chunks: BTreeMap::from([
                    (broscribe_ipc::Track::Mic, cutoff.mic.unwrap()),
                    (broscribe_ipc::Track::System, cutoff.system.unwrap()),
                ]),
                recording_s: 8.0,
            }),
        };
        let encoder = FakeEncoder::default();
        let lease = claim(fixture, &input, true);
        let id = run(
            &fixture.env(),
            &mut split(),
            &lease,
            &input,
            true,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        let provisional = fixture.lib.load_meeting(&id).unwrap();
        writer.finish(2_000_000_000, "user").unwrap();
        let input = RecordInput {
            cutoff: None,
            ..input
        };
        let lease = claim(fixture, &input, false);
        let id = run(
            &fixture.env(),
            &mut split(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        (provisional, fixture.lib.load_meeting(&id).unwrap())
    }

    fn system_speakers(m: &Meeting) -> Vec<SpeakerId> {
        m.speakers
            .iter()
            .filter(|s| s.track == "system")
            .map(|s| s.id)
            .collect()
    }

    #[test]
    fn record_final_pass_groups_an_over_split_voice_but_never_a_provisional_pass() {
        let mut fixture = Fixture::new();
        let (provisional, meeting) = record_split_voice(&mut fixture, &[]);
        assert_eq!(provisional.processing.status, ProcessingStatus::Provisional);
        assert_eq!(system_speakers(&provisional).len(), 2);
        assert!(provisional.speaker_groups.is_empty());
        assert_eq!(meeting.processing.status, ProcessingStatus::Done);
        let system = system_speakers(&meeting);
        assert_eq!(system.len(), 2, "clusters are never folded");
        assert_eq!(
            meeting.speaker_groups.len(),
            1,
            "{:?}",
            meeting.speaker_groups
        );
        let group = &meeting.speaker_groups[0];
        assert!(matches!(
            group.origin,
            schema::GroupOrigin::Auto { score } if score >= 0.9
        ));
        assert!(system.iter().all(|s| group.contains(*s)));
    }

    #[test]
    fn record_final_pass_never_groups_clusters_the_captions_prove_to_be_different_people() {
        let mut fixture = Fixture::new();
        let (_, meeting) = record_split_voice(
            &mut fixture,
            &[
                ("Ana Example", 0.0, 4.0, "w0 w1 w2 w3 w4"),
                ("Bob Example", 4.0, 8.0, "w5 w6 w7 w8 w9"),
            ],
        );
        let people = fixture.lib.list_people().unwrap();
        let named: BTreeSet<_> = meeting
            .speakers
            .iter()
            .filter(|s| s.track == "system")
            .filter_map(|s| s.person_id.clone())
            .collect();
        let names: BTreeSet<_> = people
            .iter()
            .filter(|p| named.contains(&p.id))
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, BTreeSet::from(["Ana Example", "Bob Example"]));
        assert!(
            meeting.speaker_groups.is_empty(),
            "{:?}",
            meeting.speaker_groups
        );
    }

    #[test]
    fn record_revoked_calendar_during_asr_or_encoding_publishes_nothing_and_retries_cleanly() {
        use std::sync::Arc;
        fn revoke(authority: &FakeCalendarAuthority, change: &str) {
            if change == "native" {
                *authority.generation.lock().unwrap() = None;
                return;
            }
            if change == "google" {
                // Signing in again or a denial moves the account generation.
                let mut live = authority.google_authority.lock().unwrap();
                live.as_mut().unwrap().generation += 1;
                return;
            }
            if change == "google-reselected" {
                // Reselecting calendars moves only the selection generation.
                let mut live = authority.google_authority.lock().unwrap();
                live.as_mut().unwrap().selection_generation += 1;
                return;
            }
            let mut accounts = authority.remote.lock().unwrap();
            if change == "removed" {
                accounts.remove(&remote_account('b'));
                return;
            }
            let live = accounts.get_mut(&remote_account('b')).unwrap();
            match change {
                "reconnected" => live.generation += 1,
                "reselected" => live.selection_generation += 1,
                "target-denied" => live.selected_calendars.clear(),
                "wrong-account" => live.account = remote_account('a'),
                _ => unreachable!(),
            }
        }
        struct RevokeAsr {
            inner: Box<dyn broscribe_models::Asr + Send>,
            authority: Arc<FakeCalendarAuthority>,
            change: &'static str,
        }
        impl broscribe_models::Asr for RevokeAsr {
            fn transcribe(&self, audio: &[f32]) -> Result<broscribe_models::AsrOutput> {
                revoke(&self.authority, self.change);
                self.inner.transcribe(audio)
            }
            fn accepts_hotwords(&self) -> bool {
                self.inner.accepts_hotwords()
            }
            fn set_hotwords(&mut self, words: &[String]) -> Result<()> {
                self.inner.set_hotwords(words)
            }
        }
        struct RevokeEncoder(Arc<FakeCalendarAuthority>, &'static str);
        impl AudioEncoder for RevokeEncoder {
            fn name(&self) -> &str {
                "fake"
            }
            fn extension(&self) -> &str {
                "json"
            }
            fn encode(
                &self,
                out: &std::path::Path,
                settings: &broscribe_platform::EncodeSettings,
                tracks: &mut [broscribe_platform::EncoderTrack<'_>],
            ) -> Result<Vec<broscribe_platform::EncodedTrack>> {
                let result = FakeEncoder::default().encode(out, settings, tracks)?;
                revoke(&self.0, self.1);
                Ok(result)
            }
        }
        let google = |change: &str| change.starts_with("google");
        let provider_wide = |change: &str| change == "native" || google(change);
        for (change, during_asr) in [
            "native",
            "google",
            "google-reselected",
            "removed",
            "reconnected",
            "reselected",
            "target-denied",
            "wrong-account",
        ]
        .into_iter()
        .flat_map(|change| [true, false].map(move |during_asr| (change, during_asr)))
        {
            let mut fixture = Fixture::new();
            fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
            let person = Person::new(fixture.lib.new_person_id(), "Confirmed Example");
            fixture.lib.write(|tx| tx.save_person(&person)).unwrap();
            // As saved, with the hue the library assigned.
            let person = fixture.lib.load_person(&person.id).unwrap();
            let mut writer = writer(&fixture);
            writer.set_meta(meta(Some("User title"), false)).unwrap();
            writer
                .set_participant(person.id.as_str(), &person.name, true)
                .unwrap();
            for track in CaptureTrack::ALL {
                writer.append(track, 0, &speech(2.0, -18.0, 3)).unwrap();
            }
            let dir = writer.dir().to_owned();
            let at = writer.manifest().started_at;
            writer.finish(2_000_000_000, "user").unwrap();
            broscribe_core::fsutil::write_json_atomic(
                &dir.join("calendar.json"),
                &broscribe_integrations::CalendarCache {
                    events: if change == "native" {
                        vec![calendar_event(
                            broscribe_integrations::IntegrationId::SystemCalendar,
                            "Revoked calendar title",
                            at,
                            Some(7),
                            vec![attendee("Revoked Example", Some("revoked@example.invalid"))],
                        )]
                    } else if google(change) {
                        vec![google_event(
                            "Revoked calendar title",
                            at,
                            Some(GOOGLE_AUTHORITY),
                            vec![attendee("Revoked Example", Some("revoked@example.invalid"))],
                        )]
                    } else {
                        // Identical meeting copies retain both authorities. B changes
                        // even though A is the first hint chosen by matching.
                        ['a', 'b']
                            .map(|handle| {
                                let mut event = remote_event(remote_account(handle), at);
                                event.title = Some("Revoked calendar title".into());
                                event.attendees = vec![attendee(
                                    "Revoked Example",
                                    Some("revoked@example.invalid"),
                                )];
                                event
                            })
                            .to_vec()
                    },
                },
            )
            .unwrap();
            let input = RecordInput {
                session_id: "synthetic-session".into(),
                journal_dir: dir.clone(),
                cutoff: None,
            };
            let lease = claim(&fixture, &input, false);
            let authority = Arc::new(FakeCalendarAuthority::default());
            *authority.generation.lock().unwrap() = Some(7);
            *authority.google_authority.lock().unwrap() = Some(GOOGLE_AUTHORITY);
            if !provider_wide(change) {
                for handle in ['a', 'b'] {
                    authority
                        .remote
                        .lock()
                        .unwrap()
                        .insert(remote_account(handle), remote_live(remote_account(handle)));
                }
            }
            let mut analysis_models = models();
            if during_asr {
                analysis_models.asr = Box::new(RevokeAsr {
                    inner: analysis_models.asr,
                    authority: authority.clone(),
                    change,
                });
            }
            let encoder: Box<dyn AudioEncoder> = if during_asr {
                Box::new(FakeEncoder::default())
            } else {
                Box::new(RevokeEncoder(authority.clone(), change))
            };
            let error = run(
                &fixture.env(),
                &mut analysis_models,
                &lease,
                &input,
                false,
                encoder.as_ref(),
                authority.as_ref(),
            )
            .unwrap_err();
            assert_eq!(error.code(), "calendar_authority_changed");
            assert!(dir.join("session.json").exists());
            // No attempt's encoded audio stays in the job folder.
            assert!(
                std::fs::read_dir(lease.job_dir())
                    .unwrap()
                    .flatten()
                    .all(|e| !e.file_name().to_string_lossy().starts_with("audio."))
            );
            assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
            assert_eq!(fixture.lib.list_people().unwrap(), vec![person.clone()]);
            assert!(
                fixture
                    .lib
                    .load_voiceprints(&person.id)
                    .unwrap()
                    .embeddings
                    .is_empty()
            );
            let id = run(
                &fixture.env(),
                &mut models(),
                &lease,
                &input,
                false,
                &FakeEncoder::default(),
                authority.as_ref(),
            )
            .unwrap();
            let meeting = fixture.lib.load_meeting(&id).unwrap();
            assert_eq!(meeting.title.as_deref(), Some("User title"));
            assert!(meeting.participants.iter().any(|p| {
                p.person_id.as_ref() == Some(&person.id) && p.evidence.iter().any(|e| e == "user")
            }));
            assert_eq!(
                meeting
                    .participants
                    .iter()
                    .any(|p| p.evidence.iter().any(|e| e == "calendar")),
                !provider_wide(change),
            );
            assert_eq!(
                meeting
                    .hotwords
                    .iter()
                    .any(|word| word == "Revoked Example"),
                !provider_wide(change),
            );
        }
    }

    /// The user removes a tag while "Process now" inference runs (here from
    /// inside the ASR call, on the real journal writer). The provisional
    /// commit reads the journal again, records the removal, and the final
    /// meeting keeps it removed; untouched tags stay.
    #[test]
    fn record_tag_removed_during_process_now_stays_removed_in_the_final_meeting() {
        use std::sync::{Arc, Mutex};
        struct RemoveTagAsr {
            inner: Box<dyn broscribe_models::Asr + Send>,
            writer: Arc<Mutex<Option<JournalWriter>>>,
        }
        impl broscribe_models::Asr for RemoveTagAsr {
            fn transcribe(&self, audio: &[f32]) -> Result<broscribe_models::AsrOutput> {
                if let Some(writer) = self.writer.lock().unwrap().as_mut() {
                    writer.remove_tag("alpha").unwrap();
                }
                self.inner.transcribe(audio)
            }
            fn accepts_hotwords(&self) -> bool {
                self.inner.accepts_hotwords()
            }
            fn set_hotwords(&mut self, words: &[String]) -> Result<()> {
                self.inner.set_hotwords(words)
            }
        }
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        writer.add_tag("alpha").unwrap();
        writer.add_tag("beta").unwrap();
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &speech(2.0, -18.0, 3)).unwrap();
        }
        let dir = writer.dir().to_owned();
        let input = provisional_input(&dir, writer.cutoff());
        let shared = Arc::new(Mutex::new(Some(writer)));
        let mut provisional_models = models();
        provisional_models.asr = Box::new(RemoveTagAsr {
            inner: provisional_models.asr,
            writer: shared.clone(),
        });
        let lease = claim(&fixture, &input, true);
        let provisional = run(
            &fixture.env(),
            &mut provisional_models,
            &lease,
            &input,
            true,
            &FakeEncoder::default(),
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        let published = fixture.lib.load_meeting(&provisional).unwrap();
        assert_eq!(published.effective_tags(), ["beta"]);
        assert!(published.tags_removed.iter().any(|t| t == "alpha"));
        let writer = shared.lock().unwrap().take().unwrap();
        assert_eq!(writer.manifest().meta.tags, ["beta"]);
        writer.finish(2_000_000_000, "user").unwrap();
        let input = RecordInput {
            cutoff: None,
            ..input
        };
        let lease = claim(&fixture, &input, false);
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &FakeEncoder::default(),
            &NoQuery,
        )
        .unwrap();
        assert_eq!(id, provisional);
        let saved = fixture.lib.load_meeting(&id).unwrap();
        assert_eq!(
            saved.effective_tags(),
            ["beta"],
            "the removed tag never comes back"
        );
    }

    /// Encodes like [`FakeEncoder`], then the user forgets Me (`person`: the
    /// whole person, else only the voice) while the final job still works.
    struct ForgetMeDuringEncode {
        inner: FakeEncoder,
        lib: broscribe_core::Library,
        me: broscribe_core::PersonId,
        person: bool,
    }

    impl AudioEncoder for ForgetMeDuringEncode {
        fn name(&self) -> &str {
            self.inner.name()
        }
        fn extension(&self) -> &str {
            self.inner.extension()
        }
        fn encode(
            &self,
            out: &std::path::Path,
            settings: &broscribe_platform::encoder::EncodeSettings,
            tracks: &mut [broscribe_platform::encoder::EncoderTrack<'_>],
        ) -> broscribe_platform::Result<Vec<broscribe_platform::encoder::EncodedTrack>> {
            let encoded = self.inner.encode(out, settings, tracks)?;
            ops::with_index(&self.lib, |tx, index| {
                if self.person {
                    ops::forget_person(tx, index, self.me.as_str())
                } else {
                    ops::forget_voice(tx, index, self.me.as_str())
                }
            })
            .unwrap();
            Ok(encoded)
        }
    }

    fn self_person(fixture: &Fixture) -> broscribe_core::PersonId {
        fixture
            .lib
            .list_people()
            .unwrap()
            .into_iter()
            .find(|p| p.is_self)
            .unwrap()
            .id
    }

    fn final_input(dir: &std::path::Path) -> RecordInput {
        RecordInput {
            session_id: "synthetic-session".into(),
            journal_dir: dir.to_owned(),
            cutoff: None,
        }
    }

    #[test]
    fn record_forgetting_me_while_encoding_never_resurrects_me() {
        for person in [false, true] {
            let mut fixture = Fixture::new();
            fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
            enroll_me(&mut fixture);
            let me = self_person(&fixture);
            // Me's voice on the mic, someone else on the system track.
            let mut writer = writer(&fixture);
            writer
                .append(CaptureTrack::Mic, 0, &speech(2.0, -8.0, 2))
                .unwrap();
            writer
                .append(CaptureTrack::System, 0, &speech(2.0, -35.0, 1))
                .unwrap();
            let dir = writer.dir().to_owned();
            writer.finish(2_000_000_000, "user").unwrap();
            let input = final_input(&dir);
            let lease = claim(&fixture, &input, false);
            let encoder = ForgetMeDuringEncode {
                inner: FakeEncoder::default(),
                lib: fixture.lib.clone(),
                me: me.clone(),
                person,
            };
            let id = run(
                &fixture.env(),
                &mut models(),
                &lease,
                &input,
                false,
                &encoder,
                &NoQuery,
            )
            .unwrap();
            lease.release(&fixture.lib).unwrap();
            assert_eq!(
                encoder.inner.calls.lock().unwrap().len(),
                1,
                "the second analysis reuses the verified encoding"
            );
            let meeting = fixture.lib.load_meeting(&id).unwrap();
            assert!(meeting.segments.iter().any(|s| s.track == "mic"));
            assert!(
                meeting
                    .speakers
                    .iter()
                    .all(|s| s.person_id.as_ref() != Some(&me)),
                "person={person}: {:?}",
                meeting.speakers
            );
            assert!(
                meeting
                    .participants
                    .iter()
                    .all(|p| p.person_id.as_ref() != Some(&me))
            );
            // No weak sample (nor centroid evidence) taught back to Me.
            assert!(
                fixture
                    .lib
                    .load_voiceprints(&me)
                    .unwrap()
                    .embeddings
                    .is_empty()
            );
            if person {
                assert!(!fixture.lib.person_dir(&me).exists());
            }
        }
    }

    #[test]
    fn record_mic_speech_dropped_as_echo_against_a_forgotten_voice_is_analysed_again() {
        let journal = |fixture: &Fixture| {
            let mut writer = writer(fixture);
            writer
                .set_meta(SessionMeta {
                    setup: Some("speakers_mic".into()),
                    language: Some("en".into()),
                    echo_handling: Some("none".into()),
                    ..Default::default()
                })
                .unwrap();
            // The mic hears only the loudspeaker: an echo of the remote side.
            let remote = speech(2.0, -35.0, 1);
            for track in CaptureTrack::ALL {
                writer.append(track, 0, &remote).unwrap();
            }
            let dir = writer.dir().to_owned();
            writer.finish(2_000_000_000, "user").unwrap();
            final_input(&dir)
        };
        // With Me enrolled, the echo is dropped from the mic.
        let mut control = Fixture::new();
        control.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        enroll_me(&mut control);
        let input = journal(&control);
        let lease = claim(&control, &input, false);
        let id = run(
            &control.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &FakeEncoder::default(),
            &NoQuery,
        )
        .unwrap();
        let kept = control.lib.load_meeting(&id).unwrap();
        assert!(kept.segments.iter().all(|s| s.track == "system"));
        // Forgetting Me's voice while encoding: that drop decision is stale,
        // so the publication analyses again and keeps the mic speech.
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        enroll_me(&mut fixture);
        let me = self_person(&fixture);
        let input = journal(&fixture);
        let lease = claim(&fixture, &input, false);
        let encoder = ForgetMeDuringEncode {
            inner: FakeEncoder::default(),
            lib: fixture.lib.clone(),
            me,
            person: false,
        };
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        let meeting = fixture.lib.load_meeting(&id).unwrap();
        assert!(meeting.segments.iter().any(|s| s.track == "mic"));
    }

    #[test]
    fn record_a_holder_that_lost_the_job_never_touches_the_current_attempts_audio() {
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &speech(2.0, -18.0, 3)).unwrap();
        }
        let dir = writer.dir().to_owned();
        writer.finish(2_000_000_000, "user").unwrap();
        let input = final_input(&dir);
        let queue = Queue::open(fixture.lib.root()).unwrap();
        let job = fixture
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    RECORD_FINAL_KIND,
                    &input.session_id,
                    serde_json::to_value(&input).unwrap(),
                )
            })
            .unwrap();
        // A holder whose process was taken for dead, and the takeover.
        let gone = broscribe_platform::fakes::FakeProcessProbe::new(4242);
        let stale = queue.claim(&fixture.lib, &job, &gone).unwrap().unwrap();
        let current = queue
            .claim(&fixture.lib, &job, &fixture.probe)
            .unwrap()
            .unwrap();
        let encoder = FakeEncoder::default();
        // The current attempt's encoding, in progress.
        let theirs = crate::import::staged_audio_path(&current, encoder.extension());
        std::fs::write(&theirs, b"current attempt").unwrap();
        let error = run(
            &fixture.env(),
            &mut models(),
            &stale,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap_err();
        assert_eq!(error.code(), "lease_lost", "{error}");
        assert_eq!(std::fs::read(&theirs).unwrap(), b"current attempt");
        assert!(!crate::import::staged_audio_path(&stale, encoder.extension()).exists());
        assert!(fixture.lib.list_meeting_dirs().unwrap().is_empty());
        let id = run(
            &fixture.env(),
            &mut models(),
            &current,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        assert!(fixture.lib.meeting_exists(&id).unwrap());
        drop(stale);
        current.release(&fixture.lib).unwrap();
    }

    /// Meeting folders `YYYY/MM/<name>` under `root`, with or without
    /// `meeting.json`.
    fn meeting_folders(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let digits = |p: &std::path::Path, n: usize| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|s| s.len() == n && s.bytes().all(|b| b.is_ascii_digit()))
        };
        let dirs = |p: &std::path::Path| -> Vec<std::path::PathBuf> {
            std::fs::read_dir(p)
                .map(|it| {
                    it.flatten()
                        .map(|e| e.path())
                        .filter(|p| p.is_dir())
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut out = Vec::new();
        for year in dirs(root).into_iter().filter(|p| digits(p, 4)) {
            for month in dirs(&year).into_iter().filter(|p| digits(p, 2)) {
                out.extend(dirs(&month));
            }
        }
        out
    }

    #[test]
    fn record_unreadable_meetings_are_isolated_unless_they_may_be_the_sessions_own() {
        // An unrelated unreadable meeting never blocks publication.
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let other = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            Source::Import,
            chrono::DateTime::parse_from_rfc3339("2020-01-02T03:04:00Z").unwrap(),
        );
        let other_dir = fixture
            .lib
            .write(|tx| tx.create_meeting_dir(&other))
            .unwrap();
        std::fs::write(
            other_dir.join(broscribe_core::paths::MEETING_JSON),
            b"{ unreadable",
        )
        .unwrap();
        let mut writer = writer(&fixture);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &speech(2.0, -18.0, 3)).unwrap();
        }
        let dir = writer.dir().to_owned();
        writer.finish(2_000_000_000, "user").unwrap();
        let input = final_input(&dir);
        let lease = claim(&fixture, &input, false);
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &FakeEncoder::default(),
            &NoQuery,
        )
        .unwrap();
        assert!(fixture.lib.meeting_exists(&id).unwrap());

        // An unreadable folder named for this session's start minute may be
        // its own provisional meeting: nothing is published a second time.
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = self::writer(&fixture);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &speech(2.0, -18.0, 3)).unwrap();
        }
        let dir = writer.dir().to_owned();
        let own = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            Source::Recording,
            writer.manifest().started_at.fixed_offset(),
        );
        writer.finish(2_000_000_000, "user").unwrap();
        let own_dir = fixture.lib.write(|tx| tx.create_meeting_dir(&own)).unwrap();
        std::fs::write(
            own_dir.join(broscribe_core::paths::MEETING_JSON),
            b"{ unreadable",
        )
        .unwrap();
        let input = final_input(&dir);
        let lease = claim(&fixture, &input, false);
        let error = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &FakeEncoder::default(),
            &NoQuery,
        )
        .unwrap_err();
        assert_eq!(error.code(), "invalid_input", "{error}");
        assert_eq!(meeting_folders(fixture.lib.root()), vec![own_dir]);
        assert!(dir.exists());
    }

    #[test]
    fn record_retry_after_a_title_change_reuses_the_unpublished_folder() {
        let mut fixture = Fixture::new();
        fixture.chain = DecoderChain::new(vec![Box::new(FakeEncodedDecoder)]);
        let mut writer = writer(&fixture);
        for track in CaptureTrack::ALL {
            writer.append(track, 0, &speech(2.0, -18.0, 4)).unwrap();
        }
        let dir = writer.dir().to_owned();
        writer.finish(2_000_000_000, "user").unwrap();
        let input = final_input(&dir);
        let lease = claim(&fixture, &input, false);
        let encoder = FakeEncoder::default();
        crate::batch::FAIL_AT.set(Some(crate::batch::FailPoint::RecordAfterRename));
        run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap_err();
        let left = meeting_folders(fixture.lib.root());
        assert_eq!(left.len(), 1);
        // Renamed before the retry: the folder name the retry derives differs
        // from the one the failed attempt left.
        broscribe_core::lifecycle::set_session_title(
            &fixture.lib,
            &journal_root(&fixture),
            "synthetic-session",
            "Renamed meanwhile",
            || Ok(()),
        )
        .unwrap();
        let id = run(
            &fixture.env(),
            &mut models(),
            &lease,
            &input,
            false,
            &encoder,
            &NoQuery,
        )
        .unwrap();
        lease.release(&fixture.lib).unwrap();
        let folders = meeting_folders(fixture.lib.root());
        assert_eq!(folders, vec![fixture.lib.meeting_dir(&id).unwrap()]);
        assert_ne!(folders, left);
        assert!(
            folders[0]
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("renamed-meanwhile"))
        );
        assert!(
            folders[0]
                .join(format!("audio.{}", encoder.extension()))
                .is_file()
        );
    }
}
