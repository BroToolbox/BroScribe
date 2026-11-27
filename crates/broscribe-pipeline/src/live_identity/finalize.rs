//! Live decisions into the published meeting: anchors mapped through the
//! journal timeline onto final segments, only lines that lie entirely
//! inside the user's reviewed ranges attributed, everything else kept as a
//! durable, reviewable record.
//!
//! Lines a decision created carry its number (`UtterancePerson::live_decision`):
//! when a later reconciliation (reprocessing, a clear, a forget) no longer
//! fully supports the decision, exactly those lines are retracted. Lines the
//! user set on the meeting (no live number) are never removed, and a line
//! the user edited takes the decision out of line creation for good
//! (`LiveAttribution::user_override`).

use std::collections::{BTreeMap, BTreeSet};

use broscribe_capture::journal::Track as CaptureTrack;
use broscribe_capture::timeline::Timeline;
use broscribe_core::live_identity::{
    JournalPoint, LiveAction, LiveAttribution, LiveOutcome, LiveRange, ROUNDING_S, read_sidecar,
    refresh_review_notes,
};
use broscribe_core::ops::{SegmentFingerprint, segment_fingerprint};
use broscribe_core::schema::{Meeting, UtterancePerson};
use broscribe_core::{Error, Library, PersonId, Result, WriteTx};

/// Recording seconds on `timeline` of journal position `point` of
/// `track`; `None` when that chunk is not on the timeline (cut off).
pub fn map_point(timeline: &Timeline, track: CaptureTrack, point: JournalPoint) -> Option<f64> {
    let rate = f64::from(timeline.sample_rate);
    for run in &timeline.track(track).runs {
        let mut before = 0u64;
        for chunk in &run.chunks {
            let frames = chunk.frames.unwrap_or(0);
            if chunk.seq == point.seq {
                let input = (before + point.frame.min(frames)).saturating_sub(run.skip_input);
                let output = run.start_frame as f64 + input as f64 * rate / run.effective_rate;
                return Some(output / rate);
            }
            before += frames;
        }
    }
    None
}

/// The final time range of a live range: through its journal anchors when
/// it has them, else its recording time. `None` when it does not map.
pub fn map_range(timeline: &Timeline, range: &LiveRange) -> Option<(f64, f64)> {
    let track = match range.track.as_str() {
        "mic" => CaptureTrack::Mic,
        "system" => CaptureTrack::System,
        _ => return None,
    };
    let (a, b) = match (range.from, range.to) {
        (Some(from), Some(to)) => (
            map_point(timeline, track, from)?,
            map_point(timeline, track, to)?,
        ),
        _ => (range.from_s(), range.to_s()),
    };
    let duration = timeline.duration_s();
    (b > a && a < duration).then_some((a, b.min(duration)))
}

fn overlap(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.1.min(b.1) - a.0.max(b.0)).max(0.0)
}

type Key = (String, u64, u64, String);

fn key_of(fp: &SegmentFingerprint) -> Key {
    (
        fp.track.clone(),
        fp.start.to_bits(),
        fp.end.to_bits(),
        fp.text_sha256.clone(),
    )
}

fn entry_key(u: &UtterancePerson) -> Key {
    (
        u.track.clone(),
        u.start.to_bits(),
        u.end.to_bits(),
        u.text_sha256.clone(),
    )
}

/// The names of the people `records` refer to that still exist (a person
/// missing here reads as forgotten). An unreadable person file is an
/// error, never taken for a forgotten person.
fn names_of(lib: &Library, records: &[LiveAttribution]) -> Result<BTreeMap<PersonId, String>> {
    let mut names = BTreeMap::new();
    for person in records.iter().filter_map(|r| r.person_id.as_ref()) {
        if names.contains_key(person) {
            continue;
        }
        match lib.load_person(person) {
            Ok(p) => {
                names.insert(person.clone(), p.name);
            }
            Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(names)
}

/// Recomputes every record's outcome against `meeting.segments`, adds the
/// lines of fully supported decisions, retracts the lines of decisions that
/// lost that support, and regenerates the LivePreview review notes from
/// the records ([`refresh_review_notes`], the one generator). Cleared
/// records stay cleared; records whose person is gone (not in `names`)
/// become forgotten.
fn reconcile(
    meeting: &mut Meeting,
    mut records: Vec<LiveAttribution>,
    names: &BTreeMap<PersonId, String>,
) {
    for r in &mut records {
        r.supported.clear();
        if r.action == LiveAction::Clear {
            r.outcome = LiveOutcome::Cleared;
            r.person_id = None;
        } else if r.outcome == LiveOutcome::Forgotten
            || r.person_id.as_ref().is_none_or(|p| !names.contains_key(p))
        {
            r.outcome = LiveOutcome::Forgotten;
            r.person_id = None;
        } else {
            r.outcome = LiveOutcome::Unmapped;
        }
    }
    let mapped = |r: &LiveAttribution| r.from_s.is_some() && r.to_s.is_some();
    let active = |r: &LiveAttribution| {
        !matches!(r.outcome, LiveOutcome::Cleared | LiveOutcome::Forgotten) && mapped(r)
    };
    // A forgotten decision still says the user named *someone* there: it
    // keeps blocking other names for those lines (forgetting a person must
    // never promote a decision it conflicted with). Only a Clear withdraws.
    let claims =
        |r: &LiveAttribution| (active(r) || r.outcome == LiveOutcome::Forgotten) && mapped(r);
    let span = |r: &LiveAttribution| (r.from_s.unwrap_or(0.0), r.to_s.unwrap_or(0.0));
    let keys: Vec<Key> = meeting
        .segments
        .iter()
        .map(|s| key_of(&segment_fingerprint(s)))
        .collect();
    // Final segments each claiming record overlaps by more than rounding.
    let overlapping: Vec<Vec<usize>> = records
        .iter()
        .map(|r| {
            if !claims(r) {
                return Vec::new();
            }
            meeting
                .segments
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.track == r.range.track && overlap((s.start, s.end), span(r)) > ROUNDING_S
                })
                .map(|(i, _)| i)
                .collect()
        })
        .collect();
    let mut outcomes = Vec::with_capacity(records.len());
    for (idx, r) in records.iter().enumerate() {
        if !active(r) {
            outcomes.push((r.outcome, Vec::new()));
            continue;
        }
        let segs = &overlapping[idx];
        if segs.is_empty() {
            outcomes.push((LiveOutcome::Unmapped, Vec::new()));
            continue;
        }
        let person = r.person_id.as_ref();
        let parts: Vec<[f64; 2]> = segs
            .iter()
            .map(|&i| {
                let s = &meeting.segments[i];
                [s.start.max(span(r).0), s.end.min(span(r).1)]
            })
            .collect();
        // Another decision, or a line the user set, names someone else here.
        let conflict = segs.iter().any(|&i| {
            records.iter().enumerate().any(|(j, other)| {
                j != idx
                    && claims(other)
                    && other.person_id.as_ref() != person
                    && overlapping[j].contains(&i)
            }) || meeting.utterance_people.iter().any(|u| {
                u.live_decision.is_none() && entry_key(u) == keys[i] && Some(&u.person_id) != person
            })
        });
        if conflict {
            outcomes.push((LiveOutcome::Conflicting, parts));
            continue;
        }
        // Covered only by this person's reviewed ranges on the track
        // (adjacent turns join only when they touch, up to rounding).
        let mut same: Vec<(f64, f64)> = records
            .iter()
            .filter(|o| {
                active(o) && o.person_id.as_ref() == person && o.range.track == r.range.track
            })
            .map(span)
            .collect();
        same.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (a, b) in same {
            match merged.last_mut() {
                Some(last) if a <= last.1 + ROUNDING_S => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        let full = segs.iter().all(|&i| {
            let s = &meeting.segments[i];
            merged
                .iter()
                .any(|&(a, b)| a <= s.start + ROUNDING_S && b >= s.end - ROUNDING_S)
        });
        if full {
            let whole = segs
                .iter()
                .map(|&i| [meeting.segments[i].start, meeting.segments[i].end])
                .collect();
            outcomes.push((LiveOutcome::Applied, whole));
        } else {
            outcomes.push((LiveOutcome::Partial, parts));
        }
    }

    // Lines each decision may keep: the segments it fully supports now.
    let mut entitled: BTreeSet<(u64, Key)> = BTreeSet::new();
    for (idx, (outcome, _)) in outcomes.iter().enumerate() {
        if *outcome == LiveOutcome::Applied {
            for &i in &overlapping[idx] {
                entitled.insert((records[idx].decision, keys[i].clone()));
            }
        }
    }
    // Retract every live-created line that lost its support (or whose
    // decision is gone); the user's own lines are never touched.
    meeting.utterance_people.retain(|u| match u.live_decision {
        Some(d) => entitled.contains(&(d, entry_key(u))),
        None => true,
    });

    for (idx, (outcome, supported)) in outcomes.into_iter().enumerate() {
        let r = &mut records[idx];
        r.outcome = outcome;
        r.supported = supported;
        if outcome == LiveOutcome::Applied
            && !r.user_override
            && let Some(person) = r.person_id.clone()
        {
            for &i in &overlapping[idx] {
                let fp = segment_fingerprint(&meeting.segments[i]);
                let present = meeting
                    .utterance_people
                    .iter()
                    .any(|u| entry_key(u) == keys[i] && u.person_id == person);
                if !present {
                    meeting.utterance_people.push(UtterancePerson {
                        track: fp.track,
                        start: fp.start,
                        end: fp.end,
                        text_sha256: fp.text_sha256,
                        person_id: person.clone(),
                        at: r.at,
                        live_decision: Some(r.decision),
                    });
                }
            }
        }
    }
    meeting.utterance_people.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then(a.track.cmp(&b.track))
            .then(a.end.total_cmp(&b.end))
    });
    meeting.live_attributions = records;
    refresh_review_notes(meeting, names);
}

/// Inside the publication's locked commit: carries the session's live
/// decisions (its sidecar) into `meeting`, mapped through `timeline` (the
/// 16 kHz analysis timeline the segments were made on). Lines entirely
/// inside the person's reviewed ranges become line attributions marked with
/// the decision; partial, conflicting and unmapped decisions stay as
/// records with a review note; cleared and forgotten ones are kept so
/// nothing brings them back. Idempotent; never touches speakers,
/// voiceprints or samples.
pub fn apply_session_decisions(
    tx: &mut WriteTx<'_>,
    meeting: &mut Meeting,
    session_id: &str,
    timeline: &Timeline,
) -> Result<()> {
    let Some(sidecar) = read_sidecar(tx.root(), session_id)? else {
        return Ok(());
    };
    let lib = tx.library().clone();
    let records: Vec<LiveAttribution> = sidecar
        .decisions
        .iter()
        .map(|d| {
            let mapped = map_range(timeline, &d.range);
            // A user edit on an earlier pass's line stays in force.
            let user_override = meeting
                .live_attributions
                .iter()
                .any(|a| a.decision == d.decision && a.user_override);
            LiveAttribution {
                turn_id: d.turn_id.clone(),
                decision: d.decision,
                action: d.action,
                person_id: d.person().cloned(),
                range: d.range.clone(),
                from_s: mapped.map(|m| m.0),
                to_s: mapped.map(|m| m.1),
                supported: Vec::new(),
                outcome: if d.forgotten {
                    LiveOutcome::Forgotten
                } else {
                    LiveOutcome::Unmapped
                },
                at: d.at,
                user_override,
                forget_fence: d.forget_fence.clone(),
            }
        })
        .collect();
    let names = names_of(&lib, &records)?;
    reconcile(meeting, records, &names);
    Ok(())
}

/// After reprocessing replaced the segments (line corrections already
/// moved): rechecks the meeting's live records against the new segments,
/// retracting the lines of any that lost full support.
pub fn recheck_meeting(lib: &Library, meeting: &mut Meeting) -> Result<()> {
    if meeting.live_attributions.is_empty()
        && !meeting
            .utterance_people
            .iter()
            .any(|u| u.live_decision.is_some())
    {
        return Ok(());
    }
    let records = std::mem::take(&mut meeting.live_attributions);
    let names = names_of(lib, &records)?;
    reconcile(meeting, records, &names);
    Ok(())
}
