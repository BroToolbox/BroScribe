//! Speaker grouping (design §6.4): over-split speakers of one meeting shown
//! as one voice through `Meeting::speaker_groups`, an overlay of speaker IDs.
//! Speakers, segments, centroids, assignments and samples are never
//! rewritten, so ungrouping is exact. Everything that displays, indexes or
//! names speakers reads them through [`effective_speaker`]; automatic
//! identity producers leave grouped members frozen ([`is_grouped_member`]).
//!
//! Invariants (checked on every save, [`check_overlay`]; a loaded file that
//! breaks them has the offending groups ignored, [`sanitize_overlay`]): a
//! speaker is in at most one group; the kept speaker is not a member; every
//! ID exists; members share the kept speaker's track, never the paired
//! microphone track; and no group holds two speakers proven to be different
//! people, a person another member rejected, or a `not_same` pair.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{sample_of_speaker, save_and_index};
use crate::config::Thresholds;
use crate::identity::{self, MergeInput};
use crate::index::Index;
use crate::schema::{
    Assignment, GroupOrigin, Meeting, ReviewSource, Speaker, SpeakerGroup, Strength,
    VoiceprintKind, Voiceprints,
};
use crate::{Error, Library, MeetingId, PersonId, Result, SpeakerId, WriteTx};

// ---- reading the overlay ----

/// The speaker `id` is shown as: its group's kept speaker when it is a
/// grouped member, else itself. The one rule every consumer applies.
pub fn effective_speaker(meeting: &Meeting, id: SpeakerId) -> SpeakerId {
    meeting
        .speaker_groups
        .iter()
        .find(|g| g.members.contains(&id))
        .map_or(id, |g| g.kept)
}

/// The group `id` belongs to (as kept speaker or member).
pub fn group_of(meeting: &Meeting, id: SpeakerId) -> Option<&SpeakerGroup> {
    meeting.speaker_groups.iter().find(|g| g.contains(id))
}

/// Whether `id` is a grouped member (shown as another speaker). Members are
/// frozen: automatic identity producers leave their assignment, suggestion,
/// rejections and samples exactly as they are.
pub fn is_grouped_member(meeting: &Meeting, id: SpeakerId) -> bool {
    meeting
        .speaker_groups
        .iter()
        .any(|g| g.members.contains(&id))
}

/// The speakers shown, in meeting order: every speaker that is not a
/// grouped member.
pub fn effective_speakers(meeting: &Meeting) -> impl Iterator<Item = &Speaker> {
    meeting
        .speakers
        .iter()
        .filter(|s| !is_grouped_member(meeting, s.id))
}

/// Talk time shown for `id`: its group's total when it is a kept speaker,
/// else its own.
pub fn effective_talk_s(meeting: &Meeting, id: SpeakerId) -> f64 {
    match meeting.speaker_groups.iter().find(|g| g.kept == id) {
        Some(g) => g
            .speakers()
            .filter_map(|s| meeting.speaker(s))
            .map(|s| s.talk_s)
            .sum(),
        None => meeting.speaker(id).map_or(0.0, |s| s.talk_s),
    }
}

/// Speakers of `meeting` auto-named as a person whose voiceprints hold a
/// caption sample learned from that speaker in this meeting: for grouping
/// they count like confirmed speakers.
pub fn caption_proven(lib: &Library, meeting: &Meeting) -> Result<BTreeMap<SpeakerId, PersonId>> {
    let mut out = BTreeMap::new();
    let mut prints: BTreeMap<PersonId, Voiceprints> = BTreeMap::new();
    for s in &meeting.speakers {
        let (Assignment::Auto, Some(person)) = (s.assignment, &s.person_id) else {
            continue;
        };
        if !prints.contains_key(person) {
            prints.insert(person.clone(), lib.load_voiceprints(person)?);
        }
        if prints[person].embeddings.iter().any(|e| {
            e.source.kind == VoiceprintKind::Captions && sample_of_speaker(e, meeting, s.id, None)
        }) {
            out.insert(s.id, person.clone());
        }
    }
    Ok(out)
}

// ---- invariants ----

fn identity_input(speaker: &Speaker, proven: &BTreeMap<SpeakerId, PersonId>) -> MergeInput {
    MergeInput {
        id: speaker.id,
        centroid: Vec::new(),
        talk_s: speaker.talk_s,
        confirmed_person: identity::proven_person(speaker, proven).cloned(),
        rejected: speaker.rejected_person_ids.clone(),
    }
}

/// Why `speakers` cannot be one group under the compatibility rule, naming
/// the first offending pair; `None` when they can.
fn incompatibility(
    meeting: &Meeting,
    speakers: &[SpeakerId],
    proven: &BTreeMap<SpeakerId, PersonId>,
    not_same: &[[SpeakerId; 2]],
) -> Option<String> {
    let inputs: Vec<MergeInput> = speakers
        .iter()
        .filter_map(|id| meeting.speaker(*id))
        .map(|s| identity_input(s, proven))
        .collect();
    if identity::compatible(&inputs, not_same) {
        return None;
    }
    for (i, a) in inputs.iter().enumerate() {
        for b in &inputs[i + 1..] {
            if identity::compatible(&[a.clone(), b.clone()], not_same) {
                continue;
            }
            let why = match (&a.confirmed_person, &b.confirmed_person) {
                (Some(p), Some(q)) if p != q => "are different people".to_owned(),
                (Some(p), _) if b.rejected.contains(p) => {
                    format!("{} was marked not to be {p}", b.id)
                }
                (_, Some(q)) if a.rejected.contains(q) => {
                    format!("{} was marked not to be {q}", a.id)
                }
                _ => "were marked as not the same person".to_owned(),
            };
            return Some(format!("{} and {} {why}", a.id, b.id));
        }
    }
    Some("the speakers are not compatible".to_owned())
}

/// What is wrong with `group` given the groups `accepted` before it; `None`
/// when it is valid.
fn group_problem(
    meeting: &Meeting,
    group: &SpeakerGroup,
    accepted: &[SpeakerGroup],
    proven: &BTreeMap<SpeakerId, PersonId>,
) -> Option<String> {
    if group.members.is_empty() {
        return Some("it has no members".into());
    }
    if group.members.windows(2).any(|w| w[0] >= w[1]) || group.members.contains(&group.kept) {
        return Some("it lists a speaker twice".into());
    }
    if accepted.iter().any(|g| g.id == group.id) {
        return Some(format!("another group is also {}", group.id));
    }
    let Some(kept) = meeting.speaker(group.kept) else {
        return Some(format!("speaker {} does not exist", group.kept));
    };
    for m in &group.members {
        match meeting.speaker(*m) {
            None => return Some(format!("speaker {m} does not exist")),
            Some(s) if s.track != kept.track => {
                return Some(format!("{m} is on another track than {}", group.kept));
            }
            Some(_) => {}
        }
    }
    if meeting.paired_mic_track() == Some(kept.track.as_str()) {
        return Some("it is on the paired microphone track".into());
    }
    if let Some(s) = group
        .speakers()
        .find(|s| accepted.iter().any(|g| g.contains(*s)))
    {
        return Some(format!("{s} is already in another group"));
    }
    let speakers: Vec<SpeakerId> = group.speakers().collect();
    incompatibility(meeting, &speakers, proven, &meeting.not_same)
}

/// The first problem of `meeting`'s grouping overlay; `Ok` when it holds
/// every invariant. `proven` adds caption-proven speakers to the confirmed
/// ones (`None`: confirmed only, as on load and save, which read no
/// voiceprints).
pub fn check_overlay(
    meeting: &Meeting,
    proven: Option<&BTreeMap<SpeakerId, PersonId>>,
) -> std::result::Result<(), String> {
    let none = BTreeMap::new();
    let proven = proven.unwrap_or(&none);
    for (i, g) in meeting.speaker_groups.iter().enumerate() {
        if let Some(why) = group_problem(meeting, g, &meeting.speaker_groups[..i], proven) {
            return Err(format!("group {}: {why}", g.id));
        }
    }
    for (i, &[a, b]) in meeting.not_same.iter().enumerate() {
        if a >= b || meeting.speaker(a).is_none() || meeting.speaker(b).is_none() {
            return Err(format!(
                "not_same pair {a}/{b} is not two existing speakers in order"
            ));
        }
        if meeting.not_same[..i].contains(&[a, b]) {
            return Err(format!("not_same pair {a}/{b} is listed twice"));
        }
    }
    Ok(())
}

/// The people `id` is never named or suggested as: its own rejections and,
/// for a group's kept speaker, every member's too (the group is one voice).
pub fn effective_rejections(meeting: &Meeting, id: SpeakerId) -> Vec<PersonId> {
    let mut out: Vec<PersonId> = Vec::new();
    let speakers: Vec<SpeakerId> = match meeting.speaker_groups.iter().find(|g| g.kept == id) {
        Some(g) => g.speakers().collect(),
        None => vec![id],
    };
    for s in speakers.iter().filter_map(|s| meeting.speaker(*s)) {
        for p in &s.rejected_person_ids {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    out
}

/// Removes the weak `auto` samples `members` of `meeting` learned from their
/// own auto names: for speakers a publication grouped after it had already
/// synced their weak samples (a full reprocess), so a grouped member's auto
/// name teaches nothing, as when grouping precedes the sync. Caption and
/// confirmed samples stay.
pub fn drop_member_auto_samples(
    tx: &mut WriteTx<'_>,
    meeting: &Meeting,
    members: &[SpeakerId],
) -> Result<()> {
    for &m in members {
        let Some(s) = meeting.speaker(m) else {
            continue;
        };
        let (Assignment::Auto, Some(person)) = (s.assignment, &s.person_id) else {
            continue;
        };
        let auto_id = super::weak_sample(meeting, m, person)?.map(|v| v.id);
        let mut prints = tx.library().load_voiceprints(person)?;
        let before = prints.embeddings.len();
        prints.embeddings.retain(|e| {
            !(e.strength == Strength::Weak
                && e.source.kind == VoiceprintKind::Auto
                && sample_of_speaker(e, meeting, m, auto_id.as_ref()))
        });
        if prints.embeddings.len() != before {
            tx.save_voiceprints(&prints)?;
        }
    }
    Ok(())
}

/// Dissolves the groups that break an invariant (in order, each against
/// the groups kept before it) and drops invalid `not_same` pairs. A note
/// (`grouping`) names each dissolved group when `note` says so for its
/// origin. Returns the dissolved groups.
fn dissolve_invalid(
    meeting: &mut Meeting,
    proven: &BTreeMap<SpeakerId, PersonId>,
    note: impl Fn(&SpeakerGroup) -> bool,
) -> Vec<SpeakerGroup> {
    // Pairs first: a hand-edited reversed pair still separates its speakers.
    let mut pairs: Vec<[SpeakerId; 2]> = Vec::with_capacity(meeting.not_same.len());
    for [a, b] in std::mem::take(&mut meeting.not_same) {
        let pair = sorted(a, b);
        if a != b
            && meeting.speaker(a).is_some()
            && meeting.speaker(b).is_some()
            && !pairs.contains(&pair)
        {
            pairs.push(pair);
        }
    }
    pairs.sort();
    meeting.not_same = pairs;
    let mut kept: Vec<SpeakerGroup> = Vec::with_capacity(meeting.speaker_groups.len());
    let mut dissolved = Vec::new();
    let mut notes = Vec::new();
    for g in std::mem::take(&mut meeting.speaker_groups) {
        match group_problem(meeting, &g, &kept, proven) {
            None => kept.push(g),
            Some(why) => {
                if note(&g) {
                    notes.push(format!(
                        "the merge of {} with {} was undone: {why}",
                        g.kept,
                        g.members
                            .iter()
                            .map(SpeakerId::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                dissolved.push(g);
            }
        }
    }
    meeting.speaker_groups = kept;
    meeting.add_review_notes(ReviewSource::Grouping, notes);
    dissolved
}

/// Load-time leniency: groups of `meeting` that break an invariant (only
/// possible from a hand-edited file) are ignored, each with a `grouping`
/// review note, and invalid `not_same` pairs are dropped; never an error.
/// Returns whether anything was dropped.
pub fn sanitize_overlay(meeting: &mut Meeting) -> bool {
    if meeting.speaker_groups.is_empty() && meeting.not_same.is_empty() {
        return false;
    }
    if check_overlay(meeting, None).is_ok() {
        return false;
    }
    dissolve_invalid(meeting, &BTreeMap::new(), |_| true);
    true
}

/// Dissolves every group an identity change made incompatible (with
/// caption-proven speakers counted as confirmed): `Auto` groups silently,
/// `User` groups with a `grouping` review note. Returns the dissolved groups.
pub fn reconcile_auto_groups(
    meeting: &mut Meeting,
    proven: &BTreeMap<SpeakerId, PersonId>,
) -> Vec<SpeakerGroup> {
    let mut removed = dissolve_invalid(meeting, proven, |g| g.origin == GroupOrigin::User);
    let evidence = super::UserSpeakerEvidence::of(meeting);
    let blocked: Vec<_> = meeting
        .speaker_groups
        .iter()
        .filter(|group| {
            matches!(group.origin, GroupOrigin::Auto { .. })
                && evidence.conflict(meeting, group.kept, None).is_some()
        })
        .cloned()
        .collect();
    meeting
        .speaker_groups
        .retain(|group| !blocked.contains(group));
    removed.extend(blocked);
    removed
}

/// What [`apply_auto_groups`] changed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GroupingChange {
    /// Groups created or grown.
    pub groups_added: usize,
    /// Groups dissolved because they became incompatible.
    pub dissolved: Vec<SpeakerGroup>,
}

/// The automatic grouping step every publishing path runs after
/// resolution and caption naming: [`reconcile_auto_groups`], then
/// [`identity::auto_groups`] at `auto_merge` (none set: no new groups),
/// with the meeting's roster as floor.
pub fn apply_auto_groups(
    meeting: &mut Meeting,
    model: &str,
    thresholds: Option<&Thresholds>,
    proven: &BTreeMap<SpeakerId, PersonId>,
) -> Result<GroupingChange> {
    let before = meeting.speaker_groups.clone();
    let mut dissolved = reconcile_auto_groups(meeting, proven);
    let floor = meeting.roster_floor();
    meeting.speaker_groups = identity::auto_groups(
        meeting,
        model,
        thresholds.and_then(|t| t.auto_merge),
        floor,
        proven,
    )?;
    dissolved.extend(reconcile_auto_groups(meeting, proven));
    Ok(GroupingChange {
        groups_added: meeting
            .speaker_groups
            .iter()
            .filter(|g| !before.contains(g))
            .count(),
        dissolved,
    })
}

// ---- suggestions ----

/// Two shown speakers that may be one voice ([`duplicate_suggestions`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DuplicateSuggestion {
    /// The speaker a merge would keep.
    pub a: SpeakerId,
    /// The speaker that sounds like `a`.
    pub b: SpeakerId,
    /// Cosine of the two (group) centroids.
    pub score: f32,
}

/// Pairs of shown speakers (a group counts as one, with its talk-weighted
/// centroid of `model`) on the same track, never the paired microphone,
/// that are compatible, not separated (`not_same`) and whose similarity is
/// in [`merge`, `auto_merge`) (no upper bound without `auto_merge`). Empty
/// without a calibrated `merge`. Best first. Pure: `proven` comes from
/// [`caption_proven`]. Computed on demand, never stored.
pub fn duplicate_suggestions(
    meeting: &Meeting,
    model: &str,
    thresholds: Option<&Thresholds>,
    proven: &BTreeMap<SpeakerId, PersonId>,
) -> Result<Vec<DuplicateSuggestion>> {
    let Some(merge) = thresholds.and_then(|t| t.merge) else {
        return Ok(Vec::new());
    };
    let upper = thresholds.and_then(|t| t.auto_merge);
    let paired = meeting.paired_mic_track();
    let inputs: Vec<MergeInput> = meeting
        .speakers
        .iter()
        .map(|s| identity::merge_input(s, model, proven))
        .collect::<Result<_>>()?;
    let position = |id: SpeakerId| meeting.speakers.iter().position(|s| s.id == id);
    let units: Vec<(&str, Vec<usize>, Vec<f32>)> = effective_speakers(meeting)
        .filter(|s| Some(s.track.as_str()) != paired)
        .filter_map(|s| {
            let members: Vec<usize> = match group_of(meeting, s.id) {
                Some(g) => g.speakers().filter_map(position).collect(),
                None => position(s.id).into_iter().collect(),
            };
            let centroid = identity::group_centroid(&inputs, &members)?;
            Some((s.track.as_str(), members, centroid))
        })
        .collect();
    let mut out = Vec::new();
    for (i, (track_a, a, ca)) in units.iter().enumerate() {
        for (track_b, b, cb) in &units[i + 1..] {
            if track_a != track_b {
                continue;
            }
            let score = identity::cosine(ca, cb);
            if score.is_nan() || score < merge || upper.is_some_and(|u| score >= u) {
                continue;
            }
            let both: Vec<MergeInput> = a.iter().chain(b).map(|&k| inputs[k].clone()).collect();
            if !identity::compatible(&both, &meeting.not_same) {
                continue;
            }
            let all: Vec<usize> = a.iter().chain(b).copied().collect();
            let kept = identity::kept_speaker(&inputs, &all).expect("two units");
            let (ka, kb) = (
                effective_speaker(meeting, inputs[a[0]].id),
                effective_speaker(meeting, inputs[b[0]].id),
            );
            let (first, second) = if a.contains(&kept) {
                (ka, kb)
            } else {
                (kb, ka)
            };
            out.push(DuplicateSuggestion {
                a: first,
                b: second,
                score,
            });
        }
    }
    out.sort_by(|x, y| {
        y.score
            .total_cmp(&x.score)
            .then(x.a.cmp(&y.a))
            .then(x.b.cmp(&y.b))
    });
    Ok(out)
}

// ---- writes ----

/// What a grouping or identity write prepared outside the lock assumed:
/// the meeting's raw speaker numbering and its grouping overlay. Writes
/// refuse to act on a meeting that changed since ([`check_speaker_fence`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerFence {
    pub speaker_namespace: String,
    pub grouping_revision: String,
}

impl SpeakerFence {
    pub fn of(meeting: &Meeting) -> Self {
        Self {
            speaker_namespace: meeting.speaker_namespace.clone(),
            grouping_revision: meeting.grouping_revision(),
        }
    }
}

/// Revision of a displayed speaker and the kept speaker a naming would
/// affect. The fence also detects reanalysis and changes to speaker groups.
pub fn speaker_revision(meeting: &Meeting, speaker: &Speaker) -> String {
    let kept_id = effective_speaker(meeting, speaker.id);
    let kept = meeting.speaker(kept_id).filter(|s| s.id != speaker.id);
    let snapshot = (&meeting.id, SpeakerFence::of(meeting), speaker, kept);
    crate::schema::sha256_hex(&serde_json::to_vec(&snapshot).expect("speaker snapshot serializes"))
}

pub fn check_speaker_revision(meeting: &Meeting, speaker: SpeakerId, expected: &str) -> Result<()> {
    if meeting
        .speaker(speaker)
        .is_some_and(|s| speaker_revision(meeting, s) == expected)
    {
        return Ok(());
    }
    Err(Error::Service {
        code: "speaker_changed",
        message:
            "The speaker or suggestion changed. Read the meeting again before naming the speaker."
                .into(),
    })
}

/// `speakers_reanalysed` when `meeting` was reanalysed since `fence` was
/// taken, `speakers_regrouped` when its speakers were grouped or separated
/// since. Callers of identity writes (`name_speaker`, `reject_suggestion`)
/// run [`check_speaker_fence_at`] in the same write.
pub fn check_speaker_fence(meeting: &Meeting, fence: &SpeakerFence) -> Result<()> {
    if meeting.speaker_namespace != fence.speaker_namespace {
        return Err(Error::Service {
            code: "speakers_reanalysed",
            message: format!(
                "the speakers of {} were reanalysed; review again",
                meeting.id
            ),
        });
    }
    if meeting.grouping_revision() != fence.grouping_revision {
        return Err(Error::Service {
            code: "speakers_regrouped",
            message: format!(
                "the speakers of {} were regrouped; review again",
                meeting.id
            ),
        });
    }
    Ok(())
}

/// [`check_speaker_fence`] on the meeting as stored, inside a write.
pub fn check_speaker_fence_at(
    tx: &WriteTx<'_>,
    meeting_id: &MeetingId,
    fence: &SpeakerFence,
) -> Result<()> {
    check_speaker_fence(&tx.library().load_meeting(meeting_id)?, fence)
}

/// The meeting, if grouping may change it now: processing done and no job
/// working on it ([`crate::queue::Queue::check_meeting_idle`]).
pub(super) fn writable_meeting(tx: &WriteTx<'_>, meeting_id: &MeetingId) -> Result<Meeting> {
    super::editable_meeting(
        tx,
        meeting_id,
        "changing its speakers",
        super::EditBusy::ExceptVectors,
    )
}

/// Grouping changes what every line and reference shows: generated content
/// (enrichment, template results) made before no longer matches its
/// speakers.
fn mark_grouping_stale(meeting: &mut Meeting) {
    meeting.set_generated_stale();
}

fn sorted(a: SpeakerId, b: SpeakerId) -> [SpeakerId; 2] {
    if a <= b { [a, b] } else { [b, a] }
}

fn add_not_same(meeting: &mut Meeting, a: SpeakerId, b: SpeakerId) -> Option<[SpeakerId; 2]> {
    let pair = sorted(a, b);
    (a != b && !meeting.not_same.contains(&pair)).then(|| {
        meeting.not_same.push(pair);
        meeting.not_same.sort();
        pair
    })
}

/// Takes `member` out of its group (dissolving a group left without
/// members) and remembers it as `not_same` with the kept speaker and every
/// remaining member, so it is never grouped back automatically with any of
/// them (an automatic run may still regroup the others among themselves).
/// Returns the group's kept speaker.
pub(super) fn separate_member(meeting: &mut Meeting, member: SpeakerId) -> Option<SpeakerId> {
    let g = meeting
        .speaker_groups
        .iter_mut()
        .find(|g| g.members.contains(&member))?;
    let kept = g.kept;
    g.members.retain(|m| *m != member);
    let remaining = g.members.clone();
    meeting.speaker_groups.retain(|g| !g.members.is_empty());
    for other in std::iter::once(kept).chain(remaining) {
        add_not_same(meeting, other, member);
    }
    Some(kept)
}

fn count_shown(meeting: &Meeting) -> usize {
    effective_speakers(meeting).count()
}

/// Result of [`group_speakers`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GroupOutcome {
    pub meeting_id: MeetingId,
    /// The group as saved.
    pub group: SpeakerGroup,
    /// The speaker the request asked to keep.
    pub requested_kept: SpeakerId,
    /// The kept speaker differs from the requested one: the rule keeps a
    /// confirmed (or caption-proven) member with the most talk time.
    pub kept_swapped: bool,
    /// `false` when the speakers were already one group.
    pub changed: bool,
    pub grouping_revision: String,
}

/// Groups `kept` and `members` of a `done` meeting (design §6.4): their
/// groups join too (adding to a group extends it). The kept speaker follows
/// the rule of [`identity::kept_speaker`] (proven member with the most talk
/// time, else most talk time), whatever the request named. Refused when the
/// speakers span tracks, are on the paired microphone track, or are proven
/// to be different people / a person one of them rejected; `Auto` groups
/// also respect `not_same`, while a `User` merge clears the pairs it joins.
/// Fenced by `fence`; refused while a job works on the meeting. The
/// enrichment is marked stale. A kept speaker named or suggested as a
/// person a member rejected is resolved again (centroid of `model`,
/// `thresholds`), with its weak samples synced.
#[allow(clippy::too_many_arguments)]
pub fn group_speakers(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    kept: SpeakerId,
    members: &[SpeakerId],
    origin: GroupOrigin,
    fence: &SpeakerFence,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<GroupOutcome> {
    let lib = tx.library().clone();
    let mut meeting = writable_meeting(tx, meeting_id)?;
    check_speaker_fence(&meeting, fence)?;
    let mut set: BTreeSet<SpeakerId> = BTreeSet::new();
    let mut joined: Vec<SpeakerGroup> = Vec::new();
    for id in std::iter::once(kept).chain(members.iter().copied()) {
        if meeting.speaker(id).is_none() {
            return Err(Error::NotFound(format!(
                "speaker {id} in meeting {meeting_id}"
            )));
        }
        match group_of(&meeting, id) {
            Some(g) => {
                set.extend(g.speakers());
                if !joined.contains(g) {
                    joined.push(g.clone());
                }
            }
            None => {
                set.insert(id);
            }
        }
    }
    if set.len() < 2 {
        return Err(Error::InvalidInput(
            "name at least two different speakers to merge".into(),
        ));
    }
    if let [only] = joined.as_slice()
        && only.speakers().collect::<BTreeSet<_>>() == set
    {
        return Ok(GroupOutcome {
            meeting_id: meeting_id.clone(),
            group: only.clone(),
            requested_kept: kept,
            kept_swapped: only.kept != kept,
            changed: false,
            grouping_revision: meeting.grouping_revision(),
        });
    }
    let ids: Vec<SpeakerId> = set.iter().copied().collect();
    let track = meeting
        .speaker(kept)
        .map(|s| s.track.clone())
        .unwrap_or_default();
    if let Some(other) = ids
        .iter()
        .find(|id| meeting.speaker(**id).is_some_and(|s| s.track != track))
    {
        return Err(Error::InvalidInput(format!(
            "{other} is on another track than {kept}; only speakers of one track can be merged"
        )));
    }
    if meeting.paired_mic_track() == Some(track.as_str()) {
        return Err(Error::InvalidInput(format!(
            "{kept} is on the microphone track, whose speakers the two-track analysis already \
             decided; it cannot be merged"
        )));
    }
    let proven = caption_proven(&lib, &meeting)?;
    let user = origin == GroupOrigin::User || joined.iter().any(|g| g.origin == GroupOrigin::User);
    let not_same: Vec<[SpeakerId; 2]> = if user {
        meeting
            .not_same
            .iter()
            .filter(|[a, b]| !(set.contains(a) && set.contains(b)))
            .copied()
            .collect()
    } else {
        meeting.not_same.clone()
    };
    if let Some(why) = incompatibility(&meeting, &ids, &proven, &not_same) {
        return Err(Error::InvalidInput(format!("cannot merge: {why}")));
    }
    let inputs: Vec<MergeInput> = ids
        .iter()
        .filter_map(|id| meeting.speaker(*id))
        .map(|s| identity_input(s, &proven))
        .collect();
    let all: Vec<usize> = (0..inputs.len()).collect();
    let new_kept = inputs[identity::kept_speaker(&inputs, &all).expect("two speakers")].id;
    let origin = if user {
        GroupOrigin::User
    } else {
        let score = joined
            .iter()
            .filter_map(|g| match g.origin {
                GroupOrigin::Auto { score } => Some(score),
                GroupOrigin::User => None,
            })
            .chain(match origin {
                GroupOrigin::Auto { score } => Some(score),
                GroupOrigin::User => None,
            })
            .reduce(f32::min)
            .unwrap_or(0.0);
        GroupOrigin::Auto { score }
    };
    meeting
        .speaker_groups
        .retain(|g| !g.speakers().any(|s| set.contains(&s)));
    let group = SpeakerGroup::new(new_kept, ids.iter().copied(), origin);
    meeting.speaker_groups.push(group.clone());
    meeting.speaker_groups.sort_by_key(|g| g.kept);
    meeting.not_same = not_same;
    check_overlay(&meeting, Some(&proven)).map_err(Error::Internal)?;
    let renamed = resolve_kept_against_members(&lib, &mut meeting, model, thresholds)?;
    super::live_speaker::line_evidence_changed(tx, &mut meeting)?;
    mark_grouping_stale(&mut meeting);
    save_and_index(tx, index, &mut meeting)?;
    super::sync_weak_samples(tx, &meeting, &renamed)?;
    Ok(GroupOutcome {
        meeting_id: meeting_id.clone(),
        kept_swapped: group.kept != kept,
        group,
        requested_kept: kept,
        changed: true,
        grouping_revision: meeting.grouping_revision(),
    })
}

/// Result of [`ungroup_speakers`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UngroupOutcome {
    pub meeting_id: MeetingId,
    /// The group removed.
    pub group: SpeakerGroup,
    /// Pairs newly remembered as separate: they are never grouped
    /// automatically again.
    pub not_same_added: Vec<[SpeakerId; 2]>,
    pub grouping_revision: String,
}

/// Unmerge: removes group `group_id` (e.g. `G2`, case-insensitive) of a
/// `done` meeting. Its speakers show their own, unchanged state again
/// (nothing is re-resolved or restored: grouping never changed it), and
/// every pair inside the group is remembered in `not_same`. Fenced by
/// `fence`; refused while a job works on the meeting. The enrichment is
/// marked stale.
pub fn ungroup_speakers(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    group_id: &str,
    fence: &SpeakerFence,
) -> Result<UngroupOutcome> {
    let mut meeting = writable_meeting(tx, meeting_id)?;
    check_speaker_fence(&meeting, fence)?;
    let wanted = group_id.trim();
    let Some(pos) = meeting
        .speaker_groups
        .iter()
        .position(|g| g.id.eq_ignore_ascii_case(wanted))
    else {
        return Err(Error::NotFound(format!(
            "speaker group {wanted} in meeting {meeting_id}"
        )));
    };
    let group = meeting.speaker_groups.remove(pos);
    let speakers: Vec<SpeakerId> = group.speakers().collect();
    let mut not_same_added = Vec::new();
    for (i, a) in speakers.iter().enumerate() {
        for b in &speakers[i + 1..] {
            not_same_added.extend(add_not_same(&mut meeting, *a, *b));
        }
    }
    super::live_speaker::line_evidence_changed(tx, &mut meeting)?;
    mark_grouping_stale(&mut meeting);
    save_and_index(tx, index, &mut meeting)?;
    Ok(UngroupOutcome {
        meeting_id: meeting_id.clone(),
        group,
        not_same_added,
        grouping_revision: meeting.grouping_revision(),
    })
}

/// Result of [`mark_not_same`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NotSameOutcome {
    pub meeting_id: MeetingId,
    pub pair: [SpeakerId; 2],
    /// `false` when the pair was already remembered.
    pub added: bool,
    pub grouping_revision: String,
}

/// "Not the same person": remembers that `a` and `b` of a `done` meeting
/// are different speakers, so they are never grouped automatically nor
/// suggested as duplicates. Refused while they are merged (unmerge first).
/// Fenced by `fence`; refused while a job works on the meeting.
pub fn mark_not_same(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    a: SpeakerId,
    b: SpeakerId,
    fence: &SpeakerFence,
) -> Result<NotSameOutcome> {
    let mut meeting = writable_meeting(tx, meeting_id)?;
    check_speaker_fence(&meeting, fence)?;
    for id in [a, b] {
        if meeting.speaker(id).is_none() {
            return Err(Error::NotFound(format!(
                "speaker {id} in meeting {meeting_id}"
            )));
        }
    }
    if a == b {
        return Err(Error::InvalidInput("name two different speakers".into()));
    }
    if let Some(g) = group_of(&meeting, a).filter(|g| g.contains(b)) {
        return Err(Error::InvalidInput(format!(
            "{a} and {b} are merged as {}; unmerge them first",
            g.id
        )));
    }
    let pair = sorted(a, b);
    let added = add_not_same(&mut meeting, a, b).is_some();
    if added {
        save_and_index(tx, index, &mut meeting)?;
    }
    Ok(NotSameOutcome {
        meeting_id: meeting_id.clone(),
        pair,
        added,
        grouping_revision: meeting.grouping_revision(),
    })
}

/// Result of [`auto_group_meeting`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AutoGroupOutcome {
    pub meeting_id: MeetingId,
    /// Groups created or grown.
    pub groups_added: usize,
    /// Speakers shown before and after (a group counts once).
    pub speakers_before: usize,
    pub speakers_after: usize,
    /// Why the meeting was left alone: not done, or a job is working on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// Applies automatic grouping ([`apply_auto_groups`]) to an existing `done`
/// meeting in this write; meetings not done or busy are skipped, not
/// errors. Existing groups and `not_same` pairs are respected, so a second
/// run adds nothing. The enrichment is marked stale when grouping changed.
pub fn auto_group_meeting(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<AutoGroupOutcome> {
    let lib = tx.library().clone();
    let mut out = AutoGroupOutcome {
        meeting_id: meeting_id.clone(),
        groups_added: 0,
        speakers_before: 0,
        speakers_after: 0,
        skipped: None,
    };
    let mut meeting = match writable_meeting(tx, meeting_id) {
        Ok(m) => m,
        Err(e @ (Error::LibraryBusy(_) | Error::InvalidInput(_))) => {
            let m = lib.load_meeting(meeting_id)?;
            out.speakers_before = count_shown(&m);
            out.speakers_after = out.speakers_before;
            out.skipped = Some(e.to_string());
            return Ok(out);
        }
        Err(e) => return Err(e),
    };
    out.speakers_before = count_shown(&meeting);
    let before = (meeting.speaker_groups.clone(), meeting.review_notes.clone());
    let proven = caption_proven(&lib, &meeting)?;
    let change = apply_auto_groups(&mut meeting, model, thresholds, &proven)?;
    out.groups_added = change.groups_added;
    out.speakers_after = count_shown(&meeting);
    let renamed = resolve_kept_against_members(&lib, &mut meeting, model, thresholds)?;
    if (meeting.speaker_groups.clone(), meeting.review_notes.clone()) != before
        || !renamed.is_empty()
    {
        mark_grouping_stale(&mut meeting);
        save_and_index(tx, index, &mut meeting)?;
        super::sync_weak_samples(tx, &meeting, &renamed)?;
    }
    Ok(out)
}

/// [`super::resolve_rejected_kept`] with the meeting's roster and the
/// stored samples of `model`.
fn resolve_kept_against_members(
    lib: &Library,
    meeting: &mut Meeting,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<Vec<super::SpeakerChange>> {
    let people = super::load_person_samples(lib, model)?;
    let roster = meeting.roster_person_ids();
    super::resolve_rejected_kept(meeting, model, &people, thresholds, Some(&roster))
}
