//! Speaker hints of manifest meetings: grouping hints by person, cutting
//! their clean parts, embedding them, and the `hints.json` file a manifest
//! job keeps between the batch phases (plan §13.4–13.5).

use std::collections::BTreeSet;
use std::path::Path;

use broscribe_core::schema::{Person, Speaker, decode_vector, encode_vector};
use broscribe_core::{Library, MeetingId, PersonId, Result, SpeakerId, fsutil, ops};
use broscribe_models::SpeakerEmbedder;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::analyze::{MAX_CLEAN_TURNS, MIN_CLEAN_S, Turn, embed_ranges, speaker_ranges};
use crate::manifest::{SpeakerHint, match_person};
use crate::ranges::{self, Range};

pub const HINTS_SCHEMA_VERSION: u32 = 1;
/// A single clean range at least this long is split into halves.
pub const SPLIT_SINGLE_S: f64 = 3.0;
/// Least overlap with a person's hints for a speaker to be labelled with them.
pub const MIN_LABEL_OVERLAP_S: f64 = 1.0;

/// `<job dir>/hints.json`: a manifest meeting's hint embeddings. Every
/// element that belongs to a person carries `person_key` and, once known,
/// `person_id`, so forgetting a person can scrub it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HintsFile {
    pub schema_version: u32,
    pub meeting_id: MeetingId,
    pub sha256: String,
    #[serde(default)]
    pub source_id: Option<String>,
    /// Full manifest entry, including hint ranges and user metadata.
    #[serde(default)]
    pub entry_fingerprint: String,
    /// Embedding model of every vector in the file.
    pub model: String,
    /// Persisted start of the batch's person snapshot, shared by every entry
    /// and retained on resume. People forgotten after it are dropped;
    /// unknown means any forget counts.
    #[serde(default)]
    pub analysis_started_at: Option<DateTime<FixedOffset>>,
    #[serde(default)]
    pub people: Vec<HintPerson>,
    /// Diarized speakers that mostly overlap one hinted person (the
    /// comparison pool of the other people's agreement check).
    #[serde(default)]
    pub speakers: Vec<HintSpeaker>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HintPerson {
    pub person_key: String,
    #[serde(default)]
    pub person_id: Option<PersonId>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub is_self: bool,
    #[serde(default)]
    pub samples: Vec<HintSample>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HintSample {
    pub start: f64,
    pub end: f64,
    pub dim: usize,
    pub vector_b64: String,
}

impl HintSample {
    pub fn vector(&self) -> Result<Vec<f32>> {
        decode_vector(&self.vector_b64, self.dim)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HintSpeaker {
    pub speaker: SpeakerId,
    pub person_key: String,
    #[serde(default)]
    pub person_id: Option<PersonId>,
    pub dim: usize,
    pub vector_b64: String,
}

impl HintSpeaker {
    pub fn vector(&self) -> Result<Vec<f32>> {
        decode_vector(&self.vector_b64, self.dim)
    }
}

impl HintPerson {
    /// Every key a forget records for the person this hint may be: the
    /// person ID, e-mail, name, and `self` for the user.
    fn forget_keys(&self) -> Vec<String> {
        let mut keys = vec![self.person_key.clone()];
        keys.extend(self.person_id.as_ref().map(|p| p.as_str().to_owned()));
        keys.extend(self.email.clone());
        keys.extend(self.name.clone());
        if self.is_self {
            keys.push("self".to_owned());
        }
        keys
    }
}

impl HintsFile {
    pub fn read(path: &Path) -> Result<Self> {
        fsutil::read_json(path)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        fsutil::write_json_atomic(path, self)
    }

    /// Drops the people forgotten (`people forget`, `forget-voice`) since
    /// the batch's person snapshot, and the speakers labelled with them:
    /// stale work never brings a forgotten identity or voice back. Returns
    /// how many people were dropped. Call it under the library lock.
    pub fn drop_forgotten(&mut self, lib: &Library) -> Result<usize> {
        let since = self.analysis_started_at;
        let mut dropped = BTreeSet::new();
        for hp in &self.people {
            for key in hp.forget_keys() {
                if let Some(at) = ops::forgotten_since(lib, &key)?
                    && since.is_none_or(|s| at >= s)
                {
                    dropped.insert(hp.person_key.clone());
                    break;
                }
            }
        }
        self.people.retain(|p| !dropped.contains(&p.person_key));
        self.speakers.retain(|s| !dropped.contains(&s.person_key));
        Ok(dropped.len())
    }
}

/// One person's hints in one meeting.
#[derive(Clone, Debug, PartialEq)]
pub struct HintGroup {
    pub key: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub is_self: bool,
    /// The known person, when one matches.
    pub person_id: Option<PersonId>,
    pub ranges: Vec<Range>,
}

/// Groups a meeting's hints per person. A hint matching the user (`is_self`
/// or the "Me" person's name or e-mail) goes to the `self` group.
pub fn group_hints(hints: &[SpeakerHint], people: &[Person]) -> Vec<HintGroup> {
    let me = people.iter().find(|p| p.is_self);
    let mut out: Vec<HintGroup> = Vec::new();
    for h in hints {
        let matched = match_person(people, h.name.as_deref(), h.email.as_deref());
        let is_self = h.is_self || matched.is_some_and(|p| p.is_self);
        let (key, person_id) = if is_self {
            ("self".to_owned(), me.map(|p| p.id.clone()))
        } else {
            (h.person_key(), matched.map(|p| p.id.clone()))
        };
        if key.is_empty() {
            continue;
        }
        match out.iter_mut().find(|g| g.key == key) {
            Some(g) => {
                g.ranges.extend(h.ranges.iter().copied());
                g.name = g.name.take().or_else(|| h.name.clone());
                g.email = g.email.take().or_else(|| h.email.clone());
                g.person_id = g.person_id.take().or(person_id);
            }
            None => out.push(HintGroup {
                key,
                name: h.name.clone(),
                email: h.email.clone(),
                is_self,
                person_id,
                ranges: h.ranges.clone(),
            }),
        }
    }
    for g in &mut out {
        g.ranges = ranges::union(g.ranges.iter().copied());
    }
    out
}

/// Clean parts of each group's hints: hint ranges ∩ diarized speech, minus
/// the other groups' hint ranges; pieces ≥ 1.5 s, the 20 longest; a single
/// piece ≥ 3 s is split into halves.
pub fn clean_hint_pieces(groups: &[HintGroup], turns: &[Turn]) -> Vec<Vec<Range>> {
    let speech = ranges::union(turns.iter().map(|t| (t.start, t.end)));
    groups
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let others: Vec<Range> = groups
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .flat_map(|(_, o)| o.ranges.iter().copied())
                .collect();
            let clean = ranges::subtract(&ranges::intersect(&g.ranges, &speech), &others);
            let mut pieces = ranges::longest(clean, MIN_CLEAN_S, MAX_CLEAN_TURNS);
            if pieces.len() == 1 {
                let (a, b) = pieces[0];
                if b - a >= SPLIT_SINGLE_S {
                    let mid = (a + b) / 2.0;
                    pieces = vec![(a, mid), (mid, b)];
                }
            }
            pieces
        })
        .collect()
}

/// The hinted group a speaker's turns overlap most.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeakerLabel {
    pub group: usize,
    /// Overlap with `group` over the speaker's overlap with every group:
    /// calibration's cluster purity (hinted speech only).
    pub purity: f64,
    /// Overlap with `group` over the speaker's total talk time: whether
    /// the hint covers most of the speaker (the comparison-pool label).
    pub talk_share: f64,
    /// Overlap with `group`, in seconds.
    pub overlap_s: f64,
}

/// The group a speaker's turns overlap most; `None` without any overlap.
pub fn speaker_label(
    turns: &[Turn],
    speaker: SpeakerId,
    groups: &[HintGroup],
) -> Option<SpeakerLabel> {
    let own = speaker_ranges(turns, speaker);
    let overlaps: Vec<f64> = groups
        .iter()
        .map(|g| ranges::overlap(&own, &g.ranges))
        .collect();
    let hinted: f64 = overlaps.iter().sum();
    let talk = ranges::total(&own);
    if hinted <= 0.0 || talk <= 0.0 {
        return None;
    }
    let (group, overlap_s) = overlaps
        .iter()
        .copied()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)))?;
    Some(SpeakerLabel {
        group,
        purity: overlap_s / hinted,
        talk_share: overlap_s / talk,
        overlap_s,
    })
}

/// Embeds the clean hint parts of every group and lists the speakers that
/// majority-overlap one group (with their centroids). Inference: call it
/// outside the lock.
pub fn embed_hints(
    samples: &[f32],
    turns: &[Turn],
    speakers: &[Speaker],
    groups: &[HintGroup],
    embedder: &dyn SpeakerEmbedder,
) -> Result<(Vec<HintPerson>, Vec<HintSpeaker>)> {
    let pieces = clean_hint_pieces(groups, turns);
    let mut people = Vec::with_capacity(groups.len());
    for (g, pieces) in groups.iter().zip(&pieces) {
        let samples = embed_ranges(samples, pieces, embedder)?
            .into_iter()
            .map(|((start, end), v)| HintSample {
                start,
                end,
                dim: v.len(),
                vector_b64: encode_vector(&v),
            })
            .collect();
        people.push(HintPerson {
            person_key: g.key.clone(),
            person_id: g.person_id.clone(),
            name: g.name.clone(),
            email: g.email.clone(),
            is_self: g.is_self,
            samples,
        });
    }
    let mut labelled = Vec::new();
    for s in speakers {
        let Some(c) = s
            .centroid
            .as_ref()
            .filter(|c| c.model == embedder.model_id())
        else {
            continue;
        };
        let Some(label) = speaker_label(turns, s.id, groups) else {
            continue;
        };
        let g = label.group;
        if label.talk_share > 0.5 && label.overlap_s >= MIN_LABEL_OVERLAP_S {
            labelled.push(HintSpeaker {
                speaker: s.id,
                person_key: groups[g].key.clone(),
                person_id: groups[g].person_id.clone(),
                dim: c.dim,
                vector_b64: c.vector_b64.clone(),
            });
        }
    }
    Ok((people, labelled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_hint_is_not_a_majority_of_speaker_talk() {
        let groups = group_hints(&[hint(Some("Alex Example"), false, &[(0.0, 2.0)])], &[]);
        let turns = [turn(0.0, 10.0, 1)];
        let label = speaker_label(&turns, SpeakerId::new(1).unwrap(), &groups).unwrap();
        assert!((label.talk_share - 0.2).abs() < 1e-9);
        // All of the speaker's hinted speech is this group's.
        assert_eq!(label.purity, 1.0);
    }

    fn hint(name: Option<&str>, is_self: bool, ranges: &[(f64, f64)]) -> SpeakerHint {
        SpeakerHint {
            name: name.map(str::to_owned),
            email: None,
            is_self,
            ranges: ranges.to_vec(),
        }
    }

    fn turn(a: f64, b: f64, n: u32) -> Turn {
        Turn {
            start: a,
            end: b,
            speaker: SpeakerId::new(n).unwrap(),
        }
    }

    #[test]
    fn groups_and_clean_parts() {
        let mut me = Person::new(PersonId::parse("p-00000e").unwrap(), "Bruno Sample");
        me.is_self = true;
        let people = vec![me];
        let hints = vec![
            hint(None, true, &[(0.0, 4.0)]),
            hint(Some("bruno sample"), false, &[(10.0, 12.0)]),
            hint(Some("Ana Example"), false, &[(3.0, 9.0)]),
        ];
        let groups = group_hints(&hints, &people);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].key, "self");
        assert_eq!(groups[0].ranges, vec![(0.0, 4.0), (10.0, 12.0)]);
        assert_eq!(groups[0].person_id.as_ref().unwrap().as_str(), "p-00000e");
        let turns = vec![turn(0.0, 3.5, 1), turn(3.5, 9.0, 2), turn(10.0, 11.0, 1)];
        let pieces = clean_hint_pieces(&groups, &turns);
        // Me: (0,3) after removing Ana's (3,9) — one piece ≥ 3 s → halves;
        // (10,11) is too short.
        assert_eq!(pieces[0], vec![(0.0, 1.5), (1.5, 3.0)]);
        // Ana: (4,9) after removing Me's ranges → halves.
        assert_eq!(pieces[1], vec![(4.0, 6.5), (6.5, 9.0)]);
        let label = speaker_label(&turns, SpeakerId::new(2).unwrap(), &groups).unwrap();
        assert_eq!(label.group, 1);
        assert!((label.overlap_s - 5.5).abs() < 1e-9 && label.talk_share > 0.9);
    }
}

#[cfg(test)]
mod bounded_hint_tests {
    use super::*;
    use broscribe_models::fakes::{ToneEmbedder, sine};

    #[test]
    fn subdivided_hint_samples_keep_their_person_and_source_times() {
        let groups = group_hints(
            &[SpeakerHint {
                name: Some("Alex Example".into()),
                email: None,
                is_self: false,
                ranges: vec![(0.0, 24.0)],
            }],
            &[],
        );
        let turns = [Turn {
            start: 0.0,
            end: 24.0,
            speaker: SpeakerId::new(1).unwrap(),
        }];
        let mut model = ToneEmbedder::new("bounded", 16);
        model.max_samples = Some(3 * 16_000);
        let (people, _) =
            embed_hints(&sine(200.0, 24.0, 0.5), &turns, &[], &groups, &model).unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].name.as_deref(), Some("Alex Example"));
        let samples = &people[0].samples;
        assert_eq!(samples.first().unwrap().start, 0.0);
        assert_eq!(samples.last().unwrap().end, 24.0);
        assert!(samples.windows(2).all(|p| p[0].end == p[1].start));
        assert!(samples.iter().all(|p| p.end - p.start <= 3.0));
    }
}
