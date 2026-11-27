//! `reprocess <meeting>` (design §11, plan M2 §9.6, §10.1, §10.8, §10.11):
//! runs ASR, diarization and identity again on the stored recording with the
//! active (or given) profile, keeping what the user decided.
//!
//! - New speakers map to old ones by talk-time overlap (≥ 50 % of either
//!   speaker's talk time and ≥ 1 s). The mapping is one-to-one: when an old
//!   speaker overlaps several new ones, or a new one several old ones, the
//!   affected references are left unresolved and listed for review — never
//!   bound to a reused ID.
//! - Identity: a new speaker inherits the confirmed person of the old
//!   speakers mapped to it when they agree (several old speakers confirmed
//!   as the same person count as agreeing), plus the union of their
//!   rejections; disagreeing or unmatched confirmations are listed under
//!   `needs_review`. Unconfirmed speakers are resolved again.
//! - Speaker references of generated content — the enrichment (`by_speaker`,
//!   `owner_speaker`, `{{S#}}`) and every template result (`{{S#}}`) — are
//!   rewritten through the same mapping; when the transcript's window
//!   revision changed each is marked stale (generated again only on request
//!   or by `auto_enrich`).
//! - `asr_only` keeps the stored speakers and replaces the text only: words
//!   go to the stored speaker turns; references stay.
//! - Line corrections (`utterance_people`) move to the new segment on the
//!   same track with the same text that overlaps at least half of either
//!   span; the others are dropped with a review note. `asr_only` usually
//!   changes the text, so its corrections are usually dropped. Speaker
//!   identity never depends on them.
//! - Whisper-style runtimes decode each language span in its language and get
//!   hotwords (attendees, known people, configured terms).
//!
//! Decoding and inference run outside the lock; the commit is one write,
//! always inside a queue lease's checked commit: a queued run through the
//! worker (requested explicitly with [`enqueue_reprocess_request`]), a
//! synchronous one ([`run_reprocess_interactive`]) through the meeting's
//! reserved job. Both take the same reservation, so they exclude each other
//! and a run whose lease was reclaimed publishes nothing.
//! A mic + system pair is analysed against Me's evidence as stored when the
//! analysis starts; a forget of Me (or Me's voice) before the commit makes
//! that pass publish nothing and the audio is analysed again
//! (`record::ME_EVIDENCE_PASSES` passes, then `me_evidence_changed`).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};

use broscribe_core::config::Profile;
use broscribe_core::identity;
use broscribe_core::index::{Index, meeting_window_revision};
use broscribe_core::ops::{self, attribution_revision, reapply_utterance_people};
use broscribe_core::queue::{JobId, JobState, LeaseHandle, Queue, REPROCESS_KIND};
use broscribe_core::schema::{
    Assignment, GroupOrigin, LanguageSpan, Meeting, ProcessingStatus, ReviewSource, Segment,
    SpeakerGroup,
};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result, SpeakerId, WriteTx};
use broscribe_models::{ModelProvider, ModelSet};
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::analyze::{AsrRegion, Turn, segment_runs, segments_with_overlap, transcribe_speech};
use crate::audio::{TrackSource, meeting_tracks};
use crate::enrich::remap_speakers;
use crate::finish::finish_meeting;
use crate::record::{ME_EVIDENCE_PASSES, me_evidence_changed};
use crate::{multitrack, ranges};

/// Input for a reprocessing job:
/// `{"meeting_id":"m-…","profile":"optional-name","asr_only":false,"interactive":false}`.
/// `interactive` marks a synchronous run a user waits on
/// (`Job::is_interactive`: an interrupted one is never re-run by a worker).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReprocessJobInput {
    pub meeting_id: MeetingId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default)]
    pub asr_only: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interactive: bool,
}

impl ReprocessJobInput {
    fn parse(input: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(input.clone())
            .map_err(|e| Error::InvalidInput(format!("invalid reprocess job input: {e}")))
    }

    fn to_value(&self) -> Result<serde_json::Value> {
        serde_json::to_value(self)
            .map_err(|e| Error::Internal(format!("serialising reprocess job input: {e}")))
    }
}

/// Executes the claimed reprocessing job of `lease` (the queue worker)
/// outside the library lock, publishing inside the lease's checked commit.
/// Models are loaded for this run only; missing or deleted audio is skipped
/// before loading.
pub(crate) fn run_reprocess_job_leased(
    env: &Env<'_>,
    input: &serde_json::Value,
    models: &dyn ModelProvider,
    lease: &LeaseHandle,
) -> Result<ReprocessOutcome> {
    run_input(env, &ReprocessJobInput::parse(input)?, models, lease, None)
}

/// What [`run_reprocess_interactive`] did.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum InteractiveReprocess {
    /// Ran here and published (or skipped: see [`ReprocessOutcome::skipped`]).
    Ran(ReprocessOutcome),
    /// A worker claimed the job first and runs it.
    Queued(JobId),
}

/// Runs a synchronous reprocessing (`reprocess <meeting>`) through the
/// meeting's queue job, so it and queued requests exclude each other:
/// reserves the job (a finished one restarts) with `interactive: true`,
/// claims it with a normal lease, runs outside the library lock and
/// publishes through the lease commit. A queued or running reprocessing of
/// the meeting is `library_busy`; a worker that claimed the job between the
/// reservation and the claim keeps it ([`InteractiveReprocess::Queued`]).
/// `cancelled` is checked before the run and again before the commit: once
/// set, nothing is saved, the job is failed ("interrupted", never re-run)
/// and the error code is `interrupted`.
pub fn run_reprocess_interactive(
    env: &Env<'_>,
    meeting_id: &MeetingId,
    profile: Option<&str>,
    asr_only: bool,
    models: &dyn ModelProvider,
    cancelled: &AtomicBool,
) -> Result<InteractiveReprocess> {
    let input = ReprocessJobInput {
        meeting_id: meeting_id.clone(),
        profile: profile.map(str::to_owned),
        asr_only,
        interactive: true,
    };
    let job = reserve(env, &input)?;
    let what = format!("reprocessing of {meeting_id}");
    let ran = crate::enrich::claim_and_run(env, &job, REPROCESS_KIND, &what, cancelled, |lease| {
        // The input as claimed: the reservation's, unless another request
        // replaced it before the claim.
        let claimed = ReprocessJobInput::parse(&lease.claimed_job().input)?;
        run_input(env, &claimed, models, lease, Some(cancelled))
    })?;
    Ok(match ran {
        Some(out) => InteractiveReprocess::Ran(out),
        None => InteractiveReprocess::Queued(job),
    })
}

/// Reserves the meeting's reprocessing job for `input` (a finished one
/// restarts; a queued or running one is `library_busy`).
fn reserve(env: &Env<'_>, input: &ReprocessJobInput) -> Result<JobId> {
    env.lib.write(|tx| reserve_in(tx, input))
}

/// The one reservation of the meeting's reprocessing job, synchronous and
/// queued runs alike ([`Queue::restart_finished`] under the queue lock
/// inside the library write): a `done` or `failed` job (or one whose
/// holder crashed) restarts with `input` and zero attempts, a `queued` or
/// live `running` one is `library_busy`, so two requests never run or
/// publish over each other.
fn reserve_in(tx: &mut WriteTx<'_>, input: &ReprocessJobInput) -> Result<JobId> {
    let value = input.to_value()?;
    Queue::open(tx.root())?.restart_finished(tx, REPROCESS_KIND, input.meeting_id.as_str(), value)
}

/// Queues a reprocessing of `meeting_id` on the user's explicit request
/// (`reprocess --queue`) in the caller's library write, for a worker to
/// run: never started automatically. Checked first, under that lock: the
/// meeting exists, is not being deleted and is done or failed; every
/// recording track is present; `profile` names a built-in profile; ASR-only
/// is not asked of a microphone + system-audio pair. It then takes the
/// same reservation as [`run_reprocess_interactive`] (see [`reserve_in`]):
/// a queued or running reprocessing of the meeting, interactive or not, is
/// `library_busy`; a finished or failed one restarts with this request.
/// Returns the job, `queued`.
pub fn enqueue_reprocess_request(
    tx: &mut WriteTx<'_>,
    meeting_id: &MeetingId,
    profile: Option<&str>,
    asr_only: bool,
) -> Result<JobId> {
    let lib = tx.library();
    if broscribe_core::lifecycle::meeting_deleted(lib, meeting_id) {
        return Err(Error::NotFound(format!(
            "meeting {meeting_id} (being deleted)"
        )));
    }
    let meeting = lib.load_meeting(meeting_id)?;
    if !matches!(
        meeting.processing.status,
        ProcessingStatus::Done | ProcessingStatus::Failed
    ) {
        return Err(Error::InvalidInput(format!(
            "{meeting_id} is not processed yet; reprocessing needs a finished or failed transcription"
        )));
    }
    if !audio_available(lib, &meeting)? {
        return Err(Error::InvalidInput(format!(
            "{meeting_id}: a recording source is deleted or missing; reprocessing needs every \
             audio track"
        )));
    }
    if asr_only && has_mic_system_pair(&meeting) {
        return Err(Error::InvalidInput(format!(
            "{meeting_id} has microphone and system-audio tracks, which need echo cancellation; \
             ASR-only reprocessing cannot apply it (queue it without --asr-only)"
        )));
    }
    let profile = profile.map(str::trim);
    if let Some(name) = profile {
        broscribe_models::Registry::builtin()?.profile(name)?;
    }
    reserve_in(
        tx,
        &ReprocessJobInput {
            meeting_id: meeting_id.clone(),
            profile: profile.map(str::to_owned),
            asr_only,
            interactive: false,
        },
    )
}

/// One reprocessing run of `input` under `lease`.
fn run_input(
    env: &Env<'_>,
    input: &ReprocessJobInput,
    models: &dyn ModelProvider,
    lease: &LeaseHandle,
    cancelled: Option<&AtomicBool>,
) -> Result<ReprocessOutcome> {
    let meeting = env.lib.load_meeting(&input.meeting_id)?;
    if !audio_available(env.lib, &meeting)? {
        // Nothing to publish: the job is closed under its lease.
        lease.commit(env.lib, JobState::Done, |_, _| Ok(()))?;
        return Ok(ReprocessOutcome {
            meeting_id: Some(input.meeting_id.clone()),
            asr_only: input.asr_only,
            skipped: Some(format!(
                "{}: the recording is not available (deleted or missing); reprocessing needs the audio",
                input.meeting_id,
            )),
            ..ReprocessOutcome::default()
        });
    }
    let mut config = env.config.clone();
    config.profile = profile_named(&config.profile, input.profile.as_deref())?;
    let mut loaded = models.load(&config.profile)?;
    let env = Env {
        lib: env.lib,
        config: &config,
        decoders: env.decoders,
        probe: env.probe,
        progress: env.progress,
        execution: env.execution,
    };
    reprocess_published(
        &env,
        &mut loaded,
        &input.meeting_id,
        ReprocessOptions {
            asr_only: input.asr_only,
        },
        lease,
        cancelled,
    )
}

/// Shortest overlap that links an old speaker to a new one.
const MIN_OVERLAP_S: f64 = 1.0;
/// Share of either speaker's talk time the overlap must reach.
const MIN_OVERLAP_SHARE: f64 = 0.5;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReprocessOptions {
    /// Replace the text only; keep stored speakers and diarization.
    pub asr_only: bool,
}

/// One old → new speaker link.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SpeakerLink {
    pub old: SpeakerId,
    pub new: SpeakerId,
    pub overlap_s: f64,
}

/// What a reprocessing run did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ReprocessOutcome {
    pub meeting_id: Option<MeetingId>,
    /// Why nothing was done (no recording available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    pub asr_only: bool,
    pub segments: usize,
    pub speakers: usize,
    /// One-to-one old → new mapping (empty with `asr_only`).
    pub mapping: Vec<SpeakerLink>,
    /// New speakers that kept a confirmed person.
    pub confirmed_kept: usize,
    pub auto: usize,
    pub suggested: usize,
    /// Confirmations and enrichment references that could not be carried
    /// over (descriptions with speaker IDs only).
    pub needs_review: Vec<String>,
    /// The transcript's window revision changed.
    pub revision_changed: bool,
    /// The enrichment is now marked stale.
    pub enrichment_stale: bool,
}

/// The profile `name` names: `None` for the active one, else a profile of
/// the built-in registry (its speech models; the user's text embedding and
/// LLM providers stay).
pub fn profile_named(active: &Profile, name: Option<&str>) -> Result<Profile> {
    match name.map(str::trim) {
        None => Ok(active.clone()),
        Some(id) => Ok(broscribe_models::Registry::builtin()?
            .profile(id)?
            .apply(active)),
    }
}

/// Recognition hints for a meeting: its attendees, known people (names and
/// aliases), the configured terms and the meeting's own, in that priority,
/// without duplicates.
pub fn hotwords(lib: &Library, env_terms: &[String], meeting: &Meeting) -> Result<Vec<String>> {
    let attendees = meeting
        .participants
        .iter()
        .filter_map(|p| p.name.as_deref());
    collect_hotwords(attendees, Some(lib), env_terms, &meeting.hotwords)
}

/// [`hotwords`] for audio that is not (yet) a meeting: known people (when
/// a library exists) and the configured terms.
pub fn library_hotwords(lib: Option<&Library>, env_terms: &[String]) -> Result<Vec<String>> {
    collect_hotwords(std::iter::empty(), lib, env_terms, &[])
}

fn collect_hotwords<'a>(
    attendees: impl Iterator<Item = &'a str>,
    lib: Option<&Library>,
    env_terms: &[String],
    meeting_terms: &[String],
) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut push = |t: &str| {
        let t = t.trim();
        if !t.is_empty() && seen.insert(t.to_lowercase()) {
            out.push(t.to_owned());
        }
    };
    attendees.for_each(&mut push);
    if let Some(lib) = lib {
        for p in lib.list_people()? {
            push(&p.name);
            for a in &p.aliases {
                push(a);
            }
        }
    }
    for t in env_terms.iter().chain(meeting_terms) {
        push(t);
    }
    Ok(out)
}

/// Every independent source must still be present, including referenced
/// tracks. `meeting_tracks` excludes recordings already marked deleted.
fn audio_available(lib: &Library, meeting: &Meeting) -> Result<bool> {
    let tracks = meeting_tracks(meeting);
    let dir = lib.meeting_dir(&meeting.id)?;
    Ok(!tracks.is_empty()
        && tracks.iter().all(|track| {
            if track.path.is_absolute() {
                track.path.is_file()
            } else {
                dir.join(&track.path).is_file()
            }
        }))
}

/// Whether the meeting was recorded as a microphone + system-audio pair.
fn has_mic_system_pair(meeting: &Meeting) -> bool {
    multitrack::mic_system_pair(meeting.tracks.iter().map(|t| t.role)).is_some()
}

/// The role of the meeting's track `track` (`Mixed` when not recorded).
pub(crate) fn track_role(meeting: &Meeting, track: &str) -> broscribe_core::schema::TrackRole {
    meeting
        .tracks
        .iter()
        .find(|t| t.id == track)
        .map_or(broscribe_core::schema::TrackRole::Mixed, |t| t.role)
}

/// The capture setup and requested echo handling stored with the meeting,
/// as the production analyzer needs them for a mic + system pair.
pub(crate) fn capture_setup(
    meeting: &Meeting,
) -> Result<(
    Option<broscribe_dsp::Setup>,
    Option<broscribe_dsp::EchoHandling>,
)> {
    let setup = meeting
        .setup
        .as_deref()
        .map(str::parse::<broscribe_dsp::Setup>)
        .transpose()
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
    let handling = meeting
        .processing
        .echo_report
        .as_ref()
        .and_then(|r| r.get("requested"))
        .cloned()
        .map(serde_json::from_value::<broscribe_dsp::EchoHandling>)
        .transpose()
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
    Ok((setup, handling))
}

/// Transcribes sources independently, shifting words to meeting time before
/// the commit assigns them to the latest stored turns of that track.
fn transcribe_tracks(
    env: &Env<'_>,
    models: &mut ModelSet,
    meeting: &Meeting,
    sources: &[TrackSource],
) -> Result<Vec<(String, Vec<AsrRegion>)>> {
    let mut tracks = Vec::with_capacity(sources.len());
    for source in sources {
        let samples = crate::audio::decode_meeting_track(env.lib, env.decoders, meeting, source)?;
        let languages: Vec<_> = meeting
            .languages
            .iter()
            .map(|l| LanguageSpan {
                lang: l.lang.clone(),
                from_s: l.from_s - source.offset_s,
                to_s: l.to_s - source.offset_s,
            })
            .collect();
        let mut regions = transcribe_speech(&samples, models, &languages)?;
        for (range, words, _) in &mut regions {
            range.0 = (range.0 + source.offset_s).max(0.0);
            range.1 += source.offset_s;
            for word in words.iter_mut() {
                word.start = (word.start + source.offset_s).max(0.0);
                word.end += source.offset_s;
            }
            words.retain(|word| word.end > word.start);
        }
        regions.retain(|(range, _, _)| range.1 > range.0);
        tracks.push((source.track.clone(), regions));
    }
    Ok(tracks)
}

/// Reprocesses one meeting with `models` (loaded for the active profile)
/// through its reserved, leased job, as [`run_reprocess_interactive`] does;
/// a worker that takes the job first makes it `library_busy`.
#[cfg(test)]
pub(crate) fn reprocess(
    env: &Env<'_>,
    models: &mut ModelSet,
    meeting_id: &MeetingId,
    opts: ReprocessOptions,
) -> Result<ReprocessOutcome> {
    let input = ReprocessJobInput {
        meeting_id: meeting_id.clone(),
        profile: None,
        asr_only: opts.asr_only,
        interactive: true,
    };
    let job = reserve(env, &input)?;
    let what = format!("reprocessing of {meeting_id}");
    crate::enrich::claim_and_run(
        env,
        &job,
        REPROCESS_KIND,
        &what,
        &AtomicBool::new(false),
        |lease| reprocess_published(env, models, meeting_id, opts, lease, None),
    )?
    .ok_or_else(|| Error::LibraryBusy(format!("{meeting_id} (a worker took its reprocessing)")))
}

/// Reprocessing published through `lease`'s checked commit; nothing is
/// written once `cancelled` is set.
fn reprocess_published(
    env: &Env<'_>,
    models: &mut ModelSet,
    meeting_id: &MeetingId,
    opts: ReprocessOptions,
    lease: &LeaseHandle,
    cancelled: Option<&AtomicBool>,
) -> Result<ReprocessOutcome> {
    let interrupted = || {
        cancelled
            .is_some_and(|c| c.load(Ordering::SeqCst))
            .then(|| crate::enrich::interrupted(&format!("reprocessing of {meeting_id}")))
    };
    let meeting = env.lib.load_meeting(meeting_id)?;
    let mut out = ReprocessOutcome {
        meeting_id: Some(meeting_id.clone()),
        asr_only: opts.asr_only,
        ..ReprocessOutcome::default()
    };
    if opts.asr_only && has_mic_system_pair(&meeting) {
        // ASR-only transcribes every track on its own: the microphone would
        // keep the remote voices that echo cancellation removes.
        return Err(Error::InvalidInput(format!(
            "{meeting_id} has microphone and system-audio tracks, which need echo cancellation; \
             ASR-only reprocessing cannot apply it. Reprocess fully instead \
             (`broscribe reprocess {meeting_id}` without --asr-only)"
        )));
    }
    if !audio_available(env.lib, &meeting)? {
        out.skipped = Some(format!(
            "{meeting_id}: a recording source is deleted or missing; reprocessing needs every audio track"
        ));
        lease.commit(env.lib, JobState::Done, |_, _| Ok(()))?;
        return Ok(out);
    }
    let sources = meeting_tracks(&meeting);
    if models.asr.accepts_hotwords() {
        let terms = hotwords(env.lib, &env.config.hotwords, &meeting)?;
        models.asr.set_hotwords(&terms)?;
    }
    env.say(format_args!(
        "{meeting_id}: analyzing stored recording tracks"
    ));
    let model = models.embedder.model_id().to_owned();
    let thresholds = env.thresholds(&model).copied();
    let models_used = crate::import::models_used(env, &model, true);

    if opts.asr_only {
        let regions = transcribe_tracks(env, models, &meeting, &sources)?;
        let finished = lease.commit(env.lib, JobState::Done, |tx, _| {
            if let Some(e) = interrupted() {
                return Err(e);
            }
            let mut cur = tx.library().load_meeting(meeting_id)?;
            let published = cur.clone();
            let before = meeting_window_revision(&cur);
            // Words go to the speaker turns stored now (re-read under the lock).
            let turns = segment_runs(&cur);
            let mut segments: Vec<Segment> = Vec::new();
            for (track, regions) in regions {
                let track_turns: Vec<_> = turns
                    .iter()
                    .copied()
                    .filter(|t| cur.speaker(t.speaker).is_some_and(|s| s.track == track))
                    .collect();
                let saved_overlap: Vec<_> = cur
                    .segments
                    .iter()
                    .filter(|s| s.track == track)
                    .flat_map(|s| s.speech_overlap.iter().map(|&[a, b]| (a, b)))
                    .collect();
                let mut transcribed = segments_with_overlap(regions, &track_turns, &saved_overlap);
                for segment in &mut transcribed {
                    segment.track = track.clone();
                }
                segments.extend(transcribed);
            }
            segments.sort_by(|a, b| a.start.total_cmp(&b.start).then(a.track.cmp(&b.track)));
            for (id, segment) in segments.iter_mut().enumerate() {
                segment.id = u32::try_from(id)
                    .map_err(|_| Error::InvalidInput("too many transcript segments".into()))?;
            }
            cur.segments = segments;
            cur.models.vad = models_used.vad.clone();
            cur.models.asr = models_used.asr.clone();
            let dropped = reapply_utterance_people(&mut cur);
            cur.add_review_notes(ReviewSource::Reprocess, dropped.iter().cloned());
            crate::live_identity::recheck_meeting(tx.library(), &mut cur)?;
            ops::guard_automatic_names(&mut cur);
            ops::reconcile_live_speakers(tx.library(), &mut cur, &env.config.live)?;
            ops::refresh_identity_conflict_notes(tx.library(), &mut cur)?;
            ops::sync_weak_samples(
                tx,
                &cur,
                &crate::caption_replay::speaker_changes(
                    &cur,
                    crate::caption_replay::speaker_states(&published),
                ),
            )?;
            ops::sync_live_promotions(tx, Some(&published), &cur, &model)?;
            let after = meeting_window_revision(&cur);
            let (changed, stale) = mark_stale(&mut cur, &before, &after);
            cur.updated_at = broscribe_core::schema::now();
            let segments = cur.segments.len();
            tx.save_meeting(&cur)?;
            Index::open(tx.root())?.upsert_meeting(tx, &cur)?;
            Ok((segments, changed, stale, dropped))
        })?;
        let (segments, changed, stale, dropped) = finished;
        let saved = env.lib.load_meeting(meeting_id)?;
        out.segments = segments;
        out.speakers = saved.speakers.len();
        out.auto = count(&saved, Assignment::Auto);
        out.suggested = count(&saved, Assignment::Suggested);
        out.confirmed_kept = count(&saved, Assignment::Confirmed);
        out.revision_changed = changed;
        out.enrichment_stale = stale;
        out.needs_review = dropped;
        return Ok(out);
    }

    let dir = env.lib.meeting_dir(meeting_id)?;
    let inputs: Vec<_> = sources
        .iter()
        .map(|source| crate::manifest::ManifestSource {
            path: if source.path.is_absolute() {
                source.path.clone()
            } else {
                dir.join(&source.path)
            },
            audio_track_index: Some(source.track_index),
            offset_s: source.offset_s,
            role: track_role(&meeting, &source.track),
            person_hint: None,
        })
        .collect();
    let (setup, handling) = capture_setup(&meeting)?;
    // A mic + system pair is analysed against Me's evidence as stored when
    // the pass starts; the commit checks it is still current. A voice or
    // self person forgotten meanwhile made the pass's Me naming, mic
    // centroid and echo drops (speech discarded as echo of a voice verified
    // against it) stale: that pass publishes nothing and the audio is
    // analysed again with fresh evidence, at most `ME_EVIDENCE_PASSES` times.
    let mut pass = 0;
    let finished = loop {
        pass += 1;
        let me = multitrack::me_evidence(env, &inputs, &model, &[])?;
        let mut analysis = multitrack::analyze_sources_with_evidence(
            env,
            &inputs,
            models,
            &meeting.languages,
            setup,
            handling,
            me.as_ref(),
        )?;
        // Analysis allocates track-local t0/t1 labels. Keep the durable track IDs,
        // including legacy single-track labels, for identity-overlap matching.
        for speaker in &mut analysis.speakers {
            let i = speaker
                .track
                .strip_prefix('t')
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or_else(|| Error::Internal("analysis returned an invalid track ID".into()))?;
            speaker.track = sources
                .get(i)
                .ok_or_else(|| Error::Internal("analysis returned an unknown track".into()))?
                .track
                .clone();
        }
        for segment in &mut analysis.segments {
            let i = segment
                .track
                .strip_prefix('t')
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or_else(|| Error::Internal("analysis returned an invalid track ID".into()))?;
            segment.track = sources
                .get(i)
                .ok_or_else(|| Error::Internal("analysis returned an unknown track".into()))?
                .track
                .clone();
        }
        // Caption samples embed the agreed clean speech of the speakers that
        // will be published, outside the lock: the fresh analysis as captions
        // align with it, including the groups the write below forms folded
        // where only that names a voice over-split into short clusters. The
        // write plans the same way on the re-read meeting.
        let planned = plan_publication(
            &meeting,
            fresh_meeting(&meeting, &analysis, &models_used),
            &analysis.diar_segments,
            &model,
            thresholds.as_ref(),
        )?;
        let caption_embeddings = crate::caption_replay::embed_saved_captions(
            env,
            &planned.cur,
            &planned.provisional,
            models.embedder.as_ref(),
        )?;
        let mut me_changed = false;
        let published = lease.commit(env.lib, JobState::Done, |tx, _| {
            if let Some(e) = interrupted() {
                return Err(e);
            }
            if let Some(me) = &me
                && !me.still_current(tx.library())?
            {
                me_changed = true;
                return Err(me_evidence_changed("reprocessing"));
            }
            let old = tx.library().load_meeting(meeting_id)?;
            let before = meeting_window_revision(&old);
            let plan = plan_publication(
                &old,
                fresh_meeting(&old, &analysis, &models_used),
                &analysis.diar_segments,
                &model,
                thresholds.as_ref(),
            )?;
            let mut cur = plan.cur;
            // Identity notes replace the previous reprocessing's, with or
            // without an enrichment.
            cur.set_review_notes(ReviewSource::Reprocess, plan.review.iter().cloned());
            // Line corrections follow their text to the new segments.
            let dropped = reapply_utterance_people(&mut cur);
            cur.add_review_notes(ReviewSource::Reprocess, dropped.iter().cloned());
            crate::live_identity::recheck_meeting(tx.library(), &mut cur)?;

            // Displayed references follow the raw one-to-one mapping, else the
            // shown (grouped) speaker's.
            let lookup = |s: SpeakerId| {
                plan.raw_map
                    .iter()
                    .find(|l| l.old == s)
                    .or_else(|| {
                        let shown = ops::effective_speaker(&old, s);
                        plan.eff_map.iter().find(|l| l.old == shown)
                    })
                    .map(|l| l.new)
            };
            // Every piece of generated content follows the same mapping; its
            // unmapped references belong to it: added to its source's notes,
            // which its next run replaces.
            let mut enrichment_review = Vec::new();
            let mut template_review = Vec::new();
            if let Some(e) = cur.enrichment.as_mut() {
                enrichment_review = remap_speakers(e, &lookup);
            }
            for r in &mut cur.template_results {
                template_review.extend(remap_speakers(r, &lookup));
            }
            // Retained results must not refer to a previous speaker namespace.
            if let Some(previous) = &mut cur.previous_enrichment {
                remap_speakers(previous, &lookup);
            }
            for previous in cur.previous_template_results.values_mut() {
                remap_speakers(previous, &lookup);
            }
            cur.add_review_notes(ReviewSource::Enrichment, enrichment_review.iter().cloned());
            cur.add_review_notes(ReviewSource::Template, template_review.iter().cloned());
            // Learned samples name the old speakers: move them before resolution
            // and caption replay read them. As with confirmations, an old speaker
            // linked to exactly one new speaker moves there, also when several
            // old speakers merge into it; else a shown old speaker follows its
            // group's mapping; ambiguous or vanished → cleared.
            let sample_speaker = |s: SpeakerId| successor(&plan.raw_links, &plan.eff_map, &old, s);
            let published: Vec<_> = old.speakers.iter().map(|s| s.id).collect();
            ops::migrate_sample_speakers(tx, &mut cur, &published, sample_speaker)?;
            let after = meeting_window_revision(&cur);
            let (changed, stale) = mark_stale(&mut cur, &before, &after);
            // Identity resolution below; groups come last.
            cur.processing.status = ProcessingStatus::Done;
            cur.updated_at = broscribe_core::schema::now();
            tx.save_meeting(&cur)?;
            let mut index = Index::open(tx.root())?;
            finish_meeting(tx, &mut index, meeting_id, &model, thresholds.as_ref())?;
            // Saved captions name speakers after resolution, as on recording,
            // aligned with this publish's groups folded.
            crate::caption_replay::realign(
                tx,
                &mut index,
                meeting_id,
                &caption_embeddings,
                &plan.provisional,
            )?;
            // Groups last: with every name known, a caption-proven speaker
            // never ends up hidden under someone else.
            let mut done = tx.library().load_meeting(meeting_id)?;
            let identity_before = done.clone();
            ops::guard_automatic_names(&mut done);
            ops::reconcile_live_speakers(tx.library(), &mut done, &env.config.live)?;
            let proven = ops::caption_proven(tx.library(), &done)?;
            ops::apply_auto_groups(&mut done, &model, thresholds.as_ref(), &proven)?;
            // Resolution and caption replay synced weak samples before the groups
            // existed: a grouped member's own auto name must teach nothing.
            let members: Vec<SpeakerId> = done
                .speaker_groups
                .iter()
                .filter(|g| g.origin != GroupOrigin::User)
                .flat_map(|g| g.members.iter().copied())
                .collect();
            ops::drop_member_auto_samples(tx, &done, &members)?;
            // A kept speaker never stays named as someone a member rejected.
            let people = ops::load_person_samples(tx.library(), &model)?;
            let roster = done.roster_person_ids();
            ops::resolve_rejected_kept(
                &mut done,
                &model,
                &people,
                thresholds.as_ref(),
                Some(&roster),
            )?;
            ops::guard_automatic_names(&mut done);
            ops::reconcile_live_speakers(tx.library(), &mut done, &env.config.live)?;
            ops::refresh_identity_conflict_notes(tx.library(), &mut done)?;
            ops::sync_weak_samples(
                tx,
                &done,
                &crate::caption_replay::speaker_changes(
                    &done,
                    crate::caption_replay::speaker_states(&identity_before),
                ),
            )?;
            ops::sync_live_promotions(tx, Some(&old), &done, &model)?;
            // Samples that followed a retained speaker into the new namespace
            // must also go if the new final lines no longer support its name.
            if old.speaker_namespace != identity_before.speaker_namespace {
                ops::retract_live_samples(tx, &identity_before, &done)?;
            }
            let mut stale = stale;
            if done != identity_before {
                stale |= ops::mark_generated_stale(&mut done);
                done.updated_at = broscribe_core::schema::now();
                tx.save_meeting(&done)?;
                index.upsert_meeting(tx, &done)?;
            }
            Ok((
                plan.mapping,
                [plan.review, dropped, enrichment_review, template_review].concat(),
                changed,
                stale,
            ))
        });
        match published {
            Ok(finished) => break finished,
            Err(_) if me_changed && pass < ME_EVIDENCE_PASSES => {
                tracing::info!("Me evidence changed during reprocessing; analysing again");
            }
            Err(e) => return Err(e),
        }
    };
    let (map, review, changed, stale) = finished;
    let saved = env.lib.load_meeting(meeting_id)?;
    out.segments = saved.segments.len();
    out.speakers = saved.speakers.len();
    out.mapping = map;
    out.needs_review = review;
    out.auto = count(&saved, Assignment::Auto);
    out.suggested = count(&saved, Assignment::Suggested);
    out.confirmed_kept = count(&saved, Assignment::Confirmed);
    out.revision_changed = changed;
    out.enrichment_stale = stale;
    Ok(out)
}

fn count(m: &Meeting, a: Assignment) -> usize {
    m.speakers.iter().filter(|s| s.assignment == a).count()
}

/// Marks generated content (enrichment, template results) stale when the
/// window revision changed, or it was produced for another window or
/// attribution revision (line corrections moved or dropped). Returns
/// (changed, whether the enrichment is stale).
fn mark_stale(m: &mut Meeting, before: &str, after: &str) -> (bool, bool) {
    let changed = before != after;
    let attribution = attribution_revision(m);
    for g in m.generated_mut() {
        if changed
            || (!g.revision().is_empty() && g.revision() != after)
            || g.attribution_revision() != attribution
        {
            g.set_stale(true);
        }
    }
    (changed, m.enrichment.as_ref().is_some_and(|e| e.stale))
}

/// `old` with the fresh analysis' speakers, transcript and models, and no
/// grouping yet ([`plan_publication`] carries what the user decided).
fn fresh_meeting(
    old: &Meeting,
    analysis: &crate::analyze::Analysis,
    models_used: &broscribe_core::schema::ModelsUsed,
) -> Meeting {
    let mut cur = old.clone();
    cur.speakers = analysis.speakers.clone();
    cur.segments = analysis.segments.clone();
    cur.duration_s = analysis.duration_s;
    cur.models = models_used.clone();
    cur.speaker_groups.clear();
    cur.not_same.clear();
    // Grouping notes describe the previous analysis' groups.
    cur.review_notes
        .retain(|n| n.source != ReviewSource::Grouping);
    cur
}

/// How a full reprocess publishes the fresh analysis over `old` (design
/// §6.4, reprocessing):
///
/// 1. **Raw mapping** of old speakers to fresh clusters by talk overlap.
///    `User` groups and `not_same` pairs carry through its one-to-one
///    links (else a review note); confirmations, rejections and samples
///    follow its usual rules.
/// 2. **Provisional `Auto` grouping** of the fresh analysis (respecting the
///    carried decisions), not stored: captions align with it folded, and
///    the final grouping runs once every name is known.
/// 3. **Shown mapping**: old shown speakers (groups count once) to fresh
///    shown speakers, one-to-one. Used only where the raw rule finds
///    nothing for a shown old speaker (identity and samples then go to the
///    new group's kept speaker) and for displayed references.
struct Publication {
    /// The fresh meeting with inherited identity and the carried `User`
    /// groups and `not_same` pairs.
    cur: Meeting,
    provisional: Vec<SpeakerGroup>,
    raw_links: Vec<SpeakerLink>,
    raw_map: Vec<SpeakerLink>,
    eff_map: Vec<SpeakerLink>,
    /// The raw one-to-one links, plus shown links for old speakers the raw
    /// mapping leaves out.
    mapping: Vec<SpeakerLink>,
    review: Vec<String>,
}

/// The new speaker `old`'s speaker `s` continues as: its single raw
/// successor (several old speakers may share one), else, for a shown old
/// speaker, its group's shown successor (`eff_map`).
fn successor(
    raw_links: &[SpeakerLink],
    eff_map: &[SpeakerLink],
    old: &Meeting,
    s: SpeakerId,
) -> Option<SpeakerId> {
    single_successor(raw_links, s).or_else(|| shown_successor(eff_map, old, s))
}

/// `eff_map`'s successor of `s` when it is a shown old speaker.
fn shown_successor(eff_map: &[SpeakerLink], old: &Meeting, s: SpeakerId) -> Option<SpeakerId> {
    (ops::effective_speaker(old, s) == s)
        .then(|| eff_map.iter().find(|l| l.old == s).map(|l| l.new))
        .flatten()
}

/// The only new speaker `s` links to.
fn single_successor(links: &[SpeakerLink], s: SpeakerId) -> Option<SpeakerId> {
    let mut to = links.iter().filter(|l| l.old == s).map(|l| l.new);
    to.next().filter(|_| to.next().is_none())
}

fn plan_publication(
    old: &Meeting,
    fresh: Meeting,
    diar: &[Turn],
    model: &str,
    thresholds: Option<&broscribe_core::config::Thresholds>,
) -> Result<Publication> {
    let same_track = |cur: &Meeting, links: Vec<SpeakerLink>| -> Vec<SpeakerLink> {
        links
            .into_iter()
            .filter(|link| {
                old.speaker(link.old)
                    .zip(cur.speaker(link.new))
                    .is_some_and(|(a, b)| a.track == b.track)
            })
            .collect()
    };
    let mut cur = fresh;
    // 1. Raw mapping.
    let old_turns = segment_runs(old);
    let new_turns = relabel_turns(&cur, diar, |s| s);
    let raw_links = same_track(&cur, link_speakers(&old_turns, &new_turns));
    let raw_map = one_to_one(&raw_links);
    let raw = |s: SpeakerId| raw_map.iter().find(|l| l.old == s).map(|l| l.new);
    let mut review = Vec::new();
    for g in old
        .speaker_groups
        .iter()
        .filter(|g| g.origin == GroupOrigin::User)
    {
        let mapped: Option<Vec<SpeakerId>> = g.speakers().map(raw).collect();
        match mapped {
            Some(ids) => cur.speaker_groups.push(SpeakerGroup {
                id: SpeakerGroup::id_for(ids[0]),
                kept: ids[0],
                members: {
                    let mut m = ids[1..].to_vec();
                    m.sort();
                    m
                },
                origin: GroupOrigin::User,
                at: g.at,
            }),
            None => review.push(format!(
                "a merge you made could not be kept: old {} and {} match no single new speakers",
                g.kept,
                g.members
                    .iter()
                    .map(SpeakerId::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
    for &[a, b] in &old.not_same {
        match (raw(a), raw(b)) {
            (Some(x), Some(y)) => {
                let pair = if x <= y { [x, y] } else { [y, x] };
                if !cur.not_same.contains(&pair) {
                    cur.not_same.push(pair);
                }
            }
            _ => {
                let (x, y) = (
                    single_successor(&raw_links, a),
                    single_successor(&raw_links, b),
                );
                review.push(match (x, y) {
                    (Some(x), Some(y)) if x == y => format!(
                        "two speakers you separated (old {a}, {b}) were analysed as one ({x})"
                    ),
                    _ => format!(
                        "speakers you separated (old {a}, {b}) match no single new speakers; \
                         the separation was not kept"
                    ),
                });
            }
        }
    }
    cur.not_same.sort();
    cur.speaker_groups.sort_by_key(|g| g.kept);

    // 2. Provisional grouping, with the raw mapping's confirmations.
    let mut probe = cur.clone();
    inherit_identity(old, &mut probe, &raw_links, &|_| None);
    ops::reconcile_auto_groups(&mut probe, &Default::default());
    let provisional: Vec<SpeakerGroup> = identity::auto_groups(
        &probe,
        model,
        thresholds.and_then(|t| t.auto_merge),
        probe.roster_floor(),
        &Default::default(),
    )?
    .into_iter()
    .filter(|g| g.origin != GroupOrigin::User)
    .collect();

    // 3. Shown mapping.
    let mut shown = probe;
    shown.speaker_groups.extend(provisional.iter().cloned());
    let old_shown = relabel_turns(old, &old_turns, |s| ops::effective_speaker(old, s));
    let new_shown = relabel_turns(&cur, &new_turns, |s| ops::effective_speaker(&shown, s));
    let eff_map = one_to_one(&same_track(&cur, link_speakers(&old_shown, &new_shown)));

    let fallback = |s: SpeakerId| {
        single_successor(&raw_links, s)
            .is_none()
            .then(|| shown_successor(&eff_map, old, s))
            .flatten()
    };
    review.extend(inherit_identity(old, &mut cur, &raw_links, &fallback));
    ops::carry_live_speaker_overrides(old, &mut cur);
    // "Can't tell" follows the shown mapping (one-to-one), keeping its
    // time, onto every speaker of the new shown group; an ambiguous
    // speaker is not carried and returns to the review queue.
    for l in &eff_map {
        let Some(at) = ops::review_dismissed_at(old, l.old) else {
            continue;
        };
        let group: Vec<SpeakerId> =
            ops::group_of(&shown, l.new).map_or_else(|| vec![l.new], |g| g.speakers().collect());
        for id in group {
            if let Some(s) = cur.speaker_mut(id) {
                s.review_dismissed_at = Some(at);
            }
        }
    }
    // A carried merge the inherited identities now contradict is undone,
    // with a note.
    ops::reconcile_auto_groups(&mut cur, &Default::default());
    let mut mapping = raw_map.clone();
    mapping.extend(eff_map.iter().filter(|l| raw(l.old).is_none()).copied());
    Ok(Publication {
        cur,
        provisional,
        raw_links,
        raw_map,
        eff_map,
        mapping,
        review,
    })
}

/// `turns` of speakers `m` has, relabelled through `shown`.
fn relabel_turns(m: &Meeting, turns: &[Turn], shown: impl Fn(SpeakerId) -> SpeakerId) -> Vec<Turn> {
    turns
        .iter()
        .filter(|t| m.speaker(t.speaker).is_some())
        .map(|t| Turn {
            speaker: shown(t.speaker),
            ..*t
        })
        .collect()
}

fn talk(turns: &[Turn], s: SpeakerId) -> f64 {
    turns
        .iter()
        .filter(|t| t.speaker == s)
        .map(|t| t.end - t.start)
        .sum()
}

/// Every old/new pair whose overlap is ≥ 1 s and ≥ 50 % of either
/// speaker's talk time.
pub(crate) fn link_speakers(old: &[Turn], new: &[Turn]) -> Vec<SpeakerLink> {
    let olds = crate::analyze::speaker_ids(old);
    let news = crate::analyze::speaker_ids(new);
    let mut links = Vec::new();
    for &o in &olds {
        let talk_o = talk(old, o);
        for &n in &news {
            let talk_n = talk(new, n);
            let overlap: f64 = old
                .iter()
                .filter(|t| t.speaker == o)
                .flat_map(|a| {
                    new.iter()
                        .filter(move |t| t.speaker == n)
                        .map(move |b| ranges::overlap1((a.start, a.end), (b.start, b.end)))
                })
                .sum();
            if overlap >= MIN_OVERLAP_S
                && (overlap >= MIN_OVERLAP_SHARE * talk_o || overlap >= MIN_OVERLAP_SHARE * talk_n)
            {
                links.push(SpeakerLink {
                    old: o,
                    new: n,
                    overlap_s: overlap,
                });
            }
        }
    }
    links
}

/// The links whose old and new speakers have no other link.
pub(crate) fn one_to_one(links: &[SpeakerLink]) -> Vec<SpeakerLink> {
    links
        .iter()
        .filter(|l| {
            links.iter().filter(|x| x.old == l.old).count() == 1
                && links.iter().filter(|x| x.new == l.new).count() == 1
        })
        .copied()
        .collect()
}

/// Conservative snapshot → current mapping, separately for each durable track.
pub(crate) fn snapshot_speaker_map(old: &Meeting, current: &Meeting) -> Vec<SpeakerLink> {
    let old_turns = segment_runs(old);
    let new_turns = segment_runs(current);
    let mut links = Vec::new();
    for track in old
        .speakers
        .iter()
        .map(|s| s.track.as_str())
        .collect::<BTreeSet<_>>()
    {
        let a: Vec<_> = old_turns
            .iter()
            .filter(|t| old.speaker(t.speaker).is_some_and(|s| s.track == track))
            .copied()
            .collect();
        let b: Vec<_> = new_turns
            .iter()
            .filter(|t| current.speaker(t.speaker).is_some_and(|s| s.track == track))
            .copied()
            .collect();
        links.extend(link_speakers(&a, &b));
    }
    one_to_one(&links)
}

/// Carries confirmations and rejections from `old` to the new speakers of
/// `cur`: a new speaker takes those of the old speakers linked only to it,
/// plus those of shown old speakers the raw links leave without a single
/// successor that `fallback` maps to it. Returns the review notes.
fn inherit_identity(
    old: &Meeting,
    cur: &mut Meeting,
    links: &[SpeakerLink],
    fallback: &dyn Fn(SpeakerId) -> Option<SpeakerId>,
) -> Vec<String> {
    let mut review = Vec::new();
    let mut carried: BTreeSet<SpeakerId> = BTreeSet::new();
    let new_ids: Vec<SpeakerId> = cur.speakers.iter().map(|s| s.id).collect();
    for n in new_ids {
        // Old speakers linked only to this new speaker.
        let mut sources: Vec<&broscribe_core::schema::Speaker> = links
            .iter()
            .filter(|l| l.new == n && links.iter().filter(|x| x.old == l.old).count() == 1)
            .filter_map(|l| old.speaker(l.old))
            .collect();
        sources.extend(old.speakers.iter().filter(|s| fallback(s.id) == Some(n)));
        if sources.is_empty() {
            continue;
        }
        let confirmed: BTreeSet<&PersonId> = sources
            .iter()
            .filter(|s| s.assignment == Assignment::Confirmed && s.live_promotion.is_none())
            .filter_map(|s| s.person_id.as_ref())
            .collect();
        let mut rejected: Vec<PersonId> = Vec::new();
        for s in &sources {
            for r in &s.rejected_person_ids {
                if !rejected.contains(r) {
                    rejected.push(r.clone());
                }
            }
        }
        let speaker = cur.speaker_mut(n).expect("new speaker exists");
        speaker.live_speaker_override = sources.iter().any(|source| source.live_speaker_override);
        match confirmed.len() {
            0 => {
                if !speaker.live_speaker_override
                    && let [source] = sources.as_slice()
                    && let Some(promotion) = &source.live_promotion
                {
                    speaker.assignment = Assignment::Confirmed;
                    speaker.person_id = Some(promotion.person_id.clone());
                    speaker.live_promotion = Some(promotion.clone());
                }
            }
            1 => {
                let person = (*confirmed.iter().next().expect("one person")).clone();
                rejected.retain(|r| *r != person);
                speaker.person_id = Some(person);
                speaker.assignment = Assignment::Confirmed;
                speaker.score = None;
                speaker.suggestion = None;
                for s in &sources {
                    carried.insert(s.id);
                }
            }
            _ => review.push(format!(
                "{n}: overlaps old speakers confirmed as different people ({}); left unconfirmed",
                sources
                    .iter()
                    .filter(|s| s.assignment == Assignment::Confirmed)
                    .map(|s| s.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
        speaker.rejected_person_ids = rejected;
    }
    for s in old.speakers.iter().filter(|s| {
        s.assignment == Assignment::Confirmed
            && s.live_promotion.is_none()
            && !carried.contains(&s.id)
    }) {
        let person = s.person_id.as_ref().map_or("?", PersonId::as_str);
        review.push(format!(
            "old {}: confirmed as {person} but matches no single new speaker",
            s.id
        ));
    }
    review
}

#[cfg(test)]
mod tests {
    use broscribe_core::schema::{
        Action, ActionStatus, Enrichment, Person, Segment, StatusSource, enrichment_item_id, now,
    };
    use broscribe_core::{SpeakerId, ops};

    use super::*;
    use crate::import::{ImportOptions, import_file};
    use crate::testutil::{Fixture, models};

    fn sid(n: u32) -> SpeakerId {
        SpeakerId::new(n).unwrap()
    }

    fn swap(s: SpeakerId) -> SpeakerId {
        match s.number() {
            1 => sid(2),
            2 => sid(1),
            _ => s,
        }
    }

    #[test]
    fn swapped_speaker_order_keeps_identity_owner_and_status() {
        let fx = Fixture::new();
        let wav = fx.wav(
            "a.wav",
            &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0), (None, 1.0)],
        );
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let (ana, bea) = (fx.lib.new_person_id(), fx.lib.new_person_id());
        let action_id = enrichment_item_id("{{S2}} sends the notes", 1.0);
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&Person::new(ana.clone(), "Ana Invented"))?;
            tx.save_person(&Person::new(bea.clone(), "Bea Invented"))?;
            let mut m = tx.library().load_meeting(&id)?;
            assert_eq!(m.speakers.len(), 2);
            // Stored labels swapped: S2 is the first voice.
            for s in &mut m.speakers {
                s.id = swap(s.id);
            }
            for seg in &mut m.segments {
                seg.speaker = seg.speaker.map(swap);
            }
            let first = m.speaker_mut(sid(2)).unwrap();
            first.person_id = Some(ana.clone());
            first.assignment = Assignment::Confirmed;
            first.rejected_person_ids = vec![bea.clone()];
            // An old speaker that matches nothing new (silence only).
            let mut ghost = broscribe_core::schema::Speaker::new(sid(9), "t0");
            ghost.person_id = Some(bea.clone());
            ghost.assignment = Assignment::Confirmed;
            m.speakers.push(ghost);
            m.segments.push(Segment {
                speech_overlap: Vec::new(),
                id: 900,
                track: "t0".into(),
                speaker: Some(sid(9)),
                start: 4.2,
                end: 4.8,
                text: "hm".into(),
                words: Vec::new(),
            });
            m.enrichment = Some(Enrichment {
                provider: "fake".into(),
                model: None,
                requested_model: None,
                model_reported: false,
                created_at: now(),
                stale: false,
                revision: meeting_window_revision(&m),
                attribution_revision: String::new(),
                line_people: Default::default(),
                summary: "{{S2}} met {{S9}}.".into(),
                topics: vec![],
                keywords: vec![],
                tags: vec![],
                decisions: vec![],
                actions: vec![Action {
                    id: action_id.clone(),
                    text: "{{S2}} sends the notes".into(),
                    owner_speaker: Some(sid(2)),
                    owner_name: None,
                    due: None,
                    at_s: 1.0,
                    status: ActionStatus::Done,
                    status_source: StatusSource::User,
                    status_updated_at: Some(now()),
                    original_owner: None,
                    due_user_set: false,
                    stale: false,
                }],
                instructions: None,
            });
            // A template result with the same references, no enrichment
            // involved in its mapping.
            m.set_template_result(broscribe_core::schema::TemplateResult {
                template_id: "meeting-notes".into(),
                template_name: "Meeting notes".into(),
                instructions_sha256: String::new(),
                provider: "fake".into(),
                model: None,
                requested_model: None,
                model_reported: false,
                created_at: now(),
                revision: meeting_window_revision(&m),
                attribution_revision: String::new(),
                line_people: Default::default(),
                stale: false,
                markdown: "- {{S2}} leads; {{S9}} hummed.".into(),
            });
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)
        })
        .unwrap();

        let out = reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        assert!(out.skipped.is_none());
        let m = fx.lib.load_meeting(&id).unwrap();
        // Fresh diarization labels the first voice S1.
        let s1 = m.speaker(sid(1)).unwrap();
        assert_eq!(s1.assignment, Assignment::Confirmed);
        assert_eq!(s1.person_id.as_ref(), Some(&ana));
        assert_eq!(s1.rejected_person_ids, vec![bea.clone()]);
        let e = m.enrichment.as_ref().unwrap();
        let a = &e.actions[0];
        assert_eq!(a.owner_speaker, Some(sid(1)));
        assert_eq!(a.text, "{{S1}} sends the notes");
        assert_eq!(
            (a.status, a.status_source),
            (ActionStatus::Done, StatusSource::User)
        );
        assert_eq!(e.summary, "{{S1}} met S9.");
        assert!(
            out.needs_review.iter().any(|r| r.contains("S9")),
            "{:?}",
            out.needs_review
        );
        assert!(m.speaker(sid(9)).is_none());
        // The template result follows the same mapping; its unmapped
        // reference is listed under its own source and it renders by name.
        let r = m.template_result("meeting-notes").unwrap();
        assert_eq!(r.markdown, "- {{S1}} leads; S9 hummed.");
        assert!(
            m.review_notes
                .iter()
                .any(|n| n.source == ReviewSource::Template
                    && n.text == "template meeting-notes: S9"),
            "{:?}",
            m.review_notes
        );
        let people = fx.lib.people_lookup().unwrap();
        assert_eq!(
            broscribe_core::render::template_result_md(&m, r, &people),
            "- Ana Invented leads; S9 hummed."
        );
        let texts: Vec<&str> = m.review_notes.iter().map(|n| n.text.as_str()).collect();
        for note in &out.needs_review {
            assert!(texts.contains(&note.as_str()), "{:?}", m.review_notes);
        }
        assert!(
            m.review_notes
                .iter()
                .any(|n| n.source == ReviewSource::Reprocess && n.text.contains("old S9")),
            "{:?}",
            m.review_notes
        );
    }

    /// "Can't tell" follows the one-to-one shown mapping with its original
    /// time; a speaker the new analysis splits is not carried (it returns
    /// to the review queue).
    #[test]
    fn reprocess_carries_review_dismissals_one_to_one_only() {
        let fx = Fixture::new();
        let wav = fx.wav(
            "a.wav",
            &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0), (None, 1.0)],
        );
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-10T09:00:00+01:00").unwrap();
        ops::with_index(&fx.lib, |tx, index| {
            let mut m = tx.library().load_meeting(&id)?;
            // Stored labels swapped: S2 is the first voice, dismissed.
            for s in &mut m.speakers {
                s.id = swap(s.id);
            }
            for seg in &mut m.segments {
                seg.speaker = seg.speaker.map(swap);
            }
            m.speaker_mut(sid(2)).unwrap().review_dismissed_at = Some(at);
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)
        })
        .unwrap();
        reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        let m = fx.lib.load_meeting(&id).unwrap();
        // Fresh diarization labels the first voice S1.
        assert_eq!(m.speaker(sid(1)).unwrap().review_dismissed_at, Some(at));
        assert_eq!(m.speaker(sid(2)).unwrap().review_dismissed_at, None);
        assert!(ops::review_dismissed(&m, sid(1)));

        // One old speaker holding both voices links to two new ones.
        ops::with_index(&fx.lib, |tx, index| {
            let mut m = tx.library().load_meeting(&id)?;
            for seg in &mut m.segments {
                seg.speaker = Some(sid(1));
            }
            m.speakers.retain(|s| s.id == sid(1));
            m.speakers[0].talk_s = 8.0;
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)
        })
        .unwrap();
        reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        let m = fx.lib.load_meeting(&id).unwrap();
        assert_eq!(m.speakers.len(), 2);
        assert!(m.speakers.iter().all(|s| s.review_dismissed_at.is_none()));
    }

    #[test]
    fn reprocess_moves_learned_samples_to_the_renumbered_speakers() {
        use broscribe_core::schema::{Strength, VoiceprintKind};
        let fx = Fixture::new();
        let wav = fx.wav(
            "a.wav",
            &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0), (None, 1.0)],
        );
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let (ana, bea) = (fx.lib.new_person_id(), fx.lib.new_person_id());
        let model = models().embedder.model_id().to_owned();
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&Person::new(ana.clone(), "Ana Invented"))?;
            tx.save_person(&Person::new(bea.clone(), "Bea Invented"))?;
            let mut m = tx.library().load_meeting(&id)?;
            // Stored labels swapped: S2 is the first voice; both are Ana.
            for s in &mut m.speakers {
                s.id = swap(s.id);
                s.person_id = Some(ana.clone());
                s.assignment = Assignment::Confirmed;
            }
            for seg in &mut m.segments {
                seg.speaker = seg.speaker.map(swap);
            }
            // Confirmation samples of each voice, recorded under the old
            // labels, plus one of a speaker that no longer exists.
            let mut prints = Vec::new();
            for (speaker, start) in [(sid(2), 0.5), (sid(1), 5.5), (sid(9), 4.2)] {
                let mut vp = ops::weak_sample(&m, sid(1), &ana)?.unwrap();
                vp.id = format!("synthetic-{speaker}");
                vp.source.kind = VoiceprintKind::Confirmation;
                vp.source.speaker_id = Some(speaker);
                vp.source.start = start;
                vp.source.end = start + 3.0;
                vp.source.ranges = Vec::new();
                vp.strength = Strength::Confirmed;
                prints.push(vp);
            }
            tx.upsert_voiceprints(&ana, prints)?;
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)
        })
        .unwrap();

        reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        let speaker_of = |sample: &str| {
            fx.lib
                .load_voiceprints(&ana)
                .unwrap()
                .embeddings
                .into_iter()
                .find(|v| v.id == sample)
                .map(|v| v.source.speaker_id)
        };
        // Fresh diarization labels the first voice S1.
        assert_eq!(speaker_of("synthetic-S2"), Some(Some(sid(1))));
        assert_eq!(speaker_of("synthetic-S1"), Some(Some(sid(2))));
        assert_eq!(speaker_of("synthetic-S9"), Some(None));

        // Correcting the first voice takes only its samples from Ana.
        ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(tx, index, &id, sid(1), "Bea Invented", Vec::new(), &model)
        })
        .unwrap();
        assert_eq!(speaker_of("synthetic-S2"), None);
        assert_eq!(speaker_of("synthetic-S1"), Some(Some(sid(2))));
        let m = fx.lib.load_meeting(&id).unwrap();
        assert_eq!(m.speaker(sid(1)).unwrap().person_id.as_ref(), Some(&bea));
        assert_eq!(m.speaker(sid(2)).unwrap().person_id.as_ref(), Some(&ana));
    }

    #[test]
    fn unmatched_confirmation_without_enrichment_is_kept_as_a_reprocess_note() {
        use broscribe_core::schema::ReviewNote;
        let fx = Fixture::new();
        let wav = fx.wav(
            "a.wav",
            &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0), (None, 1.0)],
        );
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let ana = fx.lib.new_person_id();
        let kept = ReviewNote {
            source: ReviewSource::Enrichment,
            text: "Check the owner of an action.".into(),
        };
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&Person::new(ana.clone(), "Ana Invented"))?;
            let mut m = tx.library().load_meeting(&id)?;
            assert!(m.enrichment.is_none());
            // A confirmed old speaker that matches nothing new (silence only).
            let mut ghost = broscribe_core::schema::Speaker::new(sid(9), "t0");
            ghost.person_id = Some(ana.clone());
            ghost.assignment = Assignment::Confirmed;
            m.speakers.push(ghost);
            m.segments.push(Segment {
                speech_overlap: Vec::new(),
                id: 900,
                track: "t0".into(),
                speaker: Some(sid(9)),
                start: 4.2,
                end: 4.8,
                text: "hm".into(),
                words: Vec::new(),
            });
            m.review_notes = vec![
                kept.clone(),
                ReviewNote {
                    source: ReviewSource::Reprocess,
                    text: "from an earlier reprocessing".into(),
                },
            ];
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)
        })
        .unwrap();

        let out = reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        let m = fx.lib.load_meeting(&id).unwrap();
        assert!(m.enrichment.is_none());
        let note = out
            .needs_review
            .iter()
            .find(|r| r.contains("old S9"))
            .expect("unmatched confirmation reported");
        assert_eq!(
            m.review_notes,
            vec![
                kept.clone(),
                ReviewNote {
                    source: ReviewSource::Reprocess,
                    text: note.clone(),
                },
            ]
        );

        // The next reprocessing replaces only its own notes.
        let out = reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        assert!(out.needs_review.is_empty(), "{:?}", out.needs_review);
        assert_eq!(fx.lib.load_meeting(&id).unwrap().review_notes, vec![kept]);
    }

    #[test]
    fn samples_of_merged_old_speakers_follow_the_inherited_confirmation() {
        use broscribe_core::schema::{Strength, VoiceprintKind};
        let fx = Fixture::new();
        let wav = fx.wav(
            "a.wav",
            &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0), (None, 1.0)],
        );
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let (ana, bea) = (fx.lib.new_person_id(), fx.lib.new_person_id());
        let model = models().embedder.model_id().to_owned();
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&Person::new(ana.clone(), "Ana Invented"))?;
            tx.save_person(&Person::new(bea.clone(), "Bea Invented"))?;
            let mut m = tx.library().load_meeting(&id)?;
            let base = ops::weak_sample(&m, sid(1), &ana)?.unwrap();
            // Over-split: the first voice's second half is S3; both are Ana.
            let first: Vec<_> = m
                .segments
                .iter()
                .filter(|s| s.speaker == Some(sid(1)))
                .map(|s| (s.start, s.end))
                .collect();
            let lo = first.iter().map(|r| r.0).fold(f64::INFINITY, f64::min);
            let hi = first.iter().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max);
            let mid = (lo + hi) / 2.0;
            let mut next_id = m.segments.iter().map(|s| s.id).max().unwrap() + 1;
            let mut tails = Vec::new();
            for seg in m.segments.iter_mut().filter(|s| s.speaker == Some(sid(1))) {
                if seg.start >= mid {
                    seg.speaker = Some(sid(3));
                } else if seg.end > mid {
                    let mut tail = seg.clone();
                    tail.id = next_id;
                    next_id += 1;
                    tail.start = mid;
                    tail.speaker = Some(sid(3));
                    tail.words.clear();
                    seg.end = mid;
                    seg.words.clear();
                    tails.push(tail);
                }
            }
            m.segments.extend(tails);
            let mut s3 = m.speaker(sid(1)).unwrap().clone();
            s3.id = sid(3);
            m.speakers.push(s3);
            for s in m.speakers.iter_mut().filter(|s| s.id != sid(2)) {
                s.person_id = Some(ana.clone());
                s.assignment = Assignment::Confirmed;
            }
            let mut prints = Vec::new();
            for (speaker, start) in [(sid(1), lo + 0.2), (sid(3), mid + 0.2)] {
                let mut vp = base.clone();
                vp.id = format!("synthetic-{speaker}");
                vp.source.kind = VoiceprintKind::Confirmation;
                vp.source.speaker_id = Some(speaker);
                vp.source.start = start;
                vp.source.end = start + 1.5;
                vp.source.ranges = Vec::new();
                vp.strength = Strength::Confirmed;
                prints.push(vp);
            }
            tx.upsert_voiceprints(&ana, prints)?;
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)
        })
        .unwrap();

        // R9: two old confirmed-Ana speakers analysed as one cluster.
        let out = reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        assert!(out.needs_review.is_empty(), "{:?}", out.needs_review);
        let m = fx.lib.load_meeting(&id).unwrap();
        assert!(
            m.review_notes.is_empty(),
            "no unmapped-confirmation note: {:?}",
            m.review_notes
        );
        let s1 = m.speaker(sid(1)).unwrap();
        assert_eq!(
            (s1.assignment, s1.person_id.as_ref()),
            (Assignment::Confirmed, Some(&ana))
        );
        let speaker_of = |sample: &str| {
            fx.lib
                .load_voiceprints(&ana)
                .unwrap()
                .embeddings
                .into_iter()
                .find(|v| v.id == sample)
                .map(|v| v.source.speaker_id)
        };
        // Both old speakers link only to the new S1, so their samples follow.
        assert_eq!(speaker_of("synthetic-S1"), Some(Some(sid(1))));
        assert_eq!(speaker_of("synthetic-S3"), Some(Some(sid(1))));

        // Correcting the voice takes all of its samples from Ana.
        ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(tx, index, &id, sid(1), "Bea Invented", Vec::new(), &model)
        })
        .unwrap();
        assert_eq!(speaker_of("synthetic-S1"), None);
        assert_eq!(speaker_of("synthetic-S3"), None);
    }

    #[test]
    fn asr_only_preserves_speakers_and_enrichment_references() {
        let fx = Fixture::new();
        let wav = fx.wav("asr.wav", &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0)]);
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        fx.lib
            .write(|tx| {
                let mut m = tx.library().load_meeting(&id)?;
                m.segments[0].text = "old synthetic transcription".into();
                m.enrichment = Some(Enrichment {
                    provider: "fake".into(),
                    model: None,
                    requested_model: None,
                    model_reported: false,
                    created_at: now(),
                    stale: false,
                    revision: meeting_window_revision(&m),
                    attribution_revision: String::new(),
                    line_people: Default::default(),
                    summary: "{{S1}} met {{S2}}".into(),
                    topics: vec![],
                    keywords: vec![],
                    tags: vec![],
                    decisions: vec![],
                    actions: vec![],
                    instructions: None,
                });
                tx.save_meeting(&m)?;
                Index::open(tx.root())?.upsert_meeting(tx, &m)
            })
            .unwrap();
        let before = fx.lib.load_meeting(&id).unwrap();
        let out = reprocess(
            &fx.env(),
            &mut models(),
            &id,
            ReprocessOptions { asr_only: true },
        )
        .unwrap();
        let after = fx.lib.load_meeting(&id).unwrap();
        assert_eq!(after.speakers, before.speakers);
        assert_eq!(
            after.enrichment.as_ref().unwrap().summary,
            "{{S1}} met {{S2}}"
        );
        assert!(out.mapping.is_empty());
        assert!(out.revision_changed && out.enrichment_stale);
    }

    #[test]
    fn reprocessing_keeps_simultaneous_reference_tracks_independent() {
        use broscribe_core::schema::{Handling, RecordingSource, Speaker, Track, TrackRole};
        let fx = Fixture::new();
        let mic = fx.wav("mic.wav", &[(Some(0), 4.0)]);
        let system = fx.wav("system.wav", &[(Some(1), 4.0)]);
        let id = import_file(&fx.env(), &mut models(), &mic, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let first = fx.lib.new_person_id();
        let second = fx.lib.new_person_id();
        fx.lib
            .write(|tx| {
                tx.save_person(&Person::new(first.clone(), "Mira Invented"))?;
                tx.save_person(&Person::new(second.clone(), "Niko Invented"))?;
                let mut m = tx.library().load_meeting(&id)?;
                // These synthetic tracks have no echo; preserve that explicit
                // capture setup when exercising the paired reprocessing path.
                m.setup = Some("headphones".into());
                m.processing.echo_report = Some(serde_json::json!({"requested": "none"}));
                let recording = m.recording.as_mut().unwrap();
                recording.handling = Handling::Reference;
                recording.path = mic.clone();
                recording.sources = vec![
                    RecordingSource {
                        path: mic.clone(),
                        track_index: 0,
                        offset_s: 0.0,
                        sha256: crate::audio::file_sha256(&mic)?.0,
                    },
                    RecordingSource {
                        path: system.clone(),
                        track_index: 0,
                        offset_s: 1.0,
                        sha256: crate::audio::file_sha256(&system)?.0,
                    },
                ];
                m.tracks = vec![
                    Track {
                        id: "t0".into(),
                        role: TrackRole::Mic,
                    },
                    Track {
                        id: "t1".into(),
                        role: TrackRole::System,
                    },
                ];
                m.speakers = vec![Speaker::new(sid(1), "t0"), Speaker::new(sid(2), "t1")];
                m.speakers[0].person_id = Some(first.clone());
                m.speakers[1].person_id = Some(second.clone());
                for speaker in &mut m.speakers {
                    speaker.assignment = Assignment::Confirmed;
                }
                m.segments = vec![
                    Segment {
                        speech_overlap: Vec::new(),
                        id: 0,
                        track: "t0".into(),
                        speaker: Some(sid(1)),
                        start: 0.0,
                        end: 4.0,
                        text: "microphone notes".into(),
                        words: vec![],
                    },
                    Segment {
                        speech_overlap: Vec::new(),
                        id: 1,
                        track: "t1".into(),
                        speaker: Some(sid(2)),
                        start: 1.0,
                        end: 5.0,
                        text: "remote notes".into(),
                        words: vec![],
                    },
                ];
                tx.save_meeting(&m)?;
                Index::open(tx.root())?.upsert_meeting(tx, &m)
            })
            .unwrap();
        let out = reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        assert_eq!(out.mapping.len(), 2);
        let m = fx.lib.load_meeting(&id).unwrap();
        let first_id = out
            .mapping
            .iter()
            .find(|link| link.old == sid(1))
            .unwrap()
            .new;
        let second_id = out
            .mapping
            .iter()
            .find(|link| link.old == sid(2))
            .unwrap()
            .new;
        assert_ne!(first_id, second_id);
        assert_eq!(
            m.speaker(first_id).unwrap().person_id.as_ref(),
            Some(&first)
        );
        assert_eq!(m.speaker(first_id).unwrap().track, "t0");
        assert_eq!(
            m.speaker(second_id).unwrap().person_id.as_ref(),
            Some(&second)
        );
        assert_eq!(m.speaker(second_id).unwrap().track, "t1");
        assert!(m.segments.iter().any(|s| s.track == "t1" && s.start >= 1.0));
        // ASR-only would transcribe the microphone without echo cancellation:
        // it is refused and the meeting stays as the full reprocess left it.
        let before = fx.lib.load_meeting(&id).unwrap();
        let err = reprocess(
            &fx.env(),
            &mut models(),
            &id,
            ReprocessOptions { asr_only: true },
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)), "{err}");
        assert!(err.to_string().contains("without --asr-only"), "{err}");
        assert_eq!(fx.lib.load_meeting(&id).unwrap(), before);
    }

    #[test]
    fn queued_reprocess_skips_deleted_audio_before_loading_models() {
        struct MissingModels;
        impl ModelProvider for MissingModels {
            fn load(&self, _: &Profile) -> Result<ModelSet> {
                panic!("deleted recordings must not load inference models")
            }
            fn load_vad(&self, _: &str) -> Result<Box<dyn broscribe_models::Vad + Send>> {
                panic!("deleted recordings must not load inference models")
            }
            fn load_embedder(
                &self,
                _: &str,
            ) -> Result<Box<dyn broscribe_models::SpeakerEmbedder + Send>> {
                panic!("deleted recordings must not load inference models")
            }
        }
        let fx = Fixture::new();
        let wav = fx.wav("queued.wav", &[(Some(0), 3.0)]);
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        fx.lib
            .write(|tx| {
                let mut m = tx.library().load_meeting(&id)?;
                m.recording.as_mut().unwrap().state =
                    broscribe_core::schema::RecordingState::Deleted;
                tx.save_meeting(&m)
            })
            .unwrap();
        let out = match run_reprocess_interactive(
            &fx.env(),
            &id,
            None,
            true,
            &MissingModels,
            &AtomicBool::new(false),
        )
        .unwrap()
        {
            InteractiveReprocess::Ran(out) => out,
            other => panic!("{other:?}"),
        };
        assert!(out.skipped.is_some());
        assert!(out.asr_only);
        // The skipped run closed its reserved job.
        let queue = broscribe_core::queue::Queue::open(fx.lib.root()).unwrap();
        assert!(queue.list(REPROCESS_KIND).unwrap().is_empty());
    }

    #[test]
    fn line_corrections_follow_reprocessing_or_are_dropped_with_a_note() {
        use broscribe_core::schema::UtterancePerson;
        for asr_only in [false, true] {
            let fx = Fixture::new();
            let wav = fx.wav("lines.wav", &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0)]);
            let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
                .unwrap()
                .meeting_id;
            let cleo = fx.lib.new_person_id();
            let entry = |s: &Segment, text_sha256: String| UtterancePerson {
                track: s.track.clone(),
                start: s.start,
                end: s.end,
                text_sha256,
                person_id: cleo.clone(),
                at: now(),
                live_decision: None,
            };
            let before = ops::with_index(&fx.lib, |tx, index| {
                tx.save_person(&Person::new(cleo.clone(), "Cleo Invented"))?;
                let mut m = tx.library().load_meeting(&id)?;
                assert!(m.segments.len() >= 2, "{:?}", m.segments);
                // IDs the new analysis renumbers.
                for s in &mut m.segments {
                    s.id += 100;
                }
                let last = m.segments.last().unwrap().clone();
                let first = m.segments[0].clone();
                m.utterance_people = vec![
                    entry(&first, schema_hash("words no model transcribes")),
                    entry(&last, schema_hash(&last.text)),
                ];
                tx.save_meeting(&m)?;
                index.upsert_meeting(tx, &m)?;
                Ok(m)
            })
            .unwrap();
            let out =
                reprocess(&fx.env(), &mut models(), &id, ReprocessOptions { asr_only }).unwrap();
            let m = fx.lib.load_meeting(&id).unwrap();
            assert!(m.segments.iter().all(|s| s.id < 100), "renumbered");
            let lines: Vec<u32> = ops::effective_attribution(&m)
                .into_iter()
                .filter(|a| a.line)
                .map(|a| a.segment_id)
                .collect();
            let last = m.segments.iter().max_by(|a, b| a.start.total_cmp(&b.start));
            assert_eq!(lines, [last.unwrap().id], "asr_only={asr_only}");
            assert_eq!(m.utterance_people.len(), 1);
            assert_eq!(m.utterance_people[0].person_id, cleo);
            assert!(
                out.needs_review
                    .iter()
                    .any(|n| n.contains("line correction") && n.contains(cleo.as_str())),
                "{:?}",
                out.needs_review
            );
            assert!(m.review_notes.iter().any(|n| {
                n.source == ReviewSource::Reprocess && n.text.contains("line correction")
            }));
            // Identity never follows a line correction.
            assert!(
                m.speakers
                    .iter()
                    .all(|s| s.person_id.as_ref() != Some(&cleo))
            );
            assert!(!fx.lib.person_dir(&cleo).join("voiceprints.json").exists());
            assert_eq!(before.speakers.len(), m.speakers.len());
        }
    }

    fn schema_hash(text: &str) -> String {
        broscribe_core::schema::sha256_hex(text.as_bytes())
    }

    #[test]
    fn deleted_recording_is_skipped_with_a_message() {
        let fx = Fixture::new();
        let wav = fx.wav("a.wav", &[(Some(0), 3.0)]);
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        fx.lib
            .write(|tx| {
                let mut m = tx.library().load_meeting(&id)?;
                m.recording.as_mut().unwrap().state =
                    broscribe_core::schema::RecordingState::Deleted;
                tx.save_meeting(&m)
            })
            .unwrap();
        let before = fx.lib.load_meeting(&id).unwrap();
        let out = reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        assert!(out.skipped.is_some());
        assert_eq!(fx.lib.load_meeting(&id).unwrap(), before);
    }

    #[test]
    fn mapping_is_one_to_one() {
        let t = |a: f64, b: f64, n: u32| Turn {
            start: a,
            end: b,
            speaker: sid(n),
        };
        // Old S1 and S2 both fall into new S1: ambiguous.
        let old = [t(0.0, 3.0, 1), t(3.0, 6.0, 2), t(6.0, 9.0, 3)];
        let new = [t(0.0, 6.0, 1), t(6.0, 9.0, 2)];
        let links = link_speakers(&old, &new);
        let map = one_to_one(&links);
        assert_eq!(map.len(), 1);
        assert_eq!((map[0].old, map[0].new), (sid(3), sid(2)));
    }

    #[test]
    fn shown_turns_follow_the_groups_on_their_own_track() {
        let mut m = Meeting::new(
            MeetingId::parse("m-0000c3").unwrap(),
            broscribe_core::schema::Source::Import,
            now(),
        );
        m.speakers = vec![
            broscribe_core::schema::Speaker::new(sid(1), "mic"),
            broscribe_core::schema::Speaker::new(sid(2), "system"),
            broscribe_core::schema::Speaker::new(sid(3), "system"),
        ];
        m.speaker_groups = vec![SpeakerGroup::new(sid(2), [sid(3)], GroupOrigin::User)];
        let seg = |id: u32, track: &str, speaker: u32| Segment {
            speech_overlap: Vec::new(),
            id,
            track: track.into(),
            speaker: Some(sid(speaker)),
            start: 0.0,
            end: 5.0,
            text: "overlapping synthetic speech".into(),
            words: Vec::new(),
        };
        // Simultaneous speech: the microphone segment comes first.
        m.segments = vec![seg(0, "mic", 1), seg(1, "system", 2)];
        let t = |n: u32| Turn {
            start: 0.0,
            end: 5.0,
            speaker: sid(n),
        };
        // System S3 is grouped under system S2; S4 is no speaker.
        let turns = relabel_turns(&m, &[t(1), t(2), t(3), t(4)], |s| {
            ops::effective_speaker(&m, s)
        });
        let speakers: Vec<_> = turns.iter().map(|t| t.speaker).collect();
        assert_eq!(speakers, vec![sid(1), sid(2), sid(2)]);
    }

    /// Labels the second voice's turns from 9.5 s on as another cluster.
    struct SplitDiarizer;

    impl broscribe_models::Diarizer for SplitDiarizer {
        fn diarize(&self, samples: &[f32]) -> Result<Vec<broscribe_models::DiarSegment>> {
            let mut turns = crate::testutil::ToneDiarizer.diarize(samples)?;
            for t in &mut turns {
                if t.speaker == 1 && t.start_s >= 9.5 {
                    t.speaker = 3;
                }
            }
            Ok(turns)
        }
    }

    fn grouping(fx: &mut Fixture) {
        fx.config.identity.insert(
            fx.config.profile.speaker_embedding.clone(),
            broscribe_core::config::Thresholds {
                merge: Some(0.5),
                auto_merge: Some(0.95),
                ..Default::default()
            },
        );
    }

    fn shown_at(m: &Meeting, t: f64) -> SpeakerId {
        let s = m
            .segments
            .iter()
            .find(|s| s.start <= t && s.end >= t)
            .and_then(|s| s.speaker)
            .unwrap();
        ops::effective_speaker(m, s)
    }

    /// An old confirmed speaker the fresh analysis splits into two clusters
    /// that get grouped keeps its confirmation through the shown mapping,
    /// with no review note; the clusters stay as diarized.
    #[test]
    fn a_confirmed_speaker_split_by_the_fresh_analysis_keeps_its_name_as_a_group() {
        let mut fx = Fixture::new();
        grouping(&mut fx);
        let wav = fx.wav(
            "split.wav",
            &[
                (Some(0), 4.0),
                (None, 1.0),
                (Some(1), 4.0),
                (None, 1.0),
                (Some(1), 4.0),
                (None, 1.0),
            ],
        );
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let m = fx.lib.load_meeting(&id).unwrap();
        assert!(m.speaker_groups.is_empty(), "one cluster per voice");
        let voice = shown_at(&m, 7.0);
        ops::with_index(&fx.lib, |tx, idx| {
            ops::name_speaker(tx, idx, &id, voice, "Ana Invented", Vec::new(), "x")
        })
        .unwrap();
        let mut split = models();
        split.diarizer = Box::new(SplitDiarizer);
        let out = reprocess(&fx.env(), &mut split, &id, ReprocessOptions::default()).unwrap();
        let m = fx.lib.load_meeting(&id).unwrap();
        let raw = |t: f64| {
            m.segments
                .iter()
                .find(|s| s.start <= t && s.end >= t)
                .and_then(|s| s.speaker)
                .unwrap()
        };
        assert_ne!(raw(7.0), raw(12.0), "two fresh clusters");
        assert_eq!(shown_at(&m, 7.0), shown_at(&m, 12.0), "grouped");
        let shown = m.speaker(shown_at(&m, 7.0)).unwrap();
        assert_eq!(shown.assignment, Assignment::Confirmed);
        let ana = fx.lib.find_person_by_name("Ana Invented").unwrap().unwrap();
        assert_eq!(shown.person_id.as_ref(), Some(&ana.id));
        assert!(out.needs_review.is_empty(), "{:?}", out.needs_review);
    }

    /// User merges and separations survive a full reprocess with unchanged
    /// clusters (no note) and an ASR-only one.
    #[test]
    fn user_groups_and_separations_carry_through_reprocessing() {
        let mut fx = Fixture::new();
        grouping(&mut fx);
        let wav = fx.wav(
            "three.wav",
            &[
                (Some(0), 4.0),
                (None, 1.0),
                (Some(1), 4.0),
                (None, 1.0),
                (Some(2), 4.0),
                (None, 1.0),
            ],
        );
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let m = fx.lib.load_meeting(&id).unwrap();
        let (a, b, c) = (shown_at(&m, 2.0), shown_at(&m, 7.0), shown_at(&m, 12.0));
        let fence = ops::SpeakerFence::of(&m);
        ops::with_index(&fx.lib, |tx, idx| {
            ops::group_speakers(tx, idx, &id, a, &[b], GroupOrigin::User, &fence, "x", None)?;
            let fence = ops::SpeakerFence::of(&tx.library().load_meeting(&id)?);
            ops::mark_not_same(tx, idx, &id, b, c, &fence)
        })
        .unwrap();
        let before = fx.lib.load_meeting(&id).unwrap();
        let out = reprocess(&fx.env(), &mut models(), &id, ReprocessOptions::default()).unwrap();
        assert!(out.needs_review.is_empty(), "{:?}", out.needs_review);
        let m = fx.lib.load_meeting(&id).unwrap();
        let (a2, b2, c2) = (shown_at(&m, 2.0), shown_at(&m, 7.0), shown_at(&m, 12.0));
        assert_eq!(a2, b2, "the user's merge is kept");
        assert_ne!(b2, c2);
        assert_eq!(m.speaker_groups.len(), 1);
        assert_eq!(m.speaker_groups[0].origin, GroupOrigin::User);
        assert_eq!(m.not_same.len(), before.not_same.len());
        // ASR-only keeps speaker IDs, so every decision stays as it is.
        let groups = (m.speaker_groups.clone(), m.not_same.clone());
        reprocess(
            &fx.env(),
            &mut models(),
            &id,
            ReprocessOptions { asr_only: true },
        )
        .unwrap();
        let m = fx.lib.load_meeting(&id).unwrap();
        assert_eq!((m.speaker_groups, m.not_same), groups);
    }

    fn raw_at(m: &Meeting, t: f64) -> SpeakerId {
        m.segments
            .iter()
            .find(|s| s.start <= t && s.end >= t)
            .and_then(|s| s.speaker)
            .unwrap()
    }

    fn split_models() -> ModelSet {
        let mut set = crate::caption_replay::tests::caption_models();
        set.diarizer = Box::new(SplitDiarizer);
        set
    }

    /// One voice the diarizer splits into two clusters, imported without
    /// automatic grouping: (meeting, first cluster, second cluster).
    fn split_import(fx: &Fixture) -> (MeetingId, SpeakerId, SpeakerId) {
        let wav = fx.wav(
            "split.wav",
            &[
                (Some(0), 4.0),
                (None, 1.0),
                (Some(1), 4.0),
                (None, 1.0),
                (Some(1), 4.0),
                (None, 1.0),
            ],
        );
        let id = import_file(
            &fx.env(),
            &mut split_models(),
            &wav,
            &ImportOptions::default(),
        )
        .unwrap()
        .meeting_id;
        let m = fx.lib.load_meeting(&id).unwrap();
        let (a, b) = (raw_at(&m, 7.0), raw_at(&m, 12.0));
        assert_ne!(a, b);
        (id, a, b)
    }

    fn confirm(fx: &Fixture, id: &MeetingId, speaker: SpeakerId, name: &str) {
        let model = models().embedder.model_id().to_owned();
        ops::with_index(&fx.lib, |tx, idx| {
            ops::name_speaker(tx, idx, id, speaker, name, Vec::new(), &model)
        })
        .unwrap();
    }

    /// A fresh cluster the saved captions prove to be Bob, as similar as can
    /// be to the cluster confirmed as Ana, is not grouped after reprocessing.
    #[test]
    fn a_cluster_the_captions_prove_to_be_someone_else_is_never_grouped() {
        let mut fx = Fixture::new();
        let (id, _, second) = split_import(&fx);
        confirm(&fx, &id, second, "Ana Invented");
        let event = broscribe_readers::ReaderEvent::Caption(broscribe_readers::CaptionEvent {
            caption_id: "synthetic-caption".into(),
            revision: 1,
            speaker_name: "Bob Example".into(),
            is_self: false,
            text: "alpha beta gamma delta".into(),
            observed_s: 8.5,
            start_s: 5.5,
            end_s: 8.5,
        });
        std::fs::write(
            fx.lib.meeting_dir(&id).unwrap().join("captions.jsonl"),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        grouping(&mut fx);
        reprocess(
            &fx.env(),
            &mut split_models(),
            &id,
            ReprocessOptions::default(),
        )
        .unwrap();
        let m = fx.lib.load_meeting(&id).unwrap();
        assert!(m.speaker_groups.is_empty(), "{:?}", m.speaker_groups);
        let bob = fx.lib.find_person_by_name("Bob Example").unwrap().unwrap();
        let ana = fx.lib.find_person_by_name("Ana Invented").unwrap().unwrap();
        let (first, second) = (
            m.speaker(raw_at(&m, 7.0)).unwrap(),
            m.speaker(raw_at(&m, 12.0)).unwrap(),
        );
        assert_eq!(first.person_id.as_ref(), Some(&bob.id));
        assert_eq!(
            (second.assignment, second.person_id.as_ref()),
            (Assignment::Confirmed, Some(&ana.id))
        );
    }

    /// Unmerging is remembered: a full reprocess with the same clusters does
    /// not group the pair again.
    #[test]
    fn an_unmerged_pair_stays_apart_through_a_full_reprocess() {
        let mut fx = Fixture::new();
        grouping(&mut fx);
        let (id, _, _) = split_import(&fx);
        let m = fx.lib.load_meeting(&id).unwrap();
        assert_eq!(m.speaker_groups.len(), 1, "grouped on import");
        let group = m.speaker_groups[0].id.clone();
        let fence = ops::SpeakerFence::of(&m);
        ops::with_index(&fx.lib, |tx, idx| {
            ops::ungroup_speakers(tx, idx, &id, &group, &fence)
        })
        .unwrap();
        let out = reprocess(
            &fx.env(),
            &mut split_models(),
            &id,
            ReprocessOptions::default(),
        )
        .unwrap();
        assert!(out.needs_review.is_empty(), "{:?}", out.needs_review);
        let m = fx.lib.load_meeting(&id).unwrap();
        assert!(m.speaker_groups.is_empty(), "{:?}", m.speaker_groups);
        let (a, b) = (raw_at(&m, 7.0), raw_at(&m, 12.0));
        let pair = if a <= b { [a, b] } else { [b, a] };
        assert_eq!(m.not_same, vec![pair]);
    }

    /// A cluster the reprocess groups as a member teaches no weak sample of
    /// its own auto name, although resolution synced samples before grouping.
    #[test]
    fn a_member_grouped_by_reprocessing_keeps_no_weak_sample_of_its_auto_name() {
        use broscribe_core::schema::{Strength, Voiceprint, VoiceprintKind, VoiceprintSource};
        let mut fx = Fixture::new();
        let (id, first, _) = split_import(&fx);
        let m = fx.lib.load_meeting(&id).unwrap();
        let centroid = m.speaker(first).unwrap().centroid.clone().unwrap();
        let bob = Person::new(fx.lib.new_person_id(), "Bob Invented");
        let print = Voiceprint {
            id: "synthetic-bob".into(),
            model: centroid.model.clone(),
            dim: centroid.dim,
            vector_b64: centroid.vector_b64.clone(),
            source: VoiceprintSource {
                kind: VoiceprintKind::Enrollment,
                meeting_id: None,
                sha256: Some("b".repeat(64)),
                start: 0.0,
                end: 4.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: now(),
        };
        fx.lib
            .write(|tx| {
                tx.save_person(&bob)?;
                tx.upsert_voiceprints(&bob.id, vec![print])
            })
            .unwrap();
        fx.config.identity.insert(
            fx.config.profile.speaker_embedding.clone(),
            broscribe_core::config::Thresholds {
                auto_name: Some(0.5),
                margin: Some(0.0),
                merge: Some(0.5),
                auto_merge: Some(0.95),
                ..Default::default()
            },
        );
        reprocess(
            &fx.env(),
            &mut split_models(),
            &id,
            ReprocessOptions::default(),
        )
        .unwrap();
        let m = fx.lib.load_meeting(&id).unwrap();
        assert_eq!(m.speaker_groups.len(), 1, "{:?}", m.speaker_groups);
        let group = &m.speaker_groups[0];
        let member = group.members[0];
        assert_eq!(m.speaker(member).unwrap().person_id.as_ref(), Some(&bob.id));
        let weak: Vec<_> = fx
            .lib
            .load_voiceprints(&bob.id)
            .unwrap()
            .embeddings
            .into_iter()
            .filter(|v| v.source.kind == VoiceprintKind::Auto)
            .collect();
        assert_eq!(weak.len(), 1, "{weak:?}");
        assert_eq!(weak[0].source.speaker_id, Some(group.kept));
    }

    /// Grouping notes describe the previous analysis' groups; a full
    /// reprocess replaces them.
    #[test]
    fn a_full_reprocess_clears_earlier_grouping_notes() {
        let fx = Fixture::new();
        let (id, _, _) = split_import(&fx);
        fx.lib
            .write(|tx| {
                tx.update_meeting(&id, |m| {
                    m.add_review_notes(
                        ReviewSource::Grouping,
                        ["the merge of S2 with S9 was undone: synthetic".to_owned()],
                    );
                    Ok(())
                })
                .map(|_| ())
            })
            .unwrap();
        reprocess(
            &fx.env(),
            &mut split_models(),
            &id,
            ReprocessOptions::default(),
        )
        .unwrap();
        let m = fx.lib.load_meeting(&id).unwrap();
        assert!(
            m.review_notes
                .iter()
                .all(|n| n.source != ReviewSource::Grouping),
            "{:?}",
            m.review_notes
        );
    }

    /// R8: two speakers confirmed as the same person and grouped keep their
    /// own confirmations and samples through a reprocess with unchanged
    /// clusters, so unmerging afterwards shows both confirmed.
    #[test]
    fn grouped_same_person_speakers_keep_their_own_identity_through_reprocessing() {
        let mut fx = Fixture::new();
        let (id, first, second) = split_import(&fx);
        confirm(&fx, &id, first, "Ana Invented");
        confirm(&fx, &id, second, "Ana Invented");
        let m = fx.lib.load_meeting(&id).unwrap();
        let fence = ops::SpeakerFence::of(&m);
        ops::with_index(&fx.lib, |tx, idx| {
            ops::group_speakers(
                tx,
                idx,
                &id,
                first,
                &[second],
                GroupOrigin::User,
                &fence,
                "x",
                None,
            )
        })
        .unwrap();
        grouping(&mut fx);
        let out = reprocess(
            &fx.env(),
            &mut split_models(),
            &id,
            ReprocessOptions::default(),
        )
        .unwrap();
        assert!(out.needs_review.is_empty(), "{:?}", out.needs_review);
        let m = fx.lib.load_meeting(&id).unwrap();
        assert!(m.review_notes.is_empty(), "{:?}", m.review_notes);
        let (a, b) = (raw_at(&m, 7.0), raw_at(&m, 12.0));
        assert_eq!(m.speaker_groups.len(), 1);
        assert_eq!(m.speaker_groups[0].origin, GroupOrigin::User);
        // Each sample follows its own source speaker.
        let ana = fx.lib.find_person_by_name("Ana Invented").unwrap().unwrap();
        let sources: BTreeSet<_> = fx
            .lib
            .load_voiceprints(&ana.id)
            .unwrap()
            .embeddings
            .iter()
            .filter_map(|v| v.source.speaker_id)
            .collect();
        assert_eq!(sources, BTreeSet::from([a, b]));
        let group = m.speaker_groups[0].id.clone();
        let fence = ops::SpeakerFence::of(&m);
        ops::with_index(&fx.lib, |tx, idx| {
            ops::ungroup_speakers(tx, idx, &id, &group, &fence)
        })
        .unwrap();
        let m = fx.lib.load_meeting(&id).unwrap();
        for s in [a, b] {
            let s = m.speaker(s).unwrap();
            assert_eq!(
                (s.assignment, s.person_id.as_ref()),
                (Assignment::Confirmed, Some(&ana.id))
            );
        }
    }

    #[test]
    fn reclaimed_lease_publishes_nothing() {
        use broscribe_core::queue::Queue;
        use broscribe_platform::fakes::FakeProcessProbe;
        let fx = Fixture::new();
        let wav = fx.wav("lease.wav", &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0)]);
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let queue = Queue::open(fx.lib.root()).unwrap();
        let job = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    REPROCESS_KIND,
                    id.as_str(),
                    serde_json::json!({ "meeting_id": id }),
                )
            })
            .unwrap();
        let holder = queue
            .claim(&fx.lib, &job, &FakeProcessProbe::new(100))
            .unwrap()
            .unwrap();
        // The worker paused; its lease went to another process.
        let _taker = queue
            .claim(&fx.lib, &job, &FakeProcessProbe::new(200))
            .unwrap()
            .unwrap();
        let before = fx.lib.load_meeting(&id).unwrap();
        for asr_only in [false, true] {
            let err = reprocess_published(
                &fx.env(),
                &mut models(),
                &id,
                ReprocessOptions { asr_only },
                &holder,
                None,
            )
            .unwrap_err();
            assert_eq!(err.code(), "lease_lost");
            assert_eq!(fx.lib.load_meeting(&id).unwrap(), before);
        }
    }

    /// A synchronous reprocessing runs through the meeting's reserved job:
    /// a queued one makes it `library_busy` (they never publish over each
    /// other), and once free it runs leased and leaves no job behind.
    #[test]
    fn interactive_reprocess_reserves_the_meetings_job() {
        use broscribe_models::fakes::FakeModelProvider;
        let fx = Fixture::new();
        let wav = fx.wav("sync.wav", &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0)]);
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let queue = Queue::open(fx.lib.root()).unwrap();
        let queued = fx
            .lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    REPROCESS_KIND,
                    id.as_str(),
                    serde_json::json!({ "meeting_id": id }),
                )
            })
            .unwrap();
        let provider = FakeModelProvider::default();
        let not_cancelled = AtomicBool::new(false);
        let err = run_reprocess_interactive(&fx.env(), &id, None, true, &provider, &not_cancelled)
            .unwrap_err();
        assert_eq!(err.code(), "library_busy");
        fx.lib
            .write(|tx| {
                queue.set_state(tx, &queued, broscribe_core::queue::JobState::Done, None)?;
                queue.remove_finished(tx, REPROCESS_KIND).map(|_| ())
            })
            .unwrap();
        let ran = run_reprocess_interactive(&fx.env(), &id, None, true, &provider, &not_cancelled)
            .unwrap();
        assert!(matches!(&ran, InteractiveReprocess::Ran(out) if out.skipped.is_none()));
        assert!(queue.list(REPROCESS_KIND).unwrap().is_empty());
    }

    /// An interrupted synchronous reprocessing saves nothing, fails its job
    /// as interrupted, and no worker ever runs it again.
    #[test]
    fn interrupted_interactive_reprocess_saves_nothing_and_is_never_rerun() {
        use broscribe_models::fakes::FakeModelProvider;
        let fx = Fixture::new();
        let wav = fx.wav("stop.wav", &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0)]);
        let id = import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id;
        let before = fx.lib.load_meeting(&id).unwrap();
        let cancelled = AtomicBool::new(true);
        let err = run_reprocess_interactive(
            &fx.env(),
            &id,
            None,
            false,
            &FakeModelProvider::default(),
            &cancelled,
        )
        .unwrap_err();
        assert_eq!(err.code(), "interrupted");
        assert_eq!(fx.lib.load_meeting(&id).unwrap(), before);
        let queue = Queue::open(fx.lib.root()).unwrap();
        let jobs = queue.list(REPROCESS_KIND).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].is_interactive());
        assert_eq!(jobs[0].state, broscribe_core::queue::JobState::Failed);
        assert!(!jobs[0].is_claimable());
    }

    /// Synthetic voice identity by level: loud speech is one voice, quiet
    /// speech another. The first embedding of the run forgets Me's voice
    /// (or Me) in the library, as a user might while reprocessing runs.
    struct ForgetMeOnFirstEmbed {
        lib: Library,
        me: PersonId,
        person: bool,
        fired: std::sync::Arc<AtomicBool>,
    }

    impl broscribe_models::SpeakerEmbedder for ForgetMeOnFirstEmbed {
        fn model_id(&self) -> &str {
            "synthetic-level"
        }
        fn dim(&self) -> usize {
            2
        }
        fn embed(&self, samples: &[f32]) -> Result<Option<Vec<f32>>> {
            if !self.fired.swap(true, Ordering::SeqCst) {
                ops::with_index(&self.lib, |tx, index| {
                    if self.person {
                        ops::forget_person(tx, index, self.me.as_str())
                    } else {
                        ops::forget_voice(tx, index, self.me.as_str())
                    }
                })?;
            }
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

    /// Detects speech wherever the audio is not silent.
    struct WholeVad;

    impl broscribe_models::Vad for WholeVad {
        fn segments(&mut self, samples: &[f32]) -> Result<Vec<broscribe_models::SpeechSegment>> {
            Ok(if samples.iter().any(|s| s.abs() > 1e-6) {
                vec![broscribe_models::SpeechSegment {
                    start_s: 0.0,
                    end_s: samples.len() as f64 / 16000.0,
                }]
            } else {
                Vec::new()
            })
        }
    }

    fn level_speech(level: f32, seed: u64) -> Vec<f32> {
        broscribe_dsp::synth::speech_like(
            16000,
            2.0,
            broscribe_dsp::synth::Voice {
                seed,
                f0_hz: 130.0 + seed as f64 * 10.0,
                level_dbfs: level,
            },
        )
    }

    /// A recorded mic + system meeting (reference tracks) with Me enrolled
    /// as the loud voice; returns it and Me.
    fn paired_meeting(
        fx: &mut Fixture,
        setup: &str,
        mic: &[f32],
        system: &[f32],
    ) -> (MeetingId, PersonId) {
        use broscribe_core::schema::{
            Handling, RecordingSource, Strength, Track, TrackRole, Voiceprint, VoiceprintKind,
            VoiceprintSource, Voiceprints,
        };
        fx.config.identity.insert(
            "synthetic-level".into(),
            broscribe_core::config::Thresholds {
                auto_name: Some(0.8),
                ..Default::default()
            },
        );
        let mic_path = fx.input.join("mic.wav");
        let system_path = fx.input.join("system.wav");
        crate::testutil::write_wav(&mic_path, 16_000, mic);
        crate::testutil::write_wav(&system_path, 16_000, system);
        let id = import_file(
            &fx.env(),
            &mut models(),
            &mic_path,
            &ImportOptions::default(),
        )
        .unwrap()
        .meeting_id;
        let mut me = Person::new(fx.lib.new_person_id(), "Synthetic Me");
        me.is_self = true;
        let mut prints = Voiceprints::empty(me.id.clone());
        prints.embeddings.push(Voiceprint {
            id: "synthetic-enrollment".into(),
            model: "synthetic-level".into(),
            dim: 2,
            vector_b64: broscribe_core::schema::encode_vector(&[1.0, 0.0]),
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
            created_at: broscribe_core::schema::now(),
        });
        fx.lib
            .write(|tx| {
                tx.save_person(&me)?;
                tx.save_voiceprints(&prints)?;
                let mut m = tx.library().load_meeting(&id)?;
                m.setup = Some(setup.into());
                m.processing.echo_report = Some(serde_json::json!({"requested": "none"}));
                m.speakers.clear();
                m.segments.clear();
                m.speaker_groups.clear();
                let recording = m.recording.as_mut().unwrap();
                recording.handling = Handling::Reference;
                recording.path = mic_path.clone();
                recording.sources = [&mic_path, &system_path]
                    .into_iter()
                    .map(|path| {
                        Ok(RecordingSource {
                            path: path.clone(),
                            track_index: 0,
                            offset_s: 0.0,
                            sha256: crate::audio::file_sha256(path)?.0,
                        })
                    })
                    .collect::<Result<_>>()?;
                m.tracks = vec![
                    Track {
                        id: "t0".into(),
                        role: TrackRole::Mic,
                    },
                    Track {
                        id: "t1".into(),
                        role: TrackRole::System,
                    },
                ];
                tx.save_meeting(&m)?;
                Index::open(tx.root())?.upsert_meeting(tx, &m)
            })
            .unwrap();
        (id, me.id)
    }

    fn level_models(
        fx: &Fixture,
        me: &PersonId,
        forget: Option<bool>,
    ) -> (ModelSet, std::sync::Arc<AtomicBool>) {
        let mut set = models();
        set.vad = Box::new(WholeVad);
        set.asr = Box::new(broscribe_models::fakes::FakeAsr::new(&[
            "alpha", "beta", "gamma", "delta",
        ]));
        // Without a forget, the flag starts set: the embedder never fires.
        let fired = std::sync::Arc::new(AtomicBool::new(forget.is_none()));
        set.embedder = Box::new(ForgetMeOnFirstEmbed {
            lib: fx.lib.clone(),
            me: me.clone(),
            person: forget == Some(true),
            fired: std::sync::Arc::clone(&fired),
        });
        (set, fired)
    }

    /// Me's voice (or Me) forgotten while reprocessing analyses a mic +
    /// system meeting: the analysis made against the forgotten voice is
    /// never published. The audio is analysed again with fresh evidence:
    /// no speaker is named Me, nothing is taught back to Me, and Me's
    /// speech is kept.
    #[test]
    fn forgetting_me_during_reprocessing_never_resurrects_me() {
        for person in [false, true] {
            let mut fx = Fixture::new();
            let (id, me) = paired_meeting(
                &mut fx,
                "headphones",
                &level_speech(-8.0, 2),
                &level_speech(-35.0, 1),
            );
            let (mut set, fired) = level_models(&fx, &me, Some(person));
            reprocess(&fx.env(), &mut set, &id, ReprocessOptions::default()).unwrap();
            assert!(fired.load(Ordering::SeqCst));
            let meeting = fx.lib.load_meeting(&id).unwrap();
            assert!(
                meeting.segments.iter().any(|s| s.track == "t0"),
                "Me's speech is kept"
            );
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
            assert!(
                fx.lib
                    .load_voiceprints(&me)
                    .map(|v| v.embeddings.is_empty())
                    .unwrap_or(true)
            );
            if person {
                assert!(!fx.lib.person_dir(&me).exists());
            }
        }
    }

    /// Mic speech dropped as echo of the loud remote voice only because it
    /// verified against Me: once Me's voice is forgotten during the
    /// analysis, that drop is stale, and the re-analysis keeps the speech.
    #[test]
    fn mic_speech_dropped_against_a_forgotten_me_is_kept_after_reprocessing() {
        let remote = level_speech(-35.0, 1);
        // Control: with Me enrolled the mic's copy of the remote voice is
        // dropped as echo.
        let mut control = Fixture::new();
        let (id, me) = paired_meeting(&mut control, "speakers_mic", &remote, &remote);
        let (mut set, _) = level_models(&control, &me, None);
        reprocess(&control.env(), &mut set, &id, ReprocessOptions::default()).unwrap();
        let kept = control.lib.load_meeting(&id).unwrap();
        assert!(
            kept.segments.iter().all(|s| s.track == "t1"),
            "{:?}",
            kept.segments
        );
        // Forgetting Me's voice mid-analysis: the drop is not published.
        let mut fx = Fixture::new();
        let (id, me) = paired_meeting(&mut fx, "speakers_mic", &remote, &remote);
        let (mut set, fired) = level_models(&fx, &me, Some(false));
        reprocess(&fx.env(), &mut set, &id, ReprocessOptions::default()).unwrap();
        assert!(fired.load(Ordering::SeqCst));
        let meeting = fx.lib.load_meeting(&id).unwrap();
        assert!(
            meeting.segments.iter().any(|s| s.track == "t0"),
            "{:?}",
            meeting.segments
        );
    }

    fn imported(fx: &Fixture, name: &str) -> MeetingId {
        let wav = fx.wav(name, &[(Some(0), 4.0), (None, 1.0), (Some(1), 4.0)]);
        import_file(&fx.env(), &mut models(), &wav, &ImportOptions::default())
            .unwrap()
            .meeting_id
    }

    /// A queued request takes the same reservation as a synchronous run:
    /// while it waits, a second request and a synchronous run are both
    /// `library_busy`; once that job is finished or interrupted, a new
    /// request restarts it as an ordinary queued job a worker may take.
    #[test]
    fn queued_request_shares_the_interactive_reservation() {
        use broscribe_core::queue::Queue;
        use broscribe_models::fakes::FakeModelProvider;
        let fx = Fixture::new();
        let id = imported(&fx, "queued.wav");
        let request = |profile: Option<&str>, asr_only: bool| {
            fx.lib
                .write(|tx| enqueue_reprocess_request(tx, &id, profile, asr_only))
        };
        let job = request(None, true).unwrap();
        let queue = Queue::open(fx.lib.root()).unwrap();
        let stored = queue.get(&job).unwrap();
        assert_eq!(stored.state, JobState::Queued);
        assert!(!stored.is_interactive());
        assert!(stored.is_claimable());
        let input = ReprocessJobInput::parse(&stored.input).unwrap();
        assert_eq!((input.meeting_id, input.asr_only), (id.clone(), true));

        assert_eq!(request(None, false).unwrap_err().code(), "library_busy");
        let not_cancelled = AtomicBool::new(false);
        let provider = FakeModelProvider::default();
        let err = run_reprocess_interactive(&fx.env(), &id, None, false, &provider, &not_cancelled)
            .unwrap_err();
        assert_eq!(err.code(), "library_busy");
        assert_eq!(
            queue.get(&job).unwrap().input,
            stored.input,
            "first request kept"
        );

        // Finished, then reserved by a synchronous run that was interrupted.
        fx.lib
            .write(|tx| queue.set_state(tx, &job, JobState::Done, None))
            .unwrap();
        let err = run_reprocess_interactive(
            &fx.env(),
            &id,
            None,
            false,
            &provider,
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert_eq!(err.code(), "interrupted");
        assert!(!queue.get(&job).unwrap().is_claimable());

        // An explicit queued request restarts it for a worker.
        assert_eq!(request(None, false).unwrap(), job);
        let restarted = queue.get(&job).unwrap();
        assert_eq!((restarted.state, restarted.attempts), (JobState::Queued, 0));
        assert!(!restarted.is_interactive() && restarted.is_claimable());
    }

    /// Concurrent requests for one meeting queue exactly one job.
    #[test]
    fn concurrent_queued_requests_reserve_once() {
        let fx = Fixture::new();
        let id = imported(&fx, "race.wav");
        let (lib, id) = (&fx.lib, &id);
        let results: Vec<Result<JobId>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(move || lib.write(|tx| enqueue_reprocess_request(tx, id, None, false)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let queued: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(queued.len(), 1, "{results:?}");
        for r in &results {
            if let Err(e) = r {
                assert_eq!(e.code(), "library_busy");
            }
        }
        let queue = broscribe_core::queue::Queue::open(fx.lib.root()).unwrap();
        assert_eq!(queue.list(REPROCESS_KIND).unwrap().len(), 1);
    }

    #[test]
    fn queued_retry_of_failed_transcription_can_publish() {
        let fx = Fixture::new();
        let id = imported(&fx, "retry.wav");
        fx.lib
            .write(|tx| {
                let mut meeting = tx.library().load_meeting(&id)?;
                meeting.processing.status = ProcessingStatus::Failed;
                meeting.processing.error = Some("decode error: synthetic".into());
                tx.save_meeting(&meeting)
            })
            .unwrap();
        let job = fx
            .lib
            .write(|tx| enqueue_reprocess_request(tx, &id, None, false))
            .unwrap();
        assert_eq!(
            Queue::open(fx.lib.root()).unwrap().get(&job).unwrap().state,
            JobState::Queued
        );
        assert_eq!(
            fx.lib.load_meeting(&id).unwrap().processing.status,
            ProcessingStatus::Failed
        );

        let queue = Queue::open(fx.lib.root()).unwrap();
        let lease = queue
            .claim(
                &fx.lib,
                &job,
                &broscribe_platform::fakes::FakeProcessProbe::new(100),
            )
            .unwrap()
            .unwrap();
        run_reprocess_job_leased(
            &fx.env(),
            &lease.claimed_job().input,
            &broscribe_models::fakes::FakeModelProvider::default(),
            &lease,
        )
        .unwrap();
        assert_eq!(queue.get(&job).unwrap().state, JobState::Done);
        let meeting = fx.lib.load_meeting(&id).unwrap();
        assert_eq!(meeting.processing.status, ProcessingStatus::Done);
        assert_eq!(meeting.processing.error, None);
    }

    /// What a queued run could never do is refused before anything is
    /// queued: an unknown meeting or profile, an unfinished transcript, a
    /// missing recording.
    #[test]
    fn queued_request_is_validated_before_queueing() {
        use broscribe_core::queue::Queue;
        let fx = Fixture::new();
        let id = imported(&fx, "checked.wav");
        let request = |id: &MeetingId, profile: Option<&str>| {
            fx.lib
                .write(|tx| enqueue_reprocess_request(tx, id, profile, false))
        };
        let queue = Queue::open(fx.lib.root()).unwrap();
        let none_queued = || queue.list(REPROCESS_KIND).unwrap().is_empty();

        let missing = MeetingId::parse("m-0f0f0f").unwrap();
        assert_eq!(request(&missing, None).unwrap_err().code(), "not_found");
        assert_eq!(
            request(&id, Some("no-such-profile")).unwrap_err().code(),
            "invalid_input"
        );
        assert!(none_queued());

        let set = |f: &dyn Fn(&mut Meeting)| {
            fx.lib
                .write(|tx| {
                    let mut m = tx.library().load_meeting(&id)?;
                    f(&mut m);
                    tx.save_meeting(&m)
                })
                .unwrap();
        };
        set(&|m: &mut Meeting| m.processing.status = ProcessingStatus::Analyzed);
        assert_eq!(request(&id, None).unwrap_err().code(), "invalid_input");
        assert!(none_queued());

        set(&|m: &mut Meeting| {
            m.processing.status = ProcessingStatus::Done;
            m.recording.as_mut().unwrap().state = broscribe_core::schema::RecordingState::Deleted;
        });
        assert_eq!(request(&id, None).unwrap_err().code(), "invalid_input");
        assert!(none_queued());
    }
}
