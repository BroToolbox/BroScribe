//! Library write operations used by the CLI and MCP (design §6.4, plan §13):
//! naming a speaker (which requests identity reevaluation of the other
//! speakers, [`crate::learning`]), rejecting suggestions, the review queue,
//! forget-voice / forget-person, tags and re-resolving every unconfirmed
//! speaker.
//!
//! Every write op takes the [`WriteTx`] of one [`Library::write`] and a
//! writable [`Index`] ([`with_index`] opens both). It re-reads the files it
//! changes under the lock, saves meetings through [`WriteTx::save_meeting`]
//! (which re-renders `meeting.md`) and updates the index. Nothing here runs
//! inference: callers compute embeddings before taking the lock.
//!
//! Automatic resolution never changes a `confirmed` speaker and always
//! honours `rejected_person_ids`; only the user naming a speaker again or
//! forgetting the person changes a confirmation.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Thresholds;
use crate::identity::{self, MAX_CONFIRMED, MAX_WEAK, PersonSamples, Resolution};
use crate::index::{Index, ReviewItem};
use crate::learning::{self, LearningRequest};
use crate::schema::{
    self, Assignment, Meeting, Person, ProcessingStatus, RecordingState, Segment, Speaker,
    Strength, Suggestion, Voiceprint, VoiceprintKind, VoiceprintSource, Voiceprints,
};
use crate::{
    Error, Library, MeetingId, PersonId, Result, SpeakerId, WriteTx, fsutil, names, paths,
};

mod attribution;
mod grouping;
mod identity_checks;
mod identity_conflict;
mod identity_review;
mod live_speaker;
mod meeting_edits;
mod person_edit;
mod person_merge;
mod person_view;
mod recording;
mod review;
mod review_history;

pub use identity_checks::{
    IdentityAlert, IdentityCheckDecision, IdentityChecks, identity_checks, keep_identification,
};

pub use attribution::{
    LineLabelRewrite, LiveDismissOutcome, STALE_REVIEW, SegmentAttribution, SegmentFingerprint,
    attribution_revision, carry_line_labels, dismiss_live_review, effective_attribution,
    enrichment_is_stale, forget_line_people, generated_is_stale, mark_generated_stale,
    meeting_references_person, reapply_utterance_people, relabel_actions, relabel_generated,
    segment_fingerprint, set_utterance_people, template_is_stale,
};
pub use grouping::{
    AutoGroupOutcome, DuplicateSuggestion, GroupOutcome, GroupingChange, NotSameOutcome,
    SpeakerFence, UngroupOutcome, apply_auto_groups, auto_group_meeting, caption_proven,
    check_overlay, check_speaker_fence, check_speaker_fence_at, check_speaker_revision,
    drop_member_auto_samples, duplicate_suggestions, effective_rejections, effective_speaker,
    effective_speakers, effective_talk_s, group_of, group_speakers, is_grouped_member,
    mark_not_same, reconcile_auto_groups, sanitize_overlay, speaker_revision, ungroup_speakers,
};
pub use identity_conflict::{
    IdentityConflict, UserSpeakerEvidence, refresh_identity_conflict_notes,
};
pub use identity_review::{
    IdentifyBatch, IdentifyOutcome, IdentityGroup, IdentityOccurrence, identify_batch,
    identity_groups,
};
pub use live_speaker::{
    carry_live_speaker_overrides, reconcile_live_speakers, retract_live_samples,
    sync_live_promotions, undo_live_promotion,
};
pub use meeting_edits::{ActionEdit, dismiss_review_note, edit_action, review_note_revision};
pub use person_edit::{
    NewPersonSpec, PERSON_NOTES_MAX_BYTES, PERSON_UNDO_FILE, PERSON_UNDO_MAX, PERSON_UNDO_WINDOW_S,
    PersonEditOutcome, PersonEditSpec, PersonField, PersonRevision, PersonState, add_person,
    edit_person, person_state, rerender_meetings_of, undo_person_edit,
};
pub use person_merge::{
    MergePeople, MergePeopleOutcome, PersonDuplicate, PersonPair, dismiss_person_duplicate,
    merge_people, pending_person_merge, person_duplicates, resolve_merged_person,
};
pub(crate) use person_merge::{check_meeting_write, check_profile_write, merge_redirect_path};
pub use person_view::{PersonMeeting, PersonView, VoiceSampleCount, person_view};
pub use recording::{
    Clip, ClipFiles, ClipSource, ENROLLMENT_DIR, PlaybackCache, clip_of, enqueue_playback_cache,
    locate_clip, playback_cache_key, playback_cache_path,
};
pub use review::{
    DismissOutcome, DismissedRow, MAX_SAMPLE_S, MAX_SAMPLES, SampleClip, dismiss_review,
    dismissed_speakers, first_speaker_segment, restore_review, review_dismissed,
    review_dismissed_at, review_dismissed_count, sample_clips,
};
pub(crate) use review_history::ReviewCapture;
pub use review_history::{ReviewEdit, record_review_edit};

/// File a manifest-import job keeps hint embeddings in, inside its queue folder.
pub const HINTS_JSON: &str = "hints.json";

/// Segments of sample text shown per review item.
const REVIEW_SAMPLE_SEGMENTS: usize = 2;

/// Runs `f` inside one library write transaction with a writable index.
pub fn with_index<T>(
    lib: &Library,
    f: impl FnOnce(&mut WriteTx<'_>, &mut Index) -> Result<T>,
) -> Result<T> {
    with_index_checked(lib, || Ok(()), f)
}

/// [`with_index`] with a request-owned admission check under the writer lock,
/// before opening or changing the index or canonical files.
pub fn with_index_checked<T>(
    lib: &Library,
    check: impl FnOnce() -> Result<()>,
    f: impl FnOnce(&mut WriteTx<'_>, &mut Index) -> Result<T>,
) -> Result<T> {
    lib.write_checked(check, |tx| {
        let mut index = Index::open(tx.root())?;
        f(tx, &mut index)
    })
}

/// One embedding of a confirmed speaker with the range (seconds) it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct ConfirmationSample {
    pub vector: Vec<f32>,
    pub start: f32,
    pub end: f32,
    /// The meeting's speaker namespace when the sample was taken from a
    /// meeting speaker; `None` for audio that is not a meeting speaker's
    /// (enrollment files, passages).
    pub namespace: Option<String>,
}

/// Result of [`name_speaker`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NameOutcome {
    pub person_id: PersonId,
    /// The speaker named: the group's kept speaker when the request named a
    /// grouped member.
    pub speaker_id: SpeakerId,
    /// The grouped member the request named, when the kept speaker was
    /// named instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirected_from: Option<SpeakerId>,
    /// A new person was created for the given name.
    pub created: bool,
    /// The person the speaker was named as before (`confirmed` or `auto`),
    /// when different; their samples from this meeting were removed.
    pub previous_person_id: Option<PersonId>,
    /// `false` when the speaker had neither samples nor a centroid.
    pub voiceprint_added: bool,
    /// Weak samples attributable to this speaker promoted to confirmed.
    pub promoted: usize,
    /// Confirmed members of the speaker's group named the same person with
    /// it (they stay grouped).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members_renamed: Vec<SpeakerId>,
    /// Members taken out of the group because their own evidence contradicts
    /// the new name; their identity is unchanged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub separated: Vec<Separation>,
    /// The reevaluation of the other speakers this naming requested: the
    /// queue's worker runs it, this meeting first ([`crate::learning`]).
    /// Nothing else changed yet.
    pub learning: LearningRequest,
}

/// Why a grouped member was taken out of its group by an identity write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeparationReason {
    /// The member was marked not to be the person its group was named as.
    Rejected,
    /// The meeting's captions named the member as another person.
    Captions,
}

/// A member an identity write separated from its group; it is remembered in
/// `not_same` with the kept speaker and every remaining member, so it is
/// not grouped back with any of them automatically.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Separation {
    pub speaker_id: SpeakerId,
    /// The group's kept speaker.
    pub kept: SpeakerId,
    pub reason: SeparationReason,
    /// The person the member was marked not to be (`rejected`) or the
    /// captions named it as (`captions`).
    pub person_id: PersonId,
    pub message: String,
}

/// A speaker's naming state: the person is the assigned one for `confirmed`
/// / `auto` and the suggested one for `suggested`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeakerState {
    pub assignment: Assignment,
    pub person_id: Option<PersonId>,
    pub score: Option<f32>,
}

impl SpeakerState {
    pub fn of(speaker: &Speaker) -> Self {
        match speaker.assignment {
            Assignment::Confirmed | Assignment::Auto => Self {
                assignment: speaker.assignment,
                person_id: speaker.person_id.clone(),
                score: speaker.score,
            },
            Assignment::Suggested => Self {
                assignment: Assignment::Suggested,
                person_id: speaker.suggestion.as_ref().map(|s| s.person_id.clone()),
                score: speaker.suggestion.as_ref().map(|s| s.score),
            },
            Assignment::None => Self {
                assignment: Assignment::None,
                person_id: None,
                score: speaker.score,
            },
        }
    }

    fn auto_person(&self) -> Option<&PersonId> {
        match self.assignment {
            Assignment::Auto => self.person_id.as_ref(),
            _ => None,
        }
    }
}

/// A change of assignment or person on one speaker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeakerChange {
    pub meeting_id: MeetingId,
    pub speaker_id: SpeakerId,
    pub before: SpeakerState,
    pub after: SpeakerState,
}

/// Result of [`reject_suggestion`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RejectOutcome {
    pub rejected: PersonId,
    /// The speaker the rejection applied to: the group's kept speaker when
    /// the request named a grouped member.
    pub speaker_id: SpeakerId,
    /// The grouped member the request named, when the kept speaker took the
    /// rejection instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirected_from: Option<SpeakerId>,
    /// The speaker's state after the rejection (re-resolved when it pointed
    /// at the rejected person).
    pub after: SpeakerState,
    /// Members the captions named as the rejected person, taken out of the
    /// group (identity unchanged).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub separated: Vec<Separation>,
}

/// A review item with the speaker's first utterances.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReviewEntry {
    #[serde(flatten)]
    pub item: ReviewItem,
    pub sample_text: Vec<String>,
    /// Why `sample_text` is empty when the meeting could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_text_error: Option<String>,
    /// Unresolved quality work, not recognized speech or a completed identity.
    /// Requires a completed canonical meeting and a still-pending speaker
    /// with neither a transcript segment nor a playable diarized sample.
    pub without_transcript: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_suggestion_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_conflict: Option<String>,
}

/// Result of [`forget_voice`] and [`forget_person`].
/// Callers must purge integration photo caches with
/// `Integrations::forget_person_photos(&outcome.person_id)` after forgetting a person.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ForgetOutcome {
    pub person_id: PersonId,
    pub voiceprints_deleted: bool,
    pub person_deleted: bool,
    pub meetings_changed: Vec<MeetingId>,
    pub speakers_changed: usize,
    /// Entries removed from `.queue/*/hints.json`.
    pub hints_removed: usize,
    /// The reevaluations this forget requested (one per affected model):
    /// automatic names and suggestions that relied on the forgotten voice
    /// are reassessed without it.
    pub learning: Vec<LearningRequest>,
}

// ---- lookups ----

/// The person `reference` names: a person ID (must exist), an e-mail
/// (case-insensitive) or a name or alias (case-folded). `None` when no
/// e-mail or name matches; a name matching several people is
/// `invalid_input` listing their IDs.
pub fn find_person(lib: &Library, reference: &str) -> Result<Option<Person>> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(Error::InvalidInput("empty person reference".into()));
    }
    if let Ok(id) = PersonId::parse(reference) {
        return lib.load_person(&id).map(Some);
    }
    if reference.contains('@') {
        return lib.find_person_by_email(reference);
    }
    let name = fold(reference);
    let mut matches: Vec<Person> = lib
        .list_people()?
        .into_iter()
        .filter(|p| fold(&p.name) == name || p.aliases.iter().any(|a| fold(a) == name))
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => Err(Error::InvalidInput(format!(
            "{reference:?} matches several people: {}",
            matches
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// The person an explicit user action names (speaker naming, attendance
/// confirmation, line correction): [`find_person`]'s match, or, for a plain
/// typed name nobody has, a new name-only person (`created`) the caller
/// must save. A person ID never falls back to a name (a missing one is
/// `not_found`), an unknown e-mail is `not_found` rather than a name, an
/// ambiguous name is `invalid_input`, and a new name must pass
/// [`names::validate_name`] (no control or bidirectional-control
/// characters, bounded length). A merge source awaiting cleanup is refused.
/// Creates nothing itself.
pub fn resolve_or_new_person(lib: &Library, person_ref: &str) -> Result<(Person, bool)> {
    match find_person(lib, person_ref)? {
        Some(p) => {
            check_profile_write(lib, &p.id)?;
            Ok((p, false))
        }
        None if person_ref.contains('@') => Err(Error::NotFound(format!(
            "person with e-mail {}",
            person_ref.trim()
        ))),
        None => Ok((
            Person::new(lib.new_person_id(), names::validate_name(person_ref)?),
            true,
        )),
    }
}

fn review_person_name(person: &Person) -> String {
    if !person.name.trim().is_empty() {
        person.name.clone()
    } else if person.is_self {
        "Me".into()
    } else {
        person.id.to_string()
    }
}

/// Current display name of every existing person, by ID: what derived
/// notes may name ([`crate::live_identity::refresh_review_notes`]).
pub(crate) fn person_names(lib: &Library) -> Result<BTreeMap<PersonId, String>> {
    Ok(lib
        .list_people()?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect())
}

/// Which queued or running jobs make a meeting busy for a user edit
/// ([`editable_meeting`]). The job-to-meeting rule itself is the queue's
/// ([`crate::queue::job_refers_to`]); only the set of jobs differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditBusy {
    /// Any job working on the meeting ([`crate::queue::Queue::check_meeting_idle`]):
    /// grouping changes what every consumer of the meeting shows.
    AnyMeetingJob,
    /// Search vectors are keyed by the current windows and may refresh concurrently.
    ExceptVectors,
    /// Only its enrichment: a line correction lets template jobs run
    /// (their result is stored stale).
    Enrichment,
}

/// The meeting, if a user edit (`what`, e.g. "correcting lines") may change
/// it now: processing done (`invalid_input` after a failure, else
/// `library_busy`) and no job of the `busy` set working on it.
pub(crate) fn editable_meeting(
    tx: &WriteTx<'_>,
    meeting_id: &MeetingId,
    what: &str,
    busy: EditBusy,
) -> Result<Meeting> {
    let meeting = tx.library().load_meeting(meeting_id)?;
    match meeting.processing.status {
        ProcessingStatus::Done => {}
        ProcessingStatus::Failed => {
            return Err(Error::InvalidInput(format!(
                "{meeting_id} failed processing; reprocess it before {what}"
            )));
        }
        _ => {
            return Err(Error::LibraryBusy(format!(
                "{meeting_id} (it is still being processed; try again when it is done)"
            )));
        }
    }
    let queue = crate::queue::Queue::open(tx.root())?;
    match busy {
        EditBusy::AnyMeetingJob => queue.check_meeting_idle(&meeting)?,
        EditBusy::ExceptVectors => {
            queue.check_meeting_idle_except(&meeting, &[crate::queue::VECTORS_KIND])?
        }
        EditBusy::Enrichment => {
            queue.check_idle(crate::queue::ENRICH_KIND, meeting_id.as_str())?;
        }
    }
    Ok(meeting)
}

/// Longest meeting title accepted, in characters.
pub const TITLE_MAX_CHARS: usize = 200;

/// A title as the user typed it, trimmed: non-empty, one line, at most
/// [`TITLE_MAX_CHARS`] characters.
pub fn validate_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        return Err(Error::InvalidInput("empty title".into()));
    }
    if title.chars().any(char::is_control) {
        return Err(Error::InvalidInput("a title is one line of text".into()));
    }
    if title.chars().count() > TITLE_MAX_CHARS {
        return Err(Error::InvalidInput(format!(
            "a title has at most {TITLE_MAX_CHARS} characters"
        )));
    }
    Ok(title.to_owned())
}

/// Renames a meeting as the user's explicit choice (`title_source = user`,
/// `title_set_at` now), which calendar matching and later analysis passes
/// keep. The folder keeps its name: meeting IDs and paths are stable.
/// Returns the meeting afterwards. A recording's live or stopped session
/// is updated by `lifecycle::set_meeting_title`, which callers use.
pub fn set_title(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    title: &str,
) -> Result<Meeting> {
    let title = validate_title(title)?;
    let mut meeting = tx.library().load_meeting(meeting_id)?;
    meeting.set_user_title(Some(title));
    save_and_index(tx, index, &mut meeting)?;
    Ok(meeting)
}

/// Every person's stored samples for one embedding model, with the meeting
/// each automatically learned weak sample came from, preventing self-matches.
#[derive(Clone, Debug, Default)]
pub struct PeopleSamples {
    people: Vec<PersonSamples>,
    /// Per person, parallel to weak samples: source meeting for Auto/Captions.
    weak_auto_meeting: Vec<Vec<Option<MeetingId>>>,
    confirmed_meeting: Vec<Vec<Option<MeetingId>>>,
    weak_meeting: Vec<Vec<Option<MeetingId>>>,
}

impl PeopleSamples {
    /// All stored samples, for ephemeral audio that belongs to no meeting.
    pub fn all(&self) -> &[PersonSamples] {
        &self.people
    }

    /// Evaluation pool excluding all evidence from the held-out meeting,
    /// including confirmed hints and enrollment derived from that meeting.
    pub fn for_benchmark(&self, meeting: &MeetingId) -> Self {
        let mut out = Self::default();
        for (i, person) in self.people.iter().enumerate() {
            let mut samples = PersonSamples {
                person_id: person.person_id.clone(),
                confirmed: Vec::new(),
                weak: Vec::new(),
            };
            let mut confirmed_meeting = Vec::new();
            let mut weak_meeting = Vec::new();
            let mut weak_auto_meeting = Vec::new();
            for (sample, source) in person.confirmed.iter().zip(&self.confirmed_meeting[i]) {
                if source.as_ref() != Some(meeting) {
                    samples.confirmed.push(sample.clone());
                    confirmed_meeting.push(source.clone());
                }
            }
            for ((sample, source), automatic) in person
                .weak
                .iter()
                .zip(&self.weak_meeting[i])
                .zip(&self.weak_auto_meeting[i])
            {
                if source.as_ref() != Some(meeting) {
                    samples.weak.push(sample.clone());
                    weak_meeting.push(source.clone());
                    weak_auto_meeting.push(automatic.clone());
                }
            }
            if !samples.confirmed.is_empty() || !samples.weak.is_empty() {
                out.people.push(samples);
                out.confirmed_meeting.push(confirmed_meeting);
                out.weak_meeting.push(weak_meeting);
                out.weak_auto_meeting.push(weak_auto_meeting);
            }
        }
        out
    }

    /// Samples to resolve speakers of `meeting` against: everyone's samples
    /// minus the `auto` and `captions` weak samples taken from `meeting` (conservative: any
    /// speaker of that meeting, not only the one that produced it). Borrows
    /// when nothing is excluded.
    pub fn for_meeting(&self, meeting: &MeetingId) -> std::borrow::Cow<'_, [PersonSamples]> {
        let excluded = |ms: &[Option<MeetingId>]| ms.iter().any(|m| m.as_ref() == Some(meeting));
        if !self.weak_auto_meeting.iter().any(|ms| excluded(ms)) {
            return std::borrow::Cow::Borrowed(&self.people);
        }
        let out = self
            .people
            .iter()
            .zip(&self.weak_auto_meeting)
            .map(|(p, ms)| {
                if !excluded(ms) {
                    return p.clone();
                }
                PersonSamples {
                    person_id: p.person_id.clone(),
                    confirmed: p.confirmed.clone(),
                    weak: p
                        .weak
                        .iter()
                        .zip(ms)
                        .filter(|(_, m)| m.as_ref() != Some(meeting))
                        .map(|(v, _)| v.clone())
                        .collect(),
                }
            })
            .filter(|p| !p.confirmed.is_empty() || !p.weak.is_empty())
            .collect();
        std::borrow::Cow::Owned(out)
    }
}

/// Every person's stored samples for embedding `model` (confirmed and weak);
/// people without samples for that model are left out. A stored sample
/// that is not a finite vector is skipped: it never ranks or names anyone
/// ([`identity_check`] reports it, [`repair_identity`] removes it).
pub fn load_person_samples(lib: &Library, model: &str) -> Result<PeopleSamples> {
    let mut out = PeopleSamples::default();
    for person in lib.list_people()? {
        let vps = lib.load_voiceprints(&person.id)?;
        let mut samples = PersonSamples {
            person_id: person.id.clone(),
            confirmed: Vec::new(),
            weak: Vec::new(),
        };
        let mut auto_meeting = Vec::new();
        let mut confirmed_meeting = Vec::new();
        let mut weak_meeting = Vec::new();
        for vp in vps.embeddings.iter().filter(|v| v.model == model) {
            let Some(v) = identity::usable_vector(&vp.vector_b64, vp.dim)? else {
                continue;
            };
            match vp.strength {
                Strength::Confirmed => {
                    samples.confirmed.push(v);
                    confirmed_meeting.push(vp.source.meeting_id.clone());
                }
                Strength::Weak => {
                    samples.weak.push(v);
                    weak_meeting.push(vp.source.meeting_id.clone());
                    auto_meeting.push(
                        matches!(
                            vp.source.kind,
                            VoiceprintKind::Auto | VoiceprintKind::Captions
                        )
                        .then(|| vp.source.meeting_id.clone())
                        .flatten(),
                    );
                }
            }
        }
        if !samples.confirmed.is_empty() || !samples.weak.is_empty() {
            out.people.push(samples);
            out.weak_auto_meeting.push(auto_meeting);
            out.confirmed_meeting.push(confirmed_meeting);
            out.weak_meeting.push(weak_meeting);
        }
    }
    Ok(out)
}

// ---- resolution helpers (shared with the pipeline) ----

/// Writes `res` onto a speaker that is not `confirmed` (confirmed speakers
/// are left alone). Returns whether the speaker changed.
pub fn apply_resolution(speaker: &mut Speaker, res: &Resolution) -> bool {
    if speaker.assignment == Assignment::Confirmed {
        return false;
    }
    let before = speaker.clone();
    speaker.live_suggestion_lines = None;
    match (res.assignment, &res.person_id) {
        (Assignment::Auto, Some(p)) => {
            speaker.person_id = Some(p.clone());
            speaker.assignment = Assignment::Auto;
            speaker.suggestion = None;
        }
        (Assignment::Suggested, Some(p)) => {
            speaker.person_id = None;
            speaker.assignment = Assignment::Suggested;
            speaker.suggestion = Some(Suggestion {
                person_id: p.clone(),
                score: res.score.unwrap_or(0.0),
            });
        }
        _ => {
            speaker.person_id = None;
            speaker.assignment = Assignment::None;
            speaker.suggestion = None;
        }
    }
    speaker.score = res.score;
    *speaker != before
}

fn apply_guarded_resolution(
    meeting: &mut Meeting,
    id: SpeakerId,
    evidence: &UserSpeakerEvidence,
    res: &Resolution,
) {
    let candidate = if res.assignment == Assignment::Auto {
        res.person_id.as_ref()
    } else {
        meeting.speaker(id).and_then(|speaker| {
            auto_person(speaker).or_else(|| {
                speaker
                    .identity_conflict
                    .as_ref()
                    .and_then(|conflict| conflict.candidate.as_ref())
            })
        })
    };
    let conflict = evidence.conflict(meeting, id, candidate);
    let has_live = evidence.has_live_line(id);
    let speaker = meeting
        .speaker_mut(id)
        .expect("resolving an existing speaker");
    if conflict.is_some() || (has_live && res.assignment == Assignment::Auto) {
        apply_resolution(
            speaker,
            &Resolution {
                assignment: Assignment::None,
                person_id: None,
                score: None,
                runner_up_score: None,
            },
        );
    } else {
        apply_resolution(speaker, res);
    }
    speaker.identity_conflict = conflict;
}

/// Clear an automatic assignment the user's lines contradict, including a
/// mic assignment made by segment-level verification without a centroid.
pub fn guard_automatic_names(meeting: &mut Meeting) -> Vec<SpeakerChange> {
    let evidence = UserSpeakerEvidence::of(meeting);
    let decisions: Vec<_> = effective_speakers(meeting)
        .filter(|speaker| speaker.assignment != Assignment::Confirmed)
        .map(|speaker| {
            let candidate = auto_person(speaker).or_else(|| {
                speaker
                    .identity_conflict
                    .as_ref()
                    .and_then(|c| c.candidate.as_ref())
            });
            let lines = evidence.lines(speaker.id);
            let caption = speaker
                .identity_conflict
                .as_ref()
                .and_then(|c| c.unlinked_caption.as_ref());
            let conflict = if let Some(caption) =
                caption.filter(|_| candidate.is_none() && !lines.is_empty())
            {
                Some(IdentityConflict {
                    candidate: None,
                    lines: lines.to_vec(),
                    rejected: false,
                    unlinked_caption: Some(caption.clone()),
                })
            } else {
                evidence.conflict(meeting, speaker.id, candidate)
            };
            (speaker.id, conflict)
        })
        .collect();
    let mut changes = Vec::new();
    for (id, conflict) in decisions {
        let speaker = meeting.speaker_mut(id).expect("existing speaker");
        let before = SpeakerState::of(speaker);
        if conflict.is_some()
            || (before.assignment == Assignment::Auto && evidence.has_live_line(id))
        {
            apply_resolution(
                speaker,
                &Resolution {
                    assignment: Assignment::None,
                    person_id: None,
                    score: None,
                    runner_up_score: None,
                },
            );
        }
        speaker.identity_conflict = conflict;
        let after = SpeakerState::of(speaker);
        if before != after {
            changes.push(SpeakerChange {
                meeting_id: meeting.id.clone(),
                speaker_id: id,
                before,
                after,
            });
        }
    }
    changes
}

/// The person `speaker` is auto-named as, if it is: the name
/// [`identity::resolve_voice`] keeps while the ordinary resolution agrees.
pub fn auto_person(speaker: &Speaker) -> Option<&PersonId> {
    match speaker.assignment {
        Assignment::Auto => speaker.person_id.as_ref(),
        _ => None,
    }
}

/// Re-resolves every speaker of `meeting` that is not `confirmed`, not a
/// grouped member (frozen while grouped) and has a centroid of `model`,
/// against `people` (samples of `model`, minus this meeting's `auto`
/// samples), honouring each speaker's rejections, with the publication
/// policy of [`identity::resolve_voice`] (a new or replacement automatic
/// name needs the confirmed samples to agree). Pure: the caller saves the
/// meeting and passes the changes to [`sync_weak_samples`].
pub fn resolve_meeting(
    meeting: &mut Meeting,
    model: &str,
    people: &PeopleSamples,
    thresholds: Option<&Thresholds>,
    roster: Option<&BTreeSet<PersonId>>,
) -> Result<Vec<SpeakerChange>> {
    let people = people.for_meeting(&meeting.id);
    let frozen: BTreeSet<SpeakerId> = meeting
        .speaker_groups
        .iter()
        .flat_map(|g| g.members.iter().copied())
        .collect();
    // A kept speaker is never named as someone a member rejected.
    let group_rejections: BTreeMap<SpeakerId, Vec<PersonId>> = meeting
        .speaker_groups
        .iter()
        .map(|g| (g.kept, effective_rejections(meeting, g.kept)))
        .collect();
    let mut changes = Vec::new();
    let evidence = UserSpeakerEvidence::of(meeting);
    let ids: Vec<_> = meeting.speakers.iter().map(|s| s.id).collect();
    for id in ids {
        let s = meeting.speaker(id).expect("existing speaker");
        if s.assignment == Assignment::Confirmed || frozen.contains(&s.id) {
            continue;
        }
        let Some(c) = s.centroid.as_ref().filter(|c| c.model == model) else {
            continue;
        };
        // An unusable (non-finite) centroid is no centroid: left as is.
        let Some(centroid) = identity::usable_vector(&c.vector_b64, c.dim)? else {
            continue;
        };
        let res = identity::resolve_voice(
            &centroid,
            &people,
            group_rejections
                .get(&s.id)
                .unwrap_or(&s.rejected_person_ids),
            thresholds,
            roster,
            auto_person(s),
        );
        let before = SpeakerState::of(s);
        apply_guarded_resolution(meeting, id, &evidence, &res);
        let after = SpeakerState::of(meeting.speaker(id).expect("existing speaker"));
        if before.assignment != after.assignment || before.person_id != after.person_id {
            changes.push(SpeakerChange {
                meeting_id: meeting.id.clone(),
                speaker_id: id,
                before,
                after,
            });
        }
    }
    Ok(changes)
}

/// Re-resolves, honouring [`effective_rejections`], every unconfirmed kept
/// speaker of `meeting`'s groups that is named or suggested as a person one
/// of its members rejected: publishing paths group after resolution, so a
/// fresh group's kept speaker never stays named against a member's
/// rejection. Other speakers are left as they are. Pure, like
/// [`resolve_meeting`]; the caller syncs the changes.
pub fn resolve_rejected_kept(
    meeting: &mut Meeting,
    model: &str,
    people: &PeopleSamples,
    thresholds: Option<&Thresholds>,
    roster: Option<&BTreeSet<PersonId>>,
) -> Result<Vec<SpeakerChange>> {
    let people = people.for_meeting(&meeting.id);
    let kept: Vec<(SpeakerId, Vec<PersonId>)> = meeting
        .speaker_groups
        .iter()
        .map(|g| (g.kept, effective_rejections(meeting, g.kept)))
        .collect();
    let mut changes = Vec::new();
    let evidence = UserSpeakerEvidence::of(meeting);
    for (id, rejected) in kept {
        let Some(s) = meeting.speaker(id) else {
            continue;
        };
        let before = SpeakerState::of(s);
        if s.assignment == Assignment::Confirmed
            || !before
                .person_id
                .as_ref()
                .is_some_and(|p| rejected.contains(p))
        {
            continue;
        }
        let centroid = match s.centroid.as_ref().filter(|c| c.model == model) {
            Some(c) => identity::usable_vector(&c.vector_b64, c.dim)?,
            None => None,
        };
        let res = match centroid {
            // Its current person is rejected: any name it gets is new.
            Some(c) => identity::resolve_voice(&c, &people, &rejected, thresholds, roster, None),
            None => Resolution {
                assignment: Assignment::None,
                person_id: None,
                score: None,
                runner_up_score: None,
            },
        };
        apply_guarded_resolution(meeting, id, &evidence, &res);
        let after = SpeakerState::of(meeting.speaker(id).expect("existing speaker"));
        changes.push(SpeakerChange {
            meeting_id: meeting.id.clone(),
            speaker_id: id,
            before,
            after,
        });
    }
    Ok(changes)
}

/// Folds `later` changes into `changes`: a speaker changed twice keeps its
/// first `before` and its last `after`, so [`sync_weak_samples`] sees one
/// change per speaker.
pub fn merge_changes(changes: &mut Vec<SpeakerChange>, later: Vec<SpeakerChange>) {
    for c in later {
        match changes
            .iter_mut()
            .find(|e| e.meeting_id == c.meeting_id && e.speaker_id == c.speaker_id)
        {
            Some(e) => e.after = c.after,
            None => changes.push(c),
        }
    }
}

/// One person whose stored voice is close to a speaker's ([`voice_matches`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VoiceMatch {
    pub person_id: PersonId,
    /// Raw similarity (design §6.4 [`identity::score`]), not a probability.
    pub score: f32,
    /// Only on the first match: what [`identity::resolve_voice`] would
    /// auto-name, the publication policy (a new automatic name needs the
    /// confirmed samples to agree), judged on every candidate (not only the
    /// returned ones). Always `false` without thresholds.
    pub likely: bool,
    /// The speaker is currently auto-named or suggested as this person.
    pub current_suggestion: bool,
}

/// The people closest to the voice of speaker `speaker_id` as shown in
/// `meeting` (a grouped member is shown as, and matched for, its group's
/// kept speaker), best first, at most `limit`, in [`identity::rank`] order
/// (the order [`identity::resolve_voice`] picks from). An unconfirmed
/// speaker is matched against the pool [`resolve_meeting`] uses; a
/// confirmed one against everyone's samples except those from this
/// meeting, so it never matches its own samples. People in its
/// [`effective_rejections`] (its own and, for a group, every member's) are
/// left out, exactly as resolution leaves them out; no centroid of `model`
/// → empty. Scores are raw similarities, never probabilities.
pub fn voice_matches(
    people: &PeopleSamples,
    meeting: &Meeting,
    speaker_id: SpeakerId,
    roster: Option<&BTreeSet<PersonId>>,
    thresholds: Option<&Thresholds>,
    model: &str,
    limit: usize,
) -> Result<Vec<VoiceMatch>> {
    if meeting.speaker(speaker_id).is_none() {
        return Err(speaker_not_found(&meeting.id, speaker_id));
    }
    let shown = effective_speaker(meeting, speaker_id);
    let speaker = meeting
        .speaker(shown)
        .ok_or_else(|| speaker_not_found(&meeting.id, shown))?;
    let Some(c) = speaker.centroid.as_ref().filter(|c| c.model == model) else {
        return Ok(Vec::new());
    };
    let Some(centroid) = identity::usable_vector(&c.vector_b64, c.dim)? else {
        return Ok(Vec::new());
    };
    let confirmed = speaker.assignment == Assignment::Confirmed;
    let benchmark;
    let pool: std::borrow::Cow<'_, [PersonSamples]> = if confirmed {
        benchmark = people.for_benchmark(&meeting.id);
        std::borrow::Cow::Borrowed(benchmark.all())
    } else {
        people.for_meeting(&meeting.id)
    };
    let rejected = effective_rejections(meeting, shown);
    let ranked = identity::rank(&centroid, &pool, &rejected, roster, thresholds);
    let likely = identity::resolve_voice(
        &centroid,
        &pool,
        &rejected,
        thresholds,
        roster,
        auto_person(speaker),
    )
    .assignment
        == Assignment::Auto;
    let current = SpeakerState::of(speaker);
    let current = (!confirmed).then_some(current.person_id).flatten();
    Ok(ranked
        .into_iter()
        .take(limit)
        .enumerate()
        .map(|(i, r)| VoiceMatch {
            current_suggestion: current.as_ref() == Some(&r.person_id),
            person_id: r.person_id,
            score: r.score,
            likely: i == 0 && likely,
        })
        .collect())
}

/// The weak sample an auto-name adds: the centroid and its exact contributing
/// clean-turn ranges (deterministic ID). `None` without a centroid or
/// speaker, or when the centroid is not a finite vector (it never enrolls).
pub fn weak_sample(
    meeting: &Meeting,
    speaker: SpeakerId,
    person: &PersonId,
) -> Result<Option<Voiceprint>> {
    let Some(c) = meeting.speaker(speaker).and_then(|s| s.centroid.as_ref()) else {
        return Ok(None);
    };
    let Some(vector) = identity::usable_vector(&c.vector_b64, c.dim)? else {
        return Ok(None);
    };
    let (start, end) = c
        .ranges
        .iter()
        .fold(None::<(f64, f64)>, |span, &[a, b]| {
            Some(match span {
                None => (f64::from(a), f64::from(b)),
                Some((start, end)) => (start.min(f64::from(a)), end.max(f64::from(b))),
            })
        })
        .unwrap_or_else(|| speaker_span(meeting, speaker));
    let mut sample = new_voiceprint(
        person,
        &c.model,
        meeting,
        &vector,
        start,
        end,
        VoiceprintKind::Auto,
        Strength::Weak,
    );
    sample.source.ranges = c.ranges.clone();
    sample.source.set_speaker(meeting, speaker);
    Ok(Some(sample))
}

/// Applies the weak-sample side of `changes` on `meeting`: a speaker that
/// became `auto` for a person adds its [`weak_sample`] to that person; one
/// that stopped being `auto` for a person removes it (weak only). Keeps at
/// most [`MAX_WEAK`] weak samples per person and model (newest). Grouped
/// members are frozen: their samples are neither added nor removed.
pub fn sync_weak_samples(
    tx: &mut WriteTx<'_>,
    meeting: &Meeting,
    changes: &[SpeakerChange],
) -> Result<()> {
    let mut edits = WeakEdits::default();
    let mut changed = BTreeSet::new();
    let user = UserSpeakerEvidence::of(meeting);
    for ch in changes
        .iter()
        .filter(|c| c.meeting_id == meeting.id && !is_grouped_member(meeting, c.speaker_id))
    {
        edits.track(meeting, ch.speaker_id, &ch.before, &ch.after)?;
        if ch.before.auto_person() != ch.after.auto_person() {
            changed.insert(ch.speaker_id);
            if let Some(person) = ch.before.auto_person() {
                if user
                    .conflict(meeting, ch.speaker_id, Some(person))
                    .is_some()
                {
                    drop_conflicting_automatic_samples(tx, person, meeting, ch.speaker_id)?;
                }
                drop_meeting_samples(tx, person, meeting, ch.speaker_id, true)?;
            }
        }
    }
    // Reconciliation also repairs an interrupted older finish that saved
    // the assignment before its weak sample. Deterministic IDs make retries
    // harmless even when there is no new assignment change. Such a re-add is
    // dated by the meeting, never now: a sample the cap already evicted as
    // the oldest must not come back as the newest.
    for speaker in effective_speakers(meeting) {
        if speaker.assignment == Assignment::Auto
            && !changed.contains(&speaker.id)
            && let Some(person) = &speaker.person_id
            && let Some(mut sample) = weak_sample(meeting, speaker.id, person)?
        {
            sample.created_at = meeting.started_at;
            edits.add.entry(person.clone()).or_default().push(sample);
        }
    }
    edits.commit(tx)
}

fn drop_conflicting_automatic_samples(
    tx: &mut WriteTx<'_>,
    person: &PersonId,
    meeting: &Meeting,
    speaker: SpeakerId,
) -> Result<()> {
    let mut prints = tx.library().load_voiceprints(person)?;
    let auto_id = weak_sample(meeting, speaker, person)?.map(|print| print.id);
    let mut models = BTreeSet::new();
    prints.embeddings.retain(|print| {
        let conflicts = matches!(
            print.source.kind,
            VoiceprintKind::Auto | VoiceprintKind::Captions
        ) && sample_of_speaker(print, meeting, speaker, auto_id.as_ref());
        if conflicts {
            models.insert(print.model.clone());
        }
        !conflicts
    });
    if !models.is_empty() {
        for model in models {
            learning::request(tx, &model, Some(&meeting.id))?;
        }
        tx.save_voiceprints(&prints)?;
    }
    Ok(())
}

#[derive(Default)]
struct WeakEdits {
    add: BTreeMap<PersonId, Vec<Voiceprint>>,
    remove: BTreeMap<PersonId, BTreeSet<String>>,
}

impl WeakEdits {
    fn track(
        &mut self,
        meeting: &Meeting,
        speaker: SpeakerId,
        before: &SpeakerState,
        after: &SpeakerState,
    ) -> Result<()> {
        let (b, a) = (before.auto_person(), after.auto_person());
        if b == a {
            return Ok(());
        }
        if let Some(p) = b
            && let Some(vp) = weak_sample(meeting, speaker, p)?
        {
            self.remove.entry(p.clone()).or_default().insert(vp.id);
        }
        if let Some(p) = a
            && let Some(vp) = weak_sample(meeting, speaker, p)?
        {
            self.add.entry(p.clone()).or_default().push(vp);
        }
        Ok(())
    }

    fn commit(self, tx: &mut WriteTx<'_>) -> Result<()> {
        let lib = tx.library().clone();
        let people: BTreeSet<&PersonId> = self.add.keys().chain(self.remove.keys()).collect();
        for p in people {
            // A forgotten person gets no samples back.
            if !lib.person_dir(p).join(paths::PERSON_MD).is_file() {
                continue;
            }
            let mut vps = lib.load_voiceprints(p)?;
            let before = vps.clone();
            if let Some(ids) = self.remove.get(p) {
                vps.embeddings
                    .retain(|e| !(e.strength == Strength::Weak && ids.contains(&e.id)));
            }
            if let Some(adds) = self.add.get(p) {
                let models: BTreeSet<String> = adds.iter().map(|v| v.model.clone()).collect();
                let known: BTreeSet<String> = vps.embeddings.iter().map(|v| v.id.clone()).collect();
                vps.upsert(
                    adds.iter()
                        .filter(|v| !known.contains(&v.id))
                        .cloned()
                        .collect(),
                );
                for m in &models {
                    cap_voiceprints(&mut vps, m)?;
                }
            }
            if vps != before {
                tx.save_voiceprints(&vps)?;
            }
        }
        Ok(())
    }
}

// ---- operations ----

/// Names a speaker (assignment `confirmed`) as the person `person_ref` names
/// (ID, e-mail or name; a new name creates the person).
///
/// Adds confirmed voiceprints (kind `confirmation`) from `samples` computed
/// with `embedder_model`; without samples the speaker's centroid is used, and
/// without either no voiceprint is added. Clears the person from the
/// speaker's rejections and promotes only weak samples attributable to this
/// speaker.
///
/// Before changing any evidence it records, in this write, a reevaluation
/// request for `embedder_model` with this meeting first
/// ([`learning::request`]): the queue's worker then reassesses the other
/// unconfirmed speakers, here and in every other `done` meeting, against
/// all people. Naming itself never scans the library.
///
/// Speaker groups (design §6.4): a grouped member names its group's kept
/// speaker instead (reported in `redirected_from`; `samples` must come from
/// the kept speaker's audio, as `confirmation_samples` cuts them). In the
/// same write, every confirmed member is named the same person (its own
/// samples of its previous person are dropped; it learns nothing new), and
/// a member that rejected the person, or that the meeting's captions named
/// as someone else, is separated from the group with its identity
/// untouched, remembered in `not_same` with every speaker of the group.
pub fn name_speaker(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    speaker_id: SpeakerId,
    person_ref: &str,
    samples: Vec<ConfirmationSample>,
    embedder_model: &str,
) -> Result<NameOutcome> {
    let lib = tx.library().clone();
    let mut meeting = lib.load_meeting(meeting_id)?;
    if meeting.speaker(speaker_id).is_none() {
        return Err(speaker_not_found(meeting_id, speaker_id));
    }
    let requested = speaker_id;
    let speaker_id = effective_speaker(&meeting, requested);
    let redirected_from = (speaker_id != requested).then_some(requested);
    let Some(speaker) = meeting.speaker(speaker_id) else {
        return Err(speaker_not_found(meeting_id, speaker_id));
    };
    validate_samples(&samples)?;
    // Samples taken before a reanalysis may come from another voice now
    // carrying this speaker ID: refuse before changing anything.
    if samples.iter().any(|s| {
        s.namespace
            .as_deref()
            .is_some_and(|ns| ns != meeting.speaker_namespace)
    }) {
        return Err(Error::InvalidInput(format!(
            "meeting {meeting_id} was reanalysed while speaker {speaker_id} was being sampled; retry"
        )));
    }

    let (mut person, created) = resolve_or_new_person(&lib, person_ref)?;
    if let Some(edit) = &mut tx.review_edit {
        edit.person(&lib, &person.id)?;
    }
    // Recorded before any evidence changes: a crash after it costs a
    // harmless extra pass, never a lost update.
    let learning = learning::request(tx, embedder_model, Some(meeting_id))?;
    let pid = person.id.clone();
    let evidence = UserSpeakerEvidence::of(&meeting);
    let conflicting_lines = evidence
        .raw_lines(speaker_id)
        .iter()
        .any(|line| line.person_id != pid);
    let samples: Vec<_> = samples
        .into_iter()
        .filter(|sample| {
            evidence.allows_sample(
                speaker_id,
                &pid,
                f64::from(sample.start),
                f64::from(sample.end),
            )
        })
        .collect();
    let review_cleared = person.needs_review;
    person.needs_review = false;
    if created || review_cleared {
        tx.save_person(&person)?;
        index.upsert_person(tx, &person)?;
    }

    let previous = speaker.named_person().filter(|q| **q != pid).cloned();
    let regroup = name_group(&lib, &meeting, speaker_id, &pid)?;

    // Voiceprints from the samples, else from the centroid.
    let mut prints = Vec::new();
    if !samples.is_empty() {
        for s in &samples {
            let mut print = new_voiceprint(
                &pid,
                embedder_model,
                &meeting,
                &s.vector,
                f64::from(s.start),
                f64::from(s.end),
                VoiceprintKind::Confirmation,
                Strength::Confirmed,
            );
            print.source.set_speaker(&meeting, speaker_id);
            prints.push(print);
        }
    } else if !conflicting_lines
        && let Some(c) = speaker.centroid.as_ref()
        && let Some(mut sample) = weak_sample(&meeting, speaker_id, &pid)?
    {
        sample.source.kind = VoiceprintKind::Confirmation;
        sample.strength = Strength::Confirmed;
        sample.id = schema::voiceprint_id(
            &pid,
            &c.model,
            schema::VoiceprintOrigin::of_meeting(&meeting),
            sample.source.start,
            sample.source.end,
            VoiceprintKind::Confirmation,
        );
        prints.push(sample);
    }

    let s = meeting
        .speaker_mut(speaker_id)
        .ok_or_else(|| speaker_not_found(meeting_id, speaker_id))?;
    s.person_id = Some(pid.clone());
    s.assignment = Assignment::Confirmed;
    s.score = None;
    s.suggestion = None;
    s.live_suggestion_lines = None;
    s.live_promotion = None;
    s.live_speaker_override = true;
    s.identity_conflict = None;
    s.rejected_person_ids.retain(|r| *r != pid);
    for (m, _) in &regroup.renamed {
        let s = meeting
            .speaker_mut(*m)
            .ok_or_else(|| speaker_not_found(meeting_id, *m))?;
        s.person_id = Some(pid.clone());
        s.live_promotion = None;
        s.live_speaker_override = true;
        s.rejected_person_ids.retain(|r| *r != pid);
    }
    for sep in &regroup.separated {
        grouping::separate_member(&mut meeting, sep.speaker_id);
    }
    if !regroup.separated.is_empty() {
        meeting.set_generated_stale();
    }
    live_speaker::mark_group_override(&mut meeting, speaker_id);
    // Naming is the decision "Can't tell" postponed.
    review::clear_dismissal(&mut meeting, speaker_id);

    // Promote only samples attributable to the speaker being confirmed.
    let mut vps = lib.load_voiceprints(&pid)?;
    let before = vps.clone();
    let mut promoted = 0;
    let span = speaker_span(&meeting, speaker_id);
    let auto_id = weak_sample(&meeting, speaker_id, &pid)?
        .filter(|_| {
            !meeting
                .speakers
                .iter()
                .any(|s| s.id != speaker_id && speaker_span(&meeting, s.id) == span)
        })
        .map(|v| v.id);
    for e in &mut vps.embeddings {
        if e.strength == Strength::Weak
            && sample_of_speaker(e, &meeting, speaker_id, auto_id.as_ref())
            && e.source
                .audio_ranges()
                .all(|[a, b]| evidence.allows_sample(speaker_id, &pid, f64::from(a), f64::from(b)))
        {
            e.strength = Strength::Confirmed;
            promoted += 1;
        }
    }
    let voiceprint_added = !prints.is_empty();
    let models: BTreeSet<String> = vps
        .embeddings
        .iter()
        .chain(&prints)
        .map(|v| v.model.clone())
        .collect();
    vps.upsert(prints);
    for m in &models {
        cap_voiceprints(&mut vps, m)?;
    }
    if vps != before {
        tx.save_voiceprints(&vps)?;
    }

    // Correcting this speaker never leaves its voice on the previous person,
    // even when another speaker in this meeting still names that person.
    if let Some(q) = &previous {
        drop_meeting_samples(tx, q, &meeting, speaker_id, false)?;
    }
    for (m, q) in &regroup.renamed {
        drop_meeting_samples(tx, q, &meeting, *m, false)?;
    }

    meeting.updated_at = schema::now();
    refresh_identity_conflict_notes(&lib, &mut meeting)?;
    tx.save_meeting(&meeting)?;
    index.upsert_meeting(tx, &meeting)?;

    Ok(NameOutcome {
        person_id: pid,
        speaker_id,
        redirected_from,
        created,
        previous_person_id: previous,
        voiceprint_added,
        promoted,
        members_renamed: regroup.renamed.into_iter().map(|(m, _)| m).collect(),
        separated: regroup.separated,
        learning,
    })
}

/// What naming a group's kept speaker does to its members
/// ([`name_speaker`]).
struct NamedGroup {
    /// Confirmed members named as someone else, with that person.
    renamed: Vec<(SpeakerId, PersonId)>,
    separated: Vec<Separation>,
}

/// How naming `kept` as `person` reconciles its group: confirmed members
/// follow the name; a member that rejected `person`, or an unconfirmed one
/// the meeting's captions named as someone else, is separated.
fn name_group(
    lib: &Library,
    meeting: &Meeting,
    kept: SpeakerId,
    person: &PersonId,
) -> Result<NamedGroup> {
    let mut out = NamedGroup {
        renamed: Vec::new(),
        separated: Vec::new(),
    };
    let Some(group) = meeting.speaker_groups.iter().find(|g| g.kept == kept) else {
        return Ok(out);
    };
    let proven = caption_proven(lib, meeting)?;
    for &m in &group.members {
        let Some(s) = meeting.speaker(m) else {
            continue;
        };
        if s.rejected_person_ids.contains(person) {
            out.separated.push(Separation {
                speaker_id: m,
                kept,
                reason: SeparationReason::Rejected,
                person_id: person.clone(),
                message: format!("{m} was marked not to be {person}; it was separated"),
            });
        } else if s.assignment == Assignment::Confirmed {
            if let Some(q) = s.person_id.as_ref().filter(|q| *q != person) {
                out.renamed.push((m, q.clone()));
            }
        } else if let Some(p) = proven.get(&m).filter(|p| *p != person) {
            out.separated.push(Separation {
                speaker_id: m,
                kept,
                reason: SeparationReason::Captions,
                person_id: p.clone(),
                message: format!(
                    "{m} was named {p} by the meeting captions; it was separated — name it \
                     separately if the captions are wrong"
                ),
            });
        }
    }
    Ok(out)
}

/// Rejects a person for a speaker: adds them to `rejected_person_ids` and,
/// when the speaker was suggested or auto-named as them, re-resolves it
/// (centroid of `model`) without them. `person_ref` defaults to the current
/// suggestion or auto-name. Rejecting the person a speaker is confirmed as
/// is `invalid_input` (name the speaker instead).
///
/// Speaker groups (design §6.4): a grouped member's rejection applies to
/// its group's kept speaker (reported in `redirected_from`). Rejecting, on
/// a group, the person one of its members is confirmed as is refused
/// (unmerge first); a member the meeting's captions named as that person is
/// separated from the group with its identity untouched.
#[allow(clippy::too_many_arguments)]
pub fn reject_suggestion(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    speaker_id: SpeakerId,
    person_ref: Option<&str>,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<RejectOutcome> {
    let lib = tx.library().clone();
    let mut meeting = lib.load_meeting(meeting_id)?;
    if meeting.speaker(speaker_id).is_none() {
        return Err(speaker_not_found(meeting_id, speaker_id));
    }
    let requested = speaker_id;
    let speaker_id = effective_speaker(&meeting, requested);
    let redirected_from = (speaker_id != requested).then_some(requested);
    let speaker = meeting
        .speaker(speaker_id)
        .ok_or_else(|| speaker_not_found(meeting_id, speaker_id))?;
    let current = SpeakerState::of(speaker);
    let rejected = match person_ref {
        Some(r) => match find_person(&lib, r)? {
            Some(p) => p.id,
            None => return Err(Error::NotFound(format!("person {}", r.trim()))),
        },
        None => match (&current.assignment, &current.person_id) {
            (Assignment::Auto | Assignment::Suggested, Some(p)) => p.clone(),
            _ => {
                return Err(Error::InvalidInput(format!(
                    "{meeting_id} {speaker_id} has no suggestion to reject"
                )));
            }
        },
    };
    if current.assignment == Assignment::Confirmed && current.person_id.as_ref() == Some(&rejected)
    {
        return Err(Error::InvalidInput(format!(
            "{meeting_id} {speaker_id} is confirmed as {rejected}; name it as someone else instead"
        )));
    }
    let mut separated = Vec::new();
    if let Some(group) = meeting.speaker_groups.iter().find(|g| g.kept == speaker_id) {
        let proven = caption_proven(&lib, &meeting)?;
        for &m in &group.members {
            let Some(s) = meeting.speaker(m) else {
                continue;
            };
            if s.assignment == Assignment::Confirmed && s.person_id.as_ref() == Some(&rejected) {
                return Err(Error::InvalidInput(format!(
                    "{m} is confirmed as {rejected} and merged into {speaker_id}; unmerge it first"
                )));
            }
            if proven.get(&m) == Some(&rejected) {
                separated.push(Separation {
                    speaker_id: m,
                    kept: speaker_id,
                    reason: SeparationReason::Captions,
                    person_id: rejected.clone(),
                    message: format!(
                        "{m} was named {rejected} by the meeting captions; it was separated — \
                         name it separately if the captions are wrong"
                    ),
                });
            }
        }
    }
    for sep in &separated {
        grouping::separate_member(&mut meeting, sep.speaker_id);
    }
    if !separated.is_empty() {
        meeting.set_generated_stale();
    }

    let people = load_person_samples(&lib, model)?;
    let roster = meeting.roster_person_ids();
    let mut rejections = effective_rejections(&meeting, speaker_id);
    if !rejections.contains(&rejected) {
        rejections.push(rejected.clone());
    }
    let s = meeting
        .speaker_mut(speaker_id)
        .ok_or_else(|| speaker_not_found(meeting_id, speaker_id))?;
    let mut changed = !separated.is_empty();
    if !s.rejected_person_ids.contains(&rejected) {
        s.rejected_person_ids.push(rejected.clone());
        changed = true;
    }
    let mut changes = Vec::new();
    let points_at_rejected = current.assignment != Assignment::Confirmed
        && current.person_id.as_ref() == Some(&rejected);
    if points_at_rejected {
        let centroid = match s.centroid.as_ref().filter(|c| c.model == model) {
            Some(c) => identity::usable_vector(&c.vector_b64, c.dim)?,
            None => None,
        };
        let res = match centroid {
            // It pointed at the rejected person: any name it gets is new.
            Some(c) => identity::resolve_voice(
                &c,
                &people.for_meeting(meeting_id),
                &rejections,
                thresholds,
                Some(&roster),
                None,
            ),
            None => Resolution {
                assignment: Assignment::None,
                person_id: None,
                score: None,
                runner_up_score: None,
            },
        };
        let evidence = UserSpeakerEvidence::of(&meeting);
        apply_guarded_resolution(&mut meeting, speaker_id, &evidence, &res);
        let s = meeting.speaker(speaker_id).expect("existing speaker");
        changed = true;
        changes.push(SpeakerChange {
            meeting_id: meeting_id.clone(),
            speaker_id,
            before: current,
            after: SpeakerState::of(s),
        });
    }
    let after = SpeakerState::of(meeting.speaker(speaker_id).expect("existing speaker"));
    if changed {
        refresh_identity_conflict_notes(&lib, &mut meeting)?;
        save_and_index(tx, index, &mut meeting)?;
        drop_meeting_samples(tx, &rejected, &meeting, speaker_id, true)?;
        sync_weak_samples(tx, &meeting, &changes)?;
    }
    Ok(RejectOutcome {
        rejected,
        speaker_id,
        redirected_from,
        after,
        separated,
    })
}

/// Unresolved speakers with canonical evidence, minus those dismissed from
/// review ([`review_dismissed`]). `limit` bounds primary naming rows only;
/// quality rows stay enumerated separately by callers.
pub fn review_queue(lib: &Library, limit: Option<usize>) -> Result<Vec<ReviewEntry>> {
    let items = Index::open_readonly(lib.root())?.review_queue(None)?;
    if items.is_empty() {
        return Ok(Vec::new());
    }
    // A folder listing that fails stays each row's sample error, as each
    // row's own load failing would.
    let lookup = lib.meeting_lookup().map_err(|e| e.to_string());
    let mut meetings: HashMap<MeetingId, std::result::Result<(PathBuf, Meeting), String>> =
        HashMap::new();
    let mut out = Vec::with_capacity(items.len());
    let mut primary = 0;
    let mut names = None;
    for mut item in items {
        let meeting = meetings.entry(item.meeting_id.clone()).or_insert_with(|| {
            lookup
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|l| l.find(&item.meeting_id).map_err(|e| e.to_string()))
        });
        if meeting.as_ref().is_ok_and(|(_, m)| {
            is_grouped_member(m, item.speaker_id) || review_dismissed(m, item.speaker_id)
        }) {
            continue;
        }
        let without_transcript = meeting
            .as_ref()
            .is_ok_and(|(dir, m)| review_without_transcript(dir, m, item.speaker_id));
        if !without_transcript {
            if limit.is_some_and(|limit| primary >= limit) {
                continue;
            }
            primary += 1;
        }
        if without_transcript {
            // Quality references describe this canonical cluster, not old
            // indexed identity evidence after a reanalysis.
            let (_, m) = meeting.as_ref().expect("classification requires a meeting");
            let speaker = m
                .speaker(item.speaker_id)
                .expect("classification requires a speaker");
            item.meeting_title.clone_from(&m.title);
            item.started_at = m.started_at;
            item.talk_s = effective_talk_s(m, item.speaker_id);
            item.assignment = speaker.assignment;
            item.person_id.clone_from(&speaker.person_id);
            item.person_name = None;
            item.score = speaker.score;
            item.suggested_person_id = speaker.suggestion.as_ref().map(|s| s.person_id.clone());
            item.suggested_name = None;
            item.suggested_score = speaker.suggestion.as_ref().map(|s| s.score);
        }
        let (sample_text, sample_text_error) = match meeting {
            Ok((_, m)) => (
                m.segments
                    .iter()
                    .filter(|s| {
                        s.speaker.map(|id| effective_speaker(m, id)) == Some(item.speaker_id)
                    })
                    .take(REVIEW_SAMPLE_SEGMENTS)
                    .map(|s| s.text.clone())
                    .collect(),
                None,
            ),
            Err(e) => (Vec::new(), Some(e.clone())),
        };
        let speaker = meeting
            .as_ref()
            .ok()
            .and_then(|(_, meeting)| meeting.speaker(item.speaker_id));
        let live_suggestion_lines = speaker.and_then(|speaker| speaker.live_suggestion_lines);
        let identity_conflict = if let Some(conflict) =
            speaker.and_then(|speaker| speaker.identity_conflict.as_ref())
        {
            if names.is_none() {
                names = Some(person_names(lib)?);
            }
            Some(conflict.message(item.speaker_id, names.as_ref().expect("loaded above")))
        } else {
            None
        };
        out.push(ReviewEntry {
            item,
            sample_text,
            sample_text_error,
            without_transcript,
            live_suggestion_lines,
            identity_conflict,
        });
    }
    Ok(out)
}

fn review_without_transcript(dir: &Path, meeting: &Meeting, id: SpeakerId) -> bool {
    meeting.processing.status == ProcessingStatus::Done
        && meeting.speaker(id).is_some_and(|speaker| {
            matches!(speaker.assignment, Assignment::None | Assignment::Suggested)
        })
        && first_speaker_segment(meeting, id).is_none()
        && (review::diarized_sample_ranges(meeting, id).next().is_none()
            || !review_recording_available(dir, meeting))
}

fn review_recording_available(dir: &Path, meeting: &Meeting) -> bool {
    let Some(recording) = &meeting.recording else {
        return false;
    };
    if recording.state != RecordingState::Available {
        return false;
    }
    // Match the meeting view: an uncertain metadata read is not proof the
    // file is missing. Multi-source recordings use the external sources.
    if recording.sources.is_empty() {
        dir.join(&recording.path).try_exists().unwrap_or(true)
    } else {
        recording
            .sources
            .iter()
            .all(|source| source.path.try_exists().unwrap_or(true))
    }
}

/// Primary naming count (dismissed speakers excluded) without allocating or
/// copying sample text. Call on a background executor: canonical evidence
/// is read once per candidate meeting.
pub fn review_naming_count(lib: &Library) -> Result<usize> {
    Ok(review_naming_counts(lib)?.values().sum())
}

/// The same primary naming count, grouped by meeting for Library rows.
pub fn review_naming_counts(lib: &Library) -> Result<BTreeMap<MeetingId, usize>> {
    let items = Index::open_readonly(lib.root())?.review_queue(None)?;
    if items.is_empty() {
        return Ok(BTreeMap::new());
    }
    // A failed folder listing counts every row, as failed loads do.
    let lookup = lib.meeting_lookup().map_err(|e| e.to_string());
    let mut meetings = HashMap::new();
    let mut counts = BTreeMap::new();
    for item in items {
        let meeting = meetings.entry(item.meeting_id.clone()).or_insert_with(|| {
            lookup
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|l| l.find(&item.meeting_id).map_err(|e| e.to_string()))
        });
        if !meeting.as_ref().is_ok_and(|(dir, m)| {
            is_grouped_member(m, item.speaker_id)
                || review_dismissed(m, item.speaker_id)
                || review_without_transcript(dir, m, item.speaker_id)
        }) {
            *counts.entry(item.meeting_id).or_default() += 1;
        }
    }
    Ok(counts)
}

/// Forgets a person's voice: deletes `voiceprints.json` and `enrollment/`,
/// clears the centroid of every speaker named as them (names stay), clears
/// suggestions of them, and removes their entries from `.queue/*/hints.json`.
pub fn forget_voice(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    person_ref: &str,
) -> Result<ForgetOutcome> {
    let (id, person) = forget_subject(tx.library(), person_ref, false)?;
    forget(tx, index, &id, person.as_ref(), false)
}

/// Forgets a person: [`forget_voice`], then their speakers become anonymous
/// (no person, assignment `none`, no suggestion), they leave every
/// `rejected_person_ids` and participant link, their line corrections and
/// enrichment line references are removed (those references then render
/// anonymously), and `people/<id>/` is deleted — with it the edit Undo
/// history ([`PERSON_UNDO_FILE`]), so nothing can bring the person back.
/// Neither forget has an Undo (design §4 People).
/// A person ID works even when `person.md` is already gone.
pub fn forget_person(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    person_ref: &str,
) -> Result<ForgetOutcome> {
    let (id, person) = forget_subject(tx.library(), person_ref, true)?;
    forget(tx, index, &id, person.as_ref(), true)
}

fn forget_subject(
    lib: &Library,
    person_ref: &str,
    anonymise: bool,
) -> Result<(PersonId, Option<Person>)> {
    // Full forget can finish cleanup after the profile is gone or unreadable.
    if anonymise && let Ok(id) = PersonId::parse(person_ref.trim()) {
        let person = lib.load_person(&id).ok();
        return Ok((id, person));
    }
    let person = find_person(lib, person_ref)?
        .ok_or_else(|| Error::NotFound(format!("person {}", person_ref.trim())))?;
    Ok((person.id.clone(), Some(person)))
}

/// Folder (under the library root) of forget tombstones: one file per folded
/// person key, named by a hash of the key, holding the RFC 3339 time it was
/// forgotten followed by a random nonce of that forget (so every forget
/// changes the bytes, even at an equal or earlier clock reading; tombstones
/// written before the nonce hold the time only). Names and aliases also
/// leave their [`names::folded_name`] key. Not a person ID, so
/// [`Library::list_people`] skips it.
pub const FORGOTTEN_DIR: &str = "people/.forgotten";

pub(crate) fn tombstone_path(root: &Path, key: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(fold(key).as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    root.join(FORGOTTEN_DIR).join(hex)
}

/// When a person known by `key` (ID, e-mail, name, alias or `self`; folded)
/// was last forgotten; `None` when never (or the tombstone is unreadable).
pub fn forgotten_since(lib: &Library, key: &str) -> Result<Option<DateTime<FixedOffset>>> {
    let path = tombstone_path(lib.root(), key);
    match fs::read_to_string(&path) {
        Ok(s) => Ok(DateTime::parse_from_rfc3339(s.split_whitespace().next().unwrap_or("")).ok()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io_at(&path, e)),
    }
}

/// When a person whose name or alias reads as `name` under caption
/// normalisation (guest/external suffixes, name order, spacing, case,
/// accents) was last forgotten.
pub fn name_forgotten_since(lib: &Library, name: &str) -> Result<Option<DateTime<FixedOffset>>> {
    let mut latest = None;
    // The literal and normalised keys also match tombstones that predate
    // the folded name keys.
    for key in [
        fold(name),
        fold(&names::normalize_name(name)),
        names::folded_name(name),
    ] {
        latest = latest.max(forgotten_since(lib, &key)?);
    }
    Ok(latest)
}

fn write_tombstones(root: &Path, keys: &HintKeys) -> Result<()> {
    let dir = root.join(FORGOTTEN_DIR);
    fsutil::create_dir_durable(&dir)?;
    // One nonce per forget, the same for all its keys.
    let at = format!(
        "{} {:032x}\n",
        schema::now().to_rfc3339(),
        rand::random::<u128>()
    );
    let folded: BTreeSet<_> = keys.names.iter().map(|n| names::folded_name(n)).collect();
    let all = std::iter::once(keys.id.as_str())
        .chain(keys.is_self.then_some("self"))
        .chain(keys.emails.iter().map(String::as_str))
        .chain(keys.names.iter().map(String::as_str))
        .chain(folded.iter().map(String::as_str).filter(|n| !n.is_empty()));
    for key in all {
        fsutil::write_atomic(&tombstone_path(root, key), at.as_bytes())?;
    }
    Ok(())
}

/// The facts shown before forgetting a person or their voice.
#[derive(Debug, Serialize)]
pub struct ForgetPreview {
    pub person_id: PersonId,
    pub is_self: bool,
    pub voice_samples: usize,
    pub kept_readings: usize,
    pub named_meetings: usize,
}

/// Uses the same subject resolution and meeting preflight as the write.
pub fn preview_forget(lib: &Library, reference: &str, anonymise: bool) -> Result<ForgetPreview> {
    let (id, person) = forget_subject(lib, reference, anonymise)?;
    let meetings = forget_meetings(lib, &id)?;
    let voice_samples = lib.load_voiceprints(&id)?.embeddings.len();
    let enrollment = lib.person_dir(&id).join("enrollment");
    let kept_readings = match fs::read_dir(&enrollment) {
        Ok(entries) => {
            let mut readings = 0;
            for entry in entries {
                let entry = entry.map_err(|e| Error::io_at(&enrollment, e))?;
                if entry.path().extension().is_some_and(|ext| ext == "wav") {
                    readings += 1;
                }
            }
            readings
        }
        Err(e) if e.kind() == ErrorKind::NotFound => 0,
        Err(e) => return Err(Error::io_at(&enrollment, e)),
    };
    let named_meetings = meetings
        .iter()
        .filter(|meeting| {
            effective_speakers(meeting).any(|speaker| speaker.named_person() == Some(&id))
        })
        .count();
    Ok(ForgetPreview {
        person_id: id,
        is_self: person.is_some_and(|person| person.is_self),
        voice_samples,
        kept_readings,
        named_meetings,
    })
}

fn forget_meetings(lib: &Library, id: &PersonId) -> Result<Vec<Meeting>> {
    person_merge::check_forget(lib, id)?;

    // Load and validate every meeting first: nothing changes unless all of
    // them can be read and rewritten.
    let mut meetings = Vec::new();
    let mut unreadable = Vec::new();
    for dir in lib.list_meeting_dirs()? {
        match lib.load_meeting_at(&dir) {
            Ok(m) => meetings.push(m),
            Err(e) => unreadable.push(format!("{}: {e}", dir.display())),
        }
    }
    if !unreadable.is_empty() {
        return Err(Error::InvalidInput(format!(
            "cannot forget {id}: unreadable meetings (nothing was changed): {}",
            unreadable.join("; ")
        )));
    }

    Ok(meetings)
}

fn forget(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    id: &PersonId,
    person: Option<&Person>,
    anonymise: bool,
) -> Result<ForgetOutcome> {
    let lib = tx.library().clone();
    let meetings = forget_meetings(&lib, id)?;

    // Every model the forgotten evidence could have taught, requested
    // before anything is removed: automatic names and suggestions that
    // relied on it are reassessed without it, and a pass already running
    // starts over ([`learning`]).
    let mut models: BTreeSet<String> = match lib.load_voiceprints(id) {
        Ok(vps) => vps.embeddings.into_iter().map(|v| v.model).collect(),
        Err(_) => BTreeSet::new(),
    };
    for s in meetings.iter().flat_map(|m| &m.speakers) {
        let names_them = s.person_id.as_ref() == Some(id)
            || s.suggestion.as_ref().is_some_and(|x| x.person_id == *id);
        if let Some(c) = s.centroid.as_ref().filter(|_| names_them) {
            models.insert(c.model.clone());
        }
    }
    let learning = learning::request_all(tx, models)?;

    let keys = HintKeys::new(id, person);
    // Who derived notes may still name: everyone but the forgotten person.
    let remaining_names = if anonymise {
        let mut names = person_names(&lib)?;
        names.remove(id);
        names
    } else {
        BTreeMap::new()
    };
    write_tombstones(tx.root(), &keys)?;
    identity_checks::forget_identity_checks(tx, id)?;
    let mut out = ForgetOutcome {
        person_id: id.clone(),
        voiceprints_deleted: false,
        person_deleted: false,
        meetings_changed: Vec::new(),
        speakers_changed: 0,
        hints_removed: scrub_hints(tx.root(), &keys)?,
        learning,
    };

    let vp_path = lib.person_dir(id).join(paths::VOICEPRINTS_JSON);
    out.voiceprints_deleted = match fs::remove_file(&vp_path) {
        Ok(()) => true,
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(Error::io_at(&vp_path, e)),
    };
    let enrollment = lib.person_dir(id).join("enrollment");
    match fs::remove_dir_all(&enrollment) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io_at(&enrollment, e)),
    }

    for mut meeting in meetings {
        let before = meeting.clone();
        live_speaker::forget_live_promotion(&mut meeting, id);
        for s in &mut meeting.speakers {
            if s.person_id.as_ref() == Some(id) {
                s.centroid = None;
                if anonymise {
                    s.person_id = None;
                    s.assignment = Assignment::None;
                    s.score = None;
                    s.suggestion = None;
                }
            }
            if s.suggestion.as_ref().is_some_and(|x| x.person_id == *id) {
                s.suggestion = None;
                if s.assignment == Assignment::Suggested {
                    s.assignment = Assignment::None;
                    s.score = None;
                }
            }
            if anonymise {
                s.rejected_person_ids.retain(|r| r != id);
                if s.identity_conflict.as_ref().is_some_and(|conflict| {
                    conflict.candidate.as_ref() == Some(id)
                        || conflict
                            .unlinked_caption
                            .as_deref()
                            .is_some_and(|caption| keys.key_matches(caption))
                }) {
                    s.identity_conflict = None;
                }
            }
        }
        if anonymise {
            for p in &mut meeting.participants {
                if p.person_id.as_ref() == Some(id) {
                    p.person_id = None;
                }
            }
            // Their line corrections and line labels go; generated content
            // (enrichment, template results) that named them that way no
            // longer matches the transcript.
            let lines = meeting.utterance_people.len();
            meeting.utterance_people.retain(|u| u.person_id != *id);
            let lines_changed = meeting.utterance_people.len() != lines;
            for g in meeting.generated_mut() {
                let dropped = forget_line_people(g, |p| p == id);
                if lines_changed || dropped {
                    g.set_stale(true);
                }
            }
            // Its live records read as forgotten; the LivePreview notes are
            // derived again, so none keeps the forgotten name.
            if crate::live_identity::forget_person_in_meeting(&mut meeting, id) {
                crate::live_identity::refresh_review_notes(&mut meeting, &remaining_names);
            }
            guard_automatic_names(&mut meeting);
            refresh_identity_conflict_notes(&lib, &mut meeting)?;
        }
        out.speakers_changed += before
            .speakers
            .iter()
            .zip(&meeting.speakers)
            .filter(|(before, after)| before != after)
            .count();
        if meeting != before {
            save_and_index(tx, index, &mut meeting)?;
            out.meetings_changed.push(meeting.id);
        }
    }

    if anonymise {
        crate::live_identity::forget_person_in_sidecars(tx, id)?;
        person_merge::forget_exclusions(&lib, id)?;
        out.person_deleted = match tx.delete_person_dir(id) {
            Ok(()) => true,
            Err(Error::NotFound(_)) => false,
            Err(e) => return Err(e),
        };
        index.remove_person(tx, id)?;
    }
    Ok(out)
}

// ---- identity integrity (doctor / library check) ----

/// One person's stored samples that cannot take part in identity
/// ([`identity_check`]): counts only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UnusableSamples {
    pub person_id: PersonId,
    /// Vectors whose data does not hold `dim` values.
    pub undecodable: usize,
    /// Vectors that decode but are empty or hold a NaN or infinity.
    pub non_finite: usize,
}

/// What [`identity_check`] found. Privacy-safe: person and session IDs and
/// counts only, never names, vectors, text or paths.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct IdentityCheck {
    /// People whose voiceprints were read.
    pub people: usize,
    /// Stored samples read, usable or not.
    pub samples: usize,
    /// People with samples that never rank, merge or enroll; removable with
    /// [`repair_identity`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unusable: Vec<UnusableSamples>,
    /// People whose `voiceprints.json` cannot be read at all (left alone by
    /// the repair: their content is unknown).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable_voiceprints: Vec<PersonId>,
    /// Recording sessions whose live-decision sidecar cannot be read (left
    /// alone by the repair).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable_live_sidecars: Vec<String>,
}

impl IdentityCheck {
    /// Nothing to report.
    pub fn is_clean(&self) -> bool {
        self.unusable.is_empty()
            && self.unreadable_voiceprints.is_empty()
            && self.unreadable_live_sidecars.is_empty()
    }
}

/// Whether a stored sample is undecodable (`Some(false)`), well-formed but
/// not a usable finite vector (`Some(true)`) or usable (`None`).
fn unusable_sample(vp: &Voiceprint) -> Option<bool> {
    match identity::usable_vector(&vp.vector_b64, vp.dim) {
        Err(_) => Some(false),
        Ok(None) => Some(true),
        Ok(Some(_)) => None,
    }
}

/// Read-only integrity check of the identity evidence: every person's
/// stored samples (decodable, finite) and every live-decision sidecar
/// (readable). Changes nothing and creates nothing.
pub fn identity_check(lib: &Library) -> Result<IdentityCheck> {
    let mut out = IdentityCheck::default();
    for person in lib.list_people()? {
        let vps = match lib.load_voiceprints(&person.id) {
            Ok(vps) => vps,
            Err(_) => {
                out.unreadable_voiceprints.push(person.id);
                continue;
            }
        };
        out.people += 1;
        out.samples += vps.embeddings.len();
        let mut bad = UnusableSamples {
            person_id: person.id,
            undecodable: 0,
            non_finite: 0,
        };
        for vp in &vps.embeddings {
            match unusable_sample(vp) {
                Some(false) => bad.undecodable += 1,
                Some(true) => bad.non_finite += 1,
                None => {}
            }
        }
        if bad.undecodable + bad.non_finite > 0 {
            out.unusable.push(bad);
        }
    }
    for session in crate::live_identity::list_sidecars(lib.root())? {
        if crate::live_identity::read_sidecar(lib.root(), &session).is_err() {
            out.unreadable_live_sidecars.push(session);
        }
    }
    Ok(out)
}

/// Result of [`repair_identity`].
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct IdentityRepair {
    /// Unusable samples removed.
    pub samples_removed: usize,
    /// People whose voiceprints changed.
    pub people: Vec<PersonId>,
    /// Reevaluation requested for the models those samples used.
    pub learning: Vec<LearningRequest>,
}

/// The explicit repair of [`identity_check`]'s `unusable` samples, in one
/// write: removes every stored sample that does not decode or is not a
/// finite vector (they never ranked, merged or enrolled), after requesting
/// reevaluation of their models as every sample-changing write does.
/// Unreadable files and sidecars are never touched. Idempotent.
pub fn repair_identity(tx: &mut WriteTx<'_>) -> Result<IdentityRepair> {
    let lib = tx.library().clone();
    let mut edits: Vec<Voiceprints> = Vec::new();
    let mut models = BTreeSet::new();
    let mut out = IdentityRepair::default();
    for person in lib.list_people()? {
        let Ok(mut vps) = lib.load_voiceprints(&person.id) else {
            continue;
        };
        let before = vps.embeddings.len();
        vps.embeddings.retain(|vp| {
            let bad = unusable_sample(vp).is_some();
            if bad {
                models.insert(vp.model.clone());
            }
            !bad
        });
        if vps.embeddings.len() != before {
            out.samples_removed += before - vps.embeddings.len();
            out.people.push(person.id);
            edits.push(vps);
        }
    }
    if edits.is_empty() {
        return Ok(out);
    }
    out.learning = learning::request_all(tx, models)?;
    for vps in &edits {
        tx.save_voiceprints(vps)?;
    }
    Ok(out)
}

/// Adds and removes tags (case-insensitive; surrounding spaces trimmed,
/// empty tags ignored). Returns the meeting's tags afterwards.
pub fn tag(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    add: &[String],
    remove: &[String],
) -> Result<Vec<String>> {
    let mut meeting = tx.library().load_meeting(meeting_id)?;
    // Adds first (each clears a recorded removal), then removals (recorded in
    // `tags_removed`), so a tag both added and removed ends up removed.
    let mut changed = false;
    for t in add {
        changed |= meeting.add_tag(t);
    }
    for t in remove {
        changed |= meeting.remove_tag(t);
    }
    if changed {
        save_and_index(tx, index, &mut meeting)?;
    }
    Ok(meeting.effective_tags())
}

/// Sets an action item's status as the user's choice (`status_source =
/// user`, `status_updated_at = now`), which re-enrichment preserves. The
/// action is named by its ID (case-insensitive). Returns the action
/// afterwards; nothing is written when it already had this user-set status.
pub fn set_action_status(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    action_id: &str,
    status: schema::ActionStatus,
) -> Result<schema::Action> {
    let mut meeting = tx.library().load_meeting(meeting_id)?;
    let (enrichment, i) = find_action(&mut meeting, action_id)?;
    let action = &mut enrichment.actions[i];
    if action.status == status && action.status_source == schema::StatusSource::User {
        return Ok(action.clone());
    }
    action.status = status;
    action.status_source = schema::StatusSource::User;
    action.status_updated_at = Some(schema::now());
    let updated = action.clone();
    save_and_index(tx, index, &mut meeting)?;
    Ok(updated)
}

fn find_action<'a>(
    meeting: &'a mut Meeting,
    action_id: &str,
) -> Result<(&'a mut schema::Enrichment, usize)> {
    let wanted = action_id.trim();
    if let Some(enrichment) = &mut meeting.enrichment
        && let Some(i) = enrichment
            .actions
            .iter()
            .position(|a| a.id.eq_ignore_ascii_case(wanted))
    {
        return Ok((enrichment, i));
    }
    Err(Error::NotFound(format!(
        "{} has no action item {wanted}",
        meeting.id
    )))
}

/// Restores a missing reference after the caller verifies its source hashes
/// outside the library lock. The complete recording snapshot must still match,
/// so a concurrent reprocess or recording edit cannot be overwritten.
pub fn restore_recording_available(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    expected_recording: &schema::Recording,
) -> Result<bool> {
    let mut meeting = tx.library().load_meeting(meeting_id)?;
    if expected_recording.state != schema::RecordingState::Missing
        || meeting.recording.as_ref() != Some(expected_recording)
    {
        return Ok(false);
    }
    if let Some(recording) = &mut meeting.recording {
        recording.state = schema::RecordingState::Available;
    }
    save_and_index(tx, index, &mut meeting)?;
    Ok(true)
}

/// Refresh the saved availability without overwriting concurrent recording changes.
pub fn reconcile_recording(lib: &Library, id: &MeetingId) -> Result<Meeting> {
    let mut meeting = lib.load_meeting(id)?;
    if let Some(recording) = meeting.recording.as_ref()
        && recording.state == RecordingState::Missing
    {
        let matches = if recording.sources.is_empty() {
            // Owned recordings are stored relative to the meeting folder.
            let path = if recording.path.is_absolute() {
                recording.path.clone()
            } else {
                lib.meeting_dir(id)?.join(&recording.path)
            };
            file_matches(&path, &recording.sha256)?
        } else {
            let mut matches = true;
            for source in &recording.sources {
                matches &= file_matches(&source.path, &source.sha256)?;
            }
            matches
        };
        if matches {
            with_index(lib, |tx, index| {
                restore_recording_available(tx, index, id, recording)
            })?;
            meeting = lib.load_meeting(id)?;
        }
    }
    if meeting
        .recording
        .as_ref()
        .is_some_and(|r| r.state == RecordingState::Available)
        && (recording_path(lib, &meeting)?.is_some_and(|p| file_is_missing(&p))
            || meeting
                .recording
                .as_ref()
                .is_some_and(|r| r.sources.iter().any(|s| file_is_missing(&s.path))))
        && let Some(updated) = mark_recording_missing(lib, id)?
    {
        meeting = updated;
    }
    Ok(meeting)
}

fn file_is_missing(path: &Path) -> bool {
    matches!(std::fs::metadata(path), Err(e) if e.kind() == ErrorKind::NotFound)
}

pub fn expected_recording_path(lib: &Library, meeting: &Meeting) -> Result<Option<PathBuf>> {
    let Some(recording) = &meeting.recording else {
        return Ok(None);
    };
    if !recording.sources.is_empty() {
        let source = recording
            .sources
            .iter()
            .find(|source| file_is_missing(&source.path))
            .or_else(|| recording.sources.first());
        return Ok(source.map(|source| source.path.clone()));
    }
    if recording.path.is_absolute() {
        Ok(Some(recording.path.clone()))
    } else {
        Ok(Some(lib.meeting_dir(&meeting.id)?.join(&recording.path)))
    }
}

pub fn recording_path(lib: &Library, meeting: &Meeting) -> Result<Option<PathBuf>> {
    match meeting
        .recording
        .as_ref()
        .and_then(|recording| recording.sources.first())
    {
        Some(source) => Ok(Some(source.path.clone())),
        None => lib.meeting_audio_path(meeting),
    }
}

fn file_matches(path: &Path, expected: &str) -> Result<bool> {
    use sha2::{Digest as _, Sha256};
    use std::io::Read as _;
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::io_at(path, error)),
    };
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|e| Error::io_at(path, e))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    let digest = hash.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    Ok(expected.len() == 64
        && digest
            .iter()
            .zip(expected.as_bytes().as_chunks::<2>().0)
            .all(|(byte, pair)| *pair == [HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]))
}

/// Sets `recording.state = missing` when the file is still gone under the
/// lock; returns the saved meeting, or `None` when nothing changed.
pub fn mark_recording_missing(lib: &Library, id: &MeetingId) -> Result<Option<Meeting>> {
    with_index(lib, |tx, index| {
        let current = tx.library().load_meeting(id)?;
        let gone = match current.recording.as_ref() {
            Some(r) if r.state == RecordingState::Available => {
                if r.sources.is_empty() {
                    recording_path(tx.library(), &current)?.is_some_and(|p| file_is_missing(&p))
                } else {
                    r.sources.iter().any(|s| file_is_missing(&s.path))
                }
            }
            _ => false,
        };
        if !gone {
            return Ok(None);
        }
        let meeting = tx.update_meeting(id, |m| {
            if let Some(r) = m.recording.as_mut() {
                r.state = RecordingState::Missing;
            }
            Ok(())
        })?;
        index.upsert_meeting(tx, &meeting)?;
        Ok(Some(meeting))
    })
}

// ---- internals ----

fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}

fn speaker_not_found(meeting: &MeetingId, speaker: SpeakerId) -> Error {
    Error::NotFound(format!("speaker {speaker} in meeting {meeting}"))
}

fn save_and_index(tx: &mut WriteTx<'_>, index: &mut Index, meeting: &mut Meeting) -> Result<()> {
    meeting.updated_at = schema::now();
    tx.save_meeting(meeting)?;
    index.upsert_meeting(tx, meeting)
}

fn validate_samples(samples: &[ConfirmationSample]) -> Result<()> {
    let Some(first) = samples.first() else {
        return Ok(());
    };
    for s in samples {
        if s.vector.is_empty() || s.vector.len() != first.vector.len() {
            return Err(Error::InvalidInput(
                "confirmation samples must be non-empty vectors of one length".into(),
            ));
        }
        if s.vector.iter().any(|x| !x.is_finite()) || !s.start.is_finite() || !s.end.is_finite() {
            return Err(Error::InvalidInput(
                "confirmation sample is not finite".into(),
            ));
        }
        if s.start < 0.0 || s.end < s.start {
            return Err(Error::InvalidInput(format!(
                "confirmation sample range {}-{} is invalid",
                s.start, s.end
            )));
        }
    }
    Ok(())
}

/// First segment start to last segment end of the speaker; `(0, 0)` without segments.
fn speaker_span(meeting: &Meeting, speaker: SpeakerId) -> (f64, f64) {
    let mut span: Option<(f64, f64)> = None;
    for seg in meeting
        .segments
        .iter()
        .filter(|s| s.speaker == Some(speaker))
    {
        span = Some(match span {
            None => (seg.start, seg.end),
            Some((a, b)) => (a.min(seg.start), b.max(seg.end)),
        });
    }
    span.unwrap_or((0.0, 0.0))
}

#[allow(clippy::too_many_arguments)]
fn new_voiceprint(
    person: &PersonId,
    model: &str,
    meeting: &Meeting,
    vector: &[f32],
    start: f64,
    end: f64,
    kind: VoiceprintKind,
    strength: Strength,
) -> Voiceprint {
    let sha = meeting.recording.as_ref().map(|r| r.sha256.clone());
    let mut v = vector.to_vec();
    identity::l2_normalize(&mut v);
    Voiceprint {
        id: schema::voiceprint_id(
            person,
            model,
            schema::VoiceprintOrigin::of_meeting(meeting),
            start,
            end,
            kind,
        ),
        model: model.to_owned(),
        dim: v.len(),
        vector_b64: schema::encode_vector(&v),
        source: VoiceprintSource {
            kind,
            meeting_id: Some(meeting.id.clone()),
            sha256: sha,
            start,
            end,
            ranges: Vec::new(),
            speaker_id: None,
            speaker_namespace: String::new(),
        },
        device: None,
        strength,
        created_at: schema::now(),
    }
}

/// Keeps at most [`MAX_CONFIRMED`] confirmed (diverse) and [`MAX_WEAK`] weak
/// (newest) samples of `model`.
fn cap_voiceprints(vps: &mut Voiceprints, model: &str) -> Result<()> {
    let mut drop: BTreeSet<usize> = BTreeSet::new();
    let confirmed: Vec<usize> = (0..vps.embeddings.len())
        .filter(|&i| {
            let e = &vps.embeddings[i];
            e.model == model && e.strength == Strength::Confirmed
        })
        .collect();
    if confirmed.len() > MAX_CONFIRMED {
        // A non-finite sample reads as empty: capping never keeps it.
        let vectors = confirmed
            .iter()
            .map(|&i| {
                let e = &vps.embeddings[i];
                Ok(identity::usable_vector(&e.vector_b64, e.dim)?.unwrap_or_default())
            })
            .collect::<Result<Vec<_>>>()?;
        let keep: BTreeSet<usize> = identity::cap_samples(&vectors, MAX_CONFIRMED)
            .into_iter()
            .collect();
        drop.extend(
            confirmed
                .iter()
                .enumerate()
                .filter(|(k, _)| !keep.contains(k))
                .map(|(_, &i)| i),
        );
    }
    let mut weak: Vec<usize> = (0..vps.embeddings.len())
        .filter(|&i| {
            let e = &vps.embeddings[i];
            e.model == model && e.strength == Strength::Weak
        })
        .collect();
    weak.sort_by_key(|&i| vps.embeddings[i].created_at);
    let kept = identity::cap_weak(weak.len(), MAX_WEAK);
    drop.extend(weak[..kept.start].iter().copied());
    if !drop.is_empty() {
        let mut i = 0;
        vps.embeddings.retain(|_| {
            let keep = !drop.contains(&i);
            i += 1;
            keep
        });
    }
    Ok(())
}

/// Removes only this speaker's learned samples (of every embedding model),
/// preserving other speakers of the same person. Rejections remove weak
/// evidence only.
fn drop_meeting_samples(
    tx: &mut WriteTx<'_>,
    person: &PersonId,
    meeting: &Meeting,
    speaker: SpeakerId,
    weak_only: bool,
) -> Result<()> {
    let mut vps = tx.library().load_voiceprints(person)?;
    let auto_id = weak_sample(meeting, speaker, person)?.map(|v| v.id);
    let n = vps.embeddings.len();
    vps.embeddings.retain(|e| {
        (weak_only && e.strength != Strength::Weak)
            || !sample_of_speaker(e, meeting, speaker, auto_id.as_ref())
    });
    if vps.embeddings.len() != n {
        tx.save_voiceprints(&vps)?;
    }
    Ok(())
}

/// Moves `meeting`'s learned samples (every person, kind and model) to the
/// speaker IDs of a new analysis, e.g. after reprocessing renumbered them:
/// `new_speaker` gives the current speaker of each `published` one. A sample
/// whose previous speaker has no single successor (ambiguous or vanished)
/// loses its recorded speaker, so attribution falls back to its audio ranges.
///
/// An auto sample stands for its speaker's auto naming, which the new
/// analysis resolves afresh: its earlier auto person is that speaker's
/// `before` state. Unless its new speaker is already auto-named as the same
/// person in `meeting`, it is removed here; [`sync_weak_samples`] adds the
/// speaker's sample again when resolution names that person once more, and
/// a different resolution leaves the earlier person without it.
///
/// On entry `meeting.speaker_namespace` is the published analysis'. Only
/// samples recorded under it move; they take the new analysis' namespace,
/// which this sets on `meeting`. Save the meeting in the same write: until
/// then the moved samples disagree with the published meeting and are
/// attributed by range. The new namespace derives from the published one
/// and the mapping, so retrying a publication that failed after this ran
/// picks the same one and skips the samples it already moved, instead of
/// applying the mapping twice (which would undo a swap). A mapping that
/// keeps every published speaker keeps the namespace too. Only voiceprint
/// files with a sample that changed (speaker or namespace) or was removed
/// are saved. Returns the number of samples whose speaker changed.
pub fn migrate_sample_speakers(
    tx: &mut WriteTx<'_>,
    meeting: &mut Meeting,
    published: &[SpeakerId],
    new_speaker: impl Fn(SpeakerId) -> Option<SpeakerId>,
) -> Result<usize> {
    let old = std::mem::take(&mut meeting.speaker_namespace);
    meeting.speaker_namespace = if published.iter().all(|&s| new_speaker(s) == Some(s)) {
        old.clone()
    } else {
        successor_namespace(&old, published, &new_speaker)
    };
    let meeting = &*meeting;
    let mut changed = 0;
    for person in tx.library().list_people()? {
        let mut vps = tx.library().load_voiceprints(&person.id)?;
        let mut dirty = false;
        let mut relearn = BTreeSet::new();
        vps.embeddings.retain_mut(|e| {
            if e.source.meeting_id.as_ref() != Some(&meeting.id)
                || e.source.speaker_id.is_none()
                || e.source.speaker_namespace != old
            {
                return true;
            }
            let speaker = e.source.speaker_id.and_then(&new_speaker);
            if e.source.kind == VoiceprintKind::LiveConfirmation
                && !speaker.and_then(|id| meeting.speaker(id)).is_some_and(|s| {
                    s.assignment == Assignment::Confirmed && s.named_person() == Some(&person.id)
                })
            {
                relearn.insert(e.model.clone());
                dirty = true;
                return false;
            }
            if speaker != e.source.speaker_id {
                changed += 1;
            }
            let auto_named = |s: &schema::Speaker| {
                s.assignment == Assignment::Auto && s.person_id.as_ref() == Some(&person.id)
            };
            if e.source.kind == VoiceprintKind::Auto
                && e.strength == Strength::Weak
                && !speaker
                    .and_then(|s| meeting.speaker(s))
                    .is_some_and(auto_named)
            {
                dirty = true;
                return false;
            }
            if speaker != e.source.speaker_id {
                e.source.speaker_id = speaker;
                dirty = true;
            }
            if e.source.speaker_namespace != meeting.speaker_namespace {
                e.source
                    .speaker_namespace
                    .clone_from(&meeting.speaker_namespace);
                dirty = true;
            }
            true
        });
        if dirty {
            for model in relearn {
                learning::request(tx, &model, Some(&meeting.id))?;
            }
            tx.save_voiceprints(&vps)?;
        }
    }
    Ok(changed)
}

/// Namespace of the analysis that renumbers the speakers `published` under
/// `old` through `new_speaker`.
fn successor_namespace(
    old: &str,
    published: &[SpeakerId],
    new_speaker: impl Fn(SpeakerId) -> Option<SpeakerId>,
) -> String {
    use sha2::{Digest, Sha256};
    let mut ids = published.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut h = Sha256::new();
    h.update(old.as_bytes());
    for id in ids {
        h.update(id.number().to_le_bytes());
        h.update(new_speaker(id).map_or(0, SpeakerId::number).to_le_bytes());
    }
    schema::hex(&h.finalize()[..16])
}

/// Boundary tolerance for samples without a recorded speaker: their times
/// went through `f32` and runs merge segments across silences.
const LEGACY_RANGE_EPS_S: f64 = 1e-3;

/// Whether `e` was learned from `speaker` of `meeting`: by its recorded
/// speaker while it belongs to the meeting's current speaker namespace;
/// otherwise by its ranges, which must overlap this speaker's segment runs
/// and no other speaker's segments (both within [`LEGACY_RANGE_EPS_S`]).
/// Within the current namespace, ID-less auto samples can also match `auto_id`.
fn sample_of_speaker(
    e: &Voiceprint,
    meeting: &Meeting,
    speaker: SpeakerId,
    auto_id: Option<&String>,
) -> bool {
    if e.source.meeting_id.as_ref() != Some(&meeting.id) {
        return false;
    }
    if let Some(recorded) = e.source.speaker_id
        && e.source.speaker_namespace == meeting.speaker_namespace
    {
        return recorded == speaker;
    }
    match e.source.kind {
        VoiceprintKind::Auto
            if e.source.speaker_namespace == meeting.speaker_namespace
                && auto_id == Some(&e.id) =>
        {
            true
        }
        VoiceprintKind::Auto
        | VoiceprintKind::Confirmation
        | VoiceprintKind::LiveConfirmation
        | VoiceprintKind::Captions => legacy_ranges_of_speaker(&e.source, meeting, speaker),
        VoiceprintKind::Hint | VoiceprintKind::Enrollment => false,
    }
}

fn legacy_ranges_of_speaker(
    source: &VoiceprintSource,
    meeting: &Meeting,
    speaker: SpeakerId,
) -> bool {
    let runs = speaker_runs(meeting, speaker);
    let overlap = |a: f64, b: f64, start: f64, end: f64| b.min(end) - a.max(start);
    let mut ranges = source
        .audio_ranges()
        .map(|[a, b]| (f64::from(a), f64::from(b)))
        .peekable();
    ranges.peek().is_some()
        && ranges.all(|(start, end)| {
            runs.iter().any(|&(a, b)| {
                overlap(a - LEGACY_RANGE_EPS_S, b + LEGACY_RANGE_EPS_S, start, end) > 0.0
            }) && !meeting.segments.iter().any(|s| {
                s.speaker.is_some_and(|other| other != speaker)
                    && overlap(s.start, s.end, start, end) > LEGACY_RANGE_EPS_S
            })
        })
}

/// `speaker`'s runs of consecutive segments on each track (time-ordered;
/// another speaker's or an unassigned segment ends a run), as confirmation
/// samples are cut.
fn speaker_runs(meeting: &Meeting, speaker: SpeakerId) -> Vec<(f64, f64)> {
    let mut segs: Vec<&Segment> = meeting.segments.iter().collect();
    segs.sort_by(|a, b| {
        a.track
            .cmp(&b.track)
            .then(a.start.total_cmp(&b.start))
            .then(a.id.cmp(&b.id))
    });
    let mut runs: Vec<(f64, f64)> = Vec::new();
    let mut open = false;
    let mut track: Option<&str> = None;
    for s in segs {
        let mine = s.speaker == Some(speaker);
        let same_track = track == Some(s.track.as_str());
        match runs.last_mut() {
            Some(last) if mine && open && same_track => last.1 = last.1.max(s.end),
            _ if mine => runs.push((s.start, s.end)),
            _ => {}
        }
        open = mine;
        track = Some(s.track.as_str());
    }
    runs
}

/// What identifies a person inside `hints.json` entries.
struct HintKeys {
    id: String,
    is_self: bool,
    emails: BTreeSet<String>,
    names: BTreeSet<String>,
}

impl HintKeys {
    fn new(id: &PersonId, person: Option<&Person>) -> Self {
        let mut keys = Self {
            id: id.to_string(),
            is_self: false,
            emails: BTreeSet::new(),
            names: BTreeSet::new(),
        };
        if let Some(p) = person {
            keys.is_self = p.is_self;
            keys.emails = p.emails.iter().map(|e| fold(e)).collect();
            keys.names = std::iter::once(&p.name)
                .chain(&p.aliases)
                .map(|n| fold(n))
                .filter(|n| !n.is_empty())
                .collect();
        }
        keys
    }

    /// A person key (`person_key` / `key`): plain or prefixed `email:` /
    /// `name:`, or `self`.
    fn key_matches(&self, key: &str) -> bool {
        let key = fold(key);
        if key == self.id || (key == "self" && self.is_self) {
            return true;
        }
        if let Some(e) = key.strip_prefix("email:") {
            return self.emails.contains(e.trim());
        }
        if let Some(n) = key.strip_prefix("name:") {
            return self.names.contains(n.trim());
        }
        self.emails.contains(&key) || self.names.contains(&key)
    }

    fn entry_matches(&self, entry: &serde_json::Map<String, Value>) -> bool {
        if str_field(entry, "person_id") == Some(self.id.as_str()) {
            return true;
        }
        if ["person_key", "key"]
            .into_iter()
            .filter_map(|k| str_field(entry, k))
            .any(|k| self.key_matches(k))
        {
            return true;
        }
        if self.is_self && entry.get("is_self").and_then(Value::as_bool) == Some(true) {
            return true;
        }
        match str_field(entry, "email") {
            Some(e) => self.emails.contains(&fold(e)),
            None => str_field(entry, "name").is_some_and(|n| self.names.contains(&fold(n))),
        }
    }
}

fn str_field<'a>(entry: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    entry.get(key).and_then(Value::as_str)
}

/// Removes the person's entries from every `.queue/*/hints.json`: array
/// elements that are objects identifying them (person ID, person key, e-mail,
/// name or `is_self`), then elements whose `majority_key` names a removed
/// key. Unknown fields are kept. Returns the number of elements removed.
fn scrub_hints(root: &Path, keys: &HintKeys) -> Result<usize> {
    let queue = root.join(paths::QUEUE_DIR);
    let entries = match fs::read_dir(&queue) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::io_at(&queue, e)),
    };
    let mut total = 0;
    for entry in entries {
        let path = entry
            .map_err(|e| Error::io_at(&queue, e))?
            .path()
            .join(HINTS_JSON);
        if !path.is_file() {
            continue;
        }
        let mut value: Value = fsutil::read_json(&path)?;
        let mut dropped_keys = BTreeSet::new();
        let mut n = scrub_value(&mut value, keys, &mut dropped_keys);
        if !dropped_keys.is_empty() {
            n += drop_majority(&mut value, &dropped_keys);
        }
        if n > 0 {
            fsutil::write_json_atomic(&path, &value)?;
            total += n;
        }
    }
    Ok(total)
}

fn scrub_value(v: &mut Value, keys: &HintKeys, dropped: &mut BTreeSet<String>) -> usize {
    match v {
        Value::Array(items) => {
            let before = items.len();
            items.retain(|item| {
                let Some(obj) = item.as_object() else {
                    return true;
                };
                if !keys.entry_matches(obj) {
                    return true;
                }
                for k in ["person_key", "key"] {
                    if let Some(s) = obj.get(k).and_then(Value::as_str) {
                        dropped.insert(s.to_owned());
                    }
                }
                false
            });
            let mut n = before - items.len();
            for item in items {
                n += scrub_value(item, keys, dropped);
            }
            n
        }
        Value::Object(map) => map
            .values_mut()
            .map(|x| scrub_value(x, keys, dropped))
            .sum(),
        _ => 0,
    }
}

fn drop_majority(v: &mut Value, dropped: &BTreeSet<String>) -> usize {
    match v {
        Value::Array(items) => {
            let before = items.len();
            items.retain(|item| {
                item.get("majority_key")
                    .and_then(Value::as_str)
                    .is_none_or(|k| !dropped.contains(k))
            });
            let mut n = before - items.len();
            for item in items {
                n += drop_majority(item, dropped);
            }
            n
        }
        Value::Object(map) => map.values_mut().map(|x| drop_majority(x, dropped)).sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests;

/// The kept enrollment passages of the Me person
/// (`<lib>/people/<me>/enrollment/*.wav`): count and bytes.
pub fn enrollment_files(library: &Library) -> Result<Vec<(PathBuf, u64)>> {
    let Some(me) = library.self_person()? else {
        return Ok(Vec::new());
    };
    let dir = library.person_dir(&me.id).join("enrollment");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(&dir, e)),
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("wav"))
        {
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push((path, len));
        }
    }
    files.sort();
    Ok(files)
}
