//! The daemon's bounded table of live-preview turns.

use std::collections::VecDeque;

use broscribe_core::live_identity::STALE_REVISION;
use broscribe_core::{Error, Result};
use broscribe_ipc::Track;
use broscribe_ipc::live::{
    LiveAttributeParams, LiveIdentitySettings, LiveIdentityState, LivePerson, LiveSegment,
    LiveSnapshot, LiveSuggestion, LiveTurn, LiveTurnStatus, LiveWord, SpeakerWindow,
    UnresolvedReason, VoiceSuggestion,
};

use super::{sample, seconds};

/// Refused: the turn is no longer in the preview (beyond the turn cap).
pub const STALE_TURN: &str = "stale_turn";
/// Refused: the voice evidence or the people changed since the picker
/// opened; reload the preview.
pub const STALE_EVIDENCE: &str = "stale_evidence";

/// Display name of a person ID, `None` when the person does not exist.
pub type Names<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Span {
    epoch: u64,
    from: u64,
    to: u64,
    voice: u32,
    overlap: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Derived {
    epoch: u64,
    status: LiveTurnStatus,
    unresolved: Option<UnresolvedReason>,
    voice: Option<u32>,
    suggestion: Option<LiveSuggestion>,
}

impl Derived {
    fn preview() -> Self {
        Self {
            epoch: 0,
            status: LiveTurnStatus::Preview,
            unresolved: None,
            voice: None,
            suggestion: None,
        }
    }

    fn unresolved(epoch: u64, reason: UnresolvedReason) -> Self {
        Self {
            epoch,
            status: LiveTurnStatus::Unresolved,
            unresolved: Some(reason),
            voice: None,
            suggestion: None,
        }
    }
}

/// The live turns of one session: ASR segments (with word times) as
/// source-anchored turns, their provisional voices and suggestions from the
/// latest windows, and the user's decisions. Bounded: `max_turns` turns,
/// spans and voices of the last `retain_s` per track; older turns keep what
/// they showed last.
pub struct PreviewBoard {
    session_id: String,
    settings: LiveIdentitySettings,
    turns: VecDeque<LiveTurn>,
    spans: [Vec<Span>; 2],
    covered: [Vec<(u64, u64)>; 2],
    voices: [Vec<(u64, VoiceSuggestion)>; 2],
    /// Latest window end per track.
    latest: [u64; 2],
    epoch: u64,
    evidence: String,
    identity: LiveIdentityState,
}

fn word_units(from: u64, to: u64, words: &[LiveWord]) -> Vec<(u64, u64)> {
    let units: Vec<(u64, u64)> = words
        .iter()
        .map(|w| (sample(w.start_s).max(from), sample(w.end_s).min(to)))
        .filter(|(a, b)| b > a)
        .collect();
    if units.is_empty() {
        vec![(from, to)]
    } else {
        units
    }
}

impl PreviewBoard {
    pub fn new(session_id: impl Into<String>, settings: LiveIdentitySettings) -> Self {
        Self {
            session_id: session_id.into(),
            settings,
            turns: VecDeque::new(),
            spans: [Vec::new(), Vec::new()],
            covered: [Vec::new(), Vec::new()],
            voices: [Vec::new(), Vec::new()],
            latest: [0, 0],
            epoch: 0,
            evidence: String::new(),
            identity: LiveIdentityState::default(),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn evidence(&self) -> &str {
        &self.evidence
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn identity(&self) -> &LiveIdentityState {
        &self.identity
    }

    pub fn len(&self) -> usize {
        self.turns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// Stored spans, both tracks (bounded by `retain_s`).
    pub fn span_count(&self) -> usize {
        self.spans.iter().map(Vec::len).sum()
    }

    pub fn turn(&self, turn_id: &str) -> Option<&LiveTurn> {
        self.turns.iter().find(|t| t.turn_id == turn_id)
    }

    /// Replaces the availability; `true` when it changed.
    pub fn set_identity(&mut self, state: LiveIdentityState) -> bool {
        if self.identity == state {
            return false;
        }
        self.identity = state;
        true
    }

    /// The authoritative rows in the order they were published, oldest
    /// first: the order `ingest_segment` drops them in once the preview is
    /// full. Two tracks close their segments independently, so this is not
    /// source-time order; clients that bound or deduplicate rows seed that
    /// from this order and sort only what they display.
    pub fn snapshot(&self) -> LiveSnapshot {
        LiveSnapshot {
            session_id: self.session_id.clone(),
            identity: self.identity.clone(),
            evidence: self.evidence.clone(),
            turns: self.turns.iter().cloned().collect(),
        }
    }

    /// A recognised segment becomes a turn identified by its track and
    /// first source sample.
    pub fn ingest_segment(&mut self, seg: &LiveSegment, names: Names<'_>) -> LiveTurn {
        let from = sample(seg.start_s);
        let to = sample(seg.end_s).max(from + 1);
        let base = format!("{}-{from}", seg.track.as_str());
        let mut turn_id = base.clone();
        let mut n = 1;
        while self.turn(&turn_id).is_some() {
            n += 1;
            turn_id = format!("{base}-{n}");
        }
        self.turns.push_back(LiveTurn {
            session_id: self.session_id.clone(),
            turn_id,
            revision: 0,
            epoch: 0,
            track: seg.track,
            from_sample: from,
            to_sample: to,
            start_s: seconds(from),
            end_s: seconds(to),
            text: seg.text.clone(),
            words: seg.words.clone(),
            status: LiveTurnStatus::Preview,
            unresolved: None,
            voice: None,
            suggestion: None,
            named: None,
            evidence: self.evidence.clone(),
        });
        while self.turns.len() > self.settings.max_turns {
            self.turns.pop_front();
        }
        let last = self.turns.len() - 1;
        self.refresh(last, names);
        if self.turns[last].revision == 0 {
            self.turns[last].revision = 1;
        }
        self.turns[last].clone()
    }

    /// Applies an analysed window; returns the turns whose view changed.
    /// Windows of an older worker epoch are ignored.
    pub fn ingest_window(&mut self, w: &SpeakerWindow, names: Names<'_>) -> Vec<LiveTurn> {
        if w.epoch < self.epoch {
            return Vec::new();
        }
        self.epoch = w.epoch;
        if w.evidence != self.evidence {
            self.evidence.clone_from(&w.evidence);
            self.drop_suggestions();
        }
        let i = w.track.index();
        let (a, b) = (w.from_sample, w.to_sample);
        let mut kept = Vec::with_capacity(self.spans[i].len() + w.spans.len());
        for s in &self.spans[i] {
            if s.to <= a || s.from >= b {
                kept.push(*s);
                continue;
            }
            if s.from < a {
                kept.push(Span { to: a, ..*s });
            }
            if s.to > b {
                kept.push(Span { from: b, ..*s });
            }
        }
        kept.extend(
            w.spans
                .iter()
                .filter(|s| s.to_sample > s.from_sample)
                .map(|s| Span {
                    epoch: w.epoch,
                    from: s.from_sample.max(a),
                    to: s.to_sample.min(b),
                    voice: s.voice,
                    overlap: s.overlap,
                }),
        );
        kept.retain(|s| s.to > s.from);
        self.spans[i] = kept;
        self.covered[i].push((a, b));
        self.covered[i].sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for &(x, y) in &self.covered[i] {
            match merged.last_mut() {
                Some(last) if x <= last.1 => last.1 = last.1.max(y),
                _ => merged.push((x, y)),
            }
        }
        self.covered[i] = merged;
        self.voices[i]
            .retain(|(e, v)| !(*e == w.epoch && w.voices.iter().any(|n| n.voice == v.voice)));
        self.voices[i].extend(w.voices.iter().map(|v| (w.epoch, v.clone())));
        self.latest[i] = self.latest[i].max(b);
        // Bounded: the last `retain_s` of spans, coverage and voices.
        let horizon = self.horizon(w.track);
        self.spans[i].retain(|s| s.to > horizon);
        self.covered[i].retain(|c| c.1 > horizon);
        let spans = &self.spans[i];
        self.voices[i].retain(|(e, v)| spans.iter().any(|s| s.epoch == *e && s.voice == v.voice));
        let mut changed = Vec::new();
        for idx in 0..self.turns.len() {
            if self.turns[idx].track == w.track
                && let Some(turn) = self.refresh(idx, names)
            {
                changed.push(turn);
            }
        }
        changed
    }

    /// The library's voice evidence is now `fingerprint` (the daemon checks
    /// it under the attribution lock; windows carry it too): suggestions
    /// computed with other evidence are dropped and named people that no
    /// longer exist are shown as unnamed. Returns the changed turns.
    pub fn refresh_evidence(&mut self, fingerprint: &str, names: Names<'_>) -> Vec<LiveTurn> {
        if fingerprint == self.evidence {
            return Vec::new();
        }
        self.evidence = fingerprint.to_owned();
        self.drop_suggestions();
        (0..self.turns.len())
            .filter_map(|idx| self.refresh(idx, names))
            .collect()
    }

    /// Fences a decision: the turn must be shown at this revision and
    /// epoch, and the picker's evidence must be current.
    pub fn check(&self, params: &LiveAttributeParams) -> Result<LiveTurn> {
        if params.session_id != self.session_id {
            return Err(broscribe_core::lifecycle::stale_session(&params.session_id));
        }
        let turn = self.turn(&params.turn_id).ok_or_else(|| Error::Service {
            code: STALE_TURN,
            message: format!("live turn {} is no longer shown", params.turn_id),
        })?;
        if turn.revision != params.revision || turn.epoch != params.epoch {
            return Err(Error::Service {
                code: STALE_REVISION,
                message: format!("live turn {} changed since it was shown", params.turn_id),
            });
        }
        if params.evidence != self.evidence {
            return Err(Error::Service {
                code: STALE_EVIDENCE,
                message: "voice evidence changed since the preview was shown; reload it".into(),
            });
        }
        Ok(turn.clone())
    }

    /// Records the user's decision on exactly this turn: `named` is the
    /// person (Name/Confirm) or `None` (Clear). Returns the new view.
    pub fn apply(
        &mut self,
        turn_id: &str,
        named: Option<LivePerson>,
        names: Names<'_>,
    ) -> Option<LiveTurn> {
        let idx = self.turns.iter().position(|t| t.turn_id == turn_id)?;
        self.turns[idx].named = named;
        let before = self.turns[idx].revision;
        self.refresh(idx, names);
        if self.turns[idx].revision == before {
            self.turns[idx].revision += 1;
        }
        Some(self.turns[idx].clone())
    }

    fn horizon(&self, track: Track) -> u64 {
        self.latest[track.index()]
            .saturating_sub(LiveIdentitySettings::samples(self.settings.retain_s))
    }

    fn drop_suggestions(&mut self) {
        for voices in &mut self.voices {
            for (_, v) in voices.iter_mut() {
                v.person_id = None;
                v.source = None;
            }
        }
    }

    /// Recomputes what turn `idx` shows; bumps its revision and returns it
    /// when that changed.
    fn refresh(&mut self, idx: usize, names: Names<'_>) -> Option<LiveTurn> {
        let turn = &self.turns[idx];
        let frozen = turn.to_sample <= self.horizon(turn.track);
        let derived = if frozen {
            // Beyond the retained analysis: keep the last view, minus a
            // suggestion from other evidence.
            let suggestion = turn
                .suggestion
                .clone()
                .filter(|_| turn.evidence == self.evidence);
            Derived {
                epoch: turn.epoch,
                status: if suggestion.is_some() {
                    LiveTurnStatus::Suggested
                } else if turn.voice.is_some() {
                    LiveTurnStatus::Voice
                } else if turn.unresolved.is_some() {
                    LiveTurnStatus::Unresolved
                } else {
                    LiveTurnStatus::Preview
                },
                unresolved: turn.unresolved,
                voice: turn.voice,
                suggestion,
            }
        } else {
            self.derive(
                turn.track,
                turn.from_sample,
                turn.to_sample,
                &turn.words,
                names,
            )
        };
        let named = turn.named.as_ref().and_then(|n| {
            names(&n.person_id).map(|name| LivePerson {
                person_id: n.person_id.clone(),
                name,
            })
        });
        let mut status = derived.status;
        if named.is_some() {
            status = LiveTurnStatus::Named;
        }
        let next = LiveTurn {
            epoch: derived.epoch,
            status,
            unresolved: derived.unresolved,
            voice: derived.voice,
            suggestion: derived.suggestion,
            named,
            evidence: self.evidence.clone(),
            ..turn.clone()
        };
        if next == *turn {
            return None;
        }
        let revision = turn.revision + 1;
        self.turns[idx] = LiveTurn { revision, ..next };
        Some(self.turns[idx].clone())
    }

    /// Who the turn's words fall on, from the retained spans.
    fn derive(
        &self,
        track: Track,
        from: u64,
        to: u64,
        words: &[LiveWord],
        names: Names<'_>,
    ) -> Derived {
        let i = track.index();
        if !self.covered[i].iter().any(|&(a, b)| a <= from && b >= to) {
            return Derived::preview();
        }
        let epoch = self.epoch;
        let mut overlap = false;
        let mut ambiguous = false;
        let mut voices: Vec<(u64, u32)> = Vec::new();
        let mut supported = 0u64;
        let mut total = 0u64;
        for (a, b) in word_units(from, to, words) {
            let len = b - a;
            total += len;
            let mut per: Vec<((u64, u32), u64)> = Vec::new();
            let mut overlapped = 0u64;
            for s in self.spans[i].iter().filter(|s| s.to > a && s.from < b) {
                let ov = s.to.min(b) - s.from.max(a);
                if s.overlap {
                    overlapped += ov;
                } else {
                    match per.iter_mut().find(|(k, _)| *k == (s.epoch, s.voice)) {
                        Some((_, n)) => *n += ov,
                        None => per.push(((s.epoch, s.voice), ov)),
                    }
                }
            }
            if overlapped > 0 && overlapped * 2 >= len {
                overlap = true;
                continue;
            }
            let best = per.iter().max_by_key(|(_, ov)| *ov).copied();
            match best {
                Some((key, ov)) if ov * 2 >= len => {
                    supported += len;
                    if !voices.contains(&key) {
                        voices.push(key);
                    }
                }
                Some(_) if per.len() > 1 => ambiguous = true,
                _ => {}
            }
        }
        if overlap {
            return Derived::unresolved(epoch, UnresolvedReason::Overlap);
        }
        if voices.len() > 1 || ambiguous {
            return Derived::unresolved(epoch, UnresolvedReason::Mixed);
        }
        let Some(&(voice_epoch, voice)) = voices.first() else {
            return Derived::unresolved(epoch, UnresolvedReason::Unsupported);
        };
        if supported * 2 < total {
            return Derived::unresolved(epoch, UnresolvedReason::Unsupported);
        }
        let suggestion = self.voices[i]
            .iter()
            .find(|(e, v)| *e == voice_epoch && v.voice == voice)
            .and_then(|(_, v)| {
                let person_id = v.person_id.as_ref()?;
                Some(LiveSuggestion {
                    person: LivePerson {
                        person_id: person_id.clone(),
                        name: names(person_id)?,
                    },
                    source: v.source?,
                    uncalibrated: v.uncalibrated,
                })
            });
        Derived {
            epoch: voice_epoch,
            status: if suggestion.is_some() {
                LiveTurnStatus::Suggested
            } else {
                LiveTurnStatus::Voice
            },
            unresolved: None,
            voice: Some(voice),
            suggestion,
        }
    }
}
