//! Caption identity evidence. Recomputed from the latest revisions; one ASR
//! span is one vote, even if caption ids or display names change.
use crate::captions::{folded_name, normalize_name};
use crate::{CaptionEvent, RosterMember};
use broscribe_core::schema::{
    Assignment, Participant, Person, Speaker, Strength, Voiceprint, VoiceprintKind,
    VoiceprintOrigin, VoiceprintSource, now, voiceprint_id,
};
use broscribe_core::{MeetingId, PersonId, SpeakerId};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    Mic,
    System,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setup {
    Headphones,
    Speakerphone,
    HybridRoom,
    InPerson,
}

#[derive(Clone, Debug)]
pub struct AsrSpan {
    /// Stable identity of this ASR turn/word span, never a caption id.
    pub id: String,
    pub speaker_id: SpeakerId,
    pub track: Track,
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct CaptionNaming {
    pub speaker_id: SpeakerId,
    pub name: String,
    pub is_self: bool,
    pub distinct_spans: usize,
    pub purity: f64,
    pub start_s: f64,
    pub end_s: f64,
    /// The agreeing ASR spans, time-ordered: the speech the naming rests on.
    pub ranges: Vec<[f32; 2]>,
}

/// Latest revision per caption id, in id order so every evaluation visits
/// captions in the same order.
#[derive(Default)]
pub struct CaptionAligner {
    latest: BTreeMap<String, CaptionEvent>,
}
impl CaptionAligner {
    pub fn ingest(&mut self, caption: CaptionEvent) {
        if self
            .latest
            .get(&caption.caption_id)
            .is_none_or(|old| caption.revision > old.revision)
        {
            self.latest.insert(caption.caption_id.clone(), caption);
        }
    }
    pub fn names(&self, spans: &[AsrSpan], setup: Setup) -> Vec<CaptionNaming> {
        // Choose one best matching ASR span per caption, then newest evidence
        // per ASR span. This also withdraws a contradictory earlier revision.
        let mut votes: BTreeMap<(u8, &str), (&AsrSpan, &CaptionEvent, usize)> = BTreeMap::new();
        let mut conflicts = HashSet::new();
        for caption in self.latest.values() {
            let words = content_words(&caption.text);
            if words.len() < 2 {
                continue;
            }
            let mut candidates: Vec<_> = spans
                .iter()
                .filter_map(|span| {
                    if !eligible(caption.is_self, span.track, setup)
                        || !span.start_s.is_finite()
                        || !span.end_s.is_finite()
                        || span.end_s < caption.start_s - 2.0
                        || span.start_s > caption.end_s + 2.0
                    {
                        return None;
                    }
                    let asr = content_words(&span.text);
                    let overlap = words.intersection(&asr).count() as f64 / words.len() as f64;
                    if overlap < 0.6 {
                        return None;
                    }
                    let time = (span.end_s.min(caption.end_s) - span.start_s.max(caption.start_s))
                        .max(0.0);
                    Some((span, time, overlap))
                })
                .collect();
            candidates.sort_by(|a, b| {
                b.1.total_cmp(&a.1)
                    .then(b.2.total_cmp(&a.2))
                    .then(a.0.id.cmp(&b.0.id))
            });
            let Some((span, time, overlap)) = candidates.first().copied() else {
                continue;
            };
            if candidates.get(1).is_some_and(|(other, t, o)| {
                other.speaker_id != span.speaker_id && *t == time && *o == overlap
            }) {
                continue;
            }
            let key = (
                if span.track == Track::Mic { 0 } else { 1 },
                span.id.as_str(),
            );
            if let Some((_, old, _)) = votes.get(&key)
                && caption.observed_s == old.observed_s
                && (caption.is_self != old.is_self
                    || folded_name(&caption.speaker_name) != folded_name(&old.speaker_name))
            {
                conflicts.insert(key);
                continue;
            }
            // Newest observation wins. Equal observations of one name are
            // the same evidence: the one with more content words (the
            // word gate below), then the smaller caption id, so the result
            // never depends on visiting order; only a newer observation
            // lifts a same-time conflict.
            let replace = votes.get(&key).is_none_or(|(_, old, old_words)| {
                caption.observed_s > old.observed_s
                    || (caption.observed_s == old.observed_s
                        && (words.len(), std::cmp::Reverse(&caption.caption_id))
                            > (*old_words, std::cmp::Reverse(&old.caption_id)))
            });
            if replace {
                if votes
                    .get(&key)
                    .is_none_or(|(_, old, _)| caption.observed_s > old.observed_s)
                {
                    conflicts.remove(&key);
                }
                votes.insert(key, (span, caption, words.len()));
            }
        }
        let mut by_speaker: BTreeMap<SpeakerId, Vec<(&AsrSpan, &CaptionEvent, usize)>> =
            BTreeMap::new();
        let mut totals = BTreeMap::<SpeakerId, usize>::new();
        for (key, vote) in votes {
            *totals.entry(vote.0.speaker_id).or_default() += 1;
            if !conflicts.contains(&key) {
                by_speaker.entry(vote.0.speaker_id).or_default().push(vote);
            }
        }
        let mut out = Vec::new();
        for (speaker_id, votes) in by_speaker {
            let mut names: BTreeMap<(bool, String), Vec<_>> = BTreeMap::new();
            for vote in &votes {
                names
                    .entry((vote.1.is_self, folded_name(&vote.1.speaker_name)))
                    .or_default()
                    .push(vote);
            }
            for ((is_self, _), agree) in names {
                let purity = agree.len() as f64 / totals[&speaker_id] as f64;
                if purity < 0.8 || !(agree.iter().any(|v| v.2 >= 4) || agree.len() >= 2) {
                    continue;
                }
                out.push(CaptionNaming {
                    speaker_id,
                    name: normalize_name(&agree[0].1.speaker_name),
                    is_self,
                    distinct_spans: agree.len(),
                    purity,
                    start_s: agree
                        .iter()
                        .map(|v| v.0.start_s)
                        .fold(f64::INFINITY, f64::min),
                    end_s: agree.iter().map(|v| v.0.end_s).fold(0.0, f64::max),
                    ranges: {
                        let mut ranges: Vec<[f32; 2]> = agree
                            .iter()
                            .map(|v| [v.0.start_s as f32, v.0.end_s as f32])
                            .collect();
                        ranges.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
                        ranges
                    },
                });
            }
        }
        out
    }
}
fn eligible(is_self: bool, track: Track, setup: Setup) -> bool {
    if is_self {
        track == Track::Mic
    } else {
        track == Track::System || matches!(setup, Setup::HybridRoom | Setup::InPerson)
    }
}
fn content_words(text: &str) -> HashSet<String> {
    folded_name(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| {
            !w.is_empty()
                && ![
                    "a", "an", "the", "and", "or", "is", "are", "to", "of", "in", "on", "it", "i",
                    "you", "we",
                ]
                .contains(w)
        })
        .map(str::to_owned)
        .collect()
}

/// Aliases/exact names, then an exact roster-email join. Never partial names.
/// Ambiguous matches are deliberately unresolved rather than merged.
pub fn match_person<'a>(
    naming: &CaptionNaming,
    people: &'a [Person],
    roster: &[RosterMember],
) -> Option<&'a Person> {
    let unique = |items: Vec<&'a Person>| {
        if items.len() == 1 {
            Some(items[0])
        } else {
            None
        }
    };
    if naming.is_self {
        return unique(people.iter().filter(|p| p.is_self).collect());
    }
    let name = folded_name(&naming.name);
    let exact: Vec<_> = people
        .iter()
        .filter(|p| {
            folded_name(&p.name) == name || p.aliases.iter().any(|a| folded_name(a) == name)
        })
        .collect();
    if !exact.is_empty() {
        return unique(exact);
    }
    let emails: Vec<_> = roster
        .iter()
        .filter(|r| folded_name(&r.name) == name)
        .filter_map(|r| r.email.as_ref())
        .collect();
    unique(
        people
            .iter()
            .filter(|p| {
                p.emails
                    .iter()
                    .any(|email| emails.iter().any(|e| e.eq_ignore_ascii_case(email)))
            })
            .collect(),
    )
}

/// Unknown captions create a separate name-only person, explicitly for review.
/// The caller allocates a collision-checked id and commits under its write lock.
pub struct CaptionPerson {
    pub person: Person,
    pub participant: Participant,
    pub needs_review: bool,
}
pub fn new_caption_person(id: PersonId, name: &str) -> CaptionPerson {
    let name = normalize_name(name);
    CaptionPerson {
        person: Person::new(id.clone(), name.clone()),
        participant: Participant {
            name: Some(name),
            person_id: Some(id),
            evidence: vec!["captions".into()],
            ..Participant::default()
        },
        needs_review: true,
    }
}

/// Does not override confirmed assignments or remembered rejections.
pub fn apply_naming(speaker: &mut Speaker, person: &Person, naming: &CaptionNaming) -> bool {
    if speaker.id != naming.speaker_id
        || speaker.assignment == Assignment::Confirmed
        || speaker.rejected_person_ids.contains(&person.id)
    {
        return false;
    }
    speaker.person_id = Some(person.id.clone());
    speaker.assignment = Assignment::Auto;
    speaker.suggestion = None;
    true
}

/// The speech a caption sample of `speaker` stands for: the caption-agreeing
/// ASR spans of `naming` cut to the speaker's clean turns (its centroid
/// ranges), time-ordered. A centroid without recorded ranges (older
/// analyses) leaves the agreeing spans as they are.
pub fn agreed_clean_ranges(speaker: &Speaker, naming: &CaptionNaming) -> Vec<[f32; 2]> {
    let clean = speaker
        .centroid
        .as_ref()
        .map(|c| c.ranges.as_slice())
        .unwrap_or_default();
    if clean.is_empty() {
        return naming.ranges.clone();
    }
    let mut out: Vec<[f32; 2]> = naming
        .ranges
        .iter()
        .flat_map(|&[a, b]| {
            clean.iter().filter_map(move |&[c, d]| {
                let (start, end) = (a.max(c), b.min(d));
                (end > start).then_some([start, end])
            })
        })
        .collect();
    out.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    out.dedup();
    out
}

/// Speaker embeddings of single audio ranges of one meeting (one model),
/// keyed by track and meeting-time range. Computed from the audio outside
/// the library lock, then combined into caption samples under it.
#[derive(Clone, Debug, Default)]
pub struct RangeEmbeddings {
    model: String,
    entries: Vec<(String, [f32; 2], Vec<f32>)>,
}

impl RangeEmbeddings {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            entries: Vec::new(),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Records the embedding of `range` on `track` (replacing an earlier one).
    pub fn insert(&mut self, track: &str, range: [f32; 2], vector: Vec<f32>) {
        match self
            .entries
            .iter_mut()
            .find(|(t, r, _)| t == track && *r == range)
        {
            Some(entry) => entry.2 = vector,
            None => self.entries.push((track.to_owned(), range, vector)),
        }
    }

    pub fn get(&self, track: &str, range: [f32; 2]) -> Option<&[f32]> {
        self.entries
            .iter()
            .find(|(t, r, _)| t == track && *r == range)
            .map(|(_, _, v)| v.as_slice())
    }
}

/// Enroll only an already accepted naming; weak forever until explicit
/// confirmation or the core's second-meeting relative hint-agreement gate.
/// `speaker_namespace` is the meeting's current one (`Meeting::speaker_namespace`).
///
/// The sample is the normalized mean of the `embeddings` of the speaker's
/// [`agreed_clean_ranges`], and records exactly the ranges it averaged, so
/// re-embedding those ranges reconstructs it. `None` when none of them has
/// an embedding (too short to embed, or no audio was available).
pub fn caption_voiceprint(
    speaker: &Speaker,
    person: &PersonId,
    meeting: &MeetingId,
    speaker_namespace: &str,
    naming: &CaptionNaming,
    embeddings: &RangeEmbeddings,
) -> Option<Voiceprint> {
    if speaker.named_person() != Some(person) || speaker.rejected_person_ids.contains(person) {
        return None;
    }
    let (ranges, vectors): (Vec<[f32; 2]>, Vec<&[f32]>) = agreed_clean_ranges(speaker, naming)
        .into_iter()
        .filter_map(|r| embeddings.get(&speaker.track, r).map(|v| (r, v)))
        .unzip();
    let vector = broscribe_core::identity::mean_normalized(&vectors)?;
    let start = f64::from(ranges.first()?[0]);
    let end = ranges.iter().map(|r| f64::from(r[1])).fold(start, f64::max);
    Some(Voiceprint {
        id: voiceprint_id(
            person,
            embeddings.model(),
            VoiceprintOrigin::Meeting { id: meeting },
            start,
            end,
            VoiceprintKind::Captions,
        ),
        model: embeddings.model().to_owned(),
        dim: vector.len(),
        vector_b64: broscribe_core::schema::encode_vector(&vector),
        source: VoiceprintSource {
            kind: VoiceprintKind::Captions,
            meeting_id: Some(meeting.clone()),
            sha256: None,
            start,
            end,
            ranges,
            speaker_id: Some(speaker.id),
            speaker_namespace: speaker_namespace.to_owned(),
        },
        device: None,
        strength: Strength::Weak,
        created_at: now(),
    })
}

/// Promote caption samples for one person/model. `between` contains only
/// other known people's vectors from the same model. Without explicit user
/// confirmation, at least two distinct meetings must survive the relative
/// hint-agreement gate; never promote merely because two spans agree.
pub fn promote_captions(
    prints: &mut [Voiceprint],
    model: &str,
    between: &[Vec<f32>],
    user_confirmed: bool,
) -> broscribe_core::Result<usize> {
    let indices: Vec<_> = prints
        .iter()
        .enumerate()
        .filter(|(_, p)| p.model == model && p.source.kind == VoiceprintKind::Captions)
        .map(|(i, _)| i)
        .collect();
    let mut promote = indices.clone();
    if !user_confirmed {
        let meetings: HashSet<_> = indices
            .iter()
            .filter_map(|&i| prints[i].source.meeting_id.as_ref())
            .collect();
        if meetings.len() < 2 {
            return Ok(0);
        }
        let vectors = indices
            .iter()
            .map(|&i| prints[i].vector())
            .collect::<broscribe_core::Result<Vec<_>>>()?;
        let agreement = broscribe_core::identity::hint_agreement(&vectors, between);
        if !agreement.enrolled {
            return Ok(0);
        }
        promote = agreement.kept.iter().map(|&i| indices[i]).collect();
        let kept_meetings: HashSet<_> = promote
            .iter()
            .filter_map(|&i| prints[i].source.meeting_id.as_ref())
            .collect();
        if kept_meetings.len() < 2 {
            return Ok(0);
        }
    }
    let mut count = 0;
    for i in promote {
        if prints[i].strength == Strength::Weak {
            prints[i].strength = Strength::Confirmed;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cap(id: &str, revision: u64, name: &str, text: &str, at: f64) -> CaptionEvent {
        CaptionEvent {
            caption_id: id.into(),
            revision,
            speaker_name: name.into(),
            is_self: name == "You",
            text: text.into(),
            observed_s: at,
            start_s: at - 1.0,
            end_s: at,
        }
    }
    fn span(id: &str, speaker: u32, track: Track, text: &str, at: f64) -> AsrSpan {
        AsrSpan {
            id: id.into(),
            speaker_id: SpeakerId::new(speaker).unwrap(),
            track,
            text: text.into(),
            start_s: at - 1.0,
            end_s: at,
        }
    }
    #[test]
    fn one_good_alignment_and_hybrid_routing_include_local_and_remote() {
        let mut a = CaptionAligner::default();
        let words = "purple rockets deliver fresh parcels";
        a.ingest(cap("remote", 1, "Niko", words, 3.0));
        a.ingest(cap(
            "local",
            1,
            "Daria",
            "silver bicycles carry green boxes",
            7.0,
        ));
        a.ingest(cap(
            "self",
            1,
            "You",
            "orange trains travel beyond hills",
            11.0,
        ));
        let spans = [
            span("r", 1, Track::System, words, 3.0),
            span("l", 2, Track::Mic, "silver bicycles carry green boxes", 7.0),
            span(
                "me",
                3,
                Track::Mic,
                "orange trains travel beyond hills",
                11.0,
            ),
            span(
                "echo",
                4,
                Track::System,
                "orange trains travel beyond hills",
                11.0,
            ),
        ];
        assert_eq!(a.names(&spans, Setup::HybridRoom).len(), 3);
        let names = a.names(&spans, Setup::Headphones);
        assert_eq!(names.len(), 2);
        assert!(
            names
                .iter()
                .all(|n| n.speaker_id != SpeakerId::new(4).unwrap())
        );
    }
    #[test]
    fn revisions_ids_and_display_names_cannot_multiply_span_evidence() {
        let mut a = CaptionAligner::default();
        let spans = [span("turn", 1, Track::System, "purple rockets", 3.0)];
        a.ingest(cap("x", 1, "Daria", "purple rockets", 3.0));
        a.ingest(cap("y", 1, "Daria (Guest)", "purple rockets", 4.0));
        assert!(a.names(&spans, Setup::Headphones).is_empty());
        a.ingest(cap("y", 2, "Niko", "purple rockets", 5.0));
        let more = [
            spans[0].clone(),
            span("turn2", 1, Track::System, "fresh parcels", 7.0),
        ];
        a.ingest(cap("z", 1, "Niko", "fresh parcels", 7.0));
        let names = a.names(&more, Setup::Headphones);
        assert_eq!(names[0].name, "Niko");
        assert_eq!(names[0].distinct_spans, 2);
        assert_eq!(names[0].purity, 1.0);
    }
    #[test]
    fn contradictory_revision_withdraws_old_alignment_and_purity_is_required() {
        let mut a = CaptionAligner::default();
        let spans = [
            span(
                "a",
                1,
                Track::System,
                "purple rockets deliver fresh parcels",
                3.0,
            ),
            span(
                "b",
                1,
                Track::System,
                "silver bicycles carry green boxes",
                7.0,
            ),
        ];
        a.ingest(cap("x", 1, "Daria", &spans[0].text, 3.0));
        a.ingest(cap("y", 1, "Niko", &spans[1].text, 7.0));
        assert!(a.names(&spans, Setup::Headphones).is_empty());
        a.ingest(cap("x", 2, "Niko", &spans[0].text, 4.0));
        assert_eq!(a.names(&spans, Setup::Headphones)[0].name, "Niko");
        a.ingest(cap("x", 3, "Niko", "unrelated revised words", 5.0));
        assert_eq!(a.names(&spans, Setup::Headphones)[0].distinct_spans, 1);
    }
    #[test]
    fn rejections_confirmation_and_weak_voiceprints() {
        let mut a = CaptionAligner::default();
        a.ingest(cap(
            "x",
            1,
            "Daria",
            "purple rockets deliver fresh parcels",
            3.0,
        ));
        let n = a
            .names(
                &[span(
                    "a",
                    1,
                    Track::System,
                    "purple rockets deliver fresh parcels",
                    3.0,
                )],
                Setup::Headphones,
            )
            .remove(0);
        let p = Person::new(PersonId::parse("p-abcdef").unwrap(), "Daria");
        let mut s = Speaker::new(n.speaker_id, "system");
        s.rejected_person_ids.push(p.id.clone());
        assert!(!apply_naming(&mut s, &p, &n));
        s.rejected_person_ids.clear();
        s.assignment = Assignment::Confirmed;
        assert!(!apply_naming(&mut s, &p, &n));
        s.assignment = Assignment::None;
        s.centroid = Some(broscribe_core::schema::Centroid::new(
            "synthetic",
            &[1.0, 0.0],
            1,
        ));
        assert!(apply_naming(&mut s, &p, &n));
        let mut embeddings = RangeEmbeddings::new("synthetic");
        embeddings.insert("system", n.ranges[0], vec![1.0, 0.0]);
        let meeting = MeetingId::parse("m-abcdef").unwrap();
        let vp = caption_voiceprint(&s, &p.id, &meeting, "", &n, &embeddings).unwrap();
        assert!(
            caption_voiceprint(&s, &p.id, &meeting, "", &n, &RangeEmbeddings::default()).is_none(),
            "no embedded audio, no sample"
        );
        assert_eq!(vp.strength, Strength::Weak);
        assert_eq!(vp.source.kind, VoiceprintKind::Captions);
    }
    #[test]
    fn caption_voiceprint_keeps_only_agreeing_spans_of_interleaved_turns() {
        let mut a = CaptionAligner::default();
        let alex = [
            "purple rockets deliver parcels",
            "silver bicycles carry boxes",
        ];
        let bob = "orange trains travel beyond hills";
        a.ingest(cap("x", 1, "Alex Example", alex[0], 3.0));
        a.ingest(cap("y", 1, "Bob Example", bob, 6.0));
        a.ingest(cap("z", 1, "Alex Example", alex[1], 9.0));
        let spans = [
            span("a1", 1, Track::System, alex[0], 3.0),
            span("b", 2, Track::System, bob, 6.0),
            span("a2", 1, Track::System, alex[1], 9.0),
        ];
        let n = a
            .names(&spans, Setup::Headphones)
            .into_iter()
            .find(|n| n.name == "Alex Example")
            .unwrap();
        assert_eq!(n.ranges, vec![[2.0, 3.0], [8.0, 9.0]]);
        let p = Person::new(PersonId::parse("p-abcdef").unwrap(), "Alex Example");
        let mut s = Speaker::new(n.speaker_id, "system");
        // Clean turns cover only part of the agreeing speech, plus speech the
        // captions never agreed with.
        let mut centroid = broscribe_core::schema::Centroid::new("synthetic", &[0.6, 0.8], 3);
        centroid.ranges = vec![[1.0, 3.0], [5.0, 7.0], [8.5, 9.5]];
        s.centroid = Some(centroid);
        assert!(apply_naming(&mut s, &p, &n));
        assert_eq!(agreed_clean_ranges(&s, &n), vec![[2.0, 3.0], [8.5, 9.0]]);
        let mut embeddings = RangeEmbeddings::new("synthetic");
        embeddings.insert("system", [2.0, 3.0], vec![1.0, 0.0]);
        embeddings.insert("system", [5.0, 7.0], vec![-1.0, 0.0]);
        embeddings.insert("system", [8.5, 9.0], vec![0.0, 1.0]);
        embeddings.insert("mic", [8.5, 9.0], vec![-1.0, 0.0]);
        let vp = caption_voiceprint(
            &s,
            &p.id,
            &MeetingId::parse("m-abcdef").unwrap(),
            "current-analysis",
            &n,
            &embeddings,
        )
        .unwrap();
        // The sample is the agreed clean speech, not the cluster centroid,
        // and records exactly those ranges, so re-embedding them rebuilds it.
        assert_eq!(vp.source.ranges, vec![[2.0, 3.0], [8.5, 9.0]]);
        assert_eq!((vp.source.start, vp.source.end), (2.0, 9.0));
        let rebuilt = broscribe_core::identity::mean_normalized(
            &vp.source
                .ranges
                .iter()
                .map(|&r| embeddings.get("system", r).unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(vp.vector().unwrap(), rebuilt);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            vp.vector()
                .unwrap()
                .iter()
                .zip([h, h])
                .all(|(a, b)| (a - b).abs() < 1e-6)
        );
        assert_eq!(vp.source.speaker_id, Some(n.speaker_id));
        assert_eq!(vp.source.speaker_namespace, "current-analysis");
    }
    #[test]
    fn exact_alias_then_roster_email_never_partial_names() {
        let people = [Person {
            aliases: vec!["Daria".into()],
            emails: vec!["invented@example.invalid".into()],
            ..Person::new(PersonId::parse("p-abcdef").unwrap(), "Daria Solis")
        }];
        let naming = CaptionNaming {
            speaker_id: SpeakerId::new(1).unwrap(),
            name: "Dária".into(),
            is_self: false,
            distinct_spans: 1,
            purity: 1.0,
            start_s: 0.0,
            end_s: 1.0,
            ranges: vec![[0.0, 1.0]],
        };
        assert_eq!(
            match_person(&naming, &people, &[]).unwrap().id,
            people[0].id
        );
        let other = CaptionNaming {
            name: "New Label".into(),
            ..naming
        };
        assert!(match_person(&other, &people, &[]).is_none());
        assert!(
            match_person(
                &other,
                &people,
                &[RosterMember {
                    name: "New Label".into(),
                    email: Some("invented@example.invalid".into()),
                    is_self: false
                }]
            )
            .is_some()
        );
    }
    #[test]
    fn simultaneous_conflicting_caption_ids_do_not_arbitrarily_name() {
        let mut a = CaptionAligner::default();
        let words = "purple rockets deliver fresh parcels";
        a.ingest(cap("a", 1, "Daria", words, 3.0));
        a.ingest(cap("b", 8, "Niko", words, 3.0));
        let spans = [span("turn", 1, Track::System, words, 3.0)];
        assert!(a.names(&spans, Setup::Headphones).is_empty());
    }
    #[test]
    fn same_time_same_name_tie_keeps_the_fuller_caption_in_every_order() {
        let full = "purple rockets deliver fresh parcels";
        let spans = [span("turn", 1, Track::System, full, 3.0)];
        // Ids in both orders, ingested in both orders: the two-word view
        // alone would fail the word gate, the full one passes it.
        for (short_id, full_id) in [("a", "b"), ("b", "a")] {
            for short_first in [true, false] {
                let mut a = CaptionAligner::default();
                let short = cap(short_id, 1, "Daria", "purple rockets", 3.0);
                let long = cap(full_id, 1, "Daria", full, 3.0);
                let (first, second) = if short_first {
                    (short, long)
                } else {
                    (long, short)
                };
                a.ingest(first);
                a.ingest(second);
                let names = a.names(&spans, Setup::Headphones);
                assert_eq!(names.len(), 1, "{short_id}/{full_id} {short_first}");
                assert_eq!(names[0].name, "Daria");
            }
        }
    }
    #[test]
    fn same_time_conflict_survives_every_visiting_order() {
        let words = "purple rockets deliver fresh parcels";
        let spans = [span("turn", 1, Track::System, words, 3.0)];
        let captions = [
            ("a", "Daria", "purple rockets deliver"),
            ("b", "Niko", words),
            ("c", "Daria", words),
        ];
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let mut a = CaptionAligner::default();
            for i in order {
                let (id, name, text) = captions[i];
                a.ingest(cap(id, 1, name, text, 3.0));
            }
            assert!(a.names(&spans, Setup::Headphones).is_empty(), "{order:?}");
            // A newer observation settles it.
            a.ingest(cap("d", 1, "Niko", words, 4.0));
            let names = a.names(&spans, Setup::Headphones);
            assert_eq!(names.len(), 1, "{order:?}");
            assert_eq!(names[0].name, "Niko");
        }
    }
    #[test]
    fn caption_promotion_requires_second_meeting_and_relative_agreement() {
        let person = PersonId::parse("p-abcdef").unwrap();
        let first = MeetingId::parse("m-abcdef").unwrap();
        let second = MeetingId::parse("m-fedcba").unwrap();
        let mut speaker = Speaker::new(SpeakerId::new(1).unwrap(), "system");
        speaker.person_id = Some(person.clone());
        speaker.assignment = Assignment::Auto;
        speaker.centroid = Some(broscribe_core::schema::Centroid::new(
            "synthetic",
            &[1.0, 0.0],
            1,
        ));
        let naming = CaptionNaming {
            speaker_id: speaker.id,
            name: "Daria".into(),
            is_self: false,
            distinct_spans: 1,
            purity: 1.0,
            start_s: 0.0,
            end_s: 2.0,
            ranges: vec![[0.0, 2.0]],
        };
        let mut embeddings = RangeEmbeddings::new("synthetic");
        embeddings.insert("system", [0.0, 2.0], vec![1.0, 0.0]);
        let print =
            caption_voiceprint(&speaker, &person, &first, "", &naming, &embeddings).unwrap();
        let mut prints = vec![print.clone(), print];
        assert_eq!(
            promote_captions(&mut prints, "synthetic", &[vec![0.0, 1.0]], false).unwrap(),
            0
        );
        prints[1].source.meeting_id = Some(second);
        assert_eq!(
            promote_captions(&mut prints, "synthetic", &[], false).unwrap(),
            0
        );
        assert_eq!(
            promote_captions(&mut prints, "synthetic", &[vec![0.0, 1.0]], false).unwrap(),
            2
        );
        prints[0].strength = Strength::Weak;
        assert_eq!(
            promote_captions(&mut prints, "synthetic", &[], true).unwrap(),
            1
        );
    }
}
