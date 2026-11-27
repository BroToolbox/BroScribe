//! Automatic speaker grouping of one meeting (design §6.4): over-split
//! clusters of the same voice become a reversible overlay
//! ([`SpeakerGroup`]), never a rewrite of speakers, segments or samples.

use std::collections::{BTreeMap, BTreeSet};

use super::{MergeConstraints, MergeInput, merge_clusters_with};
use crate::schema::{Assignment, GroupOrigin, Meeting, Speaker, SpeakerGroup};
use crate::{PersonId, Result, SpeakerId};

/// The person `speaker` is proven to be: its confirmed person, else the
/// person `proven` (caption-proven auto names of its meeting) lists for it.
pub fn proven_person<'a>(
    speaker: &'a Speaker,
    proven: &'a BTreeMap<SpeakerId, PersonId>,
) -> Option<&'a PersonId> {
    match speaker.assignment {
        Assignment::Confirmed => speaker.person_id.as_ref(),
        _ => proven.get(&speaker.id),
    }
}

/// `speaker` as [`super::merge_clusters`] sees it: its centroid of `model`
/// (empty without one, or when it is not a finite vector: such a speaker
/// never merges), talk time, proven person and rejections.
pub fn merge_input(
    speaker: &Speaker,
    model: &str,
    proven: &BTreeMap<SpeakerId, PersonId>,
) -> Result<MergeInput> {
    Ok(MergeInput {
        id: speaker.id,
        centroid: match speaker.centroid.as_ref().filter(|c| c.model == model) {
            Some(c) => super::usable_vector(&c.vector_b64, c.dim)?.unwrap_or_default(),
            None => Vec::new(),
        },
        talk_s: speaker.talk_s,
        confirmed_person: proven_person(speaker, proven).cloned(),
        rejected: speaker.rejected_person_ids.clone(),
    })
}

/// The grouping overlay `meeting` should have: its `User` groups as they
/// are, its `Auto` groups (kept, possibly grown) and new `Auto` groups of
/// same-track speakers whose voices are at least `auto_merge` similar
/// ([`super::merge_clusters_with`] over centroids of `model`, per track,
/// never on the paired microphone track). Existing groups start formed:
/// `User` groups never change, `Auto` groups may grow. `not_same` pairs and
/// proven identities (confirmed, or caption-proven in `proven`) are never
/// grouped against the rule. `roster_floor` bounds the speakers shown in
/// the whole meeting: a track's floor is the roster minus the speakers
/// shown on the other tracks (a group counts once there, whether formed
/// earlier or by this run), at least 1. Without `auto_merge` the overlay is
/// returned as is. Ordered by kept speaker.
pub fn auto_groups(
    meeting: &Meeting,
    model: &str,
    auto_merge: Option<f32>,
    roster_floor: Option<usize>,
    proven: &BTreeMap<SpeakerId, PersonId>,
) -> Result<Vec<SpeakerGroup>> {
    let Some(threshold) = auto_merge else {
        return Ok(meeting.speaker_groups.clone());
    };
    let paired_mic = meeting.paired_mic_track();
    let tracks: BTreeSet<&str> = meeting
        .speakers
        .iter()
        .map(|s| s.track.as_str())
        .filter(|t| Some(*t) != paired_mic)
        .collect();
    let mut out: Vec<SpeakerGroup> = meeting
        .speaker_groups
        .iter()
        .filter(|g| {
            meeting
                .speaker(g.kept)
                .is_none_or(|s| !tracks.contains(s.track.as_str()))
        })
        .cloned()
        .collect();
    let mut done: BTreeSet<&str> = BTreeSet::new();
    for &track in &tracks {
        let inputs: Vec<MergeInput> = meeting
            .speakers
            .iter()
            .filter(|s| s.track == track)
            .map(|s| merge_input(s, model, proven))
            .collect::<Result<_>>()?;
        let existing: Vec<&SpeakerGroup> = meeting
            .speaker_groups
            .iter()
            .filter(|g| meeting.speaker(g.kept).is_some_and(|s| s.track == track))
            .collect();
        let constraints = MergeConstraints {
            not_same: meeting.not_same.clone(),
            units: existing
                .iter()
                .map(|g| (g.speakers().collect(), g.origin == GroupOrigin::User))
                .collect(),
        };
        // Speakers shown on the other tracks: members of their groups
        // (decided by this run for tracks done already, else existing) are
        // not shown on their own.
        let grouped_elsewhere: BTreeSet<SpeakerId> = out
            .iter()
            .chain(meeting.speaker_groups.iter().filter(|g| {
                meeting.speaker(g.kept).is_some_and(|s| {
                    s.track != track
                        && tracks.contains(s.track.as_str())
                        && !done.contains(s.track.as_str())
                })
            }))
            .flat_map(|g| g.members.iter().copied())
            .collect();
        let other = meeting
            .speakers
            .iter()
            .filter(|s| s.track != track && !grouped_elsewhere.contains(&s.id))
            .count();
        let floor = roster_floor.map(|n| n.saturating_sub(other).max(1));
        for formed in merge_clusters_with(&inputs, Some(threshold), floor, &constraints) {
            if formed.merged.is_empty() {
                continue;
            }
            let set: BTreeSet<SpeakerId> = std::iter::once(formed.kept)
                .chain(formed.merged.iter().copied())
                .collect();
            if let Some(same) = existing
                .iter()
                .find(|g| g.speakers().collect::<BTreeSet<_>>() == set)
            {
                out.push((*same).clone());
                continue;
            }
            // Grown from earlier Auto groups: the lowest similarity joined.
            let score = existing
                .iter()
                .filter(|g| set.contains(&g.kept))
                .filter_map(|g| match g.origin {
                    GroupOrigin::Auto { score } => Some(score),
                    GroupOrigin::User => None,
                })
                .chain(formed.score)
                .reduce(f32::min)
                .unwrap_or(threshold);
            out.push(SpeakerGroup::new(
                formed.kept,
                formed.merged,
                GroupOrigin::Auto { score },
            ));
        }
        done.insert(track);
    }
    out.sort_by_key(|g| g.kept);
    Ok(out)
}
