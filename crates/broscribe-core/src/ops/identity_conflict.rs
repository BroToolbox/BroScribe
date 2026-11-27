//! User-supplied line names and speaker rejections constrain every automatic name.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{effective_attribution, effective_rejections, effective_speaker};
use crate::schema::{Voiceprint, VoiceprintKind};
use crate::{
    Library, PersonId, Result, SpeakerId,
    schema::{Meeting, ReviewSource},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedLine {
    pub segment_id: u32,
    pub person_id: PersonId,
    pub start: f64,
    pub end: f64,
    pub live_decision: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IdentityConflict {
    pub candidate: Option<PersonId>,
    pub lines: Vec<NamedLine>,
    pub rejected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlinked_caption: Option<String>,
}

impl IdentityConflict {
    pub fn message(&self, speaker: SpeakerId, names: &BTreeMap<PersonId, String>) -> String {
        let name = |id: &PersonId| names.get(id).map_or("someone", String::as_str);
        let lines = self
            .lines
            .iter()
            .map(|line| {
                format!(
                    "{} at {}",
                    name(&line.person_id),
                    crate::render::format_hms(line.start)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let speaker = format!("Speaker {}", speaker.number());
        if let Some(caption) = &self.unlinked_caption {
            return format!(
                "Review {speaker}. The meeting app showed {caption} and you named these lines {lines}."
            );
        }
        match (&self.candidate, self.rejected, lines.is_empty()) {
            (Some(person), true, false) => format!(
                "Review {speaker}. You ruled out {} and named these lines {lines}.",
                name(person)
            ),
            (Some(person), true, true) => format!(
                "Review {speaker}. You ruled out {} for this speaker.",
                name(person)
            ),
            (Some(person), false, _) => format!(
                "Review {speaker}. BroScribe found {} and you named these lines {lines}.",
                name(person)
            ),
            (None, _, _) => format!(
                "{speaker} still needs a name. You gave different names to these lines. {lines}."
            ),
        }
    }
}

/// Indexes the user's line attributions once, by the raw and shown speakers.
/// Speaker grouping never bypasses another member's user evidence.
pub struct UserSpeakerEvidence {
    raw: BTreeMap<SpeakerId, Vec<NamedLine>>,
    shown: BTreeMap<SpeakerId, Vec<NamedLine>>,
}

impl UserSpeakerEvidence {
    pub fn of(meeting: &Meeting) -> Self {
        let mut raw = BTreeMap::<_, Vec<_>>::new();
        let mut shown = BTreeMap::<_, Vec<_>>::new();
        if !meeting.utterance_people.is_empty() {
            for (segment, attribution) in
                meeting.segments.iter().zip(effective_attribution(meeting))
            {
                let (Some(speaker), Some(person)) = (segment.speaker, attribution.line_person())
                else {
                    continue;
                };
                let line = NamedLine {
                    segment_id: segment.id,
                    person_id: person.clone(),
                    start: segment.start,
                    end: segment.end,
                    live_decision: attribution.live_decision,
                };
                shown
                    .entry(effective_speaker(meeting, speaker))
                    .or_default()
                    .push(line.clone());
                raw.entry(speaker).or_default().push(line);
            }
        }
        Self { raw, shown }
    }

    pub fn raw_lines(&self, speaker: SpeakerId) -> &[NamedLine] {
        self.raw.get(&speaker).map_or(&[], Vec::as_slice)
    }

    pub fn lines(&self, shown: SpeakerId) -> &[NamedLine] {
        self.shown.get(&shown).map_or(&[], Vec::as_slice)
    }

    pub fn has_live_line(&self, shown: SpeakerId) -> bool {
        self.lines(shown)
            .iter()
            .any(|line| line.live_decision.is_some())
    }

    /// Pass `None` to check whether the user's decisions disagree with each other.
    pub fn conflict(
        &self,
        meeting: &Meeting,
        speaker: SpeakerId,
        candidate: Option<&PersonId>,
    ) -> Option<IdentityConflict> {
        let shown = effective_speaker(meeting, speaker);
        let lines = self.lines(shown);
        let different_names = lines
            .first()
            .is_some_and(|first| lines.iter().any(|line| line.person_id != first.person_id));
        let rejected = candidate.is_some_and(|p| effective_rejections(meeting, shown).contains(p));
        if different_names
            || rejected
            || candidate.is_some_and(|p| lines.iter().any(|line| &line.person_id != p))
        {
            return Some(IdentityConflict {
                candidate: candidate.cloned(),
                lines: lines.to_vec(),
                rejected,
                unlinked_caption: None,
            });
        }
        None
    }

    /// Explicit speaker naming is allowed, but its samples cannot include
    /// audio from lines the user attributed to another person.
    pub fn allows_sample(
        &self,
        speaker: SpeakerId,
        person: &PersonId,
        start: f64,
        end: f64,
    ) -> bool {
        self.raw_lines(speaker).iter().all(|line| {
            &line.person_id == person
                || line.end.min(end) - line.start.max(start) <= crate::live_identity::ROUNDING_S
        })
    }

    /// Re-embedding follows the original sample's source and rechecks it
    /// against current user evidence before also teaching the new model.
    pub fn allows_voiceprint(
        &self,
        meeting: &Meeting,
        print: &Voiceprint,
        person: &PersonId,
    ) -> bool {
        let kind = print.source.kind;
        if matches!(kind, VoiceprintKind::Hint | VoiceprintKind::Enrollment) {
            return true;
        }
        let speakers = meeting
            .speakers
            .iter()
            .filter(|s| super::sample_of_speaker(print, meeting, s.id, None));
        for speaker in speakers {
            if matches!(
                kind,
                VoiceprintKind::Auto | VoiceprintKind::Captions | VoiceprintKind::LiveConfirmation
            ) && speaker.assignment != crate::schema::Assignment::Confirmed
                && self.conflict(meeting, speaker.id, Some(person)).is_some()
            {
                return false;
            }
            if !print
                .source
                .audio_ranges()
                .all(|[a, b]| self.allows_sample(speaker.id, person, f64::from(a), f64::from(b)))
            {
                return false;
            }
        }
        true
    }
}

pub fn refresh_identity_conflict_notes(lib: &Library, meeting: &mut Meeting) -> Result<()> {
    let notes = if meeting
        .speakers
        .iter()
        .any(|speaker| speaker.identity_conflict.is_some())
    {
        let names = super::person_names(lib)?;
        meeting
            .speakers
            .iter()
            .filter_map(|speaker| {
                Some(
                    speaker
                        .identity_conflict
                        .as_ref()?
                        .message(speaker.id, &names),
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    meeting.set_review_notes(ReviewSource::SpeakerIdentity, notes);
    Ok(())
}
