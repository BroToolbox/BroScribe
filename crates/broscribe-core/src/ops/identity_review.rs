//! Cross-meeting identity decisions. Suggestions are derived locally; only
//! explicit decisions are saved in the library.
use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{
    NameOutcome, ReviewEntry, SpeakerFence, effective_rejections, effective_talk_s, group_of,
    is_grouped_member, name_speaker, resolve_or_new_person, review_dismissed,
};
use crate::{
    Error, Library, MeetingId, PersonId, Result, SpeakerId, WriteTx,
    config::Thresholds,
    identity,
    index::Index,
    queue::{CONFIRM_KIND, Queue, VECTORS_KIND},
    schema::{Assignment, Meeting, ProcessingStatus},
};

/// When the active model has no fitted automatic naming threshold, offer
/// only high-scoring matches for human review. Never used for auto-naming.
const UNCALIBRATED_REVIEW_MIN: f32 = 0.90;

pub(super) fn review_min(thresholds: Option<&Thresholds>) -> f32 {
    thresholds
        .and_then(|t| t.auto_name)
        .unwrap_or(UNCALIBRATED_REVIEW_MIN)
        .max(UNCALIBRATED_REVIEW_MIN)
}

pub(super) fn review_revision(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|e| Error::Internal(e.to_string()))?;
    Ok(crate::schema::sha256_hex(&bytes))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityOccurrence {
    pub meeting_id: MeetingId,
    pub speaker_id: SpeakerId,
    pub meeting_title: Option<String>,
    /// Fences the raw voice, decisions, and grouping seen at review time.
    pub revision: String,
    pub rejected: Vec<PersonId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub named_on_lines: Vec<PersonId>,
}

impl IdentityOccurrence {
    pub fn key(&self) -> (&MeetingId, SpeakerId) {
        (&self.meeting_id, self.speaker_id)
    }

    pub fn allows(&self, person: Option<&PersonId>) -> bool {
        person.is_none_or(|id| !self.rejected.contains(id))
            && self
                .named_on_lines
                .iter()
                .all(|named| Some(named) == person)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IdentityGroup {
    pub occurrences: Vec<IdentityOccurrence>,
    pub meeting_count: usize,
    pub talk_s: f64,
}

struct Candidate {
    occurrence: IdentityOccurrence,
    vector: Option<Vec<f32>>,
    separated: Vec<SpeakerId>,
    talk_s: f64,
}

fn candidate(meeting: &Meeting, speaker_id: SpeakerId, model: &str) -> Result<Option<Candidate>> {
    let Some(speaker) = meeting.speaker(speaker_id) else {
        return Ok(None);
    };
    if review_dismissed(meeting, speaker_id) || is_grouped_member(meeting, speaker_id) {
        return Ok(None);
    }
    let group = group_of(meeting, speaker_id);
    let evidence = super::UserSpeakerEvidence::of(meeting);
    let lines = evidence.lines(speaker_id);
    let members: Vec<_> = group
        .into_iter()
        .flat_map(|g| g.members.iter())
        .filter_map(|id| meeting.speaker(*id))
        .collect();
    // A settled speaker can still be shown individually by the old queue
    // (for example a confirmed placeholder), but never changed in a batch.
    let vector = if meeting.processing.status == ProcessingStatus::Done
        && speaker.assignment != Assignment::Confirmed
        && evidence.conflict(meeting, speaker_id, None).is_none()
        && members
            .iter()
            .all(|member| member.assignment != Assignment::Confirmed)
    {
        match speaker.centroid.as_ref().filter(|c| c.model == model) {
            Some(c) => identity::usable_vector(&c.vector_b64, c.dim)?
                .filter(|v| v.iter().any(|x| *x != 0.0)),
            None => None,
        }
    } else {
        None
    };
    let separated = meeting
        .not_same
        .iter()
        .filter_map(|[a, b]| {
            let a = super::effective_speaker(meeting, *a);
            let b = super::effective_speaker(meeting, *b);
            if a == speaker_id {
                Some(b)
            } else {
                (b == speaker_id).then_some(a)
            }
        })
        .collect();
    Ok(Some(Candidate {
        occurrence: IdentityOccurrence {
            meeting_id: meeting.id.clone(),
            speaker_id,
            meeting_title: meeting.title.clone(),
            revision: review_revision(&(
                SpeakerFence::of(meeting),
                speaker,
                members,
                meeting.processing.status,
                lines,
            ))?,
            rejected: effective_rejections(meeting, speaker_id),
            named_on_lines: lines
                .iter()
                .map(|line| line.person_id.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        },
        vector,
        separated,
        talk_s: effective_talk_s(meeting, speaker_id),
    }))
}

fn similar(a: &Candidate, b: &Candidate, min: f32) -> bool {
    if a.occurrence.meeting_id == b.occurrence.meeting_id
        && (a.separated.contains(&b.occurrence.speaker_id)
            || b.separated.contains(&a.occurrence.speaker_id))
    {
        return false;
    }
    matches!((&a.vector, &b.vector), (Some(a), Some(b))
        if a.len() == b.len() && identity::cosine(a, b) >= min)
}

/// Complete-link groups (every member matches every other member) prevent
/// a chain of near matches from joining two dissimilar voices. Existing
/// queue order determines the representative and breaks exact ties.
pub fn identity_groups(
    lib: &Library,
    entries: &[ReviewEntry],
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<Vec<IdentityGroup>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let lookup = lib.meeting_lookup()?;
    let mut meetings = HashMap::new();
    let mut groups: Vec<Vec<Candidate>> = Vec::new();
    let min = review_min(thresholds);
    for item in entries
        .iter()
        .filter(|e| !e.without_transcript)
        .map(|e| &e.item)
    {
        let meeting = meetings
            .entry(item.meeting_id.clone())
            .or_insert_with(|| lookup.load(&item.meeting_id).ok());
        let Some(meeting) = meeting else { continue };
        let Some(mut candidate) = candidate(meeting, item.speaker_id, model)? else {
            continue;
        };
        if let Some(speaker) = meeting.speaker(item.speaker_id)
            && speaker.assignment == Assignment::Auto
            && let Some(person) = speaker.person_id.as_ref()
            && lib.load_person(person).is_ok_and(|p| !p.needs_review)
        {
            // An out-of-date index can still list a now settled speaker.
            continue;
        }
        // A stale indexed speaker may need review without proving that the
        // current voice is the one the indexed row showed.
        if candidate.talk_s <= 0.0 {
            candidate.vector = None;
        }
        if let Some(group) = groups
            .iter_mut()
            .find(|g| g.iter().all(|m| similar(m, &candidate, min)))
        {
            group.push(candidate);
        } else {
            groups.push(vec![candidate]);
        }
    }
    let mut groups: Vec<_> = groups
        .into_iter()
        .map(|members| IdentityGroup {
            meeting_count: members
                .iter()
                .map(|m| &m.occurrence.meeting_id)
                .collect::<HashSet<_>>()
                .len(),
            talk_s: members.iter().map(|m| m.talk_s).sum(),
            occurrences: members.into_iter().map(|m| m.occurrence).collect(),
        })
        .collect();
    groups.sort_by(|a, b| {
        b.meeting_count
            .cmp(&a.meeting_count)
            .then_with(|| b.talk_s.total_cmp(&a.talk_s))
    });
    Ok(groups)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdentifyBatch {
    pub person: String,
    pub occurrences: Vec<IdentityOccurrence>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IdentifyOutcome {
    pub person_id: PersonId,
    pub person_name: String,
    pub meeting_count: usize,
    pub named: Vec<NameOutcome>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_conflicts: Vec<IdentityOccurrence>,
}

/// Verify the whole selection under the writer lock before applying any
/// names. This also rejects requests assembled from unrelated queue groups.
pub fn identify_batch(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    batch: &IdentifyBatch,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<IdentifyOutcome> {
    if batch.occurrences.is_empty() {
        return Err(Error::InvalidInput(
            "Select at least one occurrence.".into(),
        ));
    }
    let lib = tx.library().clone();
    let (person, created) = resolve_or_new_person(&lib, &batch.person)?;
    let entries = super::review_queue(&lib, None)?;
    let groups = identity_groups(&lib, &entries, model, thresholds)?;
    let selection: HashSet<_> = batch
        .occurrences
        .iter()
        .map(IdentityOccurrence::key)
        .collect();
    if selection.len() != batch.occurrences.len()
        || !groups.iter().any(|group| {
            group.occurrences.len() >= 2
                && batch.occurrences.iter().all(|shown| {
                    group.occurrences.iter().any(|current| {
                        current.key() == shown.key()
                            && current.revision == shown.revision
                            && !current.rejected.contains(&person.id)
                    })
                })
        })
    {
        return Err(Error::InvalidInput(
            "The matching speakers changed or rejected this person. Review the selection again."
                .into(),
        ));
    }
    let mut eligible = Vec::new();
    let mut skipped = Vec::new();
    for occurrence in &batch.occurrences {
        let meeting = lib.load_meeting(&occurrence.meeting_id)?;
        if let Some(conflict) = super::UserSpeakerEvidence::of(&meeting).conflict(
            &meeting,
            occurrence.speaker_id,
            Some(&person.id),
        ) {
            skipped.push((occurrence, conflict));
        } else {
            eligible.push(occurrence);
        }
    }
    if eligible.is_empty() {
        return Err(Error::InvalidInput(
            "Your earlier speaker decisions conflict with this name.".into(),
        ));
    }
    let meetings: BTreeSet<_> = eligible.iter().map(|o| &o.meeting_id).collect();
    // Create the profile once, so a batch never resolves the typed name
    // differently between meetings. The regular write clears needs_review.
    if created {
        tx.save_person(&person)?;
        index.upsert_person(tx, &person)?;
    }
    let queue = Queue::open(tx.root())?;
    let mut named = Vec::with_capacity(eligible.len());
    for occurrence in eligible {
        let meeting = &occurrence.meeting_id;
        let outcome = name_speaker(
            tx,
            index,
            meeting,
            occurrence.speaker_id,
            person.id.as_str(),
            Vec::new(),
            model,
        )?;
        let speaker = outcome.speaker_id;
        let person = &outcome.person_id;
        queue.enqueue(
            tx,
            CONFIRM_KIND,
            &format!("{meeting}/{speaker}/{person}/{model}"),
            serde_json::json!({
                "meeting_id": meeting,
                "speaker_id": speaker,
                "person_id": person,
                "model": model,
            }),
        )?;
        if !outcome.separated.is_empty() {
            queue.enqueue(
                tx,
                VECTORS_KIND,
                meeting.as_str(),
                serde_json::json!({ "meeting_id": meeting }),
            )?;
        }
        named.push(outcome);
    }
    for (occurrence, conflict) in &skipped {
        let mut meeting = lib.load_meeting(&occurrence.meeting_id)?;
        if let Some(speaker) = meeting.speaker_mut(occurrence.speaker_id) {
            speaker.identity_conflict = Some(conflict.clone());
            super::refresh_identity_conflict_notes(&lib, &mut meeting)?;
            super::save_and_index(tx, index, &mut meeting)?;
        }
    }
    let person_name = super::review_person_name(&person);
    Ok(IdentifyOutcome {
        person_id: person.id,
        person_name,
        meeting_count: meetings.len(),
        named,
        skipped_conflicts: skipped
            .into_iter()
            .map(|(occurrence, _)| occurrence.clone())
            .collect(),
    })
}
