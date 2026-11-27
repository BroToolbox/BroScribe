//! Utterance windows (plan §9.3): the unit of semantic search and of fusion.
//!
//! A window is a run of consecutive segments (transcript order) of one
//! speaker and one attribution spanning at most [`WINDOW_MAX_S`] seconds: a
//! line correction (`ops::effective_attribution`) to another person splits
//! the run. Its ID is stable while its first and last segment stay the same,
//! and its text hash keys the vector cache, so renaming a speaker or a
//! person never changes a window.

use sha2::{Digest, Sha256};

use crate::ops::{SegmentAttribution, effective_attribution, effective_speaker};
use crate::schema::{Meeting, Segment, hex, sha256_hex};
use crate::{MeetingId, PersonId, SpeakerId};

/// Longest span of one window, from its first segment's start to its last
/// segment's end (a single longer segment is a window on its own).
pub const WINDOW_MAX_S: f64 = 60.0;

/// Hex characters kept from the SHA-256 of a window ID.
const WINDOW_ID_HEX: usize = 32;

/// One window of a meeting.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// `hash(meeting ID, first segment ID, last segment ID)`, 32 hex chars.
    pub id: String,
    pub speaker: Option<SpeakerId>,
    /// The person the window is attributed to: its line correction's, else
    /// the person its speaker is named as.
    pub person_id: Option<PersonId>,
    /// Segment IDs in transcript order.
    pub segment_ids: Vec<u32>,
    pub start: f64,
    pub end: f64,
    /// [`window_text`] of the segments.
    pub text: String,
    /// Lowercase hex SHA-256 of `text`.
    pub text_sha256: String,
}

impl Window {
    /// Whether the window holds no text to embed.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// The window ID of segments `first..=last` of meeting `meeting`.
pub fn window_id(meeting: &MeetingId, first: u32, last: u32) -> String {
    let mut h = Sha256::new();
    h.update(meeting.as_str().as_bytes());
    h.update([0]);
    h.update(first.to_le_bytes());
    h.update(last.to_le_bytes());
    let mut id = hex(&h.finalize());
    id.truncate(WINDOW_ID_HEX);
    id
}

/// The text of a window: its segments' trimmed, non-empty texts joined by
/// one space. The index rebuilds window texts from its utterances with this
/// same function, so the hashes always agree.
pub fn window_text<'a>(texts: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for t in texts.into_iter().map(str::trim).filter(|t| !t.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t);
    }
    out
}

/// Segments in transcript order: by start time, then ID (as the index
/// orders utterances), each with its index in `segments`.
fn ordered(segments: &[Segment]) -> Vec<(usize, &Segment)> {
    let mut segs: Vec<(usize, &Segment)> = segments.iter().enumerate().collect();
    segs.sort_by(|(_, a), (_, b)| a.start.total_cmp(&b.start).then(a.id.cmp(&b.id)));
    segs
}

/// The meeting's windows in transcript order.
pub fn meeting_windows(meeting: &Meeting) -> Vec<Window> {
    attributed_windows(meeting, &effective_attribution(meeting))
}

/// [`meeting_windows`] with the meeting's [`effective_attribution`] given.
pub(crate) fn attributed_windows(
    meeting: &Meeting,
    attribution: &[SegmentAttribution],
) -> Vec<Window> {
    // Segments of one shown speaker (a group counts once) group while their
    // line corrections agree.
    let shown = |s: &Segment| s.speaker.map(|id| effective_speaker(meeting, id));
    let mut groups: Vec<Vec<(usize, &Segment)>> = Vec::new();
    for (i, seg) in ordered(&meeting.segments) {
        match groups.last_mut() {
            Some(g)
                if shown(g[0].1) == shown(seg)
                    && attribution[g[0].0].line_person() == attribution[i].line_person()
                    && seg.end - g[0].1.start <= WINDOW_MAX_S =>
            {
                g.push((i, seg));
            }
            _ => groups.push(vec![(i, seg)]),
        }
    }
    groups
        .into_iter()
        .map(|g| {
            let (first_i, first) = g[0];
            let last = g[g.len() - 1].1;
            let text = window_text(g.iter().map(|(_, s)| s.text.as_str()));
            Window {
                id: window_id(&meeting.id, first.id, last.id),
                speaker: shown(first),
                person_id: attribution[first_i].person_id.clone(),
                segment_ids: g.iter().map(|(_, s)| s.id).collect(),
                start: first.start,
                end: g.iter().map(|(_, s)| s.end).fold(first.end, f64::max),
                text_sha256: sha256_hex(text.as_bytes()),
                text,
            }
        })
        .collect()
}

/// The meeting's window-hash revision: the SHA-256 of every window's ID and
/// text hash in order. It changes exactly when a window's boundaries or text
/// change (never on naming), so writers of derived data re-check it.
pub fn window_revision(windows: &[Window]) -> String {
    let mut h = Sha256::new();
    for w in windows {
        h.update(w.id.as_bytes());
        h.update(b"\t");
        h.update(w.text_sha256.as_bytes());
        h.update(b"\n");
    }
    hex(&h.finalize())
}

/// [`window_revision`] of [`meeting_windows`].
pub fn meeting_window_revision(meeting: &Meeting) -> String {
    window_revision(&meeting_windows(meeting))
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;
    use crate::schema::{Meeting, Source};

    fn seg(id: u32, speaker: Option<u32>, start: f64, end: f64, text: &str) -> Segment {
        Segment {
            speech_overlap: Vec::new(),
            id,
            track: "t0".into(),
            speaker: speaker.map(|n| SpeakerId::new(n).unwrap()),
            start,
            end,
            text: text.into(),
            words: Vec::new(),
        }
    }

    fn meeting(segments: Vec<Segment>) -> Meeting {
        let mut m = Meeting::new(
            MeetingId::parse("m-00000a").unwrap(),
            Source::Import,
            DateTime::parse_from_rfc3339("2026-11-10T10:00:00Z").unwrap(),
        );
        m.segments = segments;
        m
    }

    #[test]
    fn consecutive_segments_of_one_speaker_join_up_to_sixty_seconds() {
        let m = meeting(vec![
            seg(3, Some(2), 10.0, 12.0, "  Second speaker. "),
            seg(0, Some(1), 0.0, 4.0, "Hello"),
            seg(1, Some(1), 5.0, 9.0, "there."),
            seg(4, Some(2), 13.0, 71.0, "Long turn."),
            seg(5, Some(2), 71.0, 73.0, ""),
            seg(6, None, 74.0, 75.0, "Unknown."),
            seg(7, None, 76.0, 77.0, "Still unknown."),
        ]);
        let w = meeting_windows(&m);
        let segs: Vec<Vec<u32>> = w.iter().map(|w| w.segment_ids.clone()).collect();
        assert_eq!(segs, [vec![0, 1], vec![3], vec![4, 5], vec![6, 7]]);
        assert_eq!(w[0].text, "Hello there.");
        assert_eq!(w[1].text, "Second speaker.");
        assert_eq!(w[2].text, "Long turn.");
        assert_eq!((w[2].start, w[2].end), (13.0, 73.0));
        assert_eq!(w[0].id, window_id(&m.id, 0, 1));
        assert_eq!(w[0].id.len(), 32);
        assert_eq!(w[0].text_sha256, sha256_hex(b"Hello there."));
        assert!(w.iter().all(|w| !w.is_empty()));
    }

    #[test]
    fn revision_ignores_naming_but_follows_text() {
        let mut m = meeting(vec![
            seg(0, Some(1), 0.0, 2.0, "One."),
            seg(1, Some(2), 2.0, 4.0, "Two."),
        ]);
        let before = meeting_window_revision(&m);
        m.title = Some("Renamed".into());
        assert_eq!(meeting_window_revision(&m), before);
        m.segments[1].text = "Three.".into();
        assert_ne!(meeting_window_revision(&m), before);
    }

    #[test]
    fn a_line_correction_splits_the_window_and_carries_its_person() {
        let mut m = meeting(vec![
            seg(0, Some(1), 0.0, 2.0, "One."),
            seg(1, Some(1), 2.0, 4.0, "Two."),
            seg(2, Some(1), 4.0, 6.0, "Three."),
        ]);
        let before = meeting_window_revision(&m);
        let person = PersonId::parse("p-c3c3c3").unwrap();
        let f = crate::ops::segment_fingerprint(&m.segments[1]);
        m.utterance_people.push(crate::schema::UtterancePerson {
            track: f.track,
            start: f.start,
            end: f.end,
            text_sha256: f.text_sha256,
            person_id: person.clone(),
            at: crate::schema::now(),
            live_decision: None,
        });
        let w = meeting_windows(&m);
        let segs: Vec<Vec<u32>> = w.iter().map(|w| w.segment_ids.clone()).collect();
        assert_eq!(segs, [vec![0], vec![1], vec![2]]);
        assert_eq!(w[1].person_id, Some(person));
        assert_eq!(
            (w[0].person_id.clone(), w[2].person_id.clone()),
            (None, None)
        );
        assert_ne!(meeting_window_revision(&m), before);
    }
}
