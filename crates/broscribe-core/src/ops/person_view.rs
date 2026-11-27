//! The read-only projection of one person shared by `people show` and MCP
//! `get_person` (CL17, CL20): the person's fields and notes with the
//! revision an edit is made against, voice-sample counts (never vectors),
//! and the meetings they appear in.
//!
//! Meetings come from the index's derived person relations
//! ([`Index::person_meeting_candidates`]: named speakers, line corrections,
//! linked participants and live decisions awaiting review), so no command
//! reads every meeting; only those candidates are read, and their files
//! decide (an index row never names anyone by itself). An index that
//! predates the relations is refused (`schema`, rebuild it) rather than
//! answered from a full scan or passed off as complete.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, FixedOffset};
use serde::Serialize;

use super::{
    PersonRevision, effective_attribution, effective_speakers, effective_talk_s, person_state,
};
use crate::index::Index;
use crate::live_identity::{LiveReviewItem, live_review_items};
use crate::schema::{Person, Strength};
use crate::{Library, MeetingId, PersonId, Result, SpeakerId};

/// How many stored voice samples a person has of one model, kind and
/// strength.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VoiceSampleCount {
    pub model: String,
    pub kind: String,
    pub strength: Strength,
    pub count: usize,
}

/// One meeting a person appears in, as shown.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PersonMeeting {
    pub meeting_id: MeetingId,
    pub title: Option<String>,
    pub started_at: DateTime<FixedOffset>,
    /// Shown speakers named as the person (a group counts once, by its
    /// kept speaker).
    pub speakers: Vec<SpeakerId>,
    /// Their talk time (a group's is the whole group's).
    pub talk_s: f64,
    /// Lines attributed to the person by a line correction.
    pub lines: usize,
    /// Live decisions naming the person that await review.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub live_review: Vec<LiveReviewItem>,
}

/// [`person_view`]'s result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PersonView {
    #[serde(flatten)]
    pub person: Person,
    pub notes: String,
    pub revision: PersonRevision,
    pub voiceprints: Vec<VoiceSampleCount>,
    /// Newest first, then by ID.
    pub meetings: Vec<PersonMeeting>,
}

/// Person `id` as `people show` and MCP `get_person` show it; `not_found`
/// once forgotten. `index` is read only (see the module notes); meetings
/// that cannot be read are left out.
pub fn person_view(lib: &Library, index: &Index, id: &PersonId) -> Result<PersonView> {
    let state = person_state(lib, id)?;
    // Confirmed before weak for each model and kind.
    let mut counts: BTreeMap<(String, String, bool), usize> = BTreeMap::new();
    for v in lib.load_voiceprints(id)?.embeddings {
        *counts
            .entry((
                v.model,
                v.source.kind.as_str().to_owned(),
                v.strength == Strength::Weak,
            ))
            .or_default() += 1;
    }
    let voiceprints = counts
        .into_iter()
        .map(|((model, kind, weak), count)| VoiceSampleCount {
            model,
            kind,
            strength: if weak {
                Strength::Weak
            } else {
                Strength::Confirmed
            },
            count,
        })
        .collect();

    let candidates = index.person_meeting_candidates(id)?;
    let wanted: HashSet<&MeetingId> = candidates.iter().map(|c| &c.meeting_id).collect();
    let hexes: HashSet<&str> = candidates
        .iter()
        .map(|c| c.meeting_id.short_hex())
        .collect();
    let mut meetings = Vec::new();
    for dir in lib.list_meeting_dirs()? {
        let candidate = dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.rsplit_once('-'))
            .is_some_and(|(_, hex)| hexes.contains(hex));
        if !candidate {
            continue;
        }
        let Ok(m) = lib.load_meeting_at(&dir) else {
            continue;
        };
        if !wanted.contains(&m.id) {
            continue;
        }
        let speakers: Vec<SpeakerId> = effective_speakers(&m)
            .filter(|s| s.named_person() == Some(id))
            .map(|s| s.id)
            .collect();
        let lines = effective_attribution(&m)
            .iter()
            .filter(|a| a.line_person() == Some(id))
            .count();
        let live_review: Vec<LiveReviewItem> = live_review_items(&m)
            .into_iter()
            .filter(|i| i.person_id.as_ref() == Some(id))
            .collect();
        if speakers.is_empty() && lines == 0 && live_review.is_empty() {
            continue;
        }
        meetings.push(PersonMeeting {
            // `fold` from +0.0: an empty float `sum` is -0.0, which would
            // serialize as `"talk_s": -0.0` for a line-only appearance.
            talk_s: speakers
                .iter()
                .fold(0.0, |total, s| total + effective_talk_s(&m, *s)),
            meeting_id: m.id,
            title: m.title,
            started_at: m.started_at,
            speakers,
            lines,
            live_review,
        });
    }
    meetings.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.meeting_id.cmp(&b.meeting_id))
    });
    let mut person = state.person;
    // Every reader shows the effective hue, also for a file saved before
    // hues were stored.
    person.hue = Some(person.hue());
    Ok(PersonView {
        person,
        notes: state.notes,
        revision: state.revision,
        voiceprints,
        meetings,
    })
}
