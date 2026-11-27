//! Second checks of confirmed names, based on independent local voice samples.
use std::{collections::BTreeMap, fs, io::ErrorKind, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::{identity_review::review_revision, is_grouped_member, sample_clips};
use crate::{
    Error, Library, MeetingId, PersonId, Result, SpeakerId, WriteTx,
    config::Thresholds,
    fsutil, identity,
    schema::{
        Assignment, Meeting, ProcessingStatus, RecordingState, Speaker, Strength, Voiceprint,
        VoiceprintKind, VoiceprintSource,
    },
};

const KEPT_FILE: &str = "identity-checks.json";
const UNCALIBRATED_REVIEW_BELOW: f32 = 0.35;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityCheckDecision {
    pub meeting_id: MeetingId,
    pub speaker_id: SpeakerId,
    pub person_id: PersonId,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IdentityAlert {
    pub decision: IdentityCheckDecision,
    pub person_name: String,
    pub meeting_title: Option<String>,
    pub score: f32,
    pub occurrence_sample: Option<VoiceprintSource>,
    pub reference_sample: VoiceprintSource,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct IdentityChecks {
    pub alerts: Vec<IdentityAlert>,
    pub compared: usize,
    pub unreadable_meetings: usize,
}

type Kept = BTreeMap<String, String>;

struct Profile {
    name: String,
    samples: Vec<(Voiceprint, Vec<f32>)>,
    kept: Kept,
}

fn kept_path(lib: &Library, person: &PersonId) -> PathBuf {
    lib.person_dir(person).join(KEPT_FILE)
}

fn read_kept(lib: &Library, person: &PersonId) -> Result<Kept> {
    match fsutil::read_json(&kept_path(lib, person)) {
        Ok(kept) => Ok(kept),
        Err(Error::NotFound(_)) => Ok(Kept::new()),
        Err(e) => Err(e),
    }
}

pub(super) fn forget_identity_checks(tx: &WriteTx<'_>, person: &PersonId) -> Result<()> {
    let path = kept_path(tx.library(), person);
    match fs::remove_file(&path) {
        Ok(()) => fsutil::sync_dir(&tx.library().person_dir(person)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_at(&path, e)),
    }
}

fn load_profile(lib: &Library, person: &PersonId, model: &str) -> Result<Profile> {
    let name = super::review_person_name(&lib.load_person(person)?);
    let mut samples = Vec::new();
    for print in lib.load_voiceprints(person)?.embeddings {
        if print.model == model
            && print.strength == Strength::Confirmed
            && (print.source.meeting_id.is_some()
                || (print.source.kind == VoiceprintKind::Enrollment
                    && print.source.sha256.is_some()))
            && let Some(vector) = identity::usable_vector(&print.vector_b64, print.dim)?
            && vector.iter().any(|x| *x != 0.0)
        {
            samples.push((print, vector));
        }
    }
    samples.sort_by(|(a, _), (b, _)| a.id.cmp(&b.id));
    Ok(Profile {
        name,
        samples,
        kept: read_kept(lib, person)?,
    })
}

fn threshold(thresholds: Option<&Thresholds>) -> f32 {
    thresholds
        .and_then(|t| t.suggest)
        .filter(|score| (-1.0..=1.0).contains(score))
        .unwrap_or(UNCALIBRATED_REVIEW_BELOW)
}

fn key(meeting: &MeetingId, speaker: SpeakerId, model: &str) -> String {
    format!("{meeting}/{speaker}/{model}")
}

fn compare(
    meeting: &Meeting,
    speaker: &Speaker,
    profile: &Profile,
    model: &str,
    below: f32,
    checks: &mut IdentityChecks,
) -> Result<()> {
    let Some(person) = speaker.person_id.as_ref() else {
        return Ok(());
    };
    let Some(centroid) = speaker.centroid.as_ref().filter(|c| c.model == model) else {
        return Ok(());
    };
    let Some(vector) = identity::usable_vector(&centroid.vector_b64, centroid.dim)?
        .filter(|v| v.iter().any(|x| *x != 0.0))
    else {
        return Ok(());
    };
    let independent = |print: &Voiceprint| {
        print.source.meeting_id.as_ref() != Some(&meeting.id)
            && !meeting.recording.as_ref().is_some_and(|rec| {
                let digest = print.source.sha256.as_deref();
                digest == Some(rec.sha256.as_str())
                    || (digest.is_some() && digest == rec.source_sha256.as_deref())
            })
    };
    let comparisons: Vec<_> = profile
        .samples
        .iter()
        .filter(|(print, sample)| sample.len() == vector.len() && independent(print))
        .map(|(print, sample)| (print, identity::cosine(&vector, sample)))
        .collect();
    // One vote per recording, regardless of how many clips it provided.
    // A strict majority must disagree, so one good match cannot hide a
    // minority cluster and a tie still gives the name the benefit of doubt.
    let mut by_origin = BTreeMap::<_, (&Voiceprint, f32)>::new();
    for &(print, score) in &comparisons {
        let Some(origin) = crate::schema::VoiceprintOrigin::of_source(&print.source) else {
            continue;
        };
        let origin = match origin {
            crate::schema::VoiceprintOrigin::Recording { sha256 } => (0, sha256),
            crate::schema::VoiceprintOrigin::Meeting { id } => (1, id.as_str()),
        };
        let best = by_origin.entry(origin).or_insert((print, score));
        if score > best.1 {
            *best = (print, score);
        }
    }
    let mut references: Vec<_> = by_origin.into_values().collect();
    references.sort_by(|a, b| a.1.total_cmp(&b.1));
    let Some(&(reference, score)) = references.get(references.len() / 2) else {
        return Ok(());
    };
    checks.compared += 1;
    if score >= below {
        return Ok(());
    }
    let evidence: Vec<_> = comparisons
        .iter()
        .map(|(print, _)| (&print.id, &print.vector_b64, &print.source))
        .collect();
    let revision = review_revision(&(
        &meeting.id,
        &meeting.speaker_namespace,
        meeting.recording.as_ref().map(|rec| &rec.sha256),
        speaker.id,
        person,
        centroid,
        super::group_of(meeting, speaker.id),
        below,
        evidence,
    ))?;
    if profile.kept.get(&key(&meeting.id, speaker.id, model)) == Some(&revision) {
        return Ok(());
    }
    let occurrence_sample = meeting
        .recording
        .as_ref()
        .filter(|rec| rec.state != RecordingState::Deleted)
        .and_then(|rec| {
            let clip = sample_clips(meeting, speaker.id).into_iter().find(|clip| {
                meeting.segments.iter().any(|segment| {
                    segment.speaker == Some(speaker.id)
                        && segment.start == clip.start
                        && segment.end >= clip.end
                })
            })?;
            Some(VoiceprintSource {
                kind: VoiceprintKind::Confirmation,
                meeting_id: Some(meeting.id.clone()),
                sha256: Some(rec.sha256.clone()),
                start: clip.start,
                end: clip.end,
                ranges: Vec::new(),
                speaker_id: Some(speaker.id),
                speaker_namespace: meeting.speaker_namespace.clone(),
            })
        });
    checks.alerts.push(IdentityAlert {
        decision: IdentityCheckDecision {
            meeting_id: meeting.id.clone(),
            speaker_id: speaker.id,
            person_id: person.clone(),
            revision,
        },
        person_name: profile.name.clone(),
        meeting_title: meeting.title.clone(),
        score,
        occurrence_sample,
        reference_sample: reference.source.clone(),
    });
    Ok(())
}

fn confirmed_speakers(meeting: &Meeting) -> impl Iterator<Item = &Speaker> {
    meeting.speakers.iter().filter(|speaker| {
        meeting.processing.status == ProcessingStatus::Done
            && speaker.assignment == Assignment::Confirmed
            && !is_grouped_member(meeting, speaker.id)
    })
}

/// Check the current canonical confirmations. New samples can change a
/// result in either direction, including for meetings already reviewed.
pub fn identity_checks(
    lib: &Library,
    person: Option<&PersonId>,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<IdentityChecks> {
    let people = match person {
        Some(id) => vec![id.clone()],
        None => lib.list_people()?.into_iter().map(|p| p.id).collect(),
    };
    let mut checks = IdentityChecks::default();
    let mut profiles = BTreeMap::new();
    for id in people {
        let profile = load_profile(lib, &id, model)?;
        if !profile.samples.is_empty() {
            profiles.insert(id, profile);
        }
    }
    if profiles.is_empty() {
        return Ok(checks);
    }
    let below = threshold(thresholds);
    for dir in lib.list_meeting_dirs()? {
        let Ok(meeting) = lib.load_meeting_at(&dir) else {
            checks.unreadable_meetings += 1;
            continue;
        };
        for speaker in confirmed_speakers(&meeting) {
            if let Some(profile) = speaker.person_id.as_ref().and_then(|id| profiles.get(id)) {
                compare(&meeting, speaker, profile, model, below, &mut checks)?;
            }
        }
    }
    checks.alerts.sort_by(|a, b| {
        a.score
            .total_cmp(&b.score)
            .then_with(|| a.decision.meeting_id.cmp(&b.decision.meeting_id))
            .then_with(|| a.decision.speaker_id.cmp(&b.decision.speaker_id))
    });
    Ok(checks)
}

/// Keep a reviewed name for these exact inputs. The library lock fences
/// refreshing the evidence and recording the decision.
pub fn keep_identification(
    tx: &WriteTx<'_>,
    decision: &IdentityCheckDecision,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<()> {
    let lib = tx.library();
    super::check_profile_write(lib, &decision.person_id)?;
    let meeting = lib.load_meeting(&decision.meeting_id)?;
    let mut profile = load_profile(lib, &decision.person_id, model)?;
    let key = key(&decision.meeting_id, decision.speaker_id, model);
    let kept = profile.kept.remove(&key);
    let mut checks = IdentityChecks::default();
    if let Some(speaker) = confirmed_speakers(&meeting)
        .find(|s| s.id == decision.speaker_id && s.person_id.as_ref() == Some(&decision.person_id))
    {
        compare(
            &meeting,
            speaker,
            &profile,
            model,
            threshold(thresholds),
            &mut checks,
        )?;
    }
    if !checks
        .alerts
        .iter()
        .any(|alert| alert.decision == *decision)
    {
        return Err(Error::InvalidInput(
            "The identification or its voice evidence changed. Check it again.".into(),
        ));
    }
    if kept.as_ref() == Some(&decision.revision) {
        return Ok(());
    }
    profile.kept.insert(key, decision.revision.clone());
    fsutil::write_json_atomic(&kept_path(lib, &decision.person_id), &profile.kept)
}
