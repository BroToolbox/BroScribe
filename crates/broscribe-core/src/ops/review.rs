//! Review-queue decisions that are not identities (design §4, §6.4): the
//! voice samples a reviewer plays to decide, and "Can't tell" — dismissing
//! a speaker from review without naming or rejecting anyone.
//!
//! A dismissal is `Speaker::review_dismissed_at` in `meeting.json` and
//! nothing else: no rejection, voiceprint, assignment or suggestion
//! changes. A shown speaker (a group counts once) is dismissed only while
//! every one of its speakers carries the flag ([`review_dismissed`]), so
//! merging a pending speaker into a dismissed group brings the group back
//! to the queue and unmerging leaves each speaker's own flag.

use chrono::{DateTime, FixedOffset};
use serde::Serialize;

use super::grouping::writable_meeting;
use super::{
    REVIEW_SAMPLE_SEGMENTS, SpeakerFence, check_speaker_fence, effective_attribution,
    effective_speaker, effective_speakers, effective_talk_s, group_of, save_and_index,
    speaker_not_found,
};
use crate::index::Index;
use crate::schema::{self, Assignment, Meeting, Segment};
use crate::{Library, MeetingId, PersonId, Result, SpeakerId, WriteTx};

// ---- voice samples ----

/// Longest voice sample played for a speaker, in seconds.
pub const MAX_SAMPLE_S: f64 = 15.0;
/// Samples offered per speaker.
pub const MAX_SAMPLES: usize = 5;
/// Largest share of a (capped) sample other speakers may talk over before
/// the sample is left out.
const MAX_OVERLAP_SHARE: f64 = 0.3;

/// One line of a speaker played as a voice sample (seconds), with its text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SampleClip {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// First transcript segment of a shown speaker, including grouped members.
pub fn first_speaker_segment(meeting: &Meeting, id: SpeakerId) -> Option<&Segment> {
    let shown = effective_speaker(meeting, id);
    meeting.segments.iter().find(|segment| {
        segment
            .speaker
            .is_some_and(|id| effective_speaker(meeting, id) == shown)
    })
}

/// Saved clean turns are in meeting time. A duration alone cannot locate one.
pub(super) fn diarized_sample_ranges(
    meeting: &Meeting,
    shown: SpeakerId,
) -> impl Iterator<Item = (f64, f64)> {
    meeting
        .speakers
        .iter()
        .filter(move |speaker| effective_speaker(meeting, speaker.id) == shown)
        .filter_map(|speaker| speaker.centroid.as_ref())
        .flat_map(|centroid| &centroid.ranges)
        .filter_map(|&[start, end]| {
            let (start, end) = (f64::from(start), f64::from(end));
            if !start.is_finite()
                || !end.is_finite()
                || start < 0.0
                || !meeting.duration_s.is_finite()
            {
                return None;
            }
            let end = end.min(meeting.duration_s).min(start + MAX_SAMPLE_S);
            (end > start).then_some((start, end))
        })
}

fn rank_samples((a, b): (f64, f64), (c, d): (f64, f64)) -> std::cmp::Ordering {
    (d - c).total_cmp(&(b - a)).then(a.total_cmp(&c))
}

/// The lines of the speaker shown as `shown` (a member ID means its group)
/// worth playing to recognise the voice, best first: the shown speaker's
/// segments (every group member's included) minus lines a line correction
/// attributes to someone else, each capped to [`MAX_SAMPLE_S`] from its
/// start; capped lines that other shown speakers overlap (any track: the
/// tracks play mixed) by more than 30 % are left out unless that leaves
/// none. Longest first (ties: earlier), at most [`MAX_SAMPLES`]. Length
/// only ranks: a speaker with only short lines still gets them. Without a
/// transcript, use saved clean diarization ranges as clips with empty text.
pub fn sample_clips(meeting: &Meeting, shown: SpeakerId) -> Vec<SampleClip> {
    let shown = effective_speaker(meeting, shown);
    let Some(speaker) = meeting.speaker(shown) else {
        return Vec::new();
    };
    if first_speaker_segment(meeting, shown).is_none() {
        let mut ranges: Vec<_> = diarized_sample_ranges(meeting, shown).collect();
        ranges.sort_by(|&a, &b| rank_samples(a, b));
        ranges.dedup();
        return ranges
            .into_iter()
            .take(MAX_SAMPLES)
            .map(|(start, end)| SampleClip {
                start,
                end,
                text: String::new(),
            })
            .collect();
    }
    let named = speaker.named_person();
    let attribution = effective_attribution(meeting);
    let shown_of = |s: &Segment| s.speaker.map(|id| effective_speaker(meeting, id));
    let own: Vec<&Segment> = meeting
        .segments
        .iter()
        .zip(&attribution)
        .filter(|(s, _)| shown_of(s) == Some(shown))
        .filter(|(_, a)| a.line_person().is_none_or(|p| Some(p) == named))
        .map(|(s, _)| s)
        .filter(|s| s.start.is_finite() && s.end.is_finite() && s.start >= 0.0 && s.end > s.start)
        .collect();
    let others: Vec<(f64, f64)> = meeting
        .segments
        .iter()
        .filter(|s| shown_of(s).is_some_and(|id| id != shown))
        .map(|s| (s.start, s.end))
        .collect();
    let clear: Vec<&Segment> = own
        .iter()
        .copied()
        .filter(|s| {
            let (a, b) = capped(s);
            covered(&others, a, b) <= MAX_OVERLAP_SHARE * (b - a)
        })
        .collect();
    let mut picked = if clear.is_empty() { own } else { clear };
    picked.sort_by(|x, y| rank_samples(capped(x), capped(y)));
    picked.truncate(MAX_SAMPLES);
    picked
        .into_iter()
        .map(|s| {
            let (start, end) = capped(s);
            SampleClip {
                start,
                end,
                text: s.text.clone(),
            }
        })
        .collect()
}

/// `segment`'s range cut to [`MAX_SAMPLE_S`] from its start.
fn capped(segment: &Segment) -> (f64, f64) {
    (segment.start, segment.end.min(segment.start + MAX_SAMPLE_S))
}

/// Seconds of `[a, b]` that `ranges` cover (their union).
fn covered(ranges: &[(f64, f64)], a: f64, b: f64) -> f64 {
    let mut cut: Vec<(f64, f64)> = ranges
        .iter()
        .map(|&(c, d)| (c.max(a), d.min(b)))
        .filter(|(c, d)| d > c)
        .collect();
    cut.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut total = 0.0;
    let mut end = a;
    for (c, d) in cut {
        let from = c.max(end);
        if d > from {
            total += d - from;
            end = d;
        }
    }
    total
}

// ---- "Can't tell" ----

/// Every speaker of the group shown as `shown` (kept first), or `shown`.
fn shown_group(meeting: &Meeting, shown: SpeakerId) -> Vec<SpeakerId> {
    group_of(meeting, shown).map_or_else(|| vec![shown], |g| g.speakers().collect())
}

/// When the speaker shown as `id` (a member ID means its group) was
/// dismissed from review: the latest flag of its speakers, `None` unless
/// every one of them carries it.
pub fn review_dismissed_at(meeting: &Meeting, id: SpeakerId) -> Option<DateTime<FixedOffset>> {
    let shown = effective_speaker(meeting, id);
    let mut latest: Option<DateTime<FixedOffset>> = None;
    for s in shown_group(meeting, shown) {
        let at = meeting.speaker(s)?.review_dismissed_at?;
        latest = Some(latest.map_or(at, |l| l.max(at)));
    }
    latest
}

/// Whether the speaker shown as `id` is dismissed from review ("Can't
/// tell"): its kept speaker and every member carry the flag. The one rule
/// the review queue, review counts, the Speakers tab, the CLI and MCP use.
pub fn review_dismissed(meeting: &Meeting, id: SpeakerId) -> bool {
    review_dismissed_at(meeting, id).is_some()
}

/// Result of [`dismiss_review`] and [`restore_review`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DismissOutcome {
    pub meeting_id: MeetingId,
    /// The shown speaker (its group's kept speaker for a member ID).
    pub speaker_id: SpeakerId,
    /// The member ID the request named, when it was redirected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirected_from: Option<SpeakerId>,
    /// Every speaker of the shown group whose flag was written or cleared.
    pub speakers: Vec<SpeakerId>,
    /// Whether the speaker is dismissed now.
    pub dismissed: bool,
    /// Whether anything was written (both are idempotent).
    pub changed: bool,
}

/// "Can't tell": takes the speaker shown as `speaker_id` out of review
/// without any identity effect, flagging every speaker of its group (those
/// already flagged keep their time). Refused unless the meeting is `done`
/// and idle (`library_busy`; `invalid_input` when processing failed), and
/// fenced like naming (`speakers_reanalysed`, `speakers_regrouped`).
/// Retention is not applied here: the daemon's maintenance pass counts a
/// dismissal as settled once `retention::DISMISS_GRACE` has passed.
pub fn dismiss_review(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    speaker_id: SpeakerId,
    fence: &SpeakerFence,
) -> Result<DismissOutcome> {
    set_dismissed(tx, index, meeting_id, speaker_id, fence, true)
}

/// Undoes [`dismiss_review`]: clears the flag on every speaker of the
/// group shown as `speaker_id`, which returns to the queue if still
/// pending. Refused and fenced like [`dismiss_review`].
pub fn restore_review(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    speaker_id: SpeakerId,
    fence: &SpeakerFence,
) -> Result<DismissOutcome> {
    set_dismissed(tx, index, meeting_id, speaker_id, fence, false)
}

fn set_dismissed(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    meeting_id: &MeetingId,
    speaker_id: SpeakerId,
    fence: &SpeakerFence,
    dismiss: bool,
) -> Result<DismissOutcome> {
    let mut meeting = writable_meeting(tx, meeting_id)?;
    check_speaker_fence(&meeting, fence)?;
    if meeting.speaker(speaker_id).is_none() {
        return Err(speaker_not_found(meeting_id, speaker_id));
    }
    let shown = effective_speaker(&meeting, speaker_id);
    let speakers = shown_group(&meeting, shown);
    let at = schema::now();
    let mut changed = false;
    for id in &speakers {
        let s = meeting
            .speaker_mut(*id)
            .ok_or_else(|| speaker_not_found(meeting_id, *id))?;
        if dismiss && s.review_dismissed_at.is_none() {
            s.review_dismissed_at = Some(at);
            changed = true;
        } else if !dismiss && s.review_dismissed_at.is_some() {
            s.review_dismissed_at = None;
            changed = true;
        }
    }
    if changed {
        save_and_index(tx, index, &mut meeting)?;
    }
    Ok(DismissOutcome {
        meeting_id: meeting_id.clone(),
        speaker_id: shown,
        redirected_from: (shown != speaker_id).then_some(speaker_id),
        speakers,
        dismissed: dismiss,
        changed,
    })
}

/// Clears the dismissal of every speaker of the group shown as `shown`:
/// naming or confirming it is the decision "Can't tell" postponed.
pub(super) fn clear_dismissal(meeting: &mut Meeting, shown: SpeakerId) {
    for id in shown_group(meeting, shown) {
        if let Some(s) = meeting.speaker_mut(id) {
            s.review_dismissed_at = None;
        }
    }
}

/// A shown speaker dismissed from review ([`dismissed_speakers`]).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DismissedRow {
    pub meeting_id: MeetingId,
    pub meeting_title: Option<String>,
    pub started_at: DateTime<FixedOffset>,
    pub speaker_id: SpeakerId,
    /// The group's total for a kept speaker.
    pub talk_s: f64,
    pub assignment: Assignment,
    /// The person it is named (`confirmed`/`auto`) or suggested as, if any.
    pub person_id: Option<PersonId>,
    /// The latest dismissal of the group's speakers.
    pub dismissed_at: DateTime<FixedOffset>,
    /// The speaker's first lines.
    pub sample_text: Vec<String>,
}

/// Every shown speaker dismissed from review, newest meeting first, read
/// from the meeting files (whatever happened to the speaker since, e.g. a
/// later auto-name). Unreadable meetings are skipped. Call on a background
/// executor: it reads every meeting.
pub fn dismissed_speakers(lib: &Library) -> Result<Vec<DismissedRow>> {
    let mut out = Vec::new();
    for dir in lib.list_meeting_dirs()? {
        let Ok(meeting) = lib.load_meeting_at(&dir) else {
            continue;
        };
        for s in effective_speakers(&meeting) {
            let Some(dismissed_at) = review_dismissed_at(&meeting, s.id) else {
                continue;
            };
            out.push(DismissedRow {
                meeting_id: meeting.id.clone(),
                meeting_title: meeting.title.clone(),
                started_at: meeting.started_at,
                speaker_id: s.id,
                talk_s: effective_talk_s(&meeting, s.id),
                assignment: s.assignment,
                person_id: s
                    .named_person()
                    .or(s.suggestion.as_ref().map(|g| &g.person_id))
                    .cloned(),
                dismissed_at,
                sample_text: meeting
                    .segments
                    .iter()
                    .filter(|g| g.speaker.map(|id| effective_speaker(&meeting, id)) == Some(s.id))
                    .take(REVIEW_SAMPLE_SEGMENTS)
                    .map(|g| g.text.clone())
                    .collect(),
            });
        }
    }
    out.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.meeting_id.cmp(&b.meeting_id))
            .then(a.speaker_id.cmp(&b.speaker_id))
    });
    Ok(out)
}

/// How many shown speakers are dismissed from review ([`dismissed_speakers`]).
pub fn review_dismissed_count(lib: &Library) -> Result<usize> {
    Ok(dismissed_speakers(lib)?.len())
}
