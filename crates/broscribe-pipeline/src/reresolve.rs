//! Identity re-resolution with the current naming thresholds, one meeting
//! at a time: `identity re-resolve`, calibration (`identity calibrate`,
//! `bench … --calibrate`) and the queue's identity reevaluation
//! ([`crate::reidentify`]) share it.
//!
//! The resolver leaves out a meeting's own weak samples, its caption samples
//! included, so resolution alone would drop names this meeting's captions
//! proved, and then their caption samples. Each meeting therefore replays its
//! saved captions after resolving, as recording and reprocessing do.
//!
//! [`visit`] reads one `done` meeting under the caller's library write and
//! plans its outcome on copies ([`plan_meeting`]): identity resolution with
//! the publication policy (`identity::resolve_voice`), the paired
//! microphone's speakers left as recorded, saved captions replayed against
//! the people catalog as it is in that write, and auto groups a
//! caption-proven name made incompatible dissolved. [`publish`] writes the
//! plan in the same write, so nothing changes in between and no meeting
//! refers to a person another meeting only planned to create.
//!
//! The voice evidence is a snapshot held in memory between writes
//! ([`Evidence`]): its fingerprint is checked under every write and the
//! snapshot reloaded when anything but this run changed it. A run's own
//! weak samples never feed back into the rest of the run.
//!
//! [`plan`] (the dry run) plans every meeting and writes nothing;
//! [`re_resolve`] plans and publishes each meeting in its own short write,
//! recording the changes first ([`RERESOLVE_IN_FLIGHT`]) so a publication a
//! crash cut short is repaired by the next run even though the meeting then
//! looks unchanged ([`repair_publication`]).

use std::ops::AddAssign;
use std::path::{Path, PathBuf};

use broscribe_core::config::Thresholds;
use broscribe_core::index::Index;
use broscribe_core::learning::{InFlight, evidence_fingerprint};
use broscribe_core::library::MeetingLookup;
use broscribe_core::ops::SpeakerState;
use broscribe_core::queue::{CONFIRM_KIND, PLAYBACK_KIND, Queue, VECTORS_KIND};
use broscribe_core::schema::{Assignment, Meeting, ProcessingStatus};
use broscribe_core::{Error, Library, MeetingId, Result, WriteTx, fsutil, ops};
use broscribe_readers::alignment::RangeEmbeddings;
use serde::Serialize;

use crate::caption_replay;
use crate::record::CaptionWrites;

/// Kinds of active jobs that do not make a meeting busy for re-resolution:
/// confirmation samples publish a person's voiceprints, never the meeting,
/// and the evidence fingerprint catches what they change; a search-vector
/// refresh writes only index vectors, never the meeting, and drops a vector
/// whose window revision moved meanwhile (a publication that changes the
/// windows queues it again); a playback cache writes only a WAV of the
/// recording outside the library, keyed by the recording's hash, which no
/// identity changes.
pub(crate) const IGNORED_BUSY: &[&str] = &[CONFIRM_KIND, VECTORS_KIND, PLAYBACK_KIND];

/// What re-resolution does (or did) to speakers. Each changed speaker
/// counts once by its new state (`auto_named`, `suggested`, `cleared`);
/// `demoted` also counts those that were auto-named before and are now
/// suggested or unassigned (they return to the review queue unless
/// dismissed). Skipped meetings count once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReresolveCounts {
    pub auto_named: usize,
    pub demoted: usize,
    pub suggested: usize,
    pub cleared: usize,
    /// A job was working on the meeting: left alone.
    pub skipped_busy: usize,
}

impl ReresolveCounts {
    fn of(changes: &[ops::SpeakerChange]) -> Self {
        let mut c = Self::default();
        for change in changes {
            match change.after.assignment {
                Assignment::Auto => c.auto_named += 1,
                Assignment::Suggested => c.suggested += 1,
                Assignment::None => c.cleared += 1,
                Assignment::Confirmed => {}
            }
            if change.before.assignment == Assignment::Auto
                && matches!(
                    change.after.assignment,
                    Assignment::Suggested | Assignment::None
                )
            {
                c.demoted += 1;
            }
        }
        c
    }

    /// The sum over `plans`.
    pub fn total(plans: &[MeetingPlan]) -> Self {
        let mut t = Self::default();
        for p in plans {
            t += p.counts;
        }
        t
    }
}

impl AddAssign for ReresolveCounts {
    fn add_assign(&mut self, o: Self) {
        self.auto_named += o.auto_named;
        self.demoted += o.demoted;
        self.suggested += o.suggested;
        self.cleared += o.cleared;
        self.skipped_busy += o.skipped_busy;
    }
}

/// One meeting's planned (dry run) or applied re-resolution.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MeetingPlan {
    pub meeting_id: MeetingId,
    #[serde(flatten)]
    pub counts: ReresolveCounts,
    /// The speakers whose assignment or person changes.
    #[serde(skip)]
    pub changes: Vec<ops::SpeakerChange>,
}

impl MeetingPlan {
    fn busy(meeting_id: MeetingId) -> Self {
        Self {
            meeting_id,
            counts: ReresolveCounts {
                skipped_busy: 1,
                ..ReresolveCounts::default()
            },
            changes: Vec::new(),
        }
    }
}

/// One meeting's re-resolution, planned on copies ([`plan_meeting`]).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Planned {
    base: Meeting,
    meeting: Meeting,
    model: String,
    captions: CaptionWrites,
    pub(crate) changes: Vec<ops::SpeakerChange>,
}

impl Planned {
    fn summary(&self) -> MeetingPlan {
        MeetingPlan {
            meeting_id: self.base.id.clone(),
            counts: ReresolveCounts::of(&self.changes),
            changes: self.changes.clone(),
        }
    }
}

/// The voice evidence of one model, held in memory between short writes.
pub(crate) struct Evidence {
    model: String,
    people: ops::PeopleSamples,
    /// The fingerprint the library has unless someone else wrote since.
    expected: String,
}

impl Evidence {
    /// The samples of `model` as stored now.
    pub(crate) fn load(lib: &Library, model: &str) -> Result<Self> {
        let expected = evidence_fingerprint(lib)?;
        Ok(Self {
            model: model.to_owned(),
            people: ops::load_person_samples(lib, model)?,
            expected,
        })
    }

    /// Under the library lock: reloads the snapshot when someone else
    /// changed any voice evidence (a confirmation, enrollment, caption
    /// promotion, forget…) since it was read or this run last wrote.
    /// Returns whether it was reloaded.
    pub(crate) fn check(&mut self, lib: &Library) -> Result<bool> {
        if evidence_fingerprint(lib)? == self.expected {
            return Ok(false);
        }
        *self = Self::load(lib, &self.model)?;
        Ok(true)
    }

    /// Under the library lock, after this run published: its own weak
    /// samples become the expected state, while the snapshot in memory
    /// stays as read (automatic names never teach the rest of the run).
    pub(crate) fn own_write(&mut self, lib: &Library) -> Result<()> {
        self.expected = evidence_fingerprint(lib)?;
        Ok(())
    }
}

/// Plans re-resolving `base`, a `done` meeting, on copies: every speaker
/// that is not `confirmed` nor a grouped member (frozen) and has a centroid
/// of `model`, against `people` with `thresholds`, honouring rejections and
/// leaving the paired microphone's speakers as recorded
/// (`finish::resolve_identities`); then its saved captions replayed,
/// planned against the people catalog as stored now (callers plan and
/// publish in one write), and auto groups a caption-proven name made
/// incompatible dissolved ([`ops::reconcile_auto_groups`]). `dir` is the
/// meeting's folder, as the caller found it. `None` when nothing would
/// change.
///
/// No audio is decoded: a caption naming keeps the caption sample the
/// recording stored for it (unchanged ranges, same ID) but adds none.
/// Review dismissals ("Can't tell") are kept: they are not identities.
pub(crate) fn plan_meeting(
    lib: &Library,
    dir: &Path,
    people: &ops::PeopleSamples,
    base: &Meeting,
    model: &str,
    thresholds: Option<&Thresholds>,
    live: &broscribe_core::config::LiveConfig,
) -> Result<Option<Planned>> {
    let mut meeting = base.clone();
    crate::live_identity::recheck_meeting(lib, &mut meeting)?;
    let states = caption_replay::speaker_states(&meeting);
    crate::finish::resolve_identities(&mut meeting, model, people, thresholds)?;
    let mut captions = CaptionWrites::default();
    if dir.join("captions.jsonl").exists() {
        // The catalog as stored in this write: never a person another
        // meeting only planned to create.
        let mut known = lib.list_people()?;
        caption_replay::plan_saved_captions(
            lib,
            dir,
            &mut known,
            &mut meeting,
            &RangeEmbeddings::default(),
            &[],
            &mut captions,
        )?;
    }
    let proven = ops::caption_proven(lib, &meeting)?;
    if !ops::reconcile_auto_groups(&mut meeting, &proven).is_empty() {
        meeting.set_generated_stale();
    }
    ops::guard_automatic_names(&mut meeting);
    ops::reconcile_live_speakers(lib, &mut meeting, live)?;
    ops::refresh_identity_conflict_notes(lib, &mut meeting)?;
    let changes = caption_replay::speaker_changes(&meeting, states);
    if meeting == *base && captions == CaptionWrites::default() {
        return Ok(None);
    }
    Ok(Some(Planned {
        base: base.clone(),
        meeting,
        model: model.to_owned(),
        captions,
        changes,
    }))
}

/// What [`visit`] found.
pub(crate) enum Visit {
    /// Gone, or not `done` (its own finish resolves it with the evidence
    /// current then).
    Skipped,
    /// A job other than [`IGNORED_BUSY`] ones is working on it.
    Busy,
    Unchanged,
    Planned(Box<Planned>),
    /// Its files could not be read or planned (a damaged or newer
    /// `meeting.json`, unreadable captions…): the error, for callers to
    /// report or skip; nothing was written.
    Unreadable(Error),
}

/// Whether `e`, raised reading or planning one meeting, concerns that
/// meeting's files rather than the library or the queue (a busy lock, a
/// lost lease or an internal error stop the caller instead).
pub(crate) fn meeting_error(e: &Error) -> bool {
    matches!(
        e,
        Error::Schema(_)
            | Error::Io { .. }
            | Error::Decode(_)
            | Error::InvalidInput(_)
            | Error::Unsupported(_)
    )
}

/// The meeting folders listed once for a pass of visits
/// ([`Library::meeting_lookup`]), so a pass never lists every folder again
/// for each meeting. Nothing else is kept: each visit reads the meeting's
/// `meeting.json` as it is under its write, and a meeting the listing
/// misses (created, renamed or moved since) is looked up afresh.
pub(crate) struct Meetings<'a>(MeetingLookup<'a>);

impl<'a> Meetings<'a> {
    pub(crate) fn list(lib: &'a Library) -> Result<Self> {
        Ok(Self(lib.meeting_lookup()?))
    }

    /// Meeting `id` as `lib` stores it now, with its folder.
    fn find(&self, lib: &Library, id: &MeetingId) -> Result<(PathBuf, Meeting)> {
        match self.0.find(id) {
            Err(Error::NotFound(_)) => {
                let dir = lib.meeting_dir(id)?;
                let meeting = lib.load_meeting_at(&dir)?;
                Ok((dir, meeting))
            }
            found => found,
        }
    }
}

/// Under the caller's library write: reads meeting `id` as stored now
/// (found through the pass's `meetings`), checks it is `done` and idle
/// (ignoring [`IGNORED_BUSY`] jobs), refreshes `evidence` when someone else
/// changed it ([`Evidence::check`]) and plans it ([`plan_meeting`]).
pub(crate) fn visit(
    lib: &Library,
    meetings: &Meetings<'_>,
    id: &MeetingId,
    evidence: &mut Evidence,
    thresholds: Option<&Thresholds>,
    live: &broscribe_core::config::LiveConfig,
) -> Result<Visit> {
    let (dir, meeting) = match meetings.find(lib, id) {
        Ok(found) => found,
        Err(Error::NotFound(_)) => return Ok(Visit::Skipped),
        Err(e) if meeting_error(&e) => return Ok(Visit::Unreadable(e)),
        Err(e) => return Err(e),
    };
    if meeting.processing.status != ProcessingStatus::Done {
        return Ok(Visit::Skipped);
    }
    match Queue::open(lib.root())?.check_meeting_idle_except(&meeting, IGNORED_BUSY) {
        Ok(()) => {}
        Err(Error::LibraryBusy(_)) => return Ok(Visit::Busy),
        Err(e) => return Err(e),
    }
    evidence.check(lib)?;
    match plan_meeting(
        lib,
        &dir,
        &evidence.people,
        &meeting,
        &evidence.model,
        thresholds,
        live,
    ) {
        Ok(Some(p)) => Ok(Visit::Planned(Box::new(p))),
        Ok(None) => Ok(Visit::Unchanged),
        Err(e) if meeting_error(&e) => Ok(Visit::Unreadable(e)),
        Err(e) => Err(e),
    }
}

/// Writes `plan` in the write that planned it: the people and samples
/// caption replay creates, the meeting and its index rows, and its weak
/// samples ([`ops::sync_weak_samples`]); a refresh of its search vectors is
/// queued when its windows changed. Every step is idempotent, so a retry
/// after a crash repairs what the crash left out.
pub(crate) fn publish(tx: &mut WriteTx<'_>, index: &mut Index, plan: Planned) -> Result<()> {
    if plan.captions != CaptionWrites::default() {
        let mut people = tx.library().list_people()?;
        people.extend(
            plan.captions
                .created
                .iter()
                .filter(|p| !people.iter().any(|q| q.id == p.id))
                .cloned()
                .collect::<Vec<_>>(),
        );
        crate::record::write_caption_names(tx, index, &people, &plan.captions)?;
    }
    let mut meeting = plan.meeting;
    if meeting != plan.base {
        let windows = index.window_revision(&meeting.id)?;
        ops::sync_live_promotions(tx, Some(&plan.base), &meeting, &plan.model)?;
        meeting.updated_at = broscribe_core::schema::now();
        tx.save_meeting(&meeting)?;
        index.upsert_meeting(tx, &meeting)?;
        if index.window_revision(&meeting.id)? != windows {
            let (key, input) = crate::worker::vectors_job_input(&meeting.id);
            Queue::open(tx.root())?.enqueue(tx, VECTORS_KIND, &key, input)?;
        }
    }
    ops::sync_weak_samples(tx, &meeting, &plan.changes)
}

/// What [`repair_publication`] found.
pub(crate) enum Repaired {
    /// The changes to count (none: nothing reached the meeting, or it is
    /// gone).
    Applied(Vec<ops::SpeakerChange>),
    /// The meeting cannot be read: nothing to repair now.
    Unreadable(Error),
}

/// Finishes an interrupted publication: the changes of `flight` that the
/// meeting shows now (the speaker reads as their `after` state) reached
/// `meeting.json`, so their index rows and weak samples (added and
/// removed) are repaired (idempotent; a forgotten person gets nothing
/// back) and they are returned to be counted once. Changes the meeting does
/// not show were not written, or a later decision replaced them: they are
/// left alone and the meeting is planned again when visited. Shared by
/// [`re_resolve`] and the queue's reevaluation ([`crate::reidentify`]).
pub(crate) fn repair_publication(
    tx: &mut WriteTx<'_>,
    lib: &Library,
    flight: &InFlight,
) -> Result<Repaired> {
    let meeting = match lib.load_meeting(&flight.meeting_id) {
        Ok(m) => m,
        Err(Error::NotFound(_)) => return Ok(Repaired::Applied(Vec::new())),
        Err(e) if meeting_error(&e) => return Ok(Repaired::Unreadable(e)),
        Err(e) => return Err(e),
    };
    let applied: Vec<ops::SpeakerChange> = flight
        .changes
        .iter()
        .filter(|c| {
            meeting.speaker(c.speaker_id).is_some_and(|s| {
                let now = SpeakerState::of(s);
                now.assignment == c.after.assignment && now.person_id == c.after.person_id
            })
        })
        .cloned()
        .collect();
    if applied.is_empty() {
        return Ok(Repaired::Applied(applied));
    }
    let mut index = Index::open(tx.root())?;
    index.upsert_meeting(tx, &meeting)?;
    ops::sync_weak_samples(tx, &meeting, &applied)?;
    let (key, input) = crate::worker::vectors_job_input(&meeting.id);
    Queue::open(tx.root())?.enqueue(tx, VECTORS_KIND, &key, input)?;
    Ok(Repaired::Applied(applied))
}

/// The library-root file [`re_resolve`] records a meeting's planned
/// changes in ([`InFlight`]: IDs and assignments only, no names) before
/// publishing them, and removes in the same write once they are written.
pub const RERESOLVE_IN_FLIGHT: &str = ".reresolve-in-flight.json";

fn in_flight_path(root: &Path) -> PathBuf {
    root.join(RERESOLVE_IN_FLIGHT)
}

fn remove_in_flight(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

/// Under the caller's write: repairs the publication an interrupted
/// [`re_resolve`] left (its checkpoint) and removes the checkpoint. Returns
/// what the repair counted, when anything had reached the meeting. A
/// meeting that cannot be read keeps the checkpoint and stops the run with
/// its error, as an unreadable meeting does.
fn repair_interrupted(tx: &mut WriteTx<'_>) -> Result<Option<MeetingPlan>> {
    let lib = tx.library().clone();
    let path = in_flight_path(lib.root());
    let flight: InFlight = match fsutil::read_json(&path) {
        Ok(f) => f,
        Err(Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    let applied = match repair_publication(tx, &lib, &flight)? {
        Repaired::Applied(applied) => applied,
        Repaired::Unreadable(e) => return Err(e),
    };
    remove_in_flight(&path)?;
    Ok((!applied.is_empty()).then(|| MeetingPlan {
        meeting_id: flight.meeting_id,
        counts: ReresolveCounts::of(&applied),
        changes: applied,
    }))
}

/// Plans re-resolving every `done` meeting ([`plan_meeting`]) and writes
/// nothing, not even queue folders or lock files
/// ([`Queue::check_meeting_idle_readonly`]): what `identity re-resolve
/// --dry-run` shows. Meetings a job works on are listed as `skipped_busy`;
/// meetings with nothing to change are left out. Every meeting is planned
/// against the samples and people catalog as stored when it is planned,
/// as a real run does when nothing else writes meanwhile. Listed in
/// meeting ID order, as [`re_resolve`] lists what it did.
pub fn plan(
    lib: &Library,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<Vec<MeetingPlan>> {
    plan_with_live(lib, model, thresholds, &Default::default())
}

pub fn plan_with_live(
    lib: &Library,
    model: &str,
    thresholds: Option<&Thresholds>,
    live: &broscribe_core::config::LiveConfig,
) -> Result<Vec<MeetingPlan>> {
    let people = ops::load_person_samples(lib, model)?;
    let mut out = Vec::new();
    for dir in lib.list_meeting_dirs()? {
        let base = lib.load_meeting_at(&dir)?;
        if base.processing.status != ProcessingStatus::Done {
            continue;
        }
        match Queue::check_meeting_idle_readonly(lib.root(), &base, IGNORED_BUSY) {
            Ok(()) => {}
            Err(Error::LibraryBusy(_)) => {
                out.push(MeetingPlan::busy(base.id.clone()));
                continue;
            }
            Err(e) => return Err(e),
        }
        if let Some(p) = plan_meeting(lib, &dir, &people, &base, model, thresholds, live)? {
            out.push(p.summary());
        }
    }
    // Folders sort by date and title; the real run goes by ID.
    out.sort_by(|a, b| a.meeting_id.cmp(&b.meeting_id));
    Ok(out)
}

/// Re-resolves every `done` meeting in meeting ID order, each planned and
/// published in its own short library write ([`visit`], [`publish`]): what
/// `identity re-resolve` (without `--dry-run`) and calibration run. Each
/// publication's changes are recorded first ([`RERESOLVE_IN_FLIGHT`]); a
/// publication an earlier run left unfinished is repaired before anything
/// else and listed first. Returns the meetings changed or skipped as busy,
/// in that order; a meeting that cannot be read stops it with that error,
/// as the dry run does.
pub fn re_resolve(
    lib: &Library,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<Vec<MeetingPlan>> {
    re_resolve_with_live(lib, model, thresholds, &Default::default())
}

pub fn re_resolve_with_live(
    lib: &Library,
    model: &str,
    thresholds: Option<&Thresholds>,
    live: &broscribe_core::config::LiveConfig,
) -> Result<Vec<MeetingPlan>> {
    let mut out: Vec<MeetingPlan> = lib.write(repair_interrupted)?.into_iter().collect();
    let mut evidence = Evidence::load(lib, model)?;
    let ids = lib.list_meeting_ids()?;
    let meetings = Meetings::list(lib)?;
    for id in ids {
        let done = lib.write(|tx| {
            let lib = tx.library().clone();
            match visit(&lib, &meetings, &id, &mut evidence, thresholds, live)? {
                Visit::Skipped | Visit::Unchanged => Ok(None),
                Visit::Busy => Ok(Some(MeetingPlan::busy(id.clone()))),
                Visit::Unreadable(e) => Err(e),
                Visit::Planned(plan) => {
                    let summary = plan.summary();
                    // Recorded before any file is written (see
                    // `repair_interrupted`).
                    let checkpoint = in_flight_path(lib.root());
                    fsutil::write_json_atomic(
                        &checkpoint,
                        &InFlight {
                            meeting_id: id.clone(),
                            source: false,
                            changes: plan.changes.clone(),
                        },
                    )?;
                    let mut index = Index::open(tx.root())?;
                    publish(tx, &mut index, *plan)?;
                    remove_in_flight(&checkpoint)?;
                    evidence.own_write(&lib)?;
                    Ok(Some(summary))
                }
            }
        })?;
        out.extend(done);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use broscribe_core::config::Thresholds;
    use broscribe_core::index::Index;
    use broscribe_core::schema::{Assignment, Person, VoiceprintKind};
    use broscribe_core::{MeetingId, SpeakerId, ops};

    use super::{MeetingPlan, ReresolveCounts, plan, re_resolve};
    use crate::caption_replay::tests::{caption_models, caption_named_meeting, niko, second_voice};
    use crate::testutil::Fixture;

    fn changes(plans: Vec<MeetingPlan>) -> Vec<ops::SpeakerChange> {
        plans.into_iter().flat_map(|p| p.changes).collect()
    }

    fn strict() -> Thresholds {
        Thresholds {
            auto_name: Some(0.99),
            suggest: Some(0.98),
            margin: Some(0.5),
            merge: None,
            auto_merge: None,
        }
    }

    /// Every folder (`None`) and file (its bytes) under `root`.
    fn files(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut dirs = vec![root.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    out.insert(path.clone(), None);
                    dirs.push(path);
                } else {
                    out.insert(path.clone(), Some(std::fs::read(&path).unwrap()));
                }
            }
        }
        out
    }

    fn first_voice(fx: &Fixture, id: &MeetingId) -> SpeakerId {
        let meeting = fx.lib.load_meeting(id).unwrap();
        meeting
            .segments
            .iter()
            .find(|s| s.start <= 2.0 && s.end >= 2.0)
            .and_then(|s| s.speaker)
            .unwrap()
    }

    /// The caption-named meeting, with its first voice auto-named as a
    /// person without any voice sample: strict thresholds demote it.
    fn captioned_with_stale_auto_name(fx: &Fixture) -> (MeetingId, SpeakerId, Person) {
        let id = caption_named_meeting(fx);
        let ana = Person::new(fx.lib.new_person_id(), "Synthetic Ana");
        let first = first_voice(fx, &id);
        ops::with_index(&fx.lib, |tx, index| {
            tx.save_person(&ana)?;
            index.upsert_person(tx, &ana)?;
            let mut meeting = tx.library().load_meeting(&id)?;
            let s = meeting.speaker_mut(first).unwrap();
            s.assignment = Assignment::Auto;
            s.person_id = Some(ana.id.clone());
            s.score = Some(0.95);
            tx.save_meeting(&meeting)?;
            index.upsert_meeting(tx, &meeting)
        })
        .unwrap();
        (id, first, ana)
    }

    #[test]
    fn a_pass_listing_reads_meetings_as_stored_now_and_finds_later_ones() {
        use broscribe_core::schema::{Meeting, ProcessingStatus, Source, now};
        let fx = Fixture::new();
        let new_meeting = || {
            let meeting = Meeting::new(fx.lib.new_meeting_id().unwrap(), Source::Import, now());
            let dir = fx
                .lib
                .write(|tx| {
                    let dir = tx.create_meeting_dir(&meeting)?;
                    tx.save_meeting(&meeting)?;
                    Ok(dir)
                })
                .unwrap();
            (meeting, dir)
        };
        let (mut listed, dir) = new_meeting();
        let meetings = super::Meetings::list(&fx.lib).unwrap();
        // Changed after the listing: read as stored now.
        listed.processing.status = ProcessingStatus::Done;
        fx.lib.write(|tx| tx.save_meeting(&listed)).unwrap();
        let (found_dir, found) = meetings.find(&fx.lib, &listed.id).unwrap();
        assert_eq!(
            (found_dir, found.processing.status),
            (dir.clone(), ProcessingStatus::Done)
        );
        // Moved or created after the listing: found afresh.
        let moved = dir.with_file_name(format!("moved-{}", listed.id.short_hex()));
        std::fs::rename(&dir, &moved).unwrap();
        let (found_dir, found) = meetings.find(&fx.lib, &listed.id).unwrap();
        assert_eq!((found_dir, found.id), (moved, listed.id.clone()));
        let (later, later_dir) = new_meeting();
        let (found_dir, found) = meetings.find(&fx.lib, &later.id).unwrap();
        assert_eq!((found_dir, found.id), (later_dir, later.id));
        // Gone: not found.
        let gone = fx.lib.new_meeting_id().unwrap();
        assert!(matches!(
            meetings.find(&fx.lib, &gone),
            Err(broscribe_core::Error::NotFound(_))
        ));
    }

    #[test]
    fn reresolve_keeps_caption_names_and_caption_samples_after_recalibration() {
        let fx = Fixture::new();
        let id = caption_named_meeting(&fx);
        let person = niko(&fx);
        let caption_samples = || {
            fx.lib
                .load_voiceprints(&person.id)
                .unwrap()
                .embeddings
                .into_iter()
                .filter(|v| v.source.kind == VoiceprintKind::Captions)
                .collect::<Vec<_>>()
        };
        let before = caption_samples();
        assert_eq!(before.len(), 1);
        let model = caption_models().embedder.model_id().to_owned();
        // Strict new thresholds: the resolver alone (which leaves out this
        // meeting's own samples) cannot name the speaker.
        let strict = strict();
        let changes = changes(re_resolve(&fx.lib, &model, Some(&strict)).unwrap());
        assert!(changes.is_empty(), "{changes:?}");
        let speaker = second_voice(&fx, &id);
        assert_eq!(speaker.person_id.as_ref(), Some(&person.id));
        assert_eq!(speaker.assignment, Assignment::Auto);
        let after = caption_samples();
        assert_eq!(
            after.iter().map(|v| &v.id).collect::<Vec<_>>(),
            before.iter().map(|v| &v.id).collect::<Vec<_>>()
        );
        assert_eq!(after[0].vector_b64, before[0].vector_b64);
    }

    /// Re-resolution keeps the paired microphone's segment-level decisions:
    /// a recorded headphones meeting's "Unknown (mic)" speaker stays
    /// unassigned even when loose thresholds would name its track-wide
    /// centroid (finalization's `resolve_identities` policy).
    #[test]
    fn reresolve_leaves_unknown_paired_mic_speech_unassigned() {
        use broscribe_core::SpeakerId;
        use broscribe_core::schema::{
            Centroid, Meeting, Person, ProcessingStatus, Source, Speaker, Strength, Track,
            TrackRole,
        };
        let fx = Fixture::new();
        let remote = Person::new(fx.lib.new_person_id(), "Synthetic Remote");
        let mut meeting = Meeting::new(
            fx.lib.new_meeting_id().unwrap(),
            Source::Recording,
            broscribe_core::schema::now(),
        );
        meeting.setup = Some("headphones".into());
        meeting.processing.status = ProcessingStatus::Done;
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
        for (number, track, vector) in [(1, "t0", [1.0, 0.0]), (2, "t1", [0.0, 1.0])] {
            let mut speaker = Speaker::new(SpeakerId::new(number).unwrap(), track);
            speaker.centroid = Some(Centroid::new("fake", &vector, 1));
            speaker.talk_s = 3.0;
            meeting.speakers.push(speaker);
        }
        // A confirmed sample from elsewhere that matches the mic speaker.
        let mut voiceprint = ops::weak_sample(&meeting, SpeakerId::new(1).unwrap(), &remote.id)
            .unwrap()
            .unwrap();
        voiceprint.source.meeting_id = None;
        voiceprint.strength = Strength::Confirmed;
        fx.lib
            .write(|tx| {
                tx.save_person(&remote)?;
                tx.upsert_voiceprints(&remote.id, vec![voiceprint])?;
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)
            })
            .unwrap();
        let loose = Thresholds {
            auto_name: Some(0.1),
            suggest: Some(0.05),
            margin: Some(0.0),
            merge: None,
            auto_merge: None,
        };
        let changes = changes(re_resolve(&fx.lib, "fake", Some(&loose)).unwrap());
        let saved = fx.lib.load_meeting(&meeting.id).unwrap();
        let unknown = saved.speaker(SpeakerId::new(1).unwrap()).unwrap();
        assert_eq!(unknown.assignment, Assignment::None);
        assert!(unknown.person_id.is_none());
        assert!(
            changes.iter().all(|c| c.speaker_id != unknown.id),
            "{changes:?}"
        );
        assert_eq!(
            fx.lib
                .load_voiceprints(&remote.id)
                .unwrap()
                .embeddings
                .len(),
            1,
            "no weak sample from unverified mic speech"
        );
    }

    /// `identity re-resolve --dry-run` writes nothing (every library file
    /// byte-identical), and its counts are the real run's; the caption
    /// name survives both, the sample-less auto name is demoted.
    #[test]
    fn dry_run_writes_nothing_and_counts_like_the_real_run() {
        let fx = Fixture::new();
        let (id, first, _) = captioned_with_stale_auto_name(&fx);
        let model = caption_models().embedder.model_id().to_owned();
        let person = niko(&fx);
        // Snapshot before the first dry run: it creates nothing at all.
        let before = files(fx.lib.root());
        let dry = plan(&fx.lib, &model, Some(&strict())).unwrap();
        assert_eq!(files(fx.lib.root()), before, "a dry run writes nothing");
        let expected = ReresolveCounts {
            demoted: 1,
            cleared: 1,
            ..ReresolveCounts::default()
        };
        assert_eq!(ReresolveCounts::total(&dry), expected);
        assert_eq!(dry.len(), 1);
        assert_eq!(dry[0].meeting_id, id);

        let real = re_resolve(&fx.lib, &model, Some(&strict())).unwrap();
        assert_eq!(
            real.iter()
                .map(|p| (&p.meeting_id, p.counts))
                .collect::<Vec<_>>(),
            dry.iter()
                .map(|p| (&p.meeting_id, p.counts))
                .collect::<Vec<_>>()
        );
        let meeting = fx.lib.load_meeting(&id).unwrap();
        let demoted = meeting.speaker(first).unwrap();
        assert_eq!(demoted.assignment, Assignment::None);
        assert!(demoted.person_id.is_none());
        let named = second_voice(&fx, &id);
        assert_eq!(named.person_id.as_ref(), Some(&person.id));
        assert_eq!(named.assignment, Assignment::Auto);

        // Idempotent: nothing left to change.
        assert!(plan(&fx.lib, &model, Some(&strict())).unwrap().is_empty());
        assert!(
            re_resolve(&fx.lib, &model, Some(&strict()))
                .unwrap()
                .is_empty()
        );
    }

    /// A naming written between a dry run and the real run is kept: the
    /// real run plans each meeting under the write that publishes it (no
    /// demotion counted). The naming's reevaluation request does not make
    /// the meeting busy.
    #[test]
    fn naming_after_a_dry_run_is_kept() {
        let fx = Fixture::new();
        let (id, first, ana) = captioned_with_stale_auto_name(&fx);
        let model = caption_models().embedder.model_id().to_owned();
        let dry = plan(&fx.lib, &model, Some(&strict())).unwrap();
        assert_eq!(ReresolveCounts::total(&dry).demoted, 1);

        ops::with_index(&fx.lib, |tx, index| {
            ops::name_speaker(tx, index, &id, first, ana.id.as_str(), Vec::new(), &model)
        })
        .unwrap();

        let real = re_resolve(&fx.lib, &model, Some(&strict())).unwrap();
        assert_eq!(ReresolveCounts::total(&real).demoted, 0);
        let meeting = fx.lib.load_meeting(&id).unwrap();
        let s = meeting.speaker(first).unwrap();
        assert_eq!(s.assignment, Assignment::Confirmed);
        assert_eq!(s.person_id.as_ref(), Some(&ana.id));
    }

    /// Meetings a job works on are counted and left alone, in the dry run
    /// and the real run.
    #[test]
    fn busy_meetings_are_skipped() {
        let fx = Fixture::new();
        let (id, first, ana) = captioned_with_stale_auto_name(&fx);
        let model = caption_models().embedder.model_id().to_owned();
        fx.lib
            .write(|tx| {
                broscribe_core::queue::Queue::open(tx.root())?
                    .enqueue(
                        tx,
                        "reprocess",
                        id.as_str(),
                        serde_json::json!({ "meeting_id": id.as_str() }),
                    )
                    .map(|_| ())
            })
            .unwrap();
        let busy = ReresolveCounts {
            skipped_busy: 1,
            ..ReresolveCounts::default()
        };
        let dry = plan(&fx.lib, &model, Some(&strict())).unwrap();
        assert_eq!(ReresolveCounts::total(&dry), busy);
        let real = re_resolve(&fx.lib, &model, Some(&strict())).unwrap();
        assert_eq!(ReresolveCounts::total(&real), busy);
        let s = fx
            .lib
            .load_meeting(&id)
            .unwrap()
            .speaker(first)
            .cloned()
            .unwrap();
        assert_eq!(s.assignment, Assignment::Auto);
        assert_eq!(s.person_id.as_ref(), Some(&ana.id));
    }

    /// New thresholds name more speakers, never touching confirmations,
    /// rejections or review dismissals ("Can't tell" stays: the speaker is
    /// auto-named and still listed as dismissed).
    #[test]
    fn re_resolve_keeps_confirmations_rejections_and_dismissals() {
        use broscribe_core::schema::{
            Centroid, Meeting, ProcessingStatus, Source, Speaker, Strength,
        };
        let fx = Fixture::new();
        let ana = Person::new(fx.lib.new_person_id(), "Synthetic Ana");
        let bia = Person::new(fx.lib.new_person_id(), "Synthetic Bia");
        let mut meeting = Meeting::new(
            fx.lib.new_meeting_id().unwrap(),
            Source::Import,
            broscribe_core::schema::now(),
        );
        meeting.processing.status = ProcessingStatus::Done;
        let voices: [(u32, [f32; 2]); 4] = [
            (1, [1.0, 0.0]),
            (2, [1.0, 0.0]),
            (3, [0.99, 0.14]),
            (4, [0.0, 1.0]),
        ];
        for (n, v) in voices {
            let mut s = Speaker::new(SpeakerId::new(n).unwrap(), "t0");
            s.centroid = Some(Centroid::new("fake", &v, 1));
            s.talk_s = 3.0;
            meeting.speakers.push(s);
        }
        let sid = |n| SpeakerId::new(n).unwrap();
        let dismissed_at = broscribe_core::schema::now();
        {
            let s1 = meeting.speaker_mut(sid(1)).unwrap();
            s1.assignment = Assignment::Confirmed;
            s1.person_id = Some(bia.id.clone());
            meeting.speaker_mut(sid(2)).unwrap().rejected_person_ids = vec![ana.id.clone()];
            meeting.speaker_mut(sid(3)).unwrap().review_dismissed_at = Some(dismissed_at);
        }
        let print = |person: &Person, n: u32| {
            let mut vp = ops::weak_sample(&meeting, sid(n), &person.id)
                .unwrap()
                .unwrap();
            vp.source.meeting_id = None;
            vp.strength = Strength::Confirmed;
            vp
        };
        let (ana_print, bia_print) = (print(&ana, 2), print(&bia, 4));
        fx.lib
            .write(|tx| {
                for p in [&ana, &bia] {
                    tx.save_person(p)?;
                }
                tx.upsert_voiceprints(&ana.id, vec![ana_print])?;
                tx.upsert_voiceprints(&bia.id, vec![bia_print])?;
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                Index::rebuild(tx)?;
                Ok(())
            })
            .unwrap();
        let loose = Thresholds {
            auto_name: Some(0.5),
            suggest: Some(0.3),
            margin: Some(0.0),
            merge: None,
            auto_merge: None,
        };
        // A library that never queued a job has no queue lock file; a dry
        // run creates none (nor anything else).
        let queue_lock = fx
            .lib
            .root()
            .join(broscribe_core::paths::QUEUE_DIR)
            .join(broscribe_core::paths::LOCK_FILE);
        assert!(!queue_lock.exists());
        let before = files(fx.lib.root());
        let dry = plan(&fx.lib, "fake", Some(&loose)).unwrap();
        assert_eq!(ReresolveCounts::total(&dry).auto_named, 2);
        assert_eq!(files(fx.lib.root()), before, "a dry run creates nothing");
        assert!(!queue_lock.exists());
        let applied = re_resolve(&fx.lib, "fake", Some(&loose)).unwrap();
        assert_eq!(
            ReresolveCounts::total(&applied),
            ReresolveCounts {
                auto_named: 2,
                ..ReresolveCounts::default()
            }
        );
        let saved = fx.lib.load_meeting(&meeting.id).unwrap();
        let s = |n| saved.speaker(sid(n)).unwrap();
        assert_eq!(s(1).assignment, Assignment::Confirmed);
        assert_eq!(s(1).person_id.as_ref(), Some(&bia.id));
        assert_ne!(s(2).person_id.as_ref(), Some(&ana.id));
        assert_eq!(s(2).rejected_person_ids, vec![ana.id.clone()]);
        assert_eq!(s(3).assignment, Assignment::Auto);
        assert_eq!(s(3).person_id.as_ref(), Some(&ana.id));
        assert_eq!(s(3).review_dismissed_at, Some(dismissed_at));
        assert_eq!(s(4).person_id.as_ref(), Some(&bia.id));
        let dismissed = ops::dismissed_speakers(&fx.lib).unwrap();
        assert_eq!(dismissed.len(), 1);
        assert_eq!(dismissed[0].speaker_id, sid(3));
        assert_eq!(dismissed[0].assignment, Assignment::Auto);
    }

    /// The dry run lists meetings in the order the real run reports them
    /// (meeting ID), even when folder order (start time) differs; a queued
    /// search-vector refresh makes neither skip a meeting.
    #[test]
    fn dry_run_and_real_run_list_meetings_in_the_same_order() {
        use broscribe_core::schema::{
            Centroid, Meeting, ProcessingStatus, Source, Speaker, Strength,
        };
        let fx = Fixture::new();
        let ana = Person::new(fx.lib.new_person_id(), "Synthetic Ana");
        let sid = SpeakerId::new(1).unwrap();
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap();
        // The larger ID starts first, so its folder sorts first.
        let meetings: Vec<Meeting> = [
            ("m-0000b2", "2026-11-02T10:00:00+00:00"),
            ("m-0000a1", "2026-11-03T10:00:00+00:00"),
        ]
        .into_iter()
        .map(|(id, started)| {
            let mut m = Meeting::new(MeetingId::parse(id).unwrap(), Source::Import, at(started));
            m.processing.status = ProcessingStatus::Done;
            let mut s = Speaker::new(sid, "t0");
            s.centroid = Some(Centroid::new("fake", &[1.0, 0.0], 1));
            s.talk_s = 3.0;
            m.speakers.push(s);
            m
        })
        .collect();
        let mut print = ops::weak_sample(&meetings[0], sid, &ana.id)
            .unwrap()
            .unwrap();
        print.source.meeting_id = None;
        print.strength = Strength::Confirmed;
        fx.lib
            .write(|tx| {
                tx.save_person(&ana)?;
                tx.upsert_voiceprints(&ana.id, vec![print])?;
                for m in &meetings {
                    tx.create_meeting_dir(m)?;
                    tx.save_meeting(m)?;
                }
                Index::rebuild(tx)?;
                let (key, input) = crate::worker::vectors_job_input(&meetings[1].id);
                broscribe_core::queue::Queue::open(tx.root())?
                    .enqueue(tx, broscribe_core::queue::VECTORS_KIND, &key, input)
                    .map(|_| ())
            })
            .unwrap();
        let folders: Vec<MeetingId> = fx
            .lib
            .list_meeting_dirs()
            .unwrap()
            .iter()
            .map(|d| fx.lib.load_meeting_at(d).unwrap().id)
            .collect();
        assert_eq!(folders, [meetings[0].id.clone(), meetings[1].id.clone()]);
        let loose = Thresholds {
            auto_name: Some(0.5),
            suggest: Some(0.3),
            margin: Some(0.0),
            merge: None,
            auto_merge: None,
        };
        let listed = |plans: &[MeetingPlan]| {
            plans
                .iter()
                .map(|p| (p.meeting_id.clone(), p.counts))
                .collect::<Vec<_>>()
        };
        let dry = plan(&fx.lib, "fake", Some(&loose)).unwrap();
        let real = re_resolve(&fx.lib, "fake", Some(&loose)).unwrap();
        let named = ReresolveCounts {
            auto_named: 1,
            ..ReresolveCounts::default()
        };
        assert_eq!(
            listed(&dry),
            [
                (meetings[1].id.clone(), named),
                (meetings[0].id.clone(), named)
            ]
        );
        assert_eq!(listed(&real), listed(&dry));
    }

    /// A re-resolve run crashed after saving a demotion in `meeting.json`
    /// but before removing the demoted name's weak sample. The meeting now
    /// plans as unchanged, yet the next run repairs it from the recorded
    /// changes: the sample goes, the repair is counted once and the
    /// checkpoint is removed.
    #[test]
    fn an_interrupted_publication_is_repaired_though_the_meeting_looks_unchanged() {
        use broscribe_core::learning::InFlight;
        use broscribe_core::ops::{SpeakerChange, SpeakerState};
        use broscribe_core::schema::{
            Centroid, Meeting, ProcessingStatus, Source, Speaker, Strength,
        };
        let fx = Fixture::new();
        let ana = Person::new(fx.lib.new_person_id(), "Synthetic Ana");
        let sid = SpeakerId::new(1).unwrap();
        let mut meeting = Meeting::new(
            fx.lib.new_meeting_id().unwrap(),
            Source::Import,
            broscribe_core::schema::now(),
        );
        meeting.processing.status = ProcessingStatus::Done;
        let mut speaker = Speaker::new(sid, "t0");
        speaker.centroid = Some(Centroid::new("fake", &[1.0, 0.0], 1));
        speaker.talk_s = 3.0;
        meeting.speakers.push(speaker);
        // The weak sample the earlier automatic name left; the demotion
        // itself already reached the (unassigned) meeting.
        let leftover = ops::weak_sample(&meeting, sid, &ana.id).unwrap().unwrap();
        assert_eq!(leftover.strength, Strength::Weak);
        let flight = InFlight {
            meeting_id: meeting.id.clone(),
            source: false,
            changes: vec![SpeakerChange {
                meeting_id: meeting.id.clone(),
                speaker_id: sid,
                before: SpeakerState {
                    assignment: Assignment::Auto,
                    person_id: Some(ana.id.clone()),
                    score: Some(0.95),
                },
                after: SpeakerState {
                    assignment: Assignment::None,
                    person_id: None,
                    score: None,
                },
            }],
        };
        let checkpoint = fx.lib.root().join(super::RERESOLVE_IN_FLIGHT);
        fx.lib
            .write(|tx| {
                tx.save_person(&ana)?;
                tx.upsert_voiceprints(&ana.id, vec![leftover])?;
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                Index::rebuild(tx)?;
                broscribe_core::fsutil::write_json_atomic(&checkpoint, &flight)
            })
            .unwrap();
        // Without the checkpoint the meeting plans as unchanged.
        assert!(plan(&fx.lib, "fake", Some(&strict())).unwrap().is_empty());
        let real = re_resolve(&fx.lib, "fake", Some(&strict())).unwrap();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].meeting_id, meeting.id);
        assert_eq!(
            real[0].counts,
            ReresolveCounts {
                demoted: 1,
                cleared: 1,
                ..ReresolveCounts::default()
            }
        );
        assert!(!checkpoint.exists());
        let weak_left = fx
            .lib
            .load_voiceprints(&ana.id)
            .map(|v| v.embeddings)
            .unwrap_or_default()
            .into_iter()
            .filter(|v| v.source.meeting_id.as_ref() == Some(&meeting.id))
            .count();
        assert_eq!(weak_left, 0, "the demoted name's sample is gone");
        // Counted once: the next run has nothing to repair or change.
        assert!(
            re_resolve(&fx.lib, "fake", Some(&strict()))
                .unwrap()
                .is_empty()
        );
    }
}
