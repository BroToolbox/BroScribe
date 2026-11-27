//! Shared commit-side steps: finding meetings by content, grouping
//! over-split clusters (once, on `analyzed → done`) and resolving identities.
//! Everything here runs under the library lock and does no inference.

use std::path::Path;

use broscribe_core::config::Thresholds;
use broscribe_core::identity::{self, MergeInput};
use broscribe_core::index::Index;
use broscribe_core::ops;
use broscribe_core::schema::{Assignment, Centroid, Meeting, ProcessingStatus, SpeakerGroup};
use broscribe_core::{Library, MeetingId, PersonId, Result, SpeakerId, WriteTx};
use chrono::{DateTime, FixedOffset};

use crate::manifest::fold;

/// The meeting whose recording has this content hash. An unreadable
/// `meeting.json` of another meeting never blocks the lookup once its files
/// prove that it is another meeting ([`suspicion`]); one that may be this
/// file's own meeting — its folder holds audio of exactly this content
/// (`bytes` long, hashing to `sha256`), its files still name this hash, or
/// nothing it keeps rules it out — is reported instead of letting the
/// caller import the file a second time.
pub(crate) fn find_meeting_by_sha(
    lib: &Library,
    sha256: &str,
    bytes: u64,
) -> Result<Option<Meeting>> {
    find_meeting_judged(
        lib,
        |m| m.recording.as_ref().is_some_and(|r| r.sha256 == sha256),
        |dir| {
            if holds_audio(dir, sha256, bytes)? {
                return Ok(Some(Suspicion::Named));
            }
            Ok(suspicion(dir, Sought::Recording(sha256)))
        },
    )
}

/// The first readable meeting `matches` accepts. Unreadable meeting files are
/// isolated: skipped (logged without their path) unless `could_be(dir)` says
/// that folder may be the very meeting sought; then, when no readable meeting
/// matches, the lookup fails (`invalid_input` naming the folder) so the
/// caller never creates a duplicate of a meeting it cannot read.
pub(crate) fn find_meeting(
    lib: &Library,
    matches: impl Fn(&Meeting) -> bool,
    could_be: impl Fn(&Path) -> Result<bool>,
) -> Result<Option<Meeting>> {
    find_meeting_judged(lib, matches, |dir| {
        Ok(could_be(dir)?.then_some(Suspicion::Named))
    })
}

/// [`find_meeting`], with `judge` telling why an unreadable folder may be
/// the meeting sought (`None`: it is proven to be another one).
fn find_meeting_judged(
    lib: &Library,
    matches: impl Fn(&Meeting) -> bool,
    judge: impl Fn(&Path) -> Result<Option<Suspicion>>,
) -> Result<Option<Meeting>> {
    let mut suspect = None;
    for dir in lib.list_meeting_dirs()? {
        let error = match lib.load_meeting_at(&dir) {
            Ok(m) if matches(&m) => return Ok(Some(m)),
            Ok(_) => continue,
            Err(error) => error,
        };
        if suspect.is_none()
            && let Some(why) = judge(&dir)?
        {
            suspect = Some((dir, error, why));
        } else {
            tracing::warn!(code = error.code(), "skipping an unreadable meeting file");
        }
    }
    match suspect {
        Some((dir, error, why)) => Err(why.refusal(&dir, &error, "this meeting")),
        None => Ok(None),
    }
}

/// Whether meeting folder `dir` holds an `audio.*` file with this content.
pub(crate) fn holds_audio(dir: &Path, sha256: &str, bytes: u64) -> Result<bool> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(broscribe_core::Error::io_at(dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| broscribe_core::Error::io_at(dir, e))?;
        let path = entry.path();
        let named = entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with("audio."));
        if named
            && std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == bytes)
            && crate::audio::file_sha256(&path)?.0 == sha256
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What a lookup seeks, to judge a meeting folder whose `meeting.json`
/// cannot be read ([`suspicion`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Sought<'a> {
    /// A meeting whose `recording.sha256` is this ([`find_meeting_by_sha`]).
    Recording(&'a str),
    /// A manifest entry's meeting: its `recording.sha256` or original
    /// multi-source fingerprint (`recording.source_sha256`) is one of
    /// `hashes`, or it was imported with `source_id`.
    Entry {
        hashes: &'a [&'a str],
        source_id: Option<&'a str>,
    },
}

/// Why a meeting folder whose `meeting.json` cannot be read may be the
/// meeting a lookup seeks, so the lookup refuses rather than creating a
/// second one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Suspicion {
    /// Its files still name what the lookup seeks.
    Named,
    /// Nothing it keeps can rule it out: its `meeting.md` is missing, cut
    /// short or unreadable, or an earlier version wrote it without the
    /// identity this lookup compares (`provenance`, see
    /// [`broscribe_core::render::MeetingMdIdentity`]).
    Unprovable,
}

impl Suspicion {
    /// The actionable `invalid_input` refusal for folder `dir`, unreadable
    /// with `error`, that may be `what` (e.g. "this meeting").
    pub(crate) fn refusal(
        self,
        dir: &Path,
        error: &broscribe_core::Error,
        what: &str,
    ) -> broscribe_core::Error {
        let why = match self {
            Self::Named => format!("may be {what}"),
            Self::Unprovable => format!(
                "its meeting.md does not record enough to rule out that it is {what} \
                 (missing or damaged, or written by an earlier version)"
            ),
        };
        broscribe_core::Error::InvalidInput(format!(
            "{} cannot be read ({error}) and {why}; repair or remove its meeting.json before \
             trying again",
            dir.display()
        ))
    }
}

/// Refusal-only judgment of meeting folder `dir`, whose `meeting.json`
/// cannot be read: `None` only when its files prove that it is not the
/// meeting `sought` (see [`Suspicion`]). Nothing here reads a meeting or
/// trusts the Markdown as one.
///
/// - The damaged `meeting.json` still holding one of the hashes, or the
///   `source_id` as a key's value, is evidence that it may be.
/// - The derived `meeting.md` frontmatter decides the rest exactly as the
///   readable lookup would: `recording.sha256` is in every version of it,
///   which is all a [`Sought::Recording`] compares; a [`Sought::Entry`] also
///   compares the source fingerprint and `source_id`, so it needs the
///   complete `provenance` block. A `meeting.md` marked `pending` (an
///   identity change interrupted before it was rewritten, see
///   [`broscribe_core::WriteTx::save_meeting`]) vouches for nothing.
///   Without what the lookup compares, the folder cannot be ruled out
///   ([`Suspicion::Unprovable`]): libraries whose unreadable meetings
///   predate the block refuse manifest entries until those meetings are
///   repaired, rather than risk a duplicate.
pub(crate) fn suspicion(dir: &Path, sought: Sought<'_>) -> Option<Suspicion> {
    let single;
    let (hashes, source_id) = match sought {
        Sought::Recording(sha256) => {
            single = [sha256];
            (&single[..], None)
        }
        Sought::Entry { hashes, source_id } => (hashes, source_id),
    };
    if json_names(dir, hashes, source_id) {
        return Some(Suspicion::Named);
    }
    let Some(identity) = md_identity(dir) else {
        return Some(Suspicion::Unprovable);
    };
    let named = |h: &Option<String>| h.as_deref().is_some_and(|h| hashes.contains(&h));
    if named(&identity.recording_sha256) {
        return Some(Suspicion::Named);
    }
    if let Sought::Recording(_) = sought {
        return None;
    }
    let Some(provenance) = identity.provenance else {
        return Some(Suspicion::Unprovable);
    };
    let source_id = source_id.map(broscribe_core::render::source_id_sha256);
    let names_source_id = source_id.is_some() && provenance.source_id_sha256 == source_id;
    (named(&provenance.source_sha256) || names_source_id).then_some(Suspicion::Named)
}

/// Whether the (damaged) `meeting.json` in `dir` still holds one of
/// `hashes`, or a `"source_id"` key with the value `source_id`. A file that
/// cannot be read gives no evidence.
fn json_names(dir: &Path, hashes: &[&str], source_id: Option<&str>) -> bool {
    let Some(json) = read_evidence(&dir.join(broscribe_core::paths::MEETING_JSON)) else {
        return false;
    };
    let quoted = source_id.map(|s| serde_json::Value::from(s).to_string());
    hashes
        .iter()
        .any(|h| find_bytes(&json, h.as_bytes()).is_some())
        || quoted.is_some_and(|q| names_source(&json, q.as_bytes()))
}

fn read_evidence(path: &Path) -> Option<Vec<u8>> {
    match std::fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(kind = ?e.kind(), "skipping unreadable meeting evidence");
            }
            None
        }
    }
}

/// The identity the `meeting.md` frontmatter in `dir` records
/// ([`broscribe_core::render::read_meeting_md_identity`]); `None` when there
/// is none to read.
fn md_identity(dir: &Path) -> Option<broscribe_core::render::MeetingMdIdentity> {
    let path = dir.join(broscribe_core::paths::MEETING_MD);
    match broscribe_core::render::read_meeting_md_identity(&path) {
        Ok(identity) => identity,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(kind = ?e.kind(), "skipping unreadable meeting evidence");
            }
            None
        }
    }
}

/// Whether `json` has a `"source_id"` key whose value is `quoted` (a JSON
/// string literal), however the damaged file is spaced.
fn names_source(json: &[u8], quoted: &[u8]) -> bool {
    const KEY: &[u8] = b"\"source_id\"";
    let mut rest = json;
    while let Some(at) = find_bytes(rest, KEY) {
        rest = &rest[at + KEY.len()..];
        if let Some(value) = rest.trim_ascii_start().strip_prefix(b":")
            && value.trim_ascii_start().starts_with(quoted)
        {
            return true;
        }
    }
    false
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Folds over-split clusters of `meeting` with [`identity::merge_clusters`]
/// (confirmation-aware), destructively: only for the benchmark and
/// calibration, which fit and measure `merge` on meetings they never
/// publish. Published meetings group speakers reversibly instead
/// ([`ops::apply_auto_groups`]). Returns the `(merged, kept)` speaker pairs
/// it folded.
pub(crate) fn apply_merge(
    meeting: &mut Meeting,
    model: &str,
    merge: Option<f32>,
    roster_floor: Option<usize>,
) -> Result<Vec<(SpeakerId, SpeakerId)>> {
    if merge.is_none() || meeting.speakers.len() < 2 {
        return Ok(Vec::new());
    }
    let mut inputs: std::collections::BTreeMap<&str, Vec<MergeInput>> =
        std::collections::BTreeMap::new();
    let paired_mic = meeting.paired_mic_track();
    for s in &meeting.speakers {
        if paired_mic == Some(s.track.as_str()) {
            continue;
        }
        let centroid = match s.centroid.as_ref().filter(|c| c.model == model) {
            Some(c) => c.vector()?,
            None => Vec::new(),
        };
        inputs.entry(&s.track).or_default().push(MergeInput {
            id: s.id,
            centroid,
            talk_s: s.talk_s,
            confirmed_person: (s.assignment == Assignment::Confirmed)
                .then(|| s.person_id.clone())
                .flatten(),
            rejected: s.rejected_person_ids.clone(),
        });
    }
    let groups: Vec<_> = inputs
        .into_values()
        .flat_map(|track| {
            let other = meeting.speakers.len() - track.len();
            let floor = roster_floor.map(|n| n.saturating_sub(other).max(1));
            identity::merge_clusters(&track, merge, floor)
        })
        .filter(|g| !g.merged.is_empty())
        .map(|g| (g.kept, g.merged, Some(g.rejected_union)))
        .collect();
    fold_groups(meeting, model, &groups)
}

/// `meeting` as if `groups` were folded: each group's speakers become its
/// kept speaker (segments, talk time, talk-weighted centroid of `model`
/// with the union of contributing ranges). A throwaway view for steps that
/// need one speaker per voice (caption alignment and its embeddings);
/// never published.
pub(crate) fn fold_view(
    meeting: &Meeting,
    model: &str,
    groups: &[SpeakerGroup],
) -> Result<Meeting> {
    let mut view = meeting.clone();
    view.speaker_groups.clear();
    let groups: Vec<_> = groups
        .iter()
        .map(|g| (g.kept, g.members.clone(), None))
        .collect();
    fold_groups(&mut view, model, &groups)?;
    Ok(view)
}

/// A kept speaker, the speakers folded into it and, when given, the
/// rejections it takes.
type Fold = (SpeakerId, Vec<SpeakerId>, Option<Vec<PersonId>>);

/// Folds each `(kept, merged, rejected)` into its kept speaker: it takes the
/// merged speakers' segments and talk time (and `rejected`, when given);
/// its centroid becomes the talk-weighted mean. Returns the folded pairs.
fn fold_groups(
    meeting: &mut Meeting,
    model: &str,
    groups: &[Fold],
) -> Result<Vec<(SpeakerId, SpeakerId)>> {
    let mut folds = Vec::new();
    for (kept_id, merged, rejected) in groups {
        let members: Vec<usize> = std::iter::once(*kept_id)
            .chain(merged.iter().copied())
            .filter_map(|id| meeting.speakers.iter().position(|s| s.id == id))
            .collect();
        let mut sum: Vec<f64> = Vec::new();
        let (mut talk, mut n_turns) = (0.0, 0u32);
        let mut contributing_ranges = Vec::new();
        let mut complete_ranges = true;
        for &i in &members {
            let s = &meeting.speakers[i];
            talk += s.talk_s;
            if let Some(c) = s.centroid.as_ref().filter(|c| c.model == model) {
                let v = c.vector()?;
                if sum.is_empty() {
                    sum = vec![0.0; v.len()];
                }
                if sum.len() == v.len() {
                    let w = s.talk_s.max(0.0) + 1e-6;
                    for (a, x) in sum.iter_mut().zip(&v) {
                        *a += w * f64::from(*x);
                    }
                    n_turns = n_turns.saturating_add(c.n_turns);
                    complete_ranges &= !c.ranges.is_empty();
                    contributing_ranges.extend_from_slice(&c.ranges);
                }
            }
        }
        let Some(kept) = meeting.speaker_mut(*kept_id) else {
            continue;
        };
        kept.talk_s = talk;
        if let Some(rejected) = rejected {
            kept.rejected_person_ids = rejected.clone();
        }
        if !sum.is_empty() {
            let mut v: Vec<f32> = sum.into_iter().map(|x| x as f32).collect();
            identity::l2_normalize(&mut v);
            let mut centroid = Centroid::new(model, &v, n_turns);
            if complete_ranges {
                contributing_ranges
                    .sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
                contributing_ranges.dedup();
                centroid.ranges = contributing_ranges;
            }
            kept.centroid = Some(centroid);
        }
        for seg in &mut meeting.segments {
            if seg.speaker.is_some_and(|s| merged.contains(&s)) {
                seg.speaker = Some(*kept_id);
            }
        }
        meeting.speakers.retain(|s| !merged.contains(&s.id));
        folds.extend(merged.iter().map(|&m| (m, *kept_id)));
    }
    Ok(folds)
}

/// What [`finish_meeting`] did to one meeting.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Finished {
    /// Speakers grouped under another (hidden as their own speaker).
    pub merged: usize,
    pub changes: Vec<ops::SpeakerChange>,
    pub auto: usize,
    pub suggested: usize,
}

/// Re-reads the meeting under the lock, resolves every unconfirmed speaker,
/// syncs weak samples and indexes the candidate. When it is `analyzed`
/// (an import's first finish, never for `done` meetings, so a re-finish
/// adds no groups) it also groups over-split clusters after resolution
/// ([`ops::apply_auto_groups`], before the weak samples, so grouped members
/// teach no weak sample). Publishes `done` only after those idempotent side
/// effects succeed.
pub(crate) fn finish_meeting(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    id: &MeetingId,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<Finished> {
    let lib = tx.library().clone();
    let mut meeting = lib.load_meeting(id)?;
    if meeting.processing.status == ProcessingStatus::Provisional {
        return Err(broscribe_core::Error::InvalidInput(
            "provisional recording must complete final analysis before import finalization".into(),
        ));
    }
    let mut out = Finished::default();
    let people = ops::load_person_samples(&lib, model)?;
    out.changes = resolve_identities(&mut meeting, model, &people, thresholds)?;
    if meeting.processing.status == ProcessingStatus::Analyzed {
        let shown = ops::effective_speakers(&meeting).count();
        let proven = ops::caption_proven(&lib, &meeting)?;
        ops::apply_auto_groups(&mut meeting, model, thresholds, &proven)?;
        out.merged = shown - ops::effective_speakers(&meeting).count();
        // A kept speaker never stays named as someone a member rejected.
        let roster = meeting.roster_person_ids();
        let again =
            ops::resolve_rejected_kept(&mut meeting, model, &people, thresholds, Some(&roster))?;
        ops::merge_changes(&mut out.changes, again);
    }
    let conflicted = ops::guard_automatic_names(&mut meeting);
    ops::merge_changes(&mut out.changes, conflicted);
    ops::refresh_identity_conflict_notes(&lib, &mut meeting)?;
    meeting.processing.status = ProcessingStatus::Done;
    meeting.processing.error = None;
    meeting.updated_at = broscribe_core::schema::now();
    crate::batch::failpoint(crate::batch::FailPoint::FinishBeforeIndex)?;
    ops::sync_weak_samples(tx, &meeting, &out.changes)?;
    tx.save_meeting(&meeting)?;
    index.upsert_meeting(tx, &meeting)?;
    for s in &meeting.speakers {
        match s.assignment {
            Assignment::Auto => out.auto += 1,
            Assignment::Suggested => out.suggested += 1,
            _ => {}
        }
    }
    Ok(out)
}

/// Resolves `meeting`'s unconfirmed speakers against its roster, leaving the
/// paired microphone's speakers ([`Meeting::paired_mic_track`]) as the
/// two-track pass decided: track-wide centroids must not undo its
/// segment-level Me verification or relabel Unknown mic speech, and grouped
/// members frozen. Shared by finalization and the benchmark so both apply
/// the same policy. The resolver's confirmed-speaker exclusion is borrowed
/// for that only within this call.
pub(crate) fn resolve_identities(
    meeting: &mut Meeting,
    model: &str,
    people: &ops::PeopleSamples,
    thresholds: Option<&Thresholds>,
) -> Result<Vec<ops::SpeakerChange>> {
    let roster = meeting.roster_person_ids();
    let paired_mic = meeting.paired_mic_track().map(str::to_owned);
    let protected: Vec<_> = meeting
        .speakers
        .iter_mut()
        .filter(|s| paired_mic.as_deref() == Some(s.track.as_str()))
        .map(|s| {
            let original = s.assignment;
            s.assignment = Assignment::Confirmed;
            (s.id, original)
        })
        .collect();
    let changes = ops::resolve_meeting(meeting, model, people, thresholds, Some(&roster));
    for (id, assignment) in protected {
        meeting
            .speaker_mut(id)
            .expect("protected speaker remains present")
            .assignment = assignment;
    }
    changes
}

/// Title derived from a file name: its stem.
pub(crate) fn title_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// The file's modification time in the local offset.
pub(crate) fn file_mtime(path: &Path) -> Option<DateTime<FixedOffset>> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<chrono::Local>::from(modified).fixed_offset())
}

/// Lowercase extension for `audio.<ext>`; `bin` when the file has none.
pub(crate) fn audio_ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(fold)
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "bin".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::schema::{Participant, Segment, Source, Speaker};
    use broscribe_core::{PersonId, SpeakerId};

    fn sid(n: u32) -> SpeakerId {
        SpeakerId::new(n).unwrap()
    }

    fn meeting_with(speakers: Vec<Speaker>) -> Meeting {
        let mut m = Meeting::new(
            MeetingId::parse("m-000001").unwrap(),
            Source::Import,
            broscribe_core::schema::now(),
        );
        for s in &speakers {
            m.segments.push(Segment {
                speech_overlap: Vec::new(),
                id: s.id.number(),
                track: "t0".into(),
                speaker: Some(s.id),
                start: f64::from(s.id.number()),
                end: f64::from(s.id.number()) + 0.5,
                text: "x".into(),
                words: vec![],
            });
        }
        m.speakers = speakers;
        m
    }

    fn speaker(n: u32, v: &[f32], talk: f64) -> Speaker {
        let mut s = Speaker::new(sid(n), "t0");
        s.talk_s = talk;
        s.centroid = Some(Centroid::new("fake", v, 2));
        s
    }

    #[test]
    fn merging_centroids_preserves_exact_contributing_ranges_without_filling_gaps() {
        for legacy in [false, true] {
            let mut first = speaker(1, &[1.0, 0.0], 4.0);
            first.centroid.as_mut().unwrap().ranges = vec![[0.0, 2.0], [8.0, 10.0]];
            let mut second = speaker(2, &[1.0, 0.0], 2.0);
            if !legacy {
                second.centroid.as_mut().unwrap().ranges = vec![[4.0, 6.0]];
            }
            let mut meeting = meeting_with(vec![first, second]);
            assert_eq!(
                apply_merge(&mut meeting, "fake", Some(0.9), None)
                    .unwrap()
                    .len(),
                1
            );
            let ranges = &meeting.speakers[0].centroid.as_ref().unwrap().ranges;
            if legacy {
                assert!(ranges.is_empty());
            } else {
                assert_eq!(ranges, &[[0.0, 2.0], [4.0, 6.0], [8.0, 10.0]]);
            }
        }
    }

    #[test]
    fn same_voice_on_different_tracks_never_merges_speaker_ids() {
        let first = speaker(1, &[1.0, 0.0], 5.0);
        let mut second = speaker(2, &[1.0, 0.0], 5.0);
        second.track = "t1".into();
        let mut meeting = meeting_with(vec![first, second]);
        meeting.segments[1].track = "t1".into();
        assert!(
            apply_merge(&mut meeting, "fake", Some(0.9), None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(meeting.speakers.len(), 2);
        assert_eq!(meeting.segments[1].speaker, Some(sid(2)));
    }

    #[test]
    fn confirmed_and_unconfirmed_merge_keeps_confirmed_id_and_union_of_rejections() {
        let ana = PersonId::parse("p-00000a").unwrap();
        let (r1, r2) = (
            PersonId::parse("p-0000b1").unwrap(),
            PersonId::parse("p-0000b2").unwrap(),
        );
        let mut s1 = speaker(1, &[1.0, 0.0], 50.0);
        s1.rejected_person_ids = vec![r1.clone()];
        let mut s2 = speaker(2, &[0.99, 0.05], 10.0);
        s2.assignment = Assignment::Confirmed;
        s2.person_id = Some(ana.clone());
        s2.rejected_person_ids = vec![r2.clone()];
        let s3 = speaker(3, &[0.0, 1.0], 30.0);
        let mut m = meeting_with(vec![s1, s2, s3]);
        let folds = apply_merge(&mut m, "fake", Some(0.9), None).unwrap();
        assert_eq!(folds, vec![(sid(1), sid(2))]);
        let ids: Vec<SpeakerId> = m.speakers.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![sid(2), sid(3)]);
        let kept = m.speaker(sid(2)).unwrap();
        assert_eq!(kept.assignment, Assignment::Confirmed);
        assert_eq!(kept.person_id, Some(ana));
        assert_eq!(kept.rejected_person_ids, vec![r1, r2]);
        assert!((kept.talk_s - 60.0).abs() < 1e-9);
        assert!(m.segments.iter().all(|s| s.speaker != Some(sid(1))));
        // No threshold: nothing merges.
        let mut m2 = meeting_with(vec![
            speaker(1, &[1.0, 0.0], 1.0),
            speaker(2, &[1.0, 0.0], 1.0),
        ]);
        assert!(apply_merge(&mut m2, "fake", None, None).unwrap().is_empty());
    }

    #[test]
    fn roster_counts_me_once() {
        let mut m = meeting_with(vec![]);
        assert_eq!(m.roster_floor(), None);
        m.participants = vec![
            Participant {
                name: Some("Ana".into()),
                ..Participant::default()
            },
            Participant {
                name: Some("ana".into()),
                ..Participant::default()
            },
        ];
        assert_eq!(m.roster_floor(), Some(2));
        m.participants.push(Participant {
            is_self: true,
            ..Participant::default()
        });
        assert_eq!(m.roster_floor(), Some(2));
    }

    /// The core's paired-microphone rule (it cannot use the DSP crate)
    /// classifies every setup as the DSP's `is_room` does.
    #[test]
    fn paired_mic_rule_matches_the_dsp_room_setups() {
        use broscribe_core::schema::{Track, TrackRole};
        let mut m = meeting_with(vec![]);
        m.tracks = vec![
            Track {
                id: "mic".into(),
                role: TrackRole::Mic,
            },
            Track {
                id: "system".into(),
                role: TrackRole::System,
            },
        ];
        for setup in broscribe_dsp::Setup::ALL {
            m.setup = Some(setup.as_str().into());
            assert_eq!(m.paired_mic_track().is_none(), setup.is_room(), "{setup}");
        }
        m.setup = None;
        let fallback = broscribe_dsp::SetupRules::builtin().fallback;
        assert_eq!(m.paired_mic_track().is_none(), fallback.is_room());
    }
}

#[cfg(test)]
mod paired_finish_tests {
    use super::*;
    use crate::testutil::Fixture;
    use broscribe_core::SpeakerId;
    use broscribe_core::schema::{Person, Speaker, Strength, Track, TrackRole};

    #[test]
    fn import_finalization_preserves_segment_level_mic_decisions() {
        let fixture = Fixture::new();
        let mut me = Person::new(fixture.lib.new_person_id(), "Synthetic Me");
        me.is_self = true;
        let mut meeting = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            broscribe_core::schema::Source::Import,
            broscribe_core::schema::now(),
        );
        meeting.setup = Some("headphones".into());
        meeting.processing.status = ProcessingStatus::Analyzed;
        meeting.tracks = vec![
            Track {
                id: "t0".into(),
                role: TrackRole::Mic,
            },
            Track {
                id: "t1".into(),
                role: TrackRole::System,
            },
        ];
        for (number, track, vector) in [
            (1, "t0", [1.0, 0.0]),
            (2, "t0", [1.0, 0.0]),
            (3, "t1", [0.0, 1.0]),
        ] {
            let mut speaker = Speaker::new(SpeakerId::new(number).unwrap(), track);
            speaker.centroid = Some(Centroid::new("fake", &vector, 1));
            speaker.talk_s = 3.0;
            if number == 2 {
                speaker.assignment = Assignment::Auto;
                speaker.person_id = Some(me.id.clone());
            }
            meeting.speakers.push(speaker);
        }
        let mut voiceprint = ops::weak_sample(&meeting, SpeakerId::new(1).unwrap(), &me.id)
            .unwrap()
            .unwrap();
        voiceprint.source.meeting_id = None;
        voiceprint.strength = Strength::Confirmed;
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&me)?;
                tx.upsert_voiceprints(&me.id, vec![voiceprint])?;
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                let mut index = Index::open(tx.root())?;
                finish_meeting(
                    tx,
                    &mut index,
                    &meeting.id,
                    "fake",
                    Some(&Thresholds {
                        auto_name: Some(0.8),
                        suggest: Some(0.6),
                        margin: Some(0.1),
                        merge: Some(0.9),
                        auto_merge: Some(0.9),
                    }),
                )?;
                Ok(())
            })
            .unwrap();
        let saved = fixture.lib.load_meeting(&meeting.id).unwrap();
        assert_eq!(
            saved.speakers.len(),
            3,
            "Me and Unknown mic turns must not merge"
        );
        assert!(
            saved.speaker_groups.is_empty(),
            "nor group: the paired microphone is never grouped"
        );
        assert_eq!(saved.speakers[0].assignment, Assignment::None);
        assert!(saved.speakers[0].person_id.is_none());
        assert_eq!(saved.speakers[1].assignment, Assignment::Auto);
        assert_eq!(saved.speakers[1].person_id.as_ref(), Some(&me.id));
    }
}

#[cfg(test)]
mod unpaired_finish_tests {
    use super::*;
    use crate::testutil::Fixture;
    use broscribe_core::SpeakerId;
    use broscribe_core::schema::{Person, Speaker, Strength, Track, TrackRole};

    /// Two microphones and one system track are analyzed per track (no pair),
    /// so mic speakers must go through identity resolution.
    #[test]
    fn mic_speakers_of_unpaired_tracks_are_resolved() {
        let fixture = Fixture::new();
        let mut me = Person::new(fixture.lib.new_person_id(), "Synthetic Me");
        me.is_self = true;
        let mut meeting = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            broscribe_core::schema::Source::Import,
            broscribe_core::schema::now(),
        );
        meeting.setup = Some("headphones".into());
        meeting.processing.status = ProcessingStatus::Analyzed;
        meeting.tracks = [
            ("t0", TrackRole::Mic),
            ("t1", TrackRole::Mic),
            ("t2", TrackRole::System),
        ]
        .into_iter()
        .map(|(id, role)| Track {
            id: id.into(),
            role,
        })
        .collect();
        for (number, track, vector) in [
            (1, "t0", [1.0, 0.0]),
            (2, "t1", [0.0, 1.0]),
            (3, "t2", [0.0, 1.0]),
        ] {
            let mut speaker = Speaker::new(SpeakerId::new(number).unwrap(), track);
            speaker.centroid = Some(Centroid::new("fake", &vector, 1));
            speaker.talk_s = 3.0;
            meeting.speakers.push(speaker);
        }
        let mut voiceprint = ops::weak_sample(&meeting, SpeakerId::new(1).unwrap(), &me.id)
            .unwrap()
            .unwrap();
        voiceprint.source.meeting_id = None;
        voiceprint.strength = Strength::Confirmed;
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&me)?;
                tx.upsert_voiceprints(&me.id, vec![voiceprint])?;
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                let mut index = Index::open(tx.root())?;
                finish_meeting(
                    tx,
                    &mut index,
                    &meeting.id,
                    "fake",
                    Some(&Thresholds {
                        auto_name: Some(0.8),
                        suggest: Some(0.6),
                        margin: Some(0.1),
                        merge: Some(0.9),
                        auto_merge: None,
                    }),
                )?;
                Ok(())
            })
            .unwrap();
        let saved = fixture.lib.load_meeting(&meeting.id).unwrap();
        let first = saved.speaker(SpeakerId::new(1).unwrap()).unwrap();
        assert_eq!(first.assignment, Assignment::Auto);
        assert_eq!(first.person_id.as_ref(), Some(&me.id));
    }

    /// Grouping runs after resolution: a kept speaker resolution just named
    /// as a person a member rejected is resolved again before publishing,
    /// and teaches that person no weak sample.
    #[test]
    fn import_finish_never_publishes_a_kept_speaker_named_against_a_members_rejection() {
        use broscribe_core::schema::VoiceprintKind;
        let fixture = Fixture::new();
        let bob = Person::new(fixture.lib.new_person_id(), "Bob Invented");
        let mut meeting = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            broscribe_core::schema::Source::Import,
            broscribe_core::schema::now(),
        );
        meeting.processing.status = ProcessingStatus::Analyzed;
        meeting.tracks = vec![Track {
            id: "t0".into(),
            role: TrackRole::Mixed,
        }];
        for (number, talk) in [(1, 5.0), (2, 3.0)] {
            let mut speaker = Speaker::new(SpeakerId::new(number).unwrap(), "t0");
            speaker.centroid = Some(Centroid::new("fake", &[1.0, 0.0], 1));
            speaker.talk_s = talk;
            meeting.speakers.push(speaker);
        }
        meeting
            .speaker_mut(SpeakerId::new(2).unwrap())
            .unwrap()
            .rejected_person_ids = vec![bob.id.clone()];
        let mut print = ops::weak_sample(&meeting, SpeakerId::new(1).unwrap(), &bob.id)
            .unwrap()
            .unwrap();
        print.source.meeting_id = None;
        print.source.kind = VoiceprintKind::Enrollment;
        print.strength = Strength::Confirmed;
        let thresholds = Thresholds {
            auto_name: Some(0.5),
            margin: Some(0.0),
            merge: Some(0.5),
            auto_merge: Some(0.9),
            ..Thresholds::default()
        };
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&bob)?;
                tx.upsert_voiceprints(&bob.id, vec![print])?;
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                let mut index = Index::open(tx.root())?;
                finish_meeting(tx, &mut index, &meeting.id, "fake", Some(&thresholds))?;
                Ok(())
            })
            .unwrap();
        let saved = fixture.lib.load_meeting(&meeting.id).unwrap();
        let kept = SpeakerId::new(1).unwrap();
        assert_eq!(saved.speaker_groups.len(), 1);
        assert_eq!(saved.speaker_groups[0].kept, kept);
        let s = saved.speaker(kept).unwrap();
        assert_ne!(
            ops::SpeakerState::of(s).person_id,
            Some(bob.id.clone()),
            "{s:?}"
        );
        let md = std::fs::read_to_string(
            fixture
                .lib
                .meeting_dir(&meeting.id)
                .unwrap()
                .join("meeting.md"),
        )
        .unwrap();
        assert!(!md.contains("Bob Invented"), "{md}");
        assert!(
            fixture
                .lib
                .load_voiceprints(&bob.id)
                .unwrap()
                .embeddings
                .iter()
                .all(|v| v.source.kind != VoiceprintKind::Auto),
            "no weak Bob sample from this meeting"
        );
    }

    /// An import's first finish groups the over-split voice reversibly:
    /// both speakers and the samples learned from each stay as they were,
    /// the confirmed one is kept and the other only shows as it.
    #[test]
    fn import_finish_groups_over_split_speakers_without_rewriting_them() {
        use broscribe_core::schema::{GroupOrigin, VoiceprintKind};
        let fixture = Fixture::new();
        let ana = Person::new(fixture.lib.new_person_id(), "Ana Invented");
        let analyzed = |id: broscribe_core::MeetingId| {
            let mut meeting = Meeting::new(
                id,
                broscribe_core::schema::Source::Import,
                broscribe_core::schema::now(),
            );
            meeting.processing.status = ProcessingStatus::Analyzed;
            meeting.tracks = vec![Track {
                id: "t0".into(),
                role: TrackRole::Mixed,
            }];
            for number in [1, 2] {
                let mut speaker = Speaker::new(SpeakerId::new(number).unwrap(), "t0");
                speaker.centroid = Some(Centroid::new("fake", &[1.0, 0.0], 1));
                speaker.talk_s = 3.0;
                meeting.speakers.push(speaker);
            }
            let kept = meeting.speaker_mut(SpeakerId::new(2).unwrap()).unwrap();
            kept.assignment = Assignment::Confirmed;
            kept.person_id = Some(ana.id.clone());
            meeting
        };
        let meeting = analyzed(fixture.lib.new_meeting_id().unwrap());
        let mut sample = ops::weak_sample(&meeting, SpeakerId::new(1).unwrap(), &ana.id)
            .unwrap()
            .unwrap();
        sample.id = "synthetic-S1".into();
        sample.source.kind = VoiceprintKind::Confirmation;
        sample.source.speaker_id = Some(SpeakerId::new(1).unwrap());
        sample.strength = Strength::Confirmed;
        let thresholds = Thresholds {
            merge: Some(0.5),
            auto_merge: Some(0.9),
            ..Thresholds::default()
        };
        fixture
            .lib
            .write(|tx| {
                tx.save_person(&ana)?;
                tx.upsert_voiceprints(&ana.id, vec![sample.clone()])?;
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                let mut index = Index::open(tx.root())?;
                let out = finish_meeting(tx, &mut index, &meeting.id, "fake", Some(&thresholds))?;
                assert_eq!(out.merged, 1);
                Ok(())
            })
            .unwrap();
        let saved = fixture.lib.load_meeting(&meeting.id).unwrap();
        let shape = |m: &Meeting| -> Vec<_> {
            m.speakers
                .iter()
                .map(|s| (s.id, s.centroid.clone(), s.talk_s))
                .collect()
        };
        assert_eq!(shape(&saved), shape(&meeting), "speakers never folded");
        assert_eq!(saved.segments, meeting.segments, "segments untouched");
        assert_eq!(saved.speaker_groups.len(), 1);
        let group = &saved.speaker_groups[0];
        assert_eq!(group.kept, SpeakerId::new(2).unwrap(), "the confirmed one");
        assert_eq!(group.members, vec![SpeakerId::new(1).unwrap()]);
        assert!(matches!(group.origin, GroupOrigin::Auto { score } if score >= 0.9));
        let print = fixture
            .lib
            .load_voiceprints(&ana.id)
            .unwrap()
            .embeddings
            .into_iter()
            .find(|v| v.id == "synthetic-S1")
            .unwrap();
        assert_eq!(print, sample, "samples are never migrated");
        let md = std::fs::read_to_string(
            fixture
                .lib
                .meeting_dir(&meeting.id)
                .unwrap()
                .join("meeting.md"),
        )
        .unwrap();
        assert!(!md.contains("Speaker 1"), "{md}");

        // Without `auto_merge` nothing is grouped.
        let plain = analyzed(fixture.lib.new_meeting_id().unwrap());
        fixture
            .lib
            .write(|tx| {
                tx.create_meeting_dir(&plain)?;
                tx.save_meeting(&plain)?;
                let mut index = Index::open(tx.root())?;
                let only_merge = Thresholds {
                    merge: Some(0.5),
                    ..Thresholds::default()
                };
                finish_meeting(tx, &mut index, &plain.id, "fake", Some(&only_merge))?;
                // Finishing a `done` meeting again (a manifest re-finish)
                // adds no groups either, even with `auto_merge` set now.
                let again = finish_meeting(tx, &mut index, &plain.id, "fake", Some(&thresholds))?;
                assert_eq!(again.merged, 0);
                Ok(())
            })
            .unwrap();
        assert!(
            fixture
                .lib
                .load_meeting(&plain.id)
                .unwrap()
                .speaker_groups
                .is_empty()
        );
    }
}

#[cfg(all(test, unix))]
mod finish_order_tests {
    use super::*;
    use crate::testutil::Fixture;
    use std::os::unix::fs::PermissionsExt;

    /// Files are the truth: a meeting whose files could not be saved must
    /// not reach the index as finished.
    #[test]
    fn a_failed_save_leaves_the_index_untouched() {
        let fixture = Fixture::new();
        let mut meeting = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            broscribe_core::schema::Source::Import,
            broscribe_core::schema::now(),
        );
        meeting.processing.status = ProcessingStatus::Analyzed;
        fixture
            .lib
            .write(|tx| {
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)
            })
            .unwrap();
        let dir = fixture.lib.meeting_dir(&meeting.id).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores directory permissions, so the failure can't be staged.
        let probe = dir.join(".write-probe");
        if std::fs::write(&probe, b"").is_ok() {
            std::fs::remove_file(&probe).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let result = fixture.lib.write(|tx| {
            let mut index = Index::open(tx.root())?;
            finish_meeting(tx, &mut index, &meeting.id, "fake", None)
        });
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err(), "saving into a read-only folder must fail");
        let index = Index::open_readonly(fixture.lib.root()).unwrap();
        assert!(!index.meeting_ids().unwrap().contains(&meeting.id));
    }
}

#[cfg(test)]
pub(crate) mod unreadable_lookup_tests {
    use super::*;
    use crate::testutil::Fixture;

    /// A saved meeting folder whose `meeting.json` was then destroyed; its
    /// recording (and the `audio.wav` it keeps) has the content `audio`.
    fn unreadable(fixture: &Fixture, audio: Option<&[u8]>) -> std::path::PathBuf {
        let mut meeting = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            broscribe_core::schema::Source::Import,
            broscribe_core::schema::now(),
        );
        if let Some(audio) = audio {
            let original = fixture.input.join(format!("{}.wav", meeting.id.as_str()));
            std::fs::write(&original, audio).unwrap();
            let (sha, bytes) = crate::audio::file_sha256(&original).unwrap();
            meeting.recording = Some(crate::import::recording(
                &original,
                &sha,
                bytes,
                broscribe_core::schema::Handling::Copy,
                "wav",
            ));
        }
        let dir = fixture
            .lib
            .write(|tx| {
                let dir = tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                Ok(dir)
            })
            .unwrap();
        std::fs::write(dir.join(broscribe_core::paths::MEETING_JSON), b"{ not json").unwrap();
        if let Some(audio) = audio {
            std::fs::write(dir.join("audio.wav"), audio).unwrap();
        }
        dir
    }

    /// Rewrites the `meeting.md` in `dir` as a version before the
    /// `provenance` block wrote it.
    pub(crate) fn strip_provenance(dir: &Path) {
        let path = dir.join(broscribe_core::paths::MEETING_MD);
        let md = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        let mut skipping = false;
        for line in md.split_inclusive('\n') {
            skipping = line == "provenance:\n" || (skipping && line.starts_with(' '));
            if !skipping {
                out.push_str(line);
            }
        }
        assert_ne!(out, md);
        std::fs::write(&path, out).unwrap();
    }

    fn new_recording(fixture: &Fixture) -> (String, u64) {
        let source = fixture.input.join("new.wav");
        std::fs::write(&source, b"new recording").unwrap();
        crate::audio::file_sha256(&source).unwrap()
    }

    #[test]
    fn an_unrelated_unreadable_meeting_never_blocks_the_content_lookup() {
        let fixture = Fixture::new();
        let dir = unreadable(&fixture, Some(b"another recording"));
        unreadable(&fixture, None);
        let (sha, bytes) = new_recording(&fixture);
        assert_eq!(
            find_meeting_by_sha(&fixture.lib, &sha, bytes).unwrap(),
            None
        );
        // Every version of meeting.md records the recording's hash, all
        // this lookup compares: one written before `provenance` still
        // proves the folder is another meeting.
        strip_provenance(&dir);
        assert_eq!(
            find_meeting_by_sha(&fixture.lib, &sha, bytes).unwrap(),
            None
        );
    }

    #[test]
    fn an_unreadable_meeting_nothing_rules_out_is_reported_until_repaired() {
        let fixture = Fixture::new();
        let dir = unreadable(&fixture, Some(b"another recording"));
        let md_path = dir.join(broscribe_core::paths::MEETING_MD);
        let (sha, bytes) = new_recording(&fixture);
        // A meeting.md marked pending (an identity change was interrupted
        // before it was rewritten) no longer vouches even for the recording
        // hash it still names.
        let md = std::fs::read_to_string(&md_path).unwrap();
        let line = md.lines().find(|l| l.trim() == "version: 1").unwrap();
        let indent = &line[..line.len() - line.trim_start().len()];
        let pending = md.replacen(
            &format!("{line}\n"),
            &format!("{line}\n{indent}pending: true\n"),
            1,
        );
        assert_ne!(pending, md);
        std::fs::write(&md_path, pending).unwrap();
        let error = find_meeting_by_sha(&fixture.lib, &sha, bytes).unwrap_err();
        assert_eq!(error.code(), "invalid_input");
        assert!(error.to_string().contains("rule out"), "{error}");
        // Neither does a missing one.
        std::fs::remove_file(&md_path).unwrap();
        let error = find_meeting_by_sha(&fixture.lib, &sha, bytes).unwrap_err();
        assert_eq!(error.code(), "invalid_input");
        let message = error.to_string();
        assert!(message.contains(&dir.display().to_string()), "{message}");
        assert!(message.contains("rule out"), "{message}");
        // A damaged meeting.md proves nothing either.
        std::fs::write(dir.join(broscribe_core::paths::MEETING_MD), b"---\nid: [\n").unwrap();
        assert_eq!(
            find_meeting_by_sha(&fixture.lib, &sha, bytes)
                .unwrap_err()
                .code(),
            "invalid_input"
        );
        // Removing the damaged meeting.json, as the refusal says, unblocks it.
        std::fs::remove_file(dir.join(broscribe_core::paths::MEETING_JSON)).unwrap();
        assert_eq!(
            find_meeting_by_sha(&fixture.lib, &sha, bytes).unwrap(),
            None
        );
    }

    #[test]
    fn an_unreadable_meeting_holding_the_same_content_is_reported_not_duplicated() {
        let fixture = Fixture::new();
        let dir = unreadable(&fixture, Some(b"same recording"));
        let source = fixture.input.join("same.wav");
        std::fs::write(&source, b"same recording").unwrap();
        let (sha, bytes) = crate::audio::file_sha256(&source).unwrap();
        let error = find_meeting_by_sha(&fixture.lib, &sha, bytes).unwrap_err();
        assert_eq!(error.code(), "invalid_input");
        assert!(
            error.to_string().contains(&dir.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn a_readable_match_wins_over_an_unreadable_suspect() {
        let fixture = Fixture::new();
        unreadable(&fixture, Some(b"same recording"));
        let source = fixture.input.join("same.wav");
        std::fs::write(&source, b"same recording").unwrap();
        let (sha, bytes) = crate::audio::file_sha256(&source).unwrap();
        let mut meeting = Meeting::new(
            fixture.lib.new_meeting_id().unwrap(),
            broscribe_core::schema::Source::Import,
            broscribe_core::schema::now(),
        );
        meeting.recording = Some(crate::import::recording(
            &source,
            &sha,
            bytes,
            broscribe_core::schema::Handling::Reference,
            "wav",
        ));
        fixture
            .lib
            .write(|tx| {
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)
            })
            .unwrap();
        let found = find_meeting_by_sha(&fixture.lib, &sha, bytes)
            .unwrap()
            .unwrap();
        assert_eq!(found.id, meeting.id);
    }
}
