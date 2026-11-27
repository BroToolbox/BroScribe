//! Scoring a speaker against enrolled people, resolving the best match,
//! merging over-split clusters within one meeting and capping stored samples.

use std::collections::BTreeSet;
use std::ops::Range;

use serde::{Deserialize, Serialize};

use super::{cosine, mean_normalized, weighted_mean_normalized};
use crate::config::Thresholds;
use crate::ids::{PersonId, SpeakerId};
use crate::schema::Assignment;

/// Weight of a weak (auto-named) sample in [`score`]; confirmed samples weigh 1.
pub const WEAK_WEIGHT: f32 = 0.5;
/// Confirmed samples kept per person and model ([`cap_samples`]).
pub const MAX_CONFIRMED: usize = 50;
/// Weak samples kept per person and model ([`cap_weak`]).
pub const MAX_WEAK: usize = 10;
/// Number of samples averaged by [`score`].
const TOP_K: usize = 3;

/// One person's stored embeddings for a single embedding model.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonSamples {
    pub person_id: PersonId,
    pub confirmed: Vec<Vec<f32>>,
    pub weak: Vec<Vec<f32>>,
}

/// Score of a speaker centroid against one person (plan §13.8, §16.5): the
/// top 3 confirmed samples by cosine, filled with the best weak samples when
/// fewer than 3 confirmed exist, averaged with weights 1 (confirmed) and
/// [`WEAK_WEIGHT`] (weak). Samples whose cosine is not finite (a NaN or
/// infinite vector) are ignored. `None` when the person has no usable
/// sample.
pub fn score(centroid: &[f32], person: &PersonSamples) -> Option<f32> {
    score_pool(centroid, person, Pool::Ordinary)
}

/// Which of a person's samples [`score_pool`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pool {
    /// Confirmed and weak samples ([`score`]).
    Ordinary,
    /// Confirmed samples only: the trust pool of [`resolve_voice`].
    Confirmed,
}

/// The `k` (at most [`TOP_K`]) highest finite cosines of `centroid` to
/// `samples`, best first, and how many there are: one pass with a bounded
/// insertion, no allocation and no sort of every score.
fn top_cosines(centroid: &[f32], samples: &[Vec<f32>], k: usize) -> ([f32; TOP_K], usize) {
    let k = k.min(TOP_K);
    let mut best = [f32::NEG_INFINITY; TOP_K];
    let mut n = 0;
    if k == 0 {
        return (best, 0);
    }
    for v in samples {
        let c = cosine(centroid, v);
        if !c.is_finite() {
            continue;
        }
        let mut i = if n < k {
            n += 1;
            n - 1
        } else if c > best[k - 1] {
            k - 1
        } else {
            continue;
        };
        best[i] = c;
        while i > 0 && best[i] > best[i - 1] {
            best.swap(i, i - 1);
            i -= 1;
        }
    }
    (best, n)
}

fn score_pool(centroid: &[f32], person: &PersonSamples, pool: Pool) -> Option<f32> {
    let (confirmed, nc) = top_cosines(centroid, &person.confirmed, TOP_K);
    let (weak, nw) = match pool {
        Pool::Ordinary => top_cosines(centroid, &person.weak, TOP_K - nc),
        Pool::Confirmed => ([0.0; TOP_K], 0),
    };
    if nc == 0 && nw == 0 {
        return None;
    }
    let num: f32 =
        confirmed[..nc].iter().sum::<f32>() + WEAK_WEIGHT * weak[..nw].iter().sum::<f32>();
    let den = nc as f32 + WEAK_WEIGHT * nw as f32;
    Some(num / den)
}

/// How a resolver or prediction names a speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameKind {
    Auto,
    Suggested,
}

impl From<NameKind> for Assignment {
    fn from(kind: NameKind) -> Self {
        match kind {
            NameKind::Auto => Assignment::Auto,
            NameKind::Suggested => Assignment::Suggested,
        }
    }
}

/// Outcome of [`resolve`]. `person_id` and `score` describe the best
/// non-rejected candidate even when `assignment` is `None`;
/// `runner_up_score` is the second-best candidate's score.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Resolution {
    /// `Auto`, `Suggested` or `None`; never `Confirmed`.
    pub assignment: Assignment,
    pub person_id: Option<PersonId>,
    pub score: Option<f32>,
    pub runner_up_score: Option<f32>,
}

/// One candidate of [`rank`]: the raw [`score`] and the score the order uses
/// (`raw` plus the roster boost for people on the meeting's roster).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Ranked {
    pub person_id: PersonId,
    pub score: f32,
    pub boosted: f32,
}

/// Every candidate for a speaker centroid, best first: the order [`resolve`]
/// picks from (design §6.4).
///
/// People in `rejected` and people without samples are left out. People in
/// `roster` are ranked by their raw score plus half the calibrated margin
/// (`thresholds.margin`), capped at 0.03; ties keep person-ID order.
pub fn rank(
    centroid: &[f32],
    people: &[PersonSamples],
    rejected: &[PersonId],
    roster: Option<&BTreeSet<PersonId>>,
    thresholds: Option<&Thresholds>,
) -> Vec<Ranked> {
    rank_pool(
        centroid,
        people,
        rejected,
        roster,
        thresholds,
        Pool::Ordinary,
    )
}

fn rank_pool(
    centroid: &[f32],
    people: &[PersonSamples],
    rejected: &[PersonId],
    roster: Option<&BTreeSet<PersonId>>,
    thresholds: Option<&Thresholds>,
    pool: Pool,
) -> Vec<Ranked> {
    // A NaN or infinite centroid matches no one (never a NaN score first).
    if !centroid.iter().all(|x| x.is_finite()) {
        return Vec::new();
    }
    let margin = thresholds.and_then(|t| t.margin).unwrap_or(0.0);
    let boost = (0.5 * margin.max(0.0)).min(0.03);
    let mut ranked: Vec<Ranked> = people
        .iter()
        .filter(|p| !rejected.contains(&p.person_id))
        .filter_map(|p| {
            let raw = score_pool(centroid, p, pool)?;
            let on_roster = roster.is_some_and(|r| r.contains(&p.person_id));
            Some(Ranked {
                person_id: p.person_id.clone(),
                score: raw,
                boosted: raw + if on_roster { boost } else { 0.0 },
            })
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.boosted
            .total_cmp(&a.boosted)
            .then_with(|| a.person_id.cmp(&b.person_id))
    });
    ranked
}

/// Matches a speaker centroid against every person (open set, design §6.4):
/// [`resolve_ranked`] over [`rank`], the ordinary evidence pool (confirmed
/// and weak samples).
///
/// People in `rejected` (the speaker's `rejected_person_ids`) and people
/// without samples are never candidates. Callers never apply this to
/// `confirmed` speakers: a confirmation is final. Paths that publish or
/// display a voice-derived name use [`resolve_voice`], which adds the
/// confirmed-only agreement gate; this ordinary resolution is its first
/// step and what calibration measures.
pub fn resolve(
    centroid: &[f32],
    people: &[PersonSamples],
    rejected: &[PersonId],
    thresholds: Option<&Thresholds>,
    roster: Option<&BTreeSet<PersonId>>,
) -> Resolution {
    resolve_ranked(
        &rank(centroid, people, rejected, roster, thresholds),
        thresholds,
    )
}

/// Names the first entry of a [`rank`] result (computed with the same
/// `thresholds`). The runner-up is the highest raw score among the other
/// candidates.
///
/// - Without thresholds (none calibrated for the model, or neither
///   `auto_name` nor `suggest` set): the top match is suggested.
/// - With thresholds: `Auto` when score ≥ `auto_name` and the lead over the
///   runner-up is ≥ `margin` (unset margin = 0; no runner-up = any lead);
///   otherwise `Suggested` when the boosted score ≥ `suggest` (unset
///   `suggest` = the top match is suggested); otherwise `None`.
///
/// The roster boost only ranks and suggests: auto-naming always requires the
/// raw score and raw lead.
pub fn resolve_ranked(ranked: &[Ranked], thresholds: Option<&Thresholds>) -> Resolution {
    let Some(best) = ranked.first() else {
        return Resolution {
            assignment: Assignment::None,
            person_id: None,
            score: None,
            runner_up_score: None,
        };
    };
    let runner_up_score = ranked[1..].iter().map(|r| r.score).max_by(f32::total_cmp);
    let calibrated = thresholds.filter(|t| t.auto_name.is_some() || t.suggest.is_some());
    let assignment = match calibrated {
        None => Assignment::Suggested,
        Some(t) => {
            let lead_ok = runner_up_score.is_none_or(|r| best.score - r >= t.margin.unwrap_or(0.0));
            if t.auto_name.is_some_and(|a| best.score >= a) && lead_ok {
                Assignment::Auto
            } else {
                below_auto(t, best)
            }
        }
    };
    Resolution {
        assignment,
        person_id: Some(best.person_id.clone()),
        score: Some(best.score),
        runner_up_score,
    }
}

/// What a top candidate that is not auto-named gets under calibrated
/// thresholds: `Suggested` when its boosted score reaches `suggest` (unset
/// `suggest` = suggested), otherwise `None`. Never below `suggest`.
fn below_auto(t: &Thresholds, best: &Ranked) -> Assignment {
    if t.suggest.is_none_or(|s| best.boosted >= s) {
        Assignment::Suggested
    } else {
        Assignment::None
    }
}

/// The resolution every path that publishes or displays a voice-derived
/// name uses (design §6.4, "Automatic names need confirmed evidence"):
///
/// 1. [`resolve`] over the ordinary pool (confirmed and weak samples).
/// 2. When that is `Auto(P)` and the speaker is not already auto-named `P`
///    (`current_auto`), the same resolver with the same `rejected`,
///    `roster` and `thresholds` over **confirmed samples only** (every
///    person with a confirmed sample competes) must also return `Auto(P)`.
///    Otherwise the speaker gets what the ordinary resolution gives below
///    `auto_name`: `Suggested(P)` when its boosted score reaches `suggest`
///    (unset = suggested), else `None`; never a suggestion below `suggest`.
///
/// An existing `Auto(P)` that the ordinary resolution still names `P` is
/// kept without the second check: the gate guards new and replacement
/// automatic names, so weak samples alone can never start or carry a
/// cascade, while names learned before it are not demoted by it alone.
/// Caption and paired-microphone identities do not come from here and keep
/// their own rules.
pub fn resolve_voice(
    centroid: &[f32],
    people: &[PersonSamples],
    rejected: &[PersonId],
    thresholds: Option<&Thresholds>,
    roster: Option<&BTreeSet<PersonId>>,
    current_auto: Option<&PersonId>,
) -> Resolution {
    let ranked = rank(centroid, people, rejected, roster, thresholds);
    let mut ordinary = resolve_ranked(&ranked, thresholds);
    let (Assignment::Auto, Some(person), Some(t), Some(best)) = (
        ordinary.assignment,
        ordinary.person_id.as_ref(),
        thresholds,
        ranked.first(),
    ) else {
        return ordinary;
    };
    if current_auto == Some(person) {
        return ordinary;
    }
    let confirmed = resolve_ranked(
        &rank_pool(
            centroid,
            people,
            rejected,
            roster,
            thresholds,
            Pool::Confirmed,
        ),
        thresholds,
    );
    if confirmed.assignment == Assignment::Auto && confirmed.person_id.as_ref() == Some(person) {
        return ordinary;
    }
    ordinary.assignment = below_auto(t, best);
    ordinary
}

/// One diarized speaker of a meeting, as seen by [`merge_clusters`].
#[derive(Clone, Debug, PartialEq)]
pub struct MergeInput {
    pub id: SpeakerId,
    /// Empty when the speaker has no centroid; such speakers never merge.
    pub centroid: Vec<f32>,
    pub talk_s: f64,
    /// The person the speaker is proven to be: its assignment is
    /// `confirmed`, or (for meeting speakers) it is auto-named with a
    /// caption sample from this meeting.
    pub confirmed_person: Option<PersonId>,
    pub rejected: Vec<PersonId>,
}

/// A group of speakers that are one voice. `kept` keeps its ID; `merged`
/// (sorted) are folded into it; `rejected_union` (sorted, deduplicated) is the
/// union of every member's rejected people.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MergeGroup {
    pub kept: SpeakerId,
    pub merged: Vec<SpeakerId>,
    pub rejected_union: Vec<PersonId>,
    /// The lowest similarity at which this run joined the group; `None`
    /// when it joined nothing (a singleton or an unchanged starting unit).
    pub score: Option<f32>,
}

/// What [`merge_clusters_with`] must respect besides identities.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MergeConstraints {
    /// Speaker pairs never put in one group (the user separated them).
    pub not_same: Vec<[SpeakerId; 2]>,
    /// Groups formed before this run, with whether they are fixed: a fixed
    /// group never merges with anything, another may grow.
    pub units: Vec<(Vec<SpeakerId>, bool)>,
}

struct Group {
    members: Vec<usize>,
    centroid: Option<Vec<f32>>,
    confirmed: BTreeSet<PersonId>,
    rejected: BTreeSet<PersonId>,
    fixed: bool,
    score: Option<f32>,
}

impl Group {
    fn of(speakers: &[MergeInput], members: Vec<usize>, fixed: bool) -> Self {
        Self {
            centroid: group_centroid(speakers, &members),
            confirmed: members
                .iter()
                .filter_map(|&i| speakers[i].confirmed_person.clone())
                .collect(),
            rejected: members
                .iter()
                .flat_map(|&i| speakers[i].rejected.iter().cloned())
                .collect(),
            members,
            fixed,
            score: None,
        }
    }

    /// The compatibility rule shared by automatic grouping, suggestions and
    /// user merges: two groups may merge unless the union of their proven
    /// people would hold two different people, one group proves a person
    /// the other rejected, or a pair across them was separated (`not_same`).
    fn compatible(
        &self,
        other: &Group,
        speakers: &[MergeInput],
        not_same: &[[SpeakerId; 2]],
    ) -> bool {
        self.confirmed.union(&other.confirmed).nth(1).is_none()
            && self.confirmed.is_disjoint(&other.rejected)
            && other.confirmed.is_disjoint(&self.rejected)
            && !self.members.iter().any(|&a| {
                other.members.iter().any(|&b| {
                    let pair = sorted_pair(speakers[a].id, speakers[b].id);
                    not_same.contains(&pair)
                })
            })
    }
}

fn sorted_pair(a: SpeakerId, b: SpeakerId) -> [SpeakerId; 2] {
    if a <= b { [a, b] } else { [b, a] }
}

/// Whether `speakers` may be one group under the rule [`merge_clusters`]
/// applies (proven people, rejections and `not_same`).
pub fn compatible(speakers: &[MergeInput], not_same: &[[SpeakerId; 2]]) -> bool {
    let mut all: Option<Group> = None;
    for i in 0..speakers.len() {
        let one = Group::of(speakers, vec![i], false);
        match &mut all {
            None => all = Some(one),
            Some(g) => {
                if !g.compatible(&one, speakers, not_same) {
                    return false;
                }
                g.members.extend(one.members);
                g.confirmed.extend(one.confirmed);
                g.rejected.extend(one.rejected);
            }
        }
    }
    true
}

/// The kept speaker of `members` (indices into `speakers`): the proven
/// member with the most talk time when one exists, else the member with
/// the most talk time; ties go to the lowest ID.
pub fn kept_speaker(speakers: &[MergeInput], members: &[usize]) -> Option<usize> {
    let has_proven = members
        .iter()
        .any(|&i| speakers[i].confirmed_person.is_some());
    members
        .iter()
        .copied()
        .filter(|&i| !has_proven || speakers[i].confirmed_person.is_some())
        .max_by(|&a, &b| {
            speakers[a]
                .talk_s
                .total_cmp(&speakers[b].talk_s)
                .then_with(|| speakers[b].id.cmp(&speakers[a].id))
        })
}

/// Talk-weighted centroid of `members`; `None` if any member lacks a usable
/// centroid.
pub(crate) fn group_centroid(speakers: &[MergeInput], members: &[usize]) -> Option<Vec<f32>> {
    if members
        .iter()
        .any(|&i| speakers[i].centroid.iter().all(|&x| x == 0.0))
    {
        return None;
    }
    weighted_mean_normalized(members.iter().map(|&i| {
        (
            speakers[i].centroid.as_slice(),
            speakers[i].talk_s.max(0.0) + 1e-6,
        )
    }))
}

/// Merges over-split clusters of one meeting (design §6.4 step 2):
/// [`merge_clusters_with`] without constraints.
pub fn merge_clusters(
    speakers: &[MergeInput],
    merge: Option<f32>,
    roster_floor: Option<usize>,
) -> Vec<MergeGroup> {
    merge_clusters_with(speakers, merge, roster_floor, &MergeConstraints::default())
}

/// Merges over-split clusters of one meeting (design §6.4).
///
/// Agglomerative over groups, starting from `constraints.units` (formed
/// earlier) and singletons: the most similar compatible pair (cosine of
/// talk-weighted group centroids) merges first; merging stops when the best
/// pair is below `merge` or when one more merge would leave fewer groups than
/// `roster_floor` (the roster size is a floor, never a forced count). `None`
/// threshold → no merges. Groups are never merged if their proven people
/// would include two different people (or a person the other group
/// rejected), across a `not_same` pair, or when either is fixed. The kept ID
/// is [`kept_speaker`]'s.
///
/// Returns every group, singletons included, ordered by kept ID.
pub fn merge_clusters_with(
    speakers: &[MergeInput],
    merge: Option<f32>,
    roster_floor: Option<usize>,
    constraints: &MergeConstraints,
) -> Vec<MergeGroup> {
    let index = |id: &SpeakerId| speakers.iter().position(|s| s.id == *id);
    let mut placed = vec![false; speakers.len()];
    let mut groups: Vec<Group> = Vec::new();
    for (ids, fixed) in &constraints.units {
        let members: Vec<usize> = ids
            .iter()
            .filter_map(index)
            .filter(|&i| !std::mem::replace(&mut placed[i], true))
            .collect();
        if !members.is_empty() {
            groups.push(Group::of(speakers, members, *fixed));
        }
    }
    for (i, _) in placed.iter().enumerate().filter(|(_, p)| !**p) {
        groups.push(Group::of(speakers, vec![i], false));
    }

    if let Some(threshold) = merge {
        loop {
            if roster_floor.is_some_and(|floor| groups.len() <= floor) {
                break;
            }
            let mut best: Option<(usize, usize, f32)> = None;
            for (i, gi) in groups.iter().enumerate() {
                let Some(ci) = gi.centroid.as_ref().filter(|_| !gi.fixed) else {
                    continue;
                };
                for (j, gj) in groups.iter().enumerate().skip(i + 1) {
                    let Some(cj) = gj.centroid.as_ref().filter(|_| !gj.fixed) else {
                        continue;
                    };
                    if !gi.compatible(gj, speakers, &constraints.not_same) {
                        continue;
                    }
                    let sim = cosine(ci, cj);
                    if best.is_none_or(|(_, _, b)| sim > b) {
                        best = Some((i, j, sim));
                    }
                }
            }
            let Some((i, j, sim)) = best else { break };
            if sim.is_nan() || sim < threshold {
                break;
            }
            let absorbed = groups.remove(j);
            let target = &mut groups[i];
            target.members.extend(absorbed.members);
            target.confirmed.extend(absorbed.confirmed);
            target.rejected.extend(absorbed.rejected);
            target.centroid = group_centroid(speakers, &target.members);
            target.score = [target.score, absorbed.score, Some(sim)]
                .into_iter()
                .flatten()
                .reduce(f32::min);
        }
    }

    let mut out: Vec<MergeGroup> = groups
        .into_iter()
        .map(|g| {
            let kept_idx = kept_speaker(speakers, &g.members).unwrap_or(g.members[0]);
            let mut merged: Vec<SpeakerId> = g
                .members
                .iter()
                .filter(|&&i| i != kept_idx)
                .map(|&i| speakers[i].id)
                .collect();
            merged.sort();
            MergeGroup {
                kept: speakers[kept_idx].id,
                merged,
                rejected_union: g.rejected.into_iter().collect(),
                score: g.score,
            }
        })
        .collect();
    out.sort_by_key(|g| g.kept);
    out
}

/// Diverse subset of at most `max` confirmed samples (plan §13.8, normally
/// [`MAX_CONFIRMED`]): farthest-point selection seeded with the sample closest
/// to the centroid, each next pick maximising its smallest cosine distance to
/// the picks so far. Returns the kept indices in ascending order. Within the
/// cap everything is kept; past it, a sample that is not a finite vector
/// ([`super::finite_vector`]) is never kept, and selection runs on the
/// finite ones only.
pub fn cap_samples<V: AsRef<[f32]>>(confirmed: &[V], max: usize) -> Vec<usize> {
    let n = confirmed.len();
    if n <= max {
        return (0..n).collect();
    }
    let usable: Vec<usize> = (0..n)
        .filter(|&i| super::finite_vector(confirmed[i].as_ref()))
        .collect();
    if usable.len() <= max {
        return usable;
    }
    if max == 0 {
        return Vec::new();
    }
    let vectors: Vec<&[f32]> = usable.iter().map(|&i| confirmed[i].as_ref()).collect();
    let argmax = |values: &[f32]| {
        values
            .iter()
            .enumerate()
            .fold((0, f32::NEG_INFINITY), |(bi, bv), (i, &v)| {
                if v > bv { (i, v) } else { (bi, bv) }
            })
            .0
    };
    let seed = match mean_normalized(&vectors) {
        Some(c) => {
            let sims: Vec<f32> = vectors.iter().map(|v| cosine(&c, v)).collect();
            argmax(&sims)
        }
        None => 0,
    };
    let mut picked = vec![seed];
    let mut min_dist: Vec<f32> = vectors
        .iter()
        .map(|v| 1.0 - cosine(vectors[seed], v))
        .collect();
    min_dist[seed] = f32::NEG_INFINITY;
    while picked.len() < max {
        let next = argmax(&min_dist);
        picked.push(next);
        // Picked samples stay at −∞ (min with −∞ is −∞).
        min_dist[next] = f32::NEG_INFINITY;
        let chosen = vectors[next];
        for (d, v) in min_dist.iter_mut().zip(&vectors) {
            *d = d.min(1.0 - cosine(chosen, v));
        }
    }
    let mut kept: Vec<usize> = picked.into_iter().map(|k| usable[k]).collect();
    kept.sort_unstable();
    kept
}

/// Weak samples to keep out of `count` given oldest first: the newest `max`
/// (normally [`MAX_WEAK`]); older ones are dropped.
pub fn cap_weak(count: usize, max: usize) -> Range<usize> {
    count.saturating_sub(max)..count
}

#[cfg(test)]
mod tests {
    use super::super::l2_normalize;
    use super::super::testutil::{Rng, basis};
    use super::*;

    fn pid(hex: &str) -> PersonId {
        PersonId::parse(&format!("p-{hex}")).unwrap()
    }

    fn sid(n: u32) -> SpeakerId {
        SpeakerId::new(n).unwrap()
    }

    /// Unit vector in 2-D with cosine `c` to `[1, 0]`.
    fn at(c: f32) -> Vec<f32> {
        vec![c, (1.0 - c * c).max(0.0).sqrt()]
    }

    fn unit(v: &[f32]) -> Vec<f32> {
        let mut v = v.to_vec();
        l2_normalize(&mut v);
        v
    }

    fn person(hex: &str, confirmed: &[f32], weak: &[f32]) -> PersonSamples {
        PersonSamples {
            person_id: pid(hex),
            confirmed: confirmed.iter().map(|&c| at(c)).collect(),
            weak: weak.iter().map(|&c| at(c)).collect(),
        }
    }

    /// The configured shape the gate is checked against: 0.90 / 0.10.
    fn gate_thresholds(suggest: f32) -> Thresholds {
        Thresholds {
            auto_name: Some(0.90),
            suggest: Some(suggest),
            margin: Some(0.10),
            merge: None,
            auto_merge: None,
        }
    }

    #[test]
    fn voice_auto_needs_confirmed_only_agreement() {
        let x = [1.0, 0.0];
        let t = gate_thresholds(0.85);
        let voice = |people: &[PersonSamples], t: &Thresholds, current: Option<&PersonId>| {
            resolve_voice(&x, people, &[], Some(t), None, current)
        };

        // All-weak evidence: the ordinary pool auto-names, the confirmed
        // pool has no candidate, so the speaker is only suggested…
        let weak_only = [person("a00001", &[], &[0.99, 0.99])];
        assert_eq!(
            resolve(&x, &weak_only, &[], Some(&t), None).assignment,
            Assignment::Auto
        );
        let r = voice(&weak_only, &t, None);
        assert_eq!(r.assignment, Assignment::Suggested);
        assert_eq!(r.person_id, Some(pid("a00001")));
        // …and with `suggest` above the score (even above `auto_name`) it
        // is left unassigned: no suggestion below the configured threshold.
        assert_eq!(
            voice(&weak_only, &gate_thresholds(0.995), None).assignment,
            Assignment::None
        );
        // An existing Auto the ordinary resolver still names is kept.
        assert_eq!(
            voice(&weak_only, &t, Some(&pid("a00001"))).assignment,
            Assignment::Auto
        );

        // Few confirmed plus weak: both pools agree.
        let few = [person("a00001", &[0.95], &[0.99, 0.99])];
        assert_eq!(voice(&few, &t, None).assignment, Assignment::Auto);

        // Weak samples lift the ordinary score over auto_name; the
        // confirmed sample alone stays below it.
        let lifted = [person("a00001", &[0.82], &[1.0, 1.0])];
        assert_eq!(
            resolve(&x, &lifted, &[], Some(&t), None).assignment,
            Assignment::Auto
        );
        assert_eq!(voice(&lifted, &t, None).assignment, Assignment::Suggested);

        // A confirmed competitor within the margin of the confirmed pool
        // blocks a new Auto, though weak samples give the ordinary lead.
        let close = [
            person("a00001", &[0.93], &[0.99, 0.99]),
            person("b00002", &[0.85], &[]),
        ];
        assert_eq!(
            resolve(&x, &close, &[], Some(&t), None).assignment,
            Assignment::Auto
        );
        let r = voice(&close, &t, None);
        assert_eq!(r.assignment, Assignment::Suggested);
        assert_eq!(r.person_id, Some(pid("a00001")));
        // A replacement of another person's Auto passes the same gate.
        assert_eq!(
            voice(&close, &t, Some(&pid("b00002"))).assignment,
            Assignment::Suggested
        );

        // Uncalibrated or ordinary non-Auto results are untouched.
        assert_eq!(
            resolve_voice(&x, &weak_only, &[], None, None, None).assignment,
            Assignment::Suggested
        );
    }

    #[test]
    fn score_uses_weighted_top_three() {
        let x = [1.0, 0.0];
        let p = person("a00001", &[0.9, 0.1, 0.8, 0.7], &[1.0]);
        assert!((score(&x, &p).unwrap() - 0.8).abs() < 1e-5);
        let p = person("a00001", &[0.9], &[0.4, 0.6, 0.5]);
        let expected = (0.9 + 0.5 * 0.6 + 0.5 * 0.5) / 2.0;
        assert!((score(&x, &p).unwrap() - expected).abs() < 1e-5);
        assert_eq!(score(&x, &person("a00001", &[], &[])), None);
    }

    #[test]
    fn resolve_honours_rejections_and_thresholds() {
        let x = [1.0, 0.0];
        let ana = person("a00001", &[0.9, 0.9, 0.9], &[]);
        let bob = person("b00002", &[0.6], &[]);
        let nobody = person("c00003", &[], &[]);
        let people = [ana.clone(), bob.clone(), nobody];

        let r = resolve(&x, &people, &[], None, None);
        assert_eq!(r.assignment, Assignment::Suggested);
        assert_eq!(r.person_id, Some(ana.person_id.clone()));

        let t = Thresholds {
            auto_name: Some(0.8),
            suggest: Some(0.5),
            margin: Some(0.1),
            merge: None,
            auto_merge: None,
        };
        let r = resolve(&x, &people, &[], Some(&t), None);
        assert_eq!(r.assignment, Assignment::Auto);
        assert!((r.runner_up_score.unwrap() - 0.6).abs() < 1e-5);

        let r = resolve(
            &x,
            &people,
            std::slice::from_ref(&ana.person_id),
            Some(&t),
            None,
        );
        assert_eq!(r.assignment, Assignment::Suggested);
        assert_eq!(r.person_id, Some(bob.person_id.clone()));
        assert_eq!(r.runner_up_score, None);

        let wide = Thresholds {
            margin: Some(0.4),
            ..t
        };
        assert_eq!(
            resolve(&x, &people, &[], Some(&wide), None).assignment,
            Assignment::Suggested
        );

        let high = Thresholds {
            auto_name: Some(0.97),
            suggest: Some(0.95),
            ..t
        };
        let r = resolve(&x, &people, &[], Some(&high), None);
        assert_eq!(r.assignment, Assignment::None);
        assert_eq!(r.person_id, Some(ana.person_id.clone()));

        let merge_only = Thresholds {
            merge: Some(0.9),
            ..Thresholds::default()
        };
        assert_eq!(
            resolve(&x, &people, &[], Some(&merge_only), None).assignment,
            Assignment::Suggested
        );

        let all = [ana.person_id.clone(), bob.person_id.clone()];
        let r = resolve(&x, &people, &all, None, None);
        assert_eq!((r.assignment, r.person_id), (Assignment::None, None));
    }

    #[test]
    fn roster_boost_ranks_suggestions_but_never_supplies_auto_evidence() {
        let x = [1.0, 0.0];
        let a = person("a00001", &[0.79], &[]);
        let b = person("b00002", &[0.78], &[]);
        let roster = BTreeSet::from([b.person_id.clone()]);
        let people = [a.clone(), b.clone()];
        let t = Thresholds {
            auto_name: Some(0.8),
            suggest: Some(0.75),
            margin: Some(0.1),
            merge: None,
            auto_merge: None,
        };
        let r = resolve(&x, &people, &[], Some(&t), Some(&roster));
        assert_eq!(r.person_id, Some(b.person_id.clone()));
        assert_eq!(r.assignment, Assignment::Suggested);
        assert!((r.score.unwrap() - 0.78).abs() < 1e-5);
        let people = [person("a00001", &[0.81], &[]), b.clone()];
        assert_eq!(
            resolve(&x, &people, &[], Some(&t), Some(&roster)).person_id,
            Some(a.person_id.clone()),
            "roster boost is capped at 0.03"
        );
        let people = [
            person("a00001", &[0.82], &[]),
            person("b00002", &[0.84], &[]),
        ];
        let r = resolve(&x, &people, &[], Some(&t), Some(&roster));
        assert_eq!(
            r.assignment,
            Assignment::Suggested,
            "raw lead must pass margin"
        );
        let t = Thresholds {
            margin: Some(0.02),
            ..t
        };
        let people = [
            person("a00001", &[0.795], &[]),
            person("b00002", &[0.79], &[]),
        ];
        assert_eq!(
            resolve(&x, &people, &[], Some(&t), Some(&roster)).assignment,
            Assignment::Suggested,
            "boost must not cross the auto-name threshold"
        );
        let people = [
            person("a00001", &[0.795], &[]),
            person("b00002", &[0.78], &[]),
        ];
        assert_eq!(
            resolve(&x, &people, &[], Some(&t), Some(&roster)).person_id,
            Some(a.person_id),
            "boost is at most half the margin"
        );
    }

    fn ids(ranked: &[Ranked]) -> Vec<PersonId> {
        ranked.iter().map(|r| r.person_id.clone()).collect()
    }

    #[test]
    fn rank_first_is_resolve_pick_with_and_without_roster() {
        let mut rng = Rng::new(11);
        let people: Vec<PersonSamples> = (0..6)
            .map(|i| PersonSamples {
                person_id: pid(&format!("a0000{i}")),
                confirmed: rng.voices(i % 3, 3, 0.4),
                weak: rng.voices(i % 3, 2, 0.4),
            })
            .collect();
        let t = Thresholds {
            auto_name: Some(0.8),
            suggest: Some(0.5),
            margin: Some(0.06),
            merge: None,
            auto_merge: None,
        };
        let roster = BTreeSet::from([pid("a00001"), pid("a00004")]);
        for seed in 0..20 {
            let x = Rng::new(100 + seed).voice((seed % 3) as usize, 0.4);
            for roster in [None, Some(&roster)] {
                for t in [None, Some(&t)] {
                    let ranked = rank(&x, &people, &[], roster, t);
                    assert_eq!(ranked.len(), people.len());
                    let r = resolve(&x, &people, &[], t, roster);
                    assert_eq!(r.person_id.as_ref(), Some(&ranked[0].person_id));
                    assert_eq!(r.score, Some(ranked[0].score));
                    assert!(ranked.windows(2).all(|w| w[0].boosted >= w[1].boosted));
                }
            }
        }
    }

    #[test]
    fn rank_roster_boost_follows_the_margin() {
        let x = [1.0, 0.0];
        let people = [
            person("a00001", &[0.80], &[]),
            person("b00002", &[0.78], &[]),
        ];
        let roster = BTreeSet::from([people[1].person_id.clone()]);
        let with_margin = |margin| Thresholds {
            margin: Some(margin),
            ..Thresholds::default()
        };
        let narrow = with_margin(0.02);
        let wide = with_margin(0.06);
        let a_first = ids(&rank(&x, &people, &[], Some(&roster), Some(&narrow)));
        assert_eq!(
            a_first,
            [people[0].person_id.clone(), people[1].person_id.clone()]
        );
        let b_first = ids(&rank(&x, &people, &[], Some(&roster), Some(&wide)));
        assert_eq!(
            b_first,
            [people[1].person_id.clone(), people[0].person_id.clone()]
        );
        let ranked = rank(&x, &people, &[], Some(&roster), Some(&wide));
        assert!((ranked[0].score - 0.78).abs() < 1e-5, "raw score kept");
        assert!((ranked[0].boosted - 0.81).abs() < 1e-5);
        assert_eq!(
            ids(&rank(&x, &people, &[], Some(&roster), None)),
            a_first,
            "no margin, no boost"
        );
        assert_eq!(
            ids(&rank(
                &x,
                &people,
                &[people[0].person_id.clone()],
                None,
                None
            )),
            [people[1].person_id.clone()],
            "rejected people are left out"
        );
    }

    #[test]
    fn resolve_runner_up_is_the_best_raw_score_not_the_second_row() {
        let x = [1.0, 0.0];
        let a = person("a00001", &[0.90], &[]);
        let b = person("b00002", &[0.78], &[]);
        let c = person("c00003", &[0.80], &[]);
        let people = [a.clone(), b.clone(), c.clone()];
        let roster = BTreeSet::from([b.person_id.clone()]);
        let t = Thresholds {
            auto_name: Some(0.85),
            suggest: Some(0.5),
            margin: Some(0.11),
            merge: None,
            auto_merge: None,
        };
        let ranked = rank(&x, &people, &[], Some(&roster), Some(&t));
        assert_eq!(
            ids(&ranked),
            [a.person_id.clone(), b.person_id, c.person_id]
        );
        let r = resolve_ranked(&ranked, Some(&t));
        assert!((r.runner_up_score.unwrap() - 0.80).abs() < 1e-5);
        assert_eq!(
            r.assignment,
            Assignment::Suggested,
            "a leads the raw runner-up by 0.10 < margin"
        );
        assert_eq!(r, resolve(&x, &people, &[], Some(&t), Some(&roster)));
    }

    fn input(n: u32, centroid: Vec<f32>, talk_s: f64, confirmed: Option<&str>) -> MergeInput {
        MergeInput {
            id: sid(n),
            centroid,
            talk_s,
            confirmed_person: confirmed.map(pid),
            rejected: Vec::new(),
        }
    }

    #[test]
    fn merge_refuses_groups_confirmed_to_different_people() {
        // A = Ana (confirmed), B unconfirmed and closest to A, C = Bob close to AB.
        let mut b = input(2, unit(&[0.99, 0.14]), 100.0, None);
        b.rejected = vec![pid("c00003")];
        let speakers = [
            input(1, vec![1.0, 0.0], 5.0, Some("a00001")),
            b,
            input(3, unit(&[0.9, -0.44]), 50.0, Some("b00002")),
        ];
        let groups: Vec<MergeGroup> = merge_clusters(&speakers, Some(0.8), None)
            .into_iter()
            .map(|g| {
                assert_eq!(g.score.is_some(), !g.merged.is_empty());
                MergeGroup { score: None, ..g }
            })
            .collect();
        assert_eq!(
            groups,
            vec![
                MergeGroup {
                    kept: sid(1),
                    merged: vec![sid(2)],
                    rejected_union: vec![pid("c00003")],
                    score: None,
                },
                MergeGroup {
                    kept: sid(3),
                    merged: vec![],
                    rejected_union: vec![],
                    score: None,
                },
            ]
        );
    }

    #[test]
    fn merge_refuses_confirmed_person_rejected_by_other() {
        let mut x = input(2, unit(&[0.99, 0.1]), 10.0, None);
        x.rejected = vec![pid("a00001")];
        let speakers = [input(1, vec![1.0, 0.0], 10.0, Some("a00001")), x];
        assert_eq!(merge_clusters(&speakers, Some(0.5), None).len(), 2);
    }

    #[test]
    fn merge_stops_at_threshold_floor_and_without_threshold() {
        let mut rng = Rng::new(7);
        let speakers: Vec<MergeInput> = (0..4)
            .map(|i| input(i as u32 + 1, rng.voice(0, 0.05), 10.0 + f64::from(i), None))
            .collect();
        assert_eq!(merge_clusters(&speakers, None, None).len(), 4);
        assert_eq!(merge_clusters(&speakers, Some(0.5), Some(2)).len(), 2);
        assert_eq!(merge_clusters(&speakers, Some(0.5), Some(9)).len(), 4);
        let one = merge_clusters(&speakers, Some(0.5), None);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].kept, sid(4), "most talk time is kept");

        // Two people, two clusters each: merging stops below the threshold.
        let mut speakers = vec![
            input(1, rng.voice(0, 0.05), 10.0, None),
            input(2, rng.voice(1, 0.05), 10.0, None),
            input(3, rng.voice(0, 0.05), 5.0, None),
            input(4, rng.voice(1, 0.05), 5.0, None),
        ];
        speakers.push(input(5, Vec::new(), 1.0, None));
        let groups: Vec<MergeGroup> = merge_clusters(&speakers, Some(0.7), None)
            .into_iter()
            .map(|g| MergeGroup { score: None, ..g })
            .collect();
        assert_eq!(
            groups,
            vec![
                MergeGroup {
                    kept: sid(1),
                    merged: vec![sid(3)],
                    rejected_union: vec![],
                    score: None,
                },
                MergeGroup {
                    kept: sid(2),
                    merged: vec![sid(4)],
                    rejected_union: vec![],
                    score: None,
                },
                MergeGroup {
                    kept: sid(5),
                    merged: vec![],
                    rejected_union: vec![],
                    score: None,
                },
            ]
        );
    }

    #[test]
    fn cap_keeps_diverse_samples() {
        let mut rng = Rng::new(11);
        let mut samples = rng.voices(0, 55, 0.05);
        samples.extend(rng.voices(1, 5, 0.05));
        let kept = cap_samples(&samples, 6);
        assert_eq!(kept.len(), 6);
        assert!(
            kept.iter().any(|&i| i >= 55),
            "the second direction is represented"
        );
        assert!(kept.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(cap_samples(&samples[..3], 50), vec![0, 1, 2]);
        assert_eq!(cap_samples(&[basis(0), basis(1)], 0), Vec::<usize>::new());
        assert_eq!(cap_weak(15, MAX_WEAK), 5..15);
        assert_eq!(cap_weak(4, MAX_WEAK), 0..4);
    }

    #[test]
    fn score_takes_the_top_three_deterministically_in_any_order() {
        let x = [1.0, 0.0];
        let five = person("a1a1a1", &[0.1, 0.9, 0.5, 0.8, 0.7], &[0.95]);
        let s = score(&x, &five).unwrap();
        assert!((s - (0.9 + 0.8 + 0.7) / 3.0).abs() < 1e-6, "{s}");
        let mut reversed = five.clone();
        reversed.confirmed.reverse();
        assert_eq!(score(&x, &reversed).unwrap().to_bits(), s.to_bits());
        // Fewer than three confirmed: the best weak samples fill in.
        let mixed = person("a1a1a1", &[0.6], &[0.2, 0.4, 0.3]);
        let s = score(&x, &mixed).unwrap();
        assert!((s - (0.6 + 0.5 * (0.4 + 0.3)) / 2.0).abs() < 1e-6, "{s}");
    }

    #[test]
    fn non_finite_vectors_never_rank_or_survive_capping() {
        let x = [1.0, 0.0];
        let mut ana = person("a1a1a1", &[0.6], &[0.4]);
        ana.confirmed.push(vec![f32::NAN, 0.0]);
        ana.weak.push(vec![f32::INFINITY, 1.0]);
        let s = score(&x, &ana).unwrap();
        assert!((s - (0.6 + 0.5 * 0.4) / 1.5).abs() < 1e-6, "{s}");
        let only_bad = PersonSamples {
            person_id: pid("b2b2b2"),
            confirmed: vec![vec![f32::NAN, f32::NAN]],
            weak: Vec::new(),
        };
        assert_eq!(score(&x, &only_bad), None);
        let people = [ana.clone(), only_bad];
        let ranked = rank(&x, &people, &[], None, None);
        assert_eq!(ranked.len(), 1);
        assert!(ranked[0].score.is_finite());
        // A NaN centroid matches no one: no NaN score ranks first.
        assert!(rank(&[f32::NAN, 0.0], &people, &[], None, None).is_empty());
        let res = resolve_voice(&[f32::NAN, 0.0], &people, &[], None, None, None);
        assert_eq!(res.assignment, Assignment::None);
        // Past the cap a non-finite sample is never kept.
        let mut samples: Vec<Vec<f32>> = vec![basis(0), basis(1), basis(2)];
        samples.insert(1, vec![f32::NAN; basis(0).len()]);
        assert_eq!(cap_samples(&samples, 3), vec![0, 2, 3]);
        assert_eq!(cap_samples(&samples, 2).len(), 2);
        assert!(!cap_samples(&samples, 2).contains(&1));
    }
}
