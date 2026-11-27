//! "Only this line" attribution (design §6.4): the user attributes single
//! transcript segments to a person without touching speakers, centroids,
//! voiceprints, samples, the speaker namespace or the queue.
//!
//! Entries live in `meeting.json` (`utterance_people`), keyed by a segment
//! fingerprint (track, exact start and end, SHA-256 of the text), never by
//! segment ID. [`effective_attribution`] is the one place that turns them
//! into per-segment people; the index, `meeting.md`, the CLI, MCP, the app
//! and enrichment all read attribution through it, so `index rebuild`
//! reproduces every derived row.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{EditBusy, editable_meeting, person_names, resolve_or_new_person, save_and_index};
use crate::index::{Index, meeting_window_revision};
use crate::live_identity::{self, LiveAction};
use crate::render::{format_hms, is_line_label, map_placeholders};
use crate::schema::{
    self, Action, GeneratedContent, Meeting, Segment, Speaker, TemplateResult, UtterancePerson,
    hex, sha256_hex,
};
use crate::{Error, MeetingId, PersonId, Result, WriteTx};

/// Identifies one transcript segment independently of its ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SegmentFingerprint {
    pub track: String,
    pub start: f64,
    pub end: f64,
    /// Lowercase hex SHA-256 of the segment's text.
    pub text_sha256: String,
}

/// The fingerprint of `segment`.
pub fn segment_fingerprint(segment: &Segment) -> SegmentFingerprint {
    SegmentFingerprint {
        track: segment.track.clone(),
        start: segment.start,
        end: segment.end,
        text_sha256: sha256_hex(segment.text.as_bytes()),
    }
}

/// Who one segment is attributed to.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SegmentAttribution {
    pub segment_id: u32,
    /// The line correction's person when `line`, else the person the
    /// segment's speaker is named as (`confirmed` or `auto`).
    pub person_id: Option<PersonId>,
    /// A line correction applies to this segment.
    pub line: bool,
    /// The live decision that created the line correction
    /// ([`UtterancePerson::live_decision`]); `None` for a line the user
    /// corrected on the meeting, and without a line correction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_decision: Option<u64>,
}

impl SegmentAttribution {
    /// The line correction's person, when one applies.
    pub fn line_person(&self) -> Option<&PersonId> {
        if self.line {
            self.person_id.as_ref()
        } else {
            None
        }
    }
}

/// Exact position key of a segment or entry (track, start and end bits).
type Position<'a> = (&'a str, u64, u64);

/// One line correction: its text hash, person and live decision.
type Entry<'m> = (&'m str, &'m PersonId, Option<u64>);

/// Line corrections by position; later entries for the same fingerprint
/// win.
fn overlay(meeting: &Meeting) -> HashMap<Position<'_>, Vec<Entry<'_>>> {
    let mut by_position: HashMap<Position<'_>, Vec<Entry<'_>>> = HashMap::new();
    for e in &meeting.utterance_people {
        by_position
            .entry((e.track.as_str(), e.start.to_bits(), e.end.to_bits()))
            .or_default()
            .push((e.text_sha256.as_str(), &e.person_id, e.live_decision));
    }
    by_position
}

/// The line correction matching `segment`, if any.
fn line_person<'m>(
    overlay: &HashMap<Position<'_>, Vec<Entry<'m>>>,
    segment: &Segment,
) -> Option<(&'m PersonId, Option<u64>)> {
    let candidates = overlay.get(&(
        segment.track.as_str(),
        segment.start.to_bits(),
        segment.end.to_bits(),
    ))?;
    let hash = sha256_hex(segment.text.as_bytes());
    candidates
        .iter()
        .rev()
        .find(|(h, _, _)| *h == hash)
        .map(|(_, p, d)| (*p, *d))
}

/// Per-segment attribution of `meeting`, in the order of `meeting.segments`:
/// a matching line correction wins over the person of the segment's
/// effective speaker (its group's kept speaker when grouped).
pub fn effective_attribution(meeting: &Meeting) -> Vec<SegmentAttribution> {
    let overlay = overlay(meeting);
    meeting
        .segments
        .iter()
        .map(|seg| {
            let line = if overlay.is_empty() {
                None
            } else {
                line_person(&overlay, seg)
            };
            SegmentAttribution {
                segment_id: seg.id,
                line: line.is_some(),
                live_decision: line.and_then(|(_, d)| d),
                person_id: line
                    .map(|(p, _)| p)
                    .or_else(|| {
                        seg.speaker
                            .and_then(|s| meeting.speaker(super::effective_speaker(meeting, s)))
                            .and_then(Speaker::named_person)
                    })
                    .cloned(),
            }
        })
        .collect()
}

/// SHA-256 of the meeting's line corrections (fingerprint and person),
/// sorted; empty without any. An enrichment records it and is stale once it
/// differs.
pub fn attribution_revision(meeting: &Meeting) -> String {
    if meeting.utterance_people.is_empty() {
        return String::new();
    }
    let mut entries: Vec<&UtterancePerson> = meeting.utterance_people.iter().collect();
    entries.sort_by(|a, b| {
        a.track
            .cmp(&b.track)
            .then(a.start.total_cmp(&b.start))
            .then(a.end.total_cmp(&b.end))
            .then(a.text_sha256.cmp(&b.text_sha256))
            .then(a.person_id.cmp(&b.person_id))
    });
    let mut h = Sha256::new();
    for e in entries {
        h.update(e.track.as_bytes());
        h.update(b"\t");
        h.update(e.start.to_bits().to_le_bytes());
        h.update(e.end.to_bits().to_le_bytes());
        h.update(b"\t");
        h.update(e.text_sha256.as_bytes());
        h.update(b"\t");
        h.update(e.person_id.as_str().as_bytes());
        h.update(b"\n");
    }
    hex(&h.finalize())
}

/// Whether one piece of generated content (the enrichment or a template
/// result) no longer matches the meeting's transcript: marked stale, or
/// produced for another window or attribution revision.
pub fn generated_is_stale(meeting: &Meeting, g: &dyn GeneratedContent) -> bool {
    g.marked_stale()
        || (!g.revision().is_empty() && g.revision() != meeting_window_revision(meeting))
        || g.attribution_revision() != attribution_revision(meeting)
}

/// [`generated_is_stale`] for the meeting's enrichment; `false` without one.
pub fn enrichment_is_stale(meeting: &Meeting) -> bool {
    meeting
        .enrichment
        .as_ref()
        .is_some_and(|e| generated_is_stale(meeting, e))
}

/// [`generated_is_stale`] for one template result.
pub fn template_is_stale(meeting: &Meeting, result: &TemplateResult) -> bool {
    generated_is_stale(meeting, result)
}

/// Marks each piece of generated content stale when [`generated_is_stale`]
/// says so; returns whether the enrichment is stale now.
pub fn mark_generated_stale(meeting: &mut Meeting) -> bool {
    let window = meeting_window_revision(meeting);
    let attribution = attribution_revision(meeting);
    for g in meeting.generated_mut() {
        let stale = g.marked_stale()
            || (!g.revision().is_empty() && g.revision() != window)
            || g.attribution_revision() != attribution;
        g.set_stale(stale);
    }
    meeting.enrichment.as_ref().is_some_and(|e| e.stale)
}

/// Attributes the segments `fingerprints` name to the person `person_ref`
/// names (ID, e-mail or name, as [`super::name_speaker`]; a new name creates
/// the person), or removes their line corrections with `None`. Speakers,
/// samples, voiceprints and the queue are left alone; generated content
/// (the enrichment, template results) whose revisions no longer match is
/// marked stale.
///
/// The edit is the user's final word on those lines: every live-preview
/// decision that claims one of them (its mapped range overlaps the line on
/// its track, whichever decision created the line) is marked
/// `user_override`, so no recheck, reprocess or later reconciliation can
/// add the line back from any of them, and the LivePreview review notes are
/// regenerated ([`live_identity::refresh_review_notes`]).
///
/// Refused (`library_busy`) unless the meeting's processing is done and no
/// enrichment of it is queued or running (a template job may run: its
/// result is stored stale); every fingerprint must match a current segment
/// exactly (else `invalid_input`, nothing written).
pub fn set_utterance_people(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    fingerprints: &[SegmentFingerprint],
    person_ref: Option<&str>,
) -> Result<Meeting> {
    let lib = tx.library().clone();
    let mut meeting = editable_meeting(tx, meeting_id, "correcting lines", EditBusy::Enrichment)?;
    if fingerprints.is_empty() {
        return Err(Error::InvalidInput("no transcript line given".into()));
    }
    let current: BTreeSet<(&str, u64, u64, String)> = meeting
        .segments
        .iter()
        .map(|s| {
            (
                s.track.as_str(),
                s.start.to_bits(),
                s.end.to_bits(),
                sha256_hex(s.text.as_bytes()),
            )
        })
        .collect();
    let key = |f: &SegmentFingerprint| (f.start.to_bits(), f.end.to_bits(), f.text_sha256.clone());
    if !fingerprints.iter().all(|f| {
        let (start, end, hash) = key(f);
        current.contains(&(f.track.as_str(), start, end, hash))
    }) {
        return Err(Error::InvalidInput("the transcript changed; reload".into()));
    }

    let person = match person_ref {
        None => None,
        Some(reference) => {
            let (p, created) = resolve_or_new_person(&lib, reference)?;
            if created {
                tx.save_person(&p)?;
                index.upsert_person(tx, &p)?;
            }
            Some(p.id)
        }
    };

    let targets: BTreeSet<(String, u64, u64, String)> = fingerprints
        .iter()
        .map(|f| {
            let (start, end, hash) = key(f);
            (f.track.clone(), start, end, hash)
        })
        .collect();
    let before = meeting.utterance_people.clone();
    let at = schema::now();
    let mut kept: Vec<UtterancePerson> = Vec::with_capacity(before.len() + targets.len());
    let mut unchanged = Vec::new();
    // Live-preview decisions this explicit edit settles: the user's choice
    // now stands (the line becomes theirs, or goes), and none of them may
    // ever add it back.
    let mut overridden: BTreeSet<u64> = BTreeSet::new();
    for e in before.iter() {
        let k = (
            e.track.clone(),
            e.start.to_bits(),
            e.end.to_bits(),
            e.text_sha256.clone(),
        );
        if !targets.contains(&k) {
            kept.push(e.clone());
            continue;
        }
        if let Some(decision) = e.live_decision {
            overridden.insert(decision);
        }
        if person.as_ref() == Some(&e.person_id) && !unchanged.contains(&k) {
            // Already attributed to this person: keep its original time.
            unchanged.push(k);
            kept.push(UtterancePerson {
                live_decision: None,
                ..e.clone()
            });
        }
    }
    // A line can be supported by several decisions (adjacent turns of one
    // person join), while it records only one: every decision whose range
    // claims an edited line is settled, not just the recorded one.
    overridden.extend(
        meeting
            .live_attributions
            .iter()
            .filter(|a| claims_any_line(a, fingerprints))
            .map(|a| a.decision),
    );
    let mut overrides_changed = false;
    for a in &mut meeting.live_attributions {
        if overridden.contains(&a.decision) && !a.user_override {
            a.user_override = true;
            overrides_changed = true;
        }
    }
    if let Some(person) = &person {
        for f in fingerprints {
            let (start, end, hash) = key(f);
            let k = (f.track.clone(), start, end, hash);
            if unchanged.contains(&k) {
                continue;
            }
            unchanged.push(k);
            kept.push(UtterancePerson {
                track: f.track.clone(),
                start: f.start,
                end: f.end,
                text_sha256: f.text_sha256.clone(),
                person_id: person.clone(),
                at,
                live_decision: None,
            });
        }
    }
    kept.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then(a.track.cmp(&b.track))
            .then(a.end.total_cmp(&b.end))
    });
    if kept == before && !overrides_changed {
        return Ok(meeting);
    }
    if overrides_changed {
        live_identity::refresh_review_notes(&mut meeting, &person_names(&lib)?);
    }
    meeting.utterance_people = kept;
    super::live_speaker::line_evidence_changed(tx, &mut meeting)?;
    mark_generated_stale(&mut meeting);
    save_and_index(tx, index, &mut meeting)?;
    Ok(meeting)
}

/// Refused: the live review item changed since it was shown (its
/// [`live_identity::LiveReviewItem::review_key`] differs, or it no longer
/// asks for review for another reason than the user's own settling).
pub const STALE_REVIEW: &str = "stale_review";

/// Result of [`dismiss_live_review`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LiveDismissOutcome {
    pub meeting_id: MeetingId,
    pub decision: u64,
    pub turn_id: String,
    /// `false` when the user had already settled this decision (nothing
    /// written).
    pub changed: bool,
    /// Live decisions of the meeting still awaiting review.
    pub remaining: usize,
}

/// "Leave as transcribed": settles one live decision awaiting review
/// (partial, conflicting or unmapped — an unmapped one has no line to
/// correct) without changing any line. Fenced by the item exactly as shown:
/// `decision` and its `review_key` from [`live_identity::live_review_items`];
/// a different key is `stale_review`, as is a decision that left review
/// for any reason other than the user (a reprocess applied it, a forget
/// cleared it). A decision the user already settled is unchanged
/// (`changed: false`); one the meeting does not have is `not_found`.
///
/// Only that record changes: it is marked [`live_identity::LiveAttribution::user_override`],
/// so no recheck, reprocess or republication adds a line from it, and the
/// LivePreview notes are regenerated. Line corrections (the user's and
/// other decisions'), speakers, voices, the sidecar and forget tombstones
/// are untouched, so nothing is learned and nobody forgotten comes back.
/// Refused like a regrouping while any job works on the meeting.
pub fn dismiss_live_review(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    decision: u64,
    review_key: &str,
) -> Result<LiveDismissOutcome> {
    let lib = tx.library().clone();
    let mut meeting = editable_meeting(
        tx,
        meeting_id,
        "settling live decisions",
        EditBusy::AnyMeetingJob,
    )?;
    let Some(record) = meeting
        .live_attributions
        .iter()
        .find(|a| a.decision == decision)
    else {
        return Err(Error::NotFound(format!(
            "live decision {decision} in meeting {meeting_id}"
        )));
    };
    let turn_id = record.turn_id.clone();
    let stale = || Error::Service {
        code: STALE_REVIEW,
        message: format!(
            "live decision {decision} in meeting {meeting_id} changed since it was shown; reload it"
        ),
    };
    if !record.needs_review() {
        if record.user_override && record.outcome.unresolved() {
            let remaining = live_identity::live_review_items(&meeting).len();
            return Ok(LiveDismissOutcome {
                meeting_id: meeting.id,
                decision,
                turn_id,
                changed: false,
                remaining,
            });
        }
        return Err(stale());
    }
    let current = live_identity::live_review_items(&meeting)
        .into_iter()
        .find(|i| i.decision == decision)
        .ok_or_else(stale)?;
    if current.review_key != review_key {
        return Err(stale());
    }
    for a in &mut meeting.live_attributions {
        if a.decision == decision {
            a.user_override = true;
        }
    }
    live_identity::refresh_review_notes(&mut meeting, &person_names(&lib)?);
    save_and_index(tx, index, &mut meeting)?;
    Ok(LiveDismissOutcome {
        remaining: live_identity::live_review_items(&meeting).len(),
        meeting_id: meeting.id,
        decision,
        turn_id,
        changed: true,
    })
}

/// Whether live record `a` names someone over one of `lines`: its mapped
/// range overlaps the line on the same track beyond rounding. A clear, a
/// forgotten record or an unmapped range claims nothing.
fn claims_any_line(a: &live_identity::LiveAttribution, lines: &[SegmentFingerprint]) -> bool {
    if a.action == LiveAction::Clear || a.person_id.is_none() {
        return false;
    }
    let (Some(from), Some(to)) = (a.from_s, a.to_s) else {
        return false;
    };
    lines.iter().any(|f| {
        f.track == a.range.track && f.end.min(to) - f.start.max(from) > live_identity::ROUNDING_S
    })
}

/// Shortest share of either span a moved line correction must overlap.
const REAPPLY_MIN_SHARE: f64 = 0.5;

/// After reprocessing replaced `meeting.segments`, moves each line
/// correction to the new segment on the same track with the same text whose
/// time overlap reaches half of either span (the largest overlap wins; one
/// correction per segment). Corrections that match nothing are dropped;
/// returns one review note for each (speaker identity is not involved).
pub fn reapply_utterance_people(meeting: &mut Meeting) -> Vec<String> {
    if meeting.utterance_people.is_empty() {
        return Vec::new();
    }
    let hashes: Vec<String> = meeting
        .segments
        .iter()
        .map(|s| sha256_hex(s.text.as_bytes()))
        .collect();
    let mut taken = vec![false; meeting.segments.len()];
    let mut notes = Vec::new();
    let mut kept = Vec::with_capacity(meeting.utterance_people.len());
    let entries = std::mem::take(&mut meeting.utterance_people);
    // Exact matches first, so a moved entry never takes an unchanged one's line.
    let mut pending = Vec::new();
    for e in entries {
        let exact = meeting.segments.iter().position(|s| {
            s.track == e.track
                && s.start.to_bits() == e.start.to_bits()
                && s.end.to_bits() == e.end.to_bits()
        });
        match exact.filter(|&i| !taken[i] && hashes[i] == e.text_sha256) {
            Some(i) => {
                taken[i] = true;
                kept.push(e);
            }
            None => pending.push(e),
        }
    }
    for mut e in pending {
        let best = meeting
            .segments
            .iter()
            .enumerate()
            .filter(|(i, s)| !taken[*i] && s.track == e.track && hashes[*i] == e.text_sha256)
            .filter_map(|(i, s)| {
                let overlap = s.end.min(e.end) - s.start.max(e.start);
                let enough = overlap > 0.0
                    && (overlap >= REAPPLY_MIN_SHARE * (e.end - e.start)
                        || overlap >= REAPPLY_MIN_SHARE * (s.end - s.start));
                enough.then_some((i, overlap))
            })
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match best {
            Some((i, _)) => {
                taken[i] = true;
                let s = &meeting.segments[i];
                e.start = s.start;
                e.end = s.end;
                kept.push(e);
            }
            None => notes.push(format!(
                "line correction at {} ({}) to {} was dropped: no line with the same text after reprocessing",
                format_hms(e.start),
                e.track,
                e.person_id
            )),
        }
    }
    kept.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then(a.track.cmp(&b.track))
            .then(a.end.total_cmp(&b.end))
    });
    meeting.utterance_people = kept;
    notes
}

/// Whether `meeting` refers to `person` anywhere a name is shown: a speaker
/// (named, suggested or rejected), a participant, a line correction or a
/// line label of its enrichment or any template result. Renaming the person
/// re-renders such meetings.
pub fn meeting_references_person(meeting: &Meeting, person: &PersonId) -> bool {
    meeting.speakers.iter().any(|s| {
        s.person_id.as_ref() == Some(person)
            || s.suggestion
                .as_ref()
                .is_some_and(|x| x.person_id == *person)
            || s.rejected_person_ids.contains(person)
            || s.live_promotion
                .as_ref()
                .is_some_and(|p| &p.person_id == person)
            || s.identity_conflict.as_ref().is_some_and(|conflict| {
                conflict.candidate.as_ref() == Some(person)
                    || conflict.lines.iter().any(|line| &line.person_id == person)
            })
    }) || meeting
        .participants
        .iter()
        .any(|p| p.person_id.as_ref() == Some(person))
        || meeting
            .utterance_people
            .iter()
            .any(|u| u.person_id == *person)
        || meeting
            .generated()
            .any(|g| g.line_people().values().any(|p| p == person))
}

/// What a line label becomes: `None` keeps it, `Some(Some(label))` renames
/// it, `Some(None)` makes it anonymous (`Unknown`).
pub type LineLabelRewrite<'a> = dyn FnMut(&str) -> Option<Option<String>> + 'a;

fn relabel_text(text: &mut String, f: &mut LineLabelRewrite<'_>) {
    let out = map_placeholders(text, |inner| {
        if !is_line_label(inner) {
            return None;
        }
        f(inner).map(|to| match to {
            Some(label) => format!("{{{{{label}}}}}"),
            None => "Unknown".to_owned(),
        })
    });
    if out != *text {
        *text = out;
    }
}

fn relabel_name(name: &mut Option<String>, f: &mut LineLabelRewrite<'_>) {
    let Some(n) = name.as_deref().map(str::trim).filter(|n| is_line_label(n)) else {
        return;
    };
    if let Some(to) = f(n) {
        *name = Some(to.unwrap_or_else(|| "Unknown".to_owned()));
    }
}

/// Rewrites the line labels (`{{P#}}` in text, and an owner name that is a
/// label) of `actions` through `f`.
pub fn relabel_actions(actions: &mut [Action], f: &mut LineLabelRewrite<'_>) {
    for a in actions {
        relabel_text(&mut a.text, f);
        relabel_name(&mut a.owner_name, f);
        if let Some(owner) = &mut a.original_owner {
            relabel_name(&mut owner.name, f);
        }
    }
}

/// Rewrites every line label of the generated content's prose and names
/// through `f` (its `line_people` map is left to the caller).
pub fn relabel_generated(g: &mut dyn GeneratedContent, f: &mut LineLabelRewrite<'_>) {
    g.for_each_text_mut(&mut |_, text| relabel_text(text, f));
    g.for_each_speaker_ref_mut(&mut |_, _, name| relabel_name(name, f));
}

/// Drops the line labels of people `gone` says are gone (forgotten): their
/// `line_people` entries go and their references become `Unknown`, so the
/// label can never resolve to someone else later. Returns whether anything
/// was dropped.
pub fn forget_line_people(g: &mut dyn GeneratedContent, gone: impl Fn(&PersonId) -> bool) -> bool {
    let dropped: BTreeSet<String> = g
        .line_people()
        .iter()
        .filter(|(_, p)| gone(p))
        .map(|(l, _)| l.clone())
        .collect();
    if dropped.is_empty() {
        return false;
    }
    g.line_people_mut().retain(|l, _| !dropped.contains(l));
    relabel_generated(g, &mut |l: &str| dropped.contains(l).then_some(None));
    true
}

/// Rewrites labels of `actions`, taken from an earlier enrichment whose
/// labels were `old`, into the labels of `new` for the same people, adding
/// a label to `new` for a person it lacks; a label `old` does not map
/// becomes `Unknown`. A label then never names a different person.
pub fn carry_line_labels(
    actions: &mut [Action],
    old: &BTreeMap<String, PersonId>,
    new: &mut BTreeMap<String, PersonId>,
) {
    relabel_actions(actions, &mut |label: &str| {
        let Some(person) = old.get(label) else {
            return Some(None);
        };
        Some(Some(person_label(new, person)))
    });
}

/// Reuses the generated content's label for a person or allocates one.
pub(super) fn person_label(labels: &mut BTreeMap<String, PersonId>, person: &PersonId) -> String {
    if let Some((label, _)) = labels.iter().find(|(_, p)| *p == person) {
        return label.clone();
    }
    let next = labels
        .keys()
        .filter_map(|label| label.strip_prefix('P')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let label = format!("P{next}");
    labels.insert(label.clone(), person.clone());
    label
}
