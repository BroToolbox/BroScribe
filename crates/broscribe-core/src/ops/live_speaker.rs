//! Final speaker names supported by the user's recording-time decisions.

use std::collections::{BTreeMap, BTreeSet};

use super::{UserSpeakerEvidence, caption_proven, group_of, review, save_and_index};
use crate::config::LiveConfig;
use crate::index::Index;
use crate::live_identity::{self, LiveOutcome, LivePromotion, LiveSpeakerNaming, NEVER_FORGOTTEN};
use crate::queue::{CONFIRM_KIND, Queue, VECTORS_KIND};
use crate::schema::{
    self, Assignment, Meeting, ProcessingStatus, Speaker, Suggestion, VoiceprintKind,
};
use crate::{Error, Library, MeetingId, PersonId, Result, WriteTx};

/// Keep an explicit speaker edit across a split or renumbering even when
/// the normal one-to-one confirmation mapping has no single successor.
/// Only speech on a line carrying recording-time evidence passes this fence.
pub fn carry_live_speaker_overrides(old: &Meeting, new: &mut Meeting) {
    if !old
        .speakers
        .iter()
        .any(|speaker| speaker.live_speaker_override)
    {
        return;
    }
    let overrides: Vec<_> = old
        .segments
        .iter()
        .zip(super::effective_attribution(old))
        .filter(|(segment, attribution)| {
            attribution.live_decision.is_some()
                && segment
                    .speaker
                    .and_then(|id| old.speaker(id))
                    .is_some_and(|speaker| speaker.live_speaker_override)
        })
        .map(|(segment, _)| segment)
        .collect();
    let successors: BTreeSet<_> = new
        .segments
        .iter()
        .filter(|segment| {
            overrides.iter().any(|prior| {
                segment.track == prior.track
                    && segment.end.min(prior.end) - segment.start.max(prior.start)
                        > live_identity::ROUNDING_S
            })
        })
        .filter_map(|segment| segment.speaker)
        .collect();
    for speaker in &mut new.speakers {
        speaker.live_speaker_override |= successors.contains(&speaker.id);
    }
}

/// Reconcile live names with the final line attributions. The caller saves
/// the meeting and synchronizes the accompanying learning jobs and samples.
pub fn reconcile_live_speakers(
    lib: &Library,
    meeting: &mut Meeting,
    settings: &LiveConfig,
) -> Result<()> {
    if meeting.processing.status != ProcessingStatus::Done
        || meeting.live_attributions.is_empty()
            && meeting
                .speakers
                .iter()
                .all(|s| s.live_promotion.is_none() && s.live_suggestion_lines.is_none())
    {
        return Ok(());
    }
    let evidence = UserSpeakerEvidence::of(meeting);
    let records: BTreeMap<_, _> = meeting
        .live_attributions
        .iter()
        .map(|record| (record.decision, record))
        .collect();
    let proven = caption_proven(lib, meeting)?;
    let mut updates = Vec::new();
    let required_lines = settings.promotion_min_lines();
    for original in super::effective_speakers(meeting) {
        let id = original.id;
        let mut speaker = original.clone();
        let old = speaker.live_promotion.take();
        if old.is_some() || speaker.live_suggestion_lines.take().is_some() {
            clear_name(&mut speaker);
        }
        if speaker.assignment == Assignment::Confirmed || speaker.live_speaker_override {
            updates.push(speaker);
            continue;
        }
        let group: Vec<_> =
            group_of(meeting, id).map_or_else(|| vec![id], |group| group.speakers().collect());
        let mut lines = Vec::new();
        let mut most_on_one_speaker = 0;
        for raw in &group {
            let named: Vec<_> = evidence
                .raw_lines(*raw)
                .iter()
                .filter(|line| {
                    line.live_decision
                        .and_then(|decision| records.get(&decision))
                        .is_some_and(|record| {
                            record.outcome == LiveOutcome::Applied && !record.user_override
                        })
                })
                .collect();
            most_on_one_speaker = most_on_one_speaker.max(named.len());
            lines.extend(named);
        }
        if let Some(first) = lines.first() {
            let person = &first.person_id;
            let conflict = evidence.conflict(meeting, id, Some(person));
            if conflict.is_some() {
                // When the disagreement is between the lines themselves,
                // neither of those names is a proposed speaker identity.
                speaker.identity_conflict = evidence.conflict(meeting, id, None).or(conflict);
                clear_name(&mut speaker);
            } else if let Some(profile) = live_person(lib, person)?
                && (meeting.paired_mic_track() != Some(speaker.track.as_str()) || profile.is_self)
                && group.iter().all(|raw| {
                    meeting.speaker(*raw).is_some_and(|member| {
                        !member.live_speaker_override
                            && (member.assignment != Assignment::Confirmed
                                || member.live_promotion.is_some()
                                || member.named_person() == Some(person))
                    }) && proven.get(raw).is_none_or(|named| named == person)
                })
            {
                let decisions = supporting_decisions(&records, &lines, person);
                let fence = live_identity::forget_fence(lib, person.as_str())?;
                let current = decisions.iter().all(|decision| {
                    records[decision]
                        .forget_fence
                        .as_deref()
                        .unwrap_or(NEVER_FORGOTTEN)
                        == fence
                });
                let withdrawn_claim = meeting
                    .segments
                    .iter()
                    .filter(|segment| segment.speaker.is_some_and(|id| group.contains(&id)))
                    .any(|segment| {
                        records.values().any(|record| {
                            record.outcome == LiveOutcome::Forgotten
                                && record.range.track == segment.track
                                && record.from_s.zip(record.to_s).is_some_and(|(a, b)| {
                                    b.min(segment.end) - a.max(segment.start)
                                        > live_identity::ROUNDING_S
                                })
                        })
                    });
                if current && !withdrawn_claim && most_on_one_speaker >= required_lines {
                    speaker.live_promotion = Some(LivePromotion {
                        person_id: person.clone(),
                        decision_ids: decisions.into_iter().collect(),
                        line_count: lines.len(),
                        required_lines,
                        at: old
                            .as_ref()
                            .filter(|old| old.person_id == *person)
                            .map_or_else(schema::now, |old| old.at),
                    });
                    speaker.assignment = Assignment::Confirmed;
                    speaker.person_id = Some(person.clone());
                    speaker.score = None;
                    speaker.suggestion = None;
                    speaker.identity_conflict = None;
                } else if current && !withdrawn_claim {
                    clear_name(&mut speaker);
                    speaker.assignment = Assignment::Suggested;
                    speaker.suggestion = Some(Suggestion {
                        person_id: person.clone(),
                        score: 0.0,
                    });
                    speaker.live_suggestion_lines = Some(lines.len());
                }
            }
        }
        updates.push(speaker);
    }
    for speaker in updates {
        let id = speaker.id;
        let newly_promoted = speaker.live_promotion.is_some()
            && meeting
                .speaker(id)
                .is_some_and(|s| s.live_promotion.is_none());
        *meeting.speaker_mut(id).expect("existing speaker") = speaker;
        if newly_promoted {
            review::clear_dismissal(meeting, id);
        }
    }
    super::refresh_identity_conflict_notes(lib, meeting)
}

fn live_person(lib: &Library, person: &PersonId) -> Result<Option<schema::Person>> {
    match lib.load_person(person) {
        Ok(person) => Ok(Some(person)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn supporting_decisions(
    records: &BTreeMap<u64, &live_identity::LiveAttribution>,
    lines: &[&super::identity_conflict::NamedLine],
    person: &PersonId,
) -> BTreeSet<u64> {
    let mut decisions = BTreeSet::new();
    for line in lines {
        let decision = line.live_decision.expect("filtered above");
        let record = records[&decision];
        // Adjacent ranges from the same person can support one final line.
        decisions.insert(decision);
        decisions.extend(
            records
                .values()
                .filter(|other| {
                    !other.user_override
                        && other.outcome == LiveOutcome::Applied
                        && other.person_id.as_ref() == Some(person)
                        && other.range.track == record.range.track
                        && other.supported.contains(&[line.start, line.end])
                })
                .map(|other| other.decision),
        );
    }
    decisions
}

fn clear_name(speaker: &mut Speaker) {
    speaker.assignment = Assignment::None;
    speaker.person_id = None;
    speaker.score = None;
    speaker.suggestion = None;
    speaker.live_suggestion_lines = None;
}

fn same_promotion(a: &LivePromotion, b: &LivePromotion) -> bool {
    a.at == b.at && a.person_id == b.person_id
}

/// Queue the ordinary clean-sample job, fenced to this particular promotion.
/// Remove only samples taught by a retracted live naming. Both operations
/// are safe to retry before the caller writes the meeting.
pub fn sync_live_promotions(
    tx: &mut WriteTx<'_>,
    before: Option<&Meeting>,
    meeting: &Meeting,
    model: &str,
) -> Result<()> {
    if let Some(before) = before {
        retract_live_samples(tx, before, meeting)?;
    }
    let mut queued = false;
    for new in live_identity::speaker_namings(meeting) {
        let previous = before
            .filter(|before| before.speaker_namespace == meeting.speaker_namespace)
            .and_then(|before| before.speaker(new.speaker_id))
            .and_then(|speaker| speaker.live_promotion.as_ref());
        if previous.is_some_and(|old| same_promotion(old, &new.promotion)) {
            continue;
        }
        let person = &new.promotion.person_id;
        let mut profile = tx.library().load_person(person)?;
        if profile.needs_review {
            profile.needs_review = false;
            tx.save_person(&profile)?;
            Index::open(tx.root())?.upsert_person(tx, &profile)?;
        }
        let speaker = new.speaker_id;
        let stamp = new.promotion.at.to_rfc3339();
        let queue = Queue::open(tx.root())?;
        queue.enqueue(tx, CONFIRM_KIND, &format!("{}/{speaker}/{person}/{model}/live/{}/{stamp}", meeting.id, meeting.speaker_namespace), serde_json::json!({
            "meeting_id": meeting.id, "speaker_id": speaker, "person_id": person, "model": model,
            "live_promotion_at": new.promotion.at, "live_speaker_namespace": meeting.speaker_namespace,
        }))?;
        queued = true;
    }
    if queued {
        crate::learning::request(tx, model, Some(&meeting.id))?;
    }
    Ok(())
}

/// Removes samples whose supporting speaker name was withdrawn. `before`
/// identifies the analysis namespace those samples are filed under.
pub fn retract_live_samples(
    tx: &mut WriteTx<'_>,
    before: &Meeting,
    meeting: &Meeting,
) -> Result<()> {
    let current = live_identity::speaker_namings(meeting);
    for old in live_identity::speaker_namings(before) {
        if !current
            .iter()
            .any(|new| same_promotion(&new.promotion, &old.promotion))
        {
            remove_samples(tx, before, &old)?;
        }
    }
    Ok(())
}

fn remove_samples(
    tx: &mut WriteTx<'_>,
    meeting: &Meeting,
    naming: &LiveSpeakerNaming,
) -> Result<()> {
    let person = &naming.promotion.person_id;
    let mut prints = tx.library().load_voiceprints(person)?;
    let mut models = BTreeSet::new();
    prints.embeddings.retain(|print| {
        let remove = print.source.kind == VoiceprintKind::LiveConfirmation
            && print.source.meeting_id.as_ref() == Some(&meeting.id)
            && print.source.speaker_namespace == meeting.speaker_namespace
            && print.source.speaker_id == Some(naming.speaker_id);
        if remove {
            models.insert(print.model.clone());
        }
        !remove
    });
    if !models.is_empty() {
        for model in models {
            crate::learning::request(tx, &model, Some(&meeting.id))?;
        }
        tx.save_voiceprints(&prints)?;
    }
    Ok(())
}

/// Undo from speaker history. The independently named lines remain.
pub fn undo_live_promotion(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    id: &MeetingId,
    naming: &LiveSpeakerNaming,
) -> Result<()> {
    let mut meeting = tx.library().load_meeting(id)?;
    Queue::open(tx.root())?.check_meeting_idle_except(&meeting, &[CONFIRM_KIND, VECTORS_KIND])?;
    let speaker = meeting
        .speaker_mut(naming.speaker_id)
        .filter(|speaker| speaker.live_promotion.as_ref() == Some(&naming.promotion))
        .ok_or_else(|| Error::Service {
            code: "identity_edit_changed",
            message: "The speaker name changed after it was shown.".into(),
        })?;
    speaker.live_promotion = None;
    speaker.live_speaker_override = true;
    clear_name(speaker);
    mark_group_override(&mut meeting, naming.speaker_id);
    remove_samples(tx, &meeting, naming)?;
    super::mark_generated_stale(&mut meeting);
    Queue::open(tx.root())?.enqueue(
        tx,
        VECTORS_KIND,
        id.as_str(),
        serde_json::json!({"meeting_id": id}),
    )?;
    save_and_index(tx, index, &mut meeting)
}

pub(super) fn mark_group_override(meeting: &mut Meeting, id: crate::SpeakerId) {
    if let Some(group) = group_of(meeting, id) {
        let members = group.members.clone();
        for member in members {
            if let Some(speaker) = meeting.speaker_mut(member) {
                speaker.live_speaker_override = true;
            }
        }
    }
}

pub(super) fn forget_live_promotion(meeting: &mut Meeting, id: &PersonId) {
    for speaker in &mut meeting.speakers {
        if speaker
            .live_promotion
            .as_ref()
            .is_some_and(|promotion| promotion.person_id == *id)
        {
            speaker.live_promotion = None;
            speaker.centroid = None;
            clear_name(speaker);
        }
        if speaker
            .suggestion
            .as_ref()
            .is_some_and(|suggestion| suggestion.person_id == *id)
        {
            speaker.live_suggestion_lines = None;
        }
    }
}

/// A saved transcript edit takes effect immediately. It may invalidate a
/// recording-time name, but doesn't turn any pending suggestion into a name.
pub(super) fn line_evidence_changed(tx: &mut WriteTx<'_>, meeting: &mut Meeting) -> Result<()> {
    let evidence = UserSpeakerEvidence::of(meeting);
    for naming in live_identity::speaker_namings(meeting) {
        if super::is_grouped_member(meeting, naming.speaker_id) {
            continue;
        }
        let conflict = evidence.conflict(
            meeting,
            naming.speaker_id,
            Some(&naming.promotion.person_id),
        );
        let live_count = |speaker| {
            evidence
                .raw_lines(speaker)
                .iter()
                .filter(|line| line.live_decision.is_some())
                .count()
        };
        let most_on_one_speaker = group_of(meeting, naming.speaker_id)
            .map(|group| group.speakers().map(live_count).max().unwrap_or_default())
            .unwrap_or_else(|| live_count(naming.speaker_id));
        if conflict.is_some() || most_on_one_speaker < naming.promotion.required_lines {
            remove_samples(tx, meeting, &naming)?;
            let speaker = meeting
                .speaker_mut(naming.speaker_id)
                .expect("existing speaker");
            speaker.live_promotion = None;
            speaker.identity_conflict = conflict;
            clear_name(speaker);
        }
    }
    let changes = super::guard_automatic_names(meeting);
    for change in &changes {
        if let Some(model) = meeting
            .speaker(change.speaker_id)
            .and_then(|s| s.centroid.as_ref())
            .map(|c| &c.model)
        {
            crate::learning::request(tx, model, Some(&meeting.id))?;
        }
    }
    if !changes.is_empty() {
        super::sync_weak_samples(tx, meeting, &changes)?;
    }
    super::refresh_identity_conflict_notes(tx.library(), meeting)
}
