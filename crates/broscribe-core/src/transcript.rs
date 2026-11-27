//! The one transcript projection every front end reads (CL06): the CLI
//! `transcript` (text and JSON), MCP `get_transcript`, the app's transcript
//! view, `meeting.md` and exports.
//!
//! One rule decides, for all of them:
//! - **Order:** non-empty segments by start time, then segment ID.
//! - **Who:** [`ops::effective_attribution`]: a line correction names its
//!   person over the segment's speaker; a merged member shows as its
//!   group's kept speaker ([`ops::effective_speaker`]), with the diarized
//!   one kept as `original_speaker`.
//! - **Label:** a corrected line shows its person's name; every other line
//!   (and a corrected line whose person has no name) shows its shown
//!   speaker's label ([`render::speaker_label`]: the person, else
//!   `Unknown (mic)` or `Speaker N`, `Unknown` without a speaker).
//! - **Provenance:** whether a line correction or the speaker attributes
//!   the line, the shown speaker's assignment (confirmed, auto, …) and the
//!   live decision a line correction came from.
//! - **Lines:** consecutive segments with one shown speaker and one line
//!   correction merge into one line ([`merge_lines`]); a correction starts
//!   and ends its own line.
//! - **Filters:** [`resolve_who`] reads a speaker ID, a person or a shown
//!   label the same way for every front end; times keep overlapping lines.
//!
//! Values are raw: names and text exactly as stored. Rendering applies a
//! [`NameStyle`] to names ([`format_line`]); transcript text is never
//! rewritten.
//!
//! [`prompt_transcript`] is the projection for LLM prompts. It takes no
//! people at all, so it cannot carry a name: lines a person was attributed
//! to are labelled `P1`, `P2`… (mapped back only through its
//! `line_people`), other lines by their shown speaker `S#` (`S?` without
//! one).

use std::collections::BTreeMap;

use crate::ops::{self, SegmentFingerprint};
use crate::render::{self, NameStyle, PeopleLookup, format_hms};
use crate::schema::{Assignment, Meeting, Segment};
use crate::{Error, Library, PersonId, Result, SpeakerId};

pub const OVERLAP_LABEL: &str = "Overlapping speech";

/// What attributes a transcript segment to its person.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributionSource {
    /// The shown speaker (its person, if named).
    Speaker,
    /// A line correction ("Only this line").
    Line,
}

impl AttributionSource {
    /// `"speaker"` or `"line"`, as the CLI and MCP report it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Speaker => "speaker",
            Self::Line => "line",
        }
    }
}

/// One transcript segment as every front end shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptSegment<'m> {
    pub segment_id: u32,
    pub track: &'m str,
    pub start: f64,
    pub end: f64,
    /// The speaker shown (a merged member's kept speaker).
    pub speaker: Option<SpeakerId>,
    /// The diarized speaker, only when it differs from `speaker`.
    pub original_speaker: Option<SpeakerId>,
    /// The shown speaker's label, whoever the line is attributed to.
    pub speaker_label: String,
    /// The shown speaker's assignment.
    pub assignment: Option<Assignment>,
    /// The effective person: the line correction's, else the shown
    /// speaker's named (`confirmed` or `auto`) person.
    pub person_id: Option<PersonId>,
    /// `person_id`'s display name, when it has one.
    pub person_name: Option<String>,
    pub attribution: AttributionSource,
    /// The live decision a line correction came from (`None`: the user set
    /// it on the meeting, or no correction applies).
    pub live_decision: Option<u64>,
    pub overlapping_speech: bool,
    /// The name shown for the line.
    pub label: String,
    /// The text, trimmed.
    pub text: &'m str,
}

impl TranscriptSegment<'_> {
    /// The diarized speaker.
    pub fn diarized_speaker(&self) -> Option<SpeakerId> {
        self.original_speaker.or(self.speaker)
    }

    /// The person of the line correction, when one applies.
    pub fn line_person(&self) -> Option<&PersonId> {
        match self.attribution {
            AttributionSource::Line => self.person_id.as_ref(),
            AttributionSource::Speaker => None,
        }
    }
}

/// Which lines a transcript read keeps.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptFilter {
    pub who: Option<Who>,
    /// Keeps lines ending after this (seconds from the start).
    pub from_s: Option<f64>,
    /// Keeps lines starting before this.
    pub to_s: Option<f64>,
}

/// Whose lines a transcript read keeps ([`resolve_who`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Who {
    /// The lines shown as this speaker (a group's lines for its kept
    /// speaker); with `own_lines_only` (a merged member's ID), only the
    /// lines diarized as it.
    Speaker { id: SpeakerId, own_lines_only: bool },
    /// The lines a person said: their speakers' lines and lines corrected
    /// to them, without lines corrected to someone else.
    Person(PersonId),
    /// The lines whose shown label is this (case-insensitive, trimmed).
    Label(String),
}

impl Who {
    pub fn selects(&self, s: &TranscriptSegment<'_>) -> bool {
        match self {
            Self::Speaker {
                id,
                own_lines_only: true,
            } => s.diarized_speaker() == Some(*id),
            Self::Speaker { id, .. } => s.speaker == Some(*id),
            Self::Person(p) => s.person_id.as_ref() == Some(p),
            Self::Label(label) => fold(&s.label) == fold(label),
        }
    }
}

impl TranscriptFilter {
    pub fn selects(&self, s: &TranscriptSegment<'_>) -> bool {
        self.who.as_ref().is_none_or(|w| w.selects(s))
            && self.from_s.is_none_or(|f| s.end > f)
            && self.to_s.is_none_or(|t| s.start < t)
    }
}

/// What `reference` (a CLI `--speaker` or MCP `speaker` argument) selects
/// in `meeting`: a speaker ID of the meeting (`not_found` when it has no
/// such speaker), else the person [`ops::find_person`] names (an
/// ambiguous name is `invalid_input`), else a label some line of the
/// meeting shows (`Speaker 2`, `Unknown (mic)`), else `not_found`.
pub fn resolve_who(
    lib: &Library,
    meeting: &Meeting,
    people: &PeopleLookup,
    reference: &str,
) -> Result<Who> {
    let reference = reference.trim();
    let not_found = || Error::NotFound(format!("speaker {reference:?} in meeting {}", meeting.id));
    if let Ok(id) = reference.parse::<SpeakerId>() {
        meeting.speaker(id).ok_or_else(not_found)?;
        return Ok(Who::Speaker {
            id,
            own_lines_only: ops::is_grouped_member(meeting, id),
        });
    }
    if let Some(p) = ops::find_person(lib, reference)? {
        return Ok(Who::Person(p.id));
    }
    let label = fold(reference);
    if transcript(meeting, people, &TranscriptFilter::default())
        .iter()
        .any(|s| fold(&s.label) == label)
    {
        return Ok(Who::Label(label));
    }
    Err(not_found())
}

/// Whether a saved line matches a nonempty query folded with `fold_search_text`.
pub fn line_matches(line: &TranscriptLine, folded_query: &str) -> bool {
    !folded_query.is_empty()
        && (crate::names::fold_search_text(&line.text).contains(folded_query)
            || crate::names::fold_search_text(&line.label).contains(folded_query))
}

/// The meeting's transcript segments `filter` keeps, in transcript order.
pub fn transcript<'m>(
    meeting: &'m Meeting,
    people: &PeopleLookup,
    filter: &TranscriptFilter,
) -> Vec<TranscriptSegment<'m>> {
    let attribution = ops::effective_attribution(meeting);
    order(meeting)
        .into_iter()
        .map(|i| {
            let seg = &meeting.segments[i];
            let a = &attribution[i];
            let speaker = seg.speaker.map(|id| ops::effective_speaker(meeting, id));
            let person_name = a
                .person_id
                .as_ref()
                .and_then(|p| people.display_name(p))
                .map(str::to_owned);
            let overlapping_speech = !seg.speech_overlap.is_empty();
            let speaker_label = if overlapping_speech {
                OVERLAP_LABEL.to_owned()
            } else {
                render::speaker_label(meeting, seg.speaker, people)
            };
            let label = match (&person_name, a.line) {
                (Some(name), true) => name.clone(),
                _ => speaker_label.clone(),
            };
            TranscriptSegment {
                segment_id: seg.id,
                track: &seg.track,
                start: seg.start,
                end: seg.end,
                speaker,
                original_speaker: seg.speaker.filter(|_| speaker != seg.speaker),
                speaker_label,
                assignment: speaker
                    .and_then(|s| meeting.speaker(s))
                    .map(|s| s.assignment),
                person_id: a.person_id.clone(),
                person_name,
                attribution: if a.line {
                    AttributionSource::Line
                } else {
                    AttributionSource::Speaker
                },
                live_decision: a.live_decision,
                overlapping_speech,
                label,
                text: seg.text.trim(),
            }
        })
        .filter(|s| filter.selects(s))
        .collect()
}

/// One shown line: consecutive segments with one shown speaker and one line
/// correction, merged.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptLine {
    pub start: f64,
    /// The latest end of its segments.
    pub end: f64,
    pub speaker: Option<SpeakerId>,
    /// The line correction's person, else the shown speaker's named person.
    pub person_id: Option<PersonId>,
    /// The line correction's person (shown as "corrected"), if any.
    pub line_person: Option<PersonId>,
    pub overlapping_speech: bool,
    pub label: String,
    /// The segments' texts joined by single spaces.
    pub text: String,
    /// The merged segments, in order: the scope of "Only this line".
    pub segment_ids: Vec<u32>,
}

impl TranscriptLine {
    /// `[hh:mm:ss] Label: text` with the label written as `style`.
    pub fn display(&self, style: NameStyle) -> String {
        let label = display_label(
            &self.label,
            self.overlapping_speech && self.line_person.is_some(),
            style,
        );
        format_line(self.start, &label, &self.text, NameStyle::Raw)
    }
}

/// A corrected name can appear alongside the overlap label.
pub(crate) fn display_label(label: &str, name_in_overlap: bool, style: NameStyle) -> String {
    let label = style.apply(label);
    if name_in_overlap && label != OVERLAP_LABEL {
        format!("{label} ({OVERLAP_LABEL})")
    } else {
        label
    }
}

/// `segments` (in transcript order) as shown lines.
pub fn merge_lines(segments: &[TranscriptSegment<'_>]) -> Vec<TranscriptLine> {
    let mut lines: Vec<TranscriptLine> = Vec::new();
    for s in segments {
        if let Some(last) = lines.last_mut()
            && last.speaker == s.speaker
            && last.line_person.as_ref() == s.line_person()
            && last.overlapping_speech == s.overlapping_speech
        {
            last.text.push(' ');
            last.text.push_str(s.text);
            last.end = last.end.max(s.end);
            last.segment_ids.push(s.segment_id);
            continue;
        }
        lines.push(TranscriptLine {
            start: s.start,
            end: s.end,
            speaker: s.speaker,
            person_id: s.person_id.clone(),
            line_person: s.line_person().cloned(),
            overlapping_speech: s.overlapping_speech,
            label: s.label.clone(),
            text: s.text.to_owned(),
            segment_ids: vec![s.segment_id],
        });
    }
    lines
}

/// `[hh:mm:ss] Label: text`, the label written as `style` (`Plain` for
/// terminal text, `Markdown` for `meeting.md` and Markdown exports); the
/// text is never rewritten.
pub fn format_line(start: f64, label: &str, text: &str, style: NameStyle) -> String {
    format!("[{}] {}: {}", format_hms(start), style.apply(label), text)
}

/// Fingerprints of `segment_ids` in `meeting` (all of them, or `None` when
/// one is gone): what a line correction is written against.
pub fn line_fingerprints(
    meeting: &Meeting,
    segment_ids: &[u32],
) -> Option<Vec<SegmentFingerprint>> {
    segment_ids
        .iter()
        .map(|id| {
            meeting
                .segments
                .iter()
                .find(|s| s.id == *id)
                .map(ops::segment_fingerprint)
        })
        .collect()
}

/// One segment as an LLM prompt sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptSegment<'m> {
    pub segment_id: u32,
    pub start: f64,
    pub end: f64,
    /// `P#` for a line attributed to a person, else the shown speaker
    /// `S#`, `S?` without one. Never a name.
    pub label: String,
    /// The text, trimmed.
    pub text: &'m str,
}

/// [`prompt_transcript`]'s result.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptTranscript<'m> {
    pub segments: Vec<PromptSegment<'m>>,
    /// Each `P#` label's person (numbered by first appearance).
    pub line_people: BTreeMap<String, PersonId>,
}

/// The transcript for an LLM prompt, in transcript order, without any name
/// (see the module notes).
pub fn prompt_transcript(meeting: &Meeting) -> PromptTranscript<'_> {
    let attribution = ops::effective_attribution(meeting);
    let mut people: Vec<&PersonId> = Vec::new();
    let mut segments = Vec::new();
    for i in order(meeting) {
        let seg = &meeting.segments[i];
        let label = match attribution[i].line_person() {
            Some(person) => {
                let n = match people.iter().position(|p| *p == person) {
                    Some(n) => n,
                    None => {
                        people.push(person);
                        people.len() - 1
                    }
                };
                format!("P{}", n + 1)
            }
            None => seg.speaker.map_or_else(
                || "S?".to_owned(),
                |id| ops::effective_speaker(meeting, id).to_string(),
            ),
        };
        segments.push(PromptSegment {
            segment_id: seg.id,
            start: seg.start,
            end: seg.end,
            label,
            text: seg.text.trim(),
        });
    }
    PromptTranscript {
        segments,
        line_people: people
            .into_iter()
            .enumerate()
            .map(|(n, p)| (format!("P{}", n + 1), p.clone()))
            .collect(),
    }
}

/// Indices of the non-empty segments of `meeting` in transcript order.
fn order(meeting: &Meeting) -> Vec<usize> {
    let segs: &[Segment] = &meeting.segments;
    let mut order: Vec<usize> = (0..segs.len())
        .filter(|&i| !segs[i].text.trim().is_empty())
        .collect();
    order.sort_by(|&a, &b| {
        segs[a]
            .start
            .total_cmp(&segs[b].start)
            .then(segs[a].id.cmp(&segs[b].id))
    });
    order
}

fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}
