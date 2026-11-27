//! Per-window speaker analysis of one track: provisional voices linked
//! across windows by source-range overlap only, clean-speech embeddings and
//! person suggestions.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use broscribe_core::config::{Config, Thresholds};
use broscribe_core::identity::{self, PersonSamples};
use broscribe_core::live_identity::{NEVER_FORGOTTEN, evidence_fingerprint, forget_fences};
use broscribe_core::schema::Assignment;
use broscribe_core::{Error, Library, PersonId, Result, SpeakerId, ops};
use broscribe_ipc::live::{LiveIdentitySettings, SpeakerSpan, SuggestionSource, VoiceSuggestion};
use broscribe_models::{Diarizer, SpeakerEmbedder};

use super::{sample, seconds};
use crate::analyze::{self, MIN_CLEAN_S, Turn};
use crate::ranges::{self, Range};
use crate::reprocess::{link_speakers, one_to_one};

/// The models the live-speakers worker loads (once): the profile's
/// diarizer and speaker embedder.
pub struct SpeakerModels<'a> {
    pub diarizer: &'a dyn Diarizer,
    pub embedder: &'a dyn SpeakerEmbedder,
}

/// The voice evidence suggestions are computed with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Evidence {
    /// [`evidence_fingerprint`] of the library when loaded (stored samples,
    /// people and forget tombstones).
    pub fingerprint: String,
    /// Every person's stored samples for the embedder's model.
    pub people: Vec<PersonSamples>,
    pub thresholds: Option<Thresholds>,
    /// People that exist now (session samples of others are dropped).
    pub person_ids: BTreeSet<PersonId>,
    /// The current forget fence of every existing person ever forgotten
    /// (absent: never forgotten). A session sample counts only while its
    /// decision's fence is still the current one; names and attendance stay.
    pub forget_fences: BTreeMap<PersonId, String>,
    /// The user's own person ("Me"), read whenever `thresholds` are not
    /// [`calibrated`] (the only time it matters): while uncalibrated, a
    /// suggestion of this person is withheld unless `allow_uncalibrated_me`
    /// holds.
    pub me: Option<PersonId>,
    /// `[live] allow_uncalibrated_me_suggestions`: the user explicitly
    /// opted in to see their own person suggested while uncalibrated.
    pub allow_uncalibrated_me: bool,
}

/// Whether `thresholds` hold naming thresholds: the same rule
/// `identity::resolve_ranked` applies (an `auto_name` or `suggest` value);
/// without them its top match is suggested whatever the score.
pub fn calibrated(thresholds: Option<&Thresholds>) -> bool {
    thresholds.is_some_and(|t| t.auto_name.is_some() || t.suggest.is_some())
}

/// Supplies [`Evidence`], reloading it when the library's voice evidence
/// changed (a confirmation, enrollment, forget…).
pub trait EvidenceSource {
    fn current(&mut self) -> Result<Arc<Evidence>>;
}

/// [`EvidenceSource`] over a library: the fingerprint, the forget fences,
/// the model's naming thresholds, the uncalibrated-Me policy and (while
/// uncalibrated) the user's own person are read on every call; the samples
/// are reloaded only when the fingerprint changed. A thresholds or policy
/// change in the configuration (calibration, `config set`) therefore takes
/// effect at the next window without any voice evidence changing, and a
/// forget is never served from a cache.
pub struct LibraryEvidence {
    lib: Library,
    data_dir: Option<PathBuf>,
    model: String,
    current: Option<Arc<Evidence>>,
}

impl LibraryEvidence {
    /// `data_dir` holds `config.toml` (naming thresholds); `None` uses none.
    pub fn new(lib: Library, data_dir: Option<PathBuf>, model: impl Into<String>) -> Self {
        Self {
            lib,
            data_dir,
            model: model.into(),
            current: None,
        }
    }
}

impl EvidenceSource for LibraryEvidence {
    fn current(&mut self) -> Result<Arc<Evidence>> {
        let fingerprint = evidence_fingerprint(&self.lib)?;
        let config = match &self.data_dir {
            Some(dir) => Some(Config::load(dir)?),
            None => None,
        };
        let thresholds = config
            .as_ref()
            .and_then(|c| c.thresholds(&self.model).copied());
        let allow_uncalibrated_me = config
            .as_ref()
            .is_some_and(Config::allow_uncalibrated_me_suggestions);
        let me = if calibrated(thresholds.as_ref()) {
            None
        } else {
            self.lib.self_person()?.map(|p| p.id)
        };
        let forget_fences = forget_fences(&self.lib)?;
        let cached = self
            .current
            .as_ref()
            .filter(|c| c.fingerprint == fingerprint);
        if let Some(current) = cached
            && current.thresholds == thresholds
            && current.forget_fences == forget_fences
            && current.me == me
            && current.allow_uncalibrated_me == allow_uncalibrated_me
        {
            return Ok(Arc::clone(current));
        }
        let (people, person_ids) = match cached {
            // Same samples and people; only thresholds, fences or the
            // uncalibrated-Me policy moved.
            Some(c) => (c.people.clone(), c.person_ids.clone()),
            None => (
                ops::load_person_samples(&self.lib, &self.model)?
                    .all()
                    .to_vec(),
                self.lib.list_people()?.into_iter().map(|p| p.id).collect(),
            ),
        };
        let evidence = Arc::new(Evidence {
            fingerprint,
            people,
            thresholds,
            person_ids,
            forget_fences,
            me,
            allow_uncalibrated_me,
        });
        self.current = Some(Arc::clone(&evidence));
        Ok(evidence)
    }
}

/// One transient sample: a named turn's clean speech, with the forget fence
/// its decision was committed under.
#[derive(Clone, Debug)]
struct Sample {
    turn_id: String,
    person: PersonId,
    vector: Vec<f32>,
    forget_fence: String,
}

/// Transient session-local samples: the clean speech of turns the user
/// named during this recording. Bounded, in memory only, never written.
///
/// Updates are fenced per turn by the session's monotonic decision
/// numbers: only a message newer than the last one applied to its turn
/// counts. The last decision is kept for at most `turns` turns (the board's
/// own bound); evicting a turn raises `floor` to its decision, and an
/// unknown turn's message at or below the floor is refused, so a delayed
/// update of an evicted turn never comes back.
#[derive(Clone, Debug, Default)]
pub struct SessionPool {
    cap: usize,
    turns: usize,
    samples: VecDeque<Sample>,
    latest: VecDeque<(String, u64)>,
    floor: u64,
}

impl SessionPool {
    /// At most `cap` samples, and decision order kept for `turns` turns.
    pub fn new(cap: usize, turns: usize) -> Self {
        Self {
            cap,
            turns: turns.max(1),
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Whether decision `decision` on `turn_id` is newer than the last one
    /// applied to it; when it is, it becomes that last one.
    pub fn admit(&mut self, turn_id: &str, decision: u64) -> bool {
        if let Some((_, last)) = self.latest.iter_mut().find(|(t, _)| t == turn_id) {
            if *last >= decision {
                return false;
            }
            *last = decision;
            return true;
        }
        if decision <= self.floor {
            return false;
        }
        self.latest.push_back((turn_id.to_owned(), decision));
        while self.latest.len() > self.turns {
            if let Some((_, evicted)) = self.latest.pop_front() {
                self.floor = self.floor.max(evicted);
            }
        }
        true
    }

    /// Whether a sample of `person` whose decision was committed under
    /// `forget_fence` may count under `evidence`: the person exists and
    /// has not been forgotten since (the fence is exactly the current one).
    pub fn allowed(evidence: &Evidence, person: &PersonId, forget_fence: &str) -> bool {
        evidence.person_ids.contains(person)
            && evidence
                .forget_fences
                .get(person)
                .map_or(NEVER_FORGOTTEN, String::as_str)
                == forget_fence
    }

    /// Adds (or replaces) the sample of `turn_id`, dropping the oldest
    /// beyond the cap.
    pub fn add(&mut self, turn_id: &str, person: PersonId, vector: Vec<f32>, forget_fence: String) {
        self.revoke(turn_id);
        if self.cap == 0 {
            return;
        }
        self.samples.push_back(Sample {
            turn_id: turn_id.to_owned(),
            person,
            vector,
            forget_fence,
        });
        while self.samples.len() > self.cap {
            self.samples.pop_front();
        }
    }

    pub fn revoke(&mut self, turn_id: &str) {
        self.samples.retain(|s| s.turn_id != turn_id);
    }

    /// Drops samples of people that no longer exist or were forgotten since
    /// their decision.
    pub fn prune(&mut self, evidence: &Evidence) {
        self.samples
            .retain(|s| Self::allowed(evidence, &s.person, &s.forget_fence));
    }

    /// The pool as confirmed samples per person.
    pub fn people(&self) -> Vec<PersonSamples> {
        let mut out: Vec<PersonSamples> = Vec::new();
        for sample in &self.samples {
            match out.iter_mut().find(|p| p.person_id == sample.person) {
                Some(p) => p.confirmed.push(sample.vector.clone()),
                None => out.push(PersonSamples {
                    person_id: sample.person.clone(),
                    confirmed: vec![sample.vector.clone()],
                    weak: Vec::new(),
                }),
            }
        }
        out
    }
}

/// The pool one window's suggestions resolve against: the library's
/// samples plus the session pool's, merged once per window (the library's
/// samples are borrowed, not copied, when the session holds none).
pub struct SuggestionPool<'a> {
    evidence: &'a Evidence,
    session: Vec<PersonSamples>,
    merged: Cow<'a, [PersonSamples]>,
}

impl<'a> SuggestionPool<'a> {
    pub fn new(evidence: &'a Evidence, pool: &SessionPool) -> Self {
        let session = pool.people();
        let merged = if session.is_empty() {
            Cow::Borrowed(evidence.people.as_slice())
        } else {
            let mut merged = evidence.people.clone();
            for s in &session {
                match merged.iter_mut().find(|p| p.person_id == s.person_id) {
                    Some(p) => p.confirmed.extend(s.confirmed.iter().cloned()),
                    None => merged.push(s.clone()),
                }
            }
            Cow::Owned(merged)
        };
        Self {
            evidence,
            session,
            merged,
        }
    }

    /// The suggestion for a voice centroid: `resolve_voice` over the merged
    /// pool, unchanged thresholds. Shown as a suggestion whatever the
    /// assignment (never an automatic name, never an enrollment); the
    /// source is the session when its samples score the person at least as
    /// high as the library's.
    ///
    /// Without [`calibrated`] thresholds the resolver's top match is only
    /// the closest stored voice, so the suggestion says so (`uncalibrated`).
    /// The user's own person is then suggested only under the explicit
    /// `[live] allow_uncalibrated_me_suggestions` opt-in; otherwise the
    /// voice gets no suggestion (no runner-up stands in for it).
    pub fn suggest(&self, centroid: &[f32]) -> Option<Suggestion> {
        if self.merged.is_empty() {
            return None;
        }
        let resolution = identity::resolve_voice(
            centroid,
            &self.merged,
            &[],
            self.evidence.thresholds.as_ref(),
            None,
            None,
        );
        if resolution.assignment == Assignment::None {
            return None;
        }
        let person = resolution.person_id?;
        let uncalibrated = !calibrated(self.evidence.thresholds.as_ref());
        if uncalibrated
            && !self.evidence.allow_uncalibrated_me
            && self.evidence.me.as_ref() == Some(&person)
        {
            return None;
        }
        let library = self
            .evidence
            .people
            .iter()
            .find(|p| p.person_id == person)
            .and_then(|p| identity::score(centroid, p));
        let local = self
            .session
            .iter()
            .find(|p| p.person_id == person)
            .and_then(|p| identity::score(centroid, p));
        let source = match (library, local) {
            (_, None) => SuggestionSource::Library,
            (None, Some(_)) => SuggestionSource::Session,
            (Some(l), Some(s)) if s >= l => SuggestionSource::Session,
            (Some(_), Some(_)) => SuggestionSource::Library,
        };
        Some(Suggestion {
            person,
            source,
            uncalibrated,
        })
    }
}

/// A live person suggestion for one provisional voice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub person: PersonId,
    pub source: SuggestionSource,
    /// No calibrated naming thresholds stand behind it (see
    /// [`SuggestionPool::suggest`]).
    pub uncalibrated: bool,
}

#[derive(Clone, Debug)]
struct VoiceState {
    voice: u32,
    embeds: VecDeque<Vec<f32>>,
    clean_s: f64,
}

/// A retained span (live samples). `voice` is [`UNTRACKED`] for speech of
/// clusters the voice cap left out: it has no provisional voice, but it is
/// still someone else's speech.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Recent {
    from: u64,
    to: u64,
    voice: u32,
    overlap: bool,
}

/// [`Recent::voice`] of speech no provisional voice carries (voices start
/// at 1).
const UNTRACKED: u32 = 0;

/// What one window produced, before journal anchors are attached.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Analysed {
    pub spans: Vec<SpeakerSpan>,
    pub voices: Vec<VoiceSuggestion>,
}

/// Provisional voices of one track. Voices continue only from the previous
/// window, through a one-to-one link of overlapping diarized turns
/// (≥ 1 s and ≥ 50 % of either side's talk inside the shared range);
/// anything else is a new voice. State is bounded by the window.
#[derive(Debug, Default)]
pub struct VoiceTracker {
    next_voice: u32,
    /// The previous window's turns, absolute seconds, speaker = voice.
    prev: Vec<Turn>,
    prev_end: f64,
    voices: Vec<VoiceState>,
    /// Spans of the retained audio, for session samples.
    recent: Vec<Recent>,
}

fn voice_id(voice: u32) -> Result<SpeakerId> {
    SpeakerId::new(voice).map_err(|_| Error::Internal(format!("invalid live voice {voice}")))
}

fn clip(turns: &[Turn], (a, b): Range) -> Vec<Turn> {
    turns
        .iter()
        .filter_map(|t| {
            let start = t.start.max(a);
            let end = t.end.min(b);
            (end > start).then_some(Turn { start, end, ..*t })
        })
        .collect()
}

impl VoiceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Diarizes `samples` (live samples from `from`), links its clusters to
    /// the previous window's voices, embeds clean speech within the
    /// per-window budget and suggests people. Inference only.
    ///
    /// At most `max_voices` clusters (the most talk time) become voices;
    /// the others still count as speech everywhere overlap and cleanliness
    /// are decided: a tracked voice's speech over an excluded cluster's is
    /// overlapped, never clean, and a range holding excluded speech never
    /// yields a session sample.
    pub fn analyse(
        &mut self,
        from: u64,
        samples: &[f32],
        models: &SpeakerModels<'_>,
        evidence: &Evidence,
        pool: &SessionPool,
        settings: &LiveIdentitySettings,
    ) -> Result<Analysed> {
        let offset = seconds(from);
        let to = from + samples.len() as u64;
        // Every diarized turn: the reference for overlap and clean speech.
        let diarized = analyze::label_turns(&models.diarizer.diarize(samples)?);
        // Bounded: the clusters with the most talk time.
        let mut ids = analyze::speaker_ids(&diarized);
        if ids.len() > settings.max_voices {
            let talk = |id: SpeakerId| -> f64 {
                diarized
                    .iter()
                    .filter(|t| t.speaker == id)
                    .map(|t| t.end - t.start)
                    .sum()
            };
            ids.sort_by(|a, b| talk(*b).total_cmp(&talk(*a)));
            ids.truncate(settings.max_voices);
        }
        let shift = |t: &Turn| Turn {
            start: t.start + offset,
            end: t.end + offset,
            speaker: t.speaker,
        };
        let all_absolute: Vec<Turn> = diarized.iter().map(shift).collect();
        let absolute: Vec<Turn> = all_absolute
            .iter()
            .filter(|t| ids.contains(&t.speaker))
            .copied()
            .collect();
        let untracked = ranges::union(
            all_absolute
                .iter()
                .filter(|t| !ids.contains(&t.speaker))
                .map(|t| (t.start, t.end)),
        );

        // Link to the previous window inside the range both saw.
        let shared = (offset, self.prev_end);
        let links = if shared.1 > shared.0 {
            one_to_one(&link_speakers(
                &clip(&self.prev, shared),
                &clip(&absolute, shared),
            ))
        } else {
            Vec::new()
        };
        let mut mapping: Vec<(SpeakerId, u32)> = Vec::new();
        for id in analyze::speaker_ids(&absolute) {
            let voice = match links.iter().find(|l| l.new == id) {
                Some(l) => l.old.number(),
                None => {
                    self.next_voice += 1;
                    self.next_voice
                }
            };
            mapping.push((id, voice));
        }
        let voice_of = |id: SpeakerId| -> u32 {
            mapping
                .iter()
                .find(|(l, _)| *l == id)
                .map_or(0, |(_, v)| *v)
        };

        // Carried voice state; new voices start empty.
        let mut voices: Vec<VoiceState> = mapping
            .iter()
            .map(|(_, voice)| {
                self.voices
                    .iter()
                    .find(|v| v.voice == *voice)
                    .cloned()
                    .unwrap_or(VoiceState {
                        voice: *voice,
                        embeds: VecDeque::new(),
                        clean_s: 0.0,
                    })
            })
            .collect();

        // Spans: each voice's speech, split into clean and overlapped
        // against every other diarized cluster, tracked or not.
        let mut spans = Vec::new();
        for &(id, voice) in &mapping {
            let own = ranges::union(analyze::speaker_ranges(&absolute, id));
            let others = ranges::union(
                all_absolute
                    .iter()
                    .filter(|t| t.speaker != id)
                    .map(|t| (t.start, t.end)),
            );
            for (piece, overlap) in ranges::subtract(&own, &others)
                .into_iter()
                .map(|r| (r, false))
                .chain(
                    ranges::intersect(&own, &others)
                        .into_iter()
                        .map(|r| (r, true)),
                )
            {
                let (a, b) = (sample(piece.0).max(from), sample(piece.1).min(to));
                if b > a {
                    spans.push(SpeakerSpan {
                        from_sample: a,
                        to_sample: b,
                        voice,
                        overlap,
                        from: None,
                        to: None,
                    });
                }
            }
        }
        spans.sort_by_key(|s| (s.from_sample, s.to_sample, s.voice));

        // Embeddings: the longest clean turn (clean of every diarized
        // cluster) of the voices with the fewest embeddings first, within
        // the window budget.
        let mut candidates: Vec<(usize, Range)> = Vec::new();
        for (i, &(id, _)) in mapping.iter().enumerate() {
            if let Some(longest) = analyze::clean_turns(&diarized, id)
                .into_iter()
                .max_by(|a, b| (a.1 - a.0).total_cmp(&(b.1 - b.0)))
            {
                candidates.push((i, longest));
            }
        }
        candidates.sort_by_key(|(i, _)| voices[*i].embeds.len());
        for (i, range) in candidates.into_iter().take(settings.max_embeds_per_window) {
            for (_, vector) in analyze::embed_ranges(samples, &[range], models.embedder)? {
                let state = &mut voices[i];
                state.embeds.push_back(vector);
                while state.embeds.len() > settings.max_embeds_per_voice {
                    state.embeds.pop_front();
                }
            }
            voices[i].clean_s += range.1 - range.0;
        }

        // Suggestions: one merged pool for every voice of the window.
        let pool = SuggestionPool::new(evidence, pool);
        let suggestions = voices
            .iter()
            .map(|state| {
                let centroid = identity::mean_normalized(
                    &state.embeds.iter().map(Vec::as_slice).collect::<Vec<_>>(),
                );
                let suggestion = centroid.and_then(|c| pool.suggest(&c));
                VoiceSuggestion {
                    voice: state.voice,
                    clean_s: state.clean_s,
                    person_id: suggestion.as_ref().map(|s| s.person.as_str().to_owned()),
                    source: suggestion.as_ref().map(|s| s.source),
                    uncalibrated: suggestion.is_some_and(|s| s.uncalibrated),
                }
            })
            .collect();

        // Keep this window for the next link, and its spans for session
        // samples (the latest window wins where windows overlap).
        self.prev = absolute
            .iter()
            .map(|t| {
                Ok(Turn {
                    speaker: voice_id(voice_of(t.speaker))?,
                    ..*t
                })
            })
            .collect::<Result<_>>()?;
        self.prev_end = seconds(to);
        self.voices = voices;
        let mut kept = Vec::with_capacity(self.recent.len() + spans.len());
        for r in &self.recent {
            if r.to <= from || r.from >= to {
                kept.push(*r);
                continue;
            }
            if r.from < from {
                kept.push(Recent { to: from, ..*r });
            }
            if r.to > to {
                kept.push(Recent { from: to, ..*r });
            }
        }
        self.recent = kept;
        self.recent.extend(spans.iter().map(|s| Recent {
            from: s.from_sample,
            to: s.to_sample,
            voice: s.voice,
            overlap: s.overlap,
        }));
        self.recent.extend(untracked.iter().filter_map(|&(a, b)| {
            let (a, b) = (sample(a).max(from), sample(b).min(to));
            (b > a).then_some(Recent {
                from: a,
                to: b,
                voice: UNTRACKED,
                overlap: false,
            })
        }));
        let keep_from = to.saturating_sub(LiveIdentitySettings::samples(settings.retain_s));
        self.recent.retain(|r| r.to > keep_from);
        Ok(Analysed {
            spans,
            voices: suggestions,
        })
    }

    /// The clean speech of one voice inside `[from, to)` (live samples), as
    /// ranges relative to `from` in seconds: `None` when the range holds
    /// more than one voice or speech of a cluster the voice cap left out,
    /// overlapped speech is all there is, or less than [`MIN_CLEAN_S`]
    /// remains.
    pub fn clean_for(&self, from: u64, to: u64) -> Option<Vec<Range>> {
        let inside: Vec<&Recent> = self
            .recent
            .iter()
            .filter(|r| r.to > from && r.from < to)
            .collect();
        let voices: BTreeSet<u32> = inside
            .iter()
            .filter(|r| !r.overlap)
            .map(|r| r.voice)
            .collect();
        if voices.len() != 1 || voices.contains(&UNTRACKED) {
            return None;
        }
        let clean = ranges::union(
            inside
                .iter()
                .filter(|r| !r.overlap)
                .map(|r| (seconds(r.from.max(from)), seconds(r.to.min(to)))),
        );
        let overlapped = ranges::union(
            inside
                .iter()
                .filter(|r| r.overlap)
                .map(|r| (seconds(r.from), seconds(r.to))),
        );
        let clean = ranges::subtract(&clean, &overlapped);
        if ranges::total(&clean) < MIN_CLEAN_S {
            return None;
        }
        let base = seconds(from);
        Some(
            clean
                .into_iter()
                .map(|(a, b)| (a - base, b - base))
                .collect(),
        )
    }
}

/// The embedding of a named turn's clean speech, for the session pool.
pub fn session_sample(
    tracker: &VoiceTracker,
    audio: &[f32],
    from: u64,
    to: u64,
    embedder: &dyn SpeakerEmbedder,
) -> Result<Option<Vec<f32>>> {
    let Some(clean) = tracker.clean_for(from, to) else {
        return Ok(None);
    };
    let vectors: Vec<Vec<f32>> = analyze::embed_ranges(audio, &clean, embedder)?
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    Ok(identity::mean_normalized(&vectors))
}
