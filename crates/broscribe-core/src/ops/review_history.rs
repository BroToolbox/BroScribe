//! Session history for explicit identity decisions. Receipts stay in memory
//! and carry only changed identity fields, never transcript text or audio.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    path::Path,
};

use crate::{
    live_identity::{self, LiveAttribution, LiveSidecar},
    queue::{self, Queue},
    schema::{Participant, SpeakerGroup, UtterancePerson},
};

use super::{
    person_merge::{self, MergePeople, PersonPair},
    *,
};

type LineMaps = Vec<(String, BTreeMap<String, PersonId>)>;

#[derive(Clone)]
struct Change<T> {
    before: T,
    after: T,
}

impl<T: Clone + PartialEq> Change<T> {
    fn between(before: T, after: T) -> Option<Self> {
        (before != after).then_some(Self { before, after })
    }

    fn sides(&self, redo: bool) -> (&T, &T) {
        if redo {
            (&self.before, &self.after)
        } else {
            (&self.after, &self.before)
        }
    }

    fn replace(&self, current: &mut T, redo: bool, retry: bool) -> Result<()> {
        let (expected, desired) = self.sides(redo);
        if current != expected && !(retry && current == desired) {
            return Err(changed());
        }
        current.clone_from(desired);
        Ok(())
    }
}

fn replace<T: Clone + PartialEq>(
    change: &Option<Change<T>>,
    value: &mut T,
    redo: bool,
    retry: bool,
) -> Result<()> {
    if let Some(change) = change {
        change.replace(value, redo, retry)?;
    }
    Ok(())
}

fn changed() -> Error {
    Error::Service {
        code: "identity_edit_changed",
        message: "The identity data changed after this decision.".into(),
    }
}

fn missing_as_changed(error: Error) -> Error {
    match error {
        Error::NotFound(_) => changed(),
        error => error,
    }
}

fn optional_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

fn speaker_identity(speaker: &Speaker) -> Speaker {
    let mut identity = speaker.clone();
    identity.centroid = None;
    identity
}

fn line_maps(meeting: &Meeting) -> LineMaps {
    meeting
        .generated()
        .map(|item| (item.revision().to_owned(), item.line_people().clone()))
        .collect()
}

type Title = (
    Option<String>,
    Option<schema::TitleSource>,
    Option<DateTime<FixedOffset>>,
);
type Tags = (Vec<String>, Vec<String>);

fn saved_title(meeting: &Meeting) -> Title {
    (
        meeting.title.clone(),
        meeting.title_source,
        meeting.title_set_at,
    )
}

fn saved_tags(meeting: &Meeting) -> Tags {
    (meeting.tags.clone(), meeting.tags_removed.clone())
}

#[derive(Clone)]
struct MeetingState {
    title: Title,
    tags: Tags,
    actions: Option<(DateTime<FixedOffset>, Vec<schema::Action>)>,
    namespace: String,
    speakers: Vec<Speaker>,
    groups: Vec<SpeakerGroup>,
    separated: Vec<[SpeakerId; 2]>,
    participants: Vec<Participant>,
    lines: Vec<UtterancePerson>,
    live: Vec<LiveAttribution>,
    generated: LineMaps,
}

impl MeetingState {
    fn of(meeting: &Meeting) -> Self {
        Self {
            title: saved_title(meeting),
            tags: saved_tags(meeting),
            actions: meeting
                .enrichment
                .as_ref()
                .map(|e| (e.created_at, e.actions.clone())),
            namespace: meeting.speaker_namespace.clone(),
            speakers: meeting.speakers.iter().map(speaker_identity).collect(),
            groups: meeting.speaker_groups.clone(),
            separated: meeting.not_same.clone(),
            participants: meeting.participants.clone(),
            lines: meeting.utterance_people.clone(),
            live: meeting.live_attributions.clone(),
            generated: line_maps(meeting),
        }
    }
}

#[derive(Clone)]
struct MeetingChange {
    title: Option<Change<Title>>,
    tags: Option<Change<Tags>>,
    actions: Vec<Change<schema::Action>>,
    actions_revision: Option<DateTime<FixedOffset>>,
    namespace: String,
    speakers: Vec<Change<Speaker>>,
    groups: Option<Change<Vec<SpeakerGroup>>>,
    separated: Option<Change<Vec<[SpeakerId; 2]>>>,
    participants: Option<Change<Vec<Participant>>>,
    lines: Option<Change<Vec<UtterancePerson>>>,
    live: Option<Change<Vec<LiveAttribution>>>,
    generated: Option<Change<LineMaps>>,
}

impl MeetingChange {
    fn between(before: MeetingState, after: MeetingState) -> Option<Self> {
        let (actions, actions_revision) = match (before.actions, after.actions) {
            (Some((old_at, old)), Some((new_at, new))) if old_at == new_at => {
                let actions = old
                    .into_iter()
                    .zip(new)
                    .filter(|(old, new)| old.id == new.id)
                    .filter_map(|(old, new)| Change::between(old, new))
                    .collect::<Vec<_>>();
                let revision = (!actions.is_empty()).then_some(old_at);
                (actions, revision)
            }
            _ => (Vec::new(), None),
        };
        let change = Self {
            title: Change::between(before.title, after.title),
            tags: Change::between(before.tags, after.tags),
            actions,
            actions_revision,
            namespace: before.namespace,
            speakers: before
                .speakers
                .into_iter()
                .zip(after.speakers)
                .filter_map(|(before, after)| Change::between(before, after))
                .collect(),
            groups: Change::between(before.groups, after.groups),
            separated: Change::between(before.separated, after.separated),
            participants: Change::between(before.participants, after.participants),
            lines: Change::between(before.lines, after.lines),
            live: Change::between(before.live, after.live),
            generated: Change::between(before.generated, after.generated),
        };
        (change.title.is_some()
            || change.tags.is_some()
            || !change.actions.is_empty()
            || !change.speakers.is_empty()
            || change.groups.is_some()
            || change.separated.is_some()
            || change.participants.is_some()
            || change.lines.is_some()
            || change.live.is_some()
            || change.generated.is_some())
        .then_some(change)
    }

    fn affects_identity(&self) -> bool {
        !self.speakers.is_empty()
            || self.groups.is_some()
            || self.separated.is_some()
            || self.participants.is_some()
            || self.lines.is_some()
            || self.live.is_some()
    }

    fn apply(&self, meeting: &mut Meeting, redo: bool, retry: bool) -> Result<()> {
        if self.namespace != meeting.speaker_namespace {
            return Err(changed());
        }
        if let Some(change) = &self.title {
            let mut title = saved_title(meeting);
            change.replace(&mut title, redo, retry)?;
            (meeting.title, meeting.title_source, meeting.title_set_at) = title;
        }
        if let Some(change) = &self.tags {
            let mut tags = saved_tags(meeting);
            change.replace(&mut tags, redo, retry)?;
            (meeting.tags, meeting.tags_removed) = tags;
        }
        if let Some(revision) = self.actions_revision {
            let e = meeting
                .enrichment
                .as_mut()
                .filter(|e| e.created_at == revision)
                .ok_or_else(changed)?;
            for change in &self.actions {
                let action = e
                    .actions
                    .iter_mut()
                    .find(|a| a.id == change.before.id)
                    .ok_or_else(changed)?;
                change.replace(action, redo, retry)?;
            }
        }
        for change in &self.speakers {
            let speaker = meeting.speaker_mut(change.before.id).ok_or_else(changed)?;
            let mut identity = speaker_identity(speaker);
            let keep_override = identity.live_speaker_override;
            let (expected, desired) = change.sides(redo);
            let preserve_explicit_edit = |mut state: Speaker| {
                if keep_override {
                    state.live_speaker_override = true;
                    state.live_promotion = None;
                }
                state
            };
            let expected = preserve_explicit_edit(expected.clone());
            let desired = preserve_explicit_edit(desired.clone());
            if identity != expected && !(retry && identity == desired) {
                return Err(changed());
            }
            identity = desired;
            identity.centroid = speaker.centroid.take();
            *speaker = identity;
        }
        replace(&self.groups, &mut meeting.speaker_groups, redo, retry)?;
        replace(&self.separated, &mut meeting.not_same, redo, retry)?;
        replace(&self.participants, &mut meeting.participants, redo, retry)?;
        replace(&self.lines, &mut meeting.utterance_people, redo, retry)?;
        replace(&self.live, &mut meeting.live_attributions, redo, retry)?;
        if let Some(change) = &self.generated {
            let mut maps = line_maps(meeting);
            change.replace(&mut maps, redo, retry)?;
            for (generated, (_, map)) in meeting.generated_mut().zip(maps) {
                *generated.line_people_mut() = map;
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ProfileState {
    details: Option<(Person, String)>,
    prints: Voiceprints,
}

impl ProfileState {
    fn load(lib: &Library, id: &PersonId) -> Result<Self> {
        let details = match lib.load_person_with_notes(id) {
            Ok((mut person, notes)) => {
                person.hue = Some(person.hue());
                Some((person, notes))
            }
            Err(Error::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        let prints = lib.load_voiceprints(id)?;
        if prints.person_id != *id {
            return Err(Error::InvalidInput(
                "Voice samples belong to a different profile.".into(),
            ));
        }
        Ok(Self { details, prints })
    }
}

#[derive(Clone)]
struct ProfileChange {
    details: Option<Change<Option<(Person, String)>>>,
    prints: BTreeMap<String, Change<Option<Voiceprint>>>,
    known_prints: BTreeSet<String>,
}

impl ProfileChange {
    fn between(before: ProfileState, after: ProfileState, merge: bool) -> Self {
        let old: BTreeMap<_, _> = before
            .prints
            .embeddings
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect();
        let mut new: BTreeMap<_, _> = after
            .prints
            .embeddings
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect();
        let known_prints = new.keys().cloned().collect();
        let prints = old
            .into_iter()
            .filter_map(|(id, print)| {
                let change = Change::between(Some(print), new.remove(&id))?;
                Some((id, change))
            })
            .collect::<BTreeMap<_, _>>();
        let prints = prints
            .into_iter()
            .chain(new.into_iter().map(|(id, print)| {
                (
                    id,
                    Change {
                        before: None,
                        after: Some(print),
                    },
                )
            }))
            .collect();
        Self {
            // An identification can create a profile. Keep the name in People,
            // where later independent decisions might already refer to it.
            details: if merge || before.details.is_some() {
                Change::between(before.details, after.details)
            } else {
                None
            },
            prints,
            known_prints,
        }
    }

    fn apply(&self, state: &mut ProfileState, redo: bool, retry: bool) -> Result<()> {
        let exists = state.details.is_some();
        replace(&self.details, &mut state.details, redo, retry)?;
        if !exists && self.details.is_none() {
            return Err(changed());
        }
        for (id, change) in &self.prints {
            let at = state
                .prints
                .embeddings
                .iter()
                .position(|print| &print.id == id);
            let mut value = at.map(|at| state.prints.embeddings[at].clone());
            change.replace(&mut value, redo, retry)?;
            match (at, value) {
                (Some(at), Some(print)) => state.prints.embeddings[at] = print,
                (Some(at), None) => {
                    state.prints.embeddings.remove(at);
                }
                (None, Some(print)) => state.prints.embeddings.push(print),
                (None, None) => {}
            }
        }
        // A merged-away profile must never lose samples learned since Undo.
        if state.details.is_none() && !state.prints.embeddings.is_empty() {
            return Err(changed());
        }
        Ok(())
    }
}

#[derive(Clone)]
struct EnrollmentCopy {
    name: std::ffi::OsString,
    hash: String,
    already_in_keep: bool,
}

#[derive(Clone)]
struct MergeFiles {
    keep: PersonId,
    source: PersonId,
    enrollment: Vec<EnrollmentCopy>,
}

impl MergeFiles {
    fn load(lib: &Library, request: &MergePeople) -> Result<Self> {
        let source = lib.person_dir(&request.source).join("enrollment");
        let keep = lib.person_dir(&request.keep).join("enrollment");
        let mut enrollment = Vec::new();
        match fs::read_dir(&source) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| Error::io_at(&source, e))?;
                    let path = entry.path();
                    if entry
                        .file_type()
                        .map_err(|e| Error::io_at(&path, e))?
                        .is_file()
                    {
                        enrollment.push(EnrollmentCopy {
                            name: entry.file_name(),
                            hash: schema::sha256_hex(
                                &fs::read(&path).map_err(|e| Error::io_at(&path, e))?,
                            ),
                            already_in_keep: keep.join(entry.file_name()).is_file(),
                        });
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io_at(&source, e)),
        }
        Ok(Self {
            keep: request.keep.clone(),
            source: request.source.clone(),
            enrollment,
        })
    }

    fn stage(
        &self,
        lib: &Library,
        redo: bool,
        retry: bool,
    ) -> Result<Vec<(std::path::PathBuf, Vec<u8>)>> {
        let keep = lib.person_dir(&self.keep).join("enrollment");
        let source = lib.person_dir(&self.source).join("enrollment");
        let mut copies = Vec::new();
        for file in &self.enrollment {
            let kept = keep.join(&file.name);
            let original = source.join(&file.name);
            let kept_bytes = optional_bytes(&kept)?;
            let source_bytes = optional_bytes(&original)?;
            let expected_keep = !redo || file.already_in_keep;
            if (kept_bytes.is_some() != expected_keep || source_bytes.is_some() != redo) && !retry {
                return Err(changed());
            }
            if kept_bytes
                .iter()
                .chain(&source_bytes)
                .any(|bytes| schema::sha256_hex(bytes) != file.hash)
            {
                return Err(changed());
            }
            let bytes = kept_bytes.or(source_bytes).ok_or_else(changed)?;
            copies.push((if redo { kept } else { original }, bytes));
        }
        if redo
            && source.is_dir()
            && fs::read_dir(&source)
                .map_err(|e| Error::io_at(&source, e))?
                .count()
                != self.enrollment.len()
        {
            return Err(changed());
        }
        Ok(copies)
    }

    fn clean_keep(&self, lib: &Library) -> Result<()> {
        let keep = lib.person_dir(&self.keep).join("enrollment");
        for file in self.enrollment.iter().filter(|file| !file.already_in_keep) {
            let path = keep.join(&file.name);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io_at(&path, e)),
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct ReviewCapture {
    pub(super) incomplete_baseline: bool,
    meetings: BTreeMap<MeetingId, MeetingState>,
    people: BTreeMap<PersonId, ProfileState>,
    merge: Option<MergeFiles>,
    sidecars: BTreeMap<String, LiveSidecar>,
    exclusions: Option<BTreeSet<PersonPair>>,
}

impl ReviewCapture {
    pub(crate) fn meeting(&mut self, lib: &Library, id: &MeetingId, dir: &Path) -> Result<()> {
        if !self.meetings.contains_key(id) {
            self.meetings
                .insert(id.clone(), MeetingState::of(&lib.load_meeting_at(dir)?));
        }
        Ok(())
    }

    pub(crate) fn person(&mut self, lib: &Library, id: &PersonId) -> Result<()> {
        if !self.people.contains_key(id) {
            self.people.insert(id.clone(), ProfileState::load(lib, id)?);
        }
        Ok(())
    }

    pub(super) fn exclusions(&mut self, lib: &Library) -> Result<()> {
        self.exclusions = Some(load_exclusions(lib)?);
        Ok(())
    }

    pub(super) fn merge(&mut self, lib: &Library, request: &MergePeople) -> Result<()> {
        self.merge = Some(MergeFiles::load(lib, request)?);
        self.exclusions(lib)?;
        for session in live_identity::list_sidecars(lib.root())? {
            if let Some(sidecar) = live_identity::read_sidecar(lib.root(), &session)?
                && sidecar
                    .decisions
                    .iter()
                    .any(|decision| decision.person_id.as_ref() == Some(&request.source))
            {
                self.sidecars.insert(session, sidecar);
            }
        }
        Ok(())
    }

    fn finish(self, lib: &Library, model: &str) -> Result<Option<ReviewEdit>> {
        if self.incomplete_baseline {
            return Ok(None);
        }
        let mut meetings = BTreeMap::new();
        let mut people = BTreeMap::new();
        let mut affected = BTreeSet::new();
        let mut sidecars = BTreeMap::new();
        let lookup = lib.meeting_lookup()?;
        for (id, before) in self.meetings {
            if let Some(change) =
                MeetingChange::between(before, MeetingState::of(&lookup.load(&id)?))
            {
                affected.extend(
                    change
                        .speakers
                        .iter()
                        .flat_map(|change| [&change.before, &change.after])
                        .flat_map(|speaker| {
                            speaker
                                .person_id
                                .iter()
                                .chain(speaker.suggestion.iter().map(|s| &s.person_id))
                                .chain(&speaker.rejected_person_ids)
                                .cloned()
                        }),
                );
                meetings.insert(id, change);
            }
        }
        for (id, before) in self.people {
            let change =
                ProfileChange::between(before, ProfileState::load(lib, &id)?, self.merge.is_some());
            affected.insert(id.clone());
            people.insert(id, change);
        }
        for (session, before) in self.sidecars {
            let after = live_identity::read_sidecar(lib.root(), &session)?.ok_or_else(changed)?;
            if let Some(change) = Change::between(before, after) {
                sidecars.insert(session, change);
            }
        }
        let exclusions = self
            .exclusions
            .map(|before| -> Result<_> { Ok(Change::between(before, load_exclusions(lib)?)) })
            .transpose()?
            .flatten();
        if let Some(exclusions) = &exclusions {
            affected.extend(
                exclusions
                    .before
                    .symmetric_difference(&exclusions.after)
                    .flat_map(|pair| [pair.0.clone(), pair.1.clone()]),
            );
        }
        if meetings.is_empty()
            && people
                .values()
                .all(|p| p.details.is_none() && p.prints.is_empty())
            && exclusions.is_none()
            && sidecars.is_empty()
        {
            return Ok(None);
        }
        let dependencies = people
            .values()
            .flat_map(|person| person.prints.values())
            .flat_map(|change| change.before.iter().chain(&change.after))
            .filter_map(|print| print.source.meeting_id.clone())
            .filter(|id| !meetings.contains_key(id))
            .collect::<BTreeSet<_>>();
        let mut sample_meetings = BTreeMap::new();
        for id in dependencies {
            match lookup.load(&id) {
                Ok(meeting) => {
                    sample_meetings.insert(id, meeting.speaker_namespace);
                }
                Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let tombstones = affected
            .into_iter()
            .map(|id| {
                Ok((
                    id.clone(),
                    optional_bytes(&tombstone_path(lib.root(), id.as_str()))?,
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Some(ReviewEdit {
            meetings,
            people,
            sidecars,
            exclusions,
            tombstones,
            sample_meetings,
            merge: self.merge,
            model: model.to_owned(),
            retrying: false,
        }))
    }
}

fn load_exclusions(lib: &Library) -> Result<BTreeSet<PersonPair>> {
    Ok(person_merge::read_optional(lib, person_merge::SEPARATE)?.unwrap_or_default())
}

/// A completed decision that can be reversed under the library writer lock.
#[derive(Clone)]
pub struct ReviewEdit {
    meetings: BTreeMap<MeetingId, MeetingChange>,
    people: BTreeMap<PersonId, ProfileChange>,
    sidecars: BTreeMap<String, Change<LiveSidecar>>,
    exclusions: Option<Change<BTreeSet<PersonPair>>>,
    tombstones: BTreeMap<PersonId, Option<Vec<u8>>>,
    sample_meetings: BTreeMap<MeetingId, String>,
    merge: Option<MergeFiles>,
    model: String,
    retrying: bool,
}

/// Capture the effects of a decision in the same writer lock as its writes.
/// An applied decision still succeeds if its undo receipt cannot be built.
pub fn record_review_edit<T>(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    model: &str,
    work: impl FnOnce(&mut WriteTx<'_>, &mut Index) -> Result<T>,
) -> Result<(T, Option<ReviewEdit>)> {
    debug_assert!(tx.review_edit.is_none());
    tx.review_edit = Some(ReviewCapture::default());
    let result = work(tx, index);
    let capture = tx
        .review_edit
        .take()
        .expect("the decision owns its capture");
    let outcome = result?;
    let edit = match capture.finish(tx.library(), model) {
        Ok(edit) => edit,
        Err(e) => {
            tracing::warn!(code = e.code(), "review decision applied; undo unavailable");
            None
        }
    };
    Ok((outcome, edit))
}

impl ReviewEdit {
    /// Replay a decision once its complete preflight has passed. If an I/O
    /// error interrupts publication, the receipt can finish that same write.
    pub fn replay(&mut self, lib: &Library, redo: bool) -> Result<()> {
        with_index(lib, |tx, index| self.replay_locked(tx, index, redo))
    }

    fn replay_locked(&mut self, tx: &mut WriteTx<'_>, index: &mut Index, redo: bool) -> Result<()> {
        let lib = tx.library().clone();
        if person_merge::pending_person_merge(&lib)?.is_some() {
            return Err(Error::LibraryBusy(
                "a profile merge is waiting to finish".into(),
            ));
        }
        for (person, stamp) in &self.tombstones {
            if optional_bytes(&tombstone_path(lib.root(), person.as_str()))? != *stamp {
                return Err(changed());
            }
        }
        let lookup = lib.meeting_lookup()?;
        for (id, namespace) in &self.sample_meetings {
            if lookup
                .load(id)
                .map_err(missing_as_changed)?
                .speaker_namespace
                != *namespace
            {
                return Err(changed());
            }
        }
        let queue = Queue::open(tx.root())?;
        let mut originals = BTreeMap::new();
        let mut meetings = BTreeMap::new();
        for (id, change) in &self.meetings {
            let mut meeting = lookup.load(id).map_err(missing_as_changed)?;
            queue
                .check_meeting_idle_except(&meeting, &[queue::CONFIRM_KIND, queue::VECTORS_KIND])?;
            originals.insert(id.clone(), meeting.clone());
            change.apply(&mut meeting, redo, self.retrying)?;
            check_overlay(&meeting, Some(&caption_proven(&lib, &meeting)?))
                .map_err(|_| changed())?;
            meetings.insert(id.clone(), meeting);
        }
        // The confirmation worker may have learned from a fresh sample of
        // the exact speaker this edit named. Include those samples in Undo.
        let mut people = self.people.clone();
        if !redo && self.merge.is_none() && !self.retrying {
            for (id, change) in &self.meetings {
                let meeting = &originals[id];
                for change in &change.speakers {
                    let Some(person) = change.after.named_person() else {
                        continue;
                    };
                    if change.after.assignment != Assignment::Confirmed
                        || change.before.assignment == Assignment::Confirmed
                            && change.before.person_id == change.after.person_id
                    {
                        continue;
                    }
                    let Some(profile) = people.get_mut(person) else {
                        continue;
                    };
                    for print in lib.load_voiceprints(person)?.embeddings {
                        if print.strength == Strength::Confirmed
                            && print.source.kind == VoiceprintKind::Confirmation
                            && sample_of_speaker(&print, meeting, change.after.id, None)
                        {
                            if let Some(sample) = profile.prints.get_mut(&print.id) {
                                sample.after = Some(print);
                            } else if !profile.known_prints.contains(&print.id) {
                                profile.prints.insert(
                                    print.id.clone(),
                                    Change {
                                        before: None,
                                        after: Some(print),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
        let mut profiles = Vec::new();
        let mut models = BTreeSet::new();
        for (id, change) in &people {
            let mut profile = ProfileState::load(&lib, id)?;
            for change in change.prints.values() {
                models.extend(
                    change
                        .before
                        .iter()
                        .chain(&change.after)
                        .map(|print| print.model.clone()),
                );
            }
            change.apply(&mut profile, redo, self.retrying)?;
            profiles.push(profile);
        }
        let copies = self
            .merge
            .as_ref()
            .map(|merge| merge.stage(&lib, redo, self.retrying))
            .transpose()?
            .unwrap_or_default();
        let mut sidecars = Vec::new();
        for (session, change) in &self.sidecars {
            let mut current =
                live_identity::read_sidecar(lib.root(), session)?.ok_or_else(changed)?;
            change.replace(&mut current, redo, self.retrying)?;
            sidecars.push(current);
        }
        let mut exclusions = load_exclusions(&lib)?;
        replace(&self.exclusions, &mut exclusions, redo, self.retrying)?;
        // Redo removes a profile again. Refuse when a newer action linked
        // it to a meeting or live turn outside the original merge.
        if let Some(merge) = &self.merge
            && redo
        {
            for dir in lib.list_meeting_dirs()? {
                let original = lib.load_meeting_at(&dir)?;
                let meeting = meetings.get(&original.id).unwrap_or(&original);
                if meeting_references_person(meeting, &merge.source)
                    || meeting
                        .live_attributions
                        .iter()
                        .any(|turn| turn.person_id.as_ref() == Some(&merge.source))
                {
                    return Err(changed());
                }
            }
            for session in live_identity::list_sidecars(lib.root())? {
                if !self.sidecars.contains_key(&session)
                    && live_identity::read_sidecar(lib.root(), &session)?.is_some_and(|sidecar| {
                        sidecar
                            .decisions
                            .iter()
                            .any(|turn| turn.person_id.as_ref() == Some(&merge.source))
                    })
                {
                    return Err(changed());
                }
            }
        }

        self.people = people;
        self.retrying = true;
        if self.meetings.values().any(MeetingChange::affects_identity) || !models.is_empty() {
            models.insert(self.model.clone());
            learning::request_all(tx, models)?;
        }
        for profile in profiles.iter().filter(|p| p.details.is_some()) {
            let (person, notes) = profile.details.as_ref().expect("existing profile");
            tx.save_person_with_notes(person, notes)?;
            tx.save_voiceprints(&profile.prints)?;
            index.upsert_person(tx, person)?;
        }
        for (path, bytes) in copies {
            fsutil::create_dir_durable(path.parent().expect("enrollment file has a parent"))?;
            fsutil::write_atomic_with_mode(&path, &bytes, Some(0o600))?;
        }
        let names = self
            .merge
            .is_some()
            .then(|| person_names(&lib))
            .transpose()?;
        for mut meeting in meetings.into_values() {
            if let Some(names) = &names {
                live_identity::refresh_review_notes(&mut meeting, names);
            }
            let change = &self.meetings[&meeting.id];
            if change.affects_identity() {
                refresh_identity_conflict_notes(&lib, &mut meeting)?;
                meeting.set_generated_stale();
            }
            save_and_index(tx, index, &mut meeting)?;
            // Reindex search windows when the shown speaker or line attribution changed.
            if change.groups.is_some() || change.lines.is_some() {
                queue.enqueue(
                    tx,
                    queue::VECTORS_KIND,
                    meeting.id.as_str(),
                    serde_json::json!({ "meeting_id": meeting.id }),
                )?;
            }
        }
        for sidecar in sidecars {
            live_identity::write_sidecar(tx.root(), &sidecar)?;
        }
        if self.exclusions.is_some() {
            fsutil::write_json_atomic(
                &person_merge::file(&lib, person_merge::SEPARATE),
                &exclusions,
            )?;
        }
        if let Some(merge) = &self.merge {
            if redo {
                person_merge::save_merge_redirect(tx, &merge.source, &merge.keep)?;
            } else {
                person_merge::remove_merge_redirect(tx, &merge.source)?;
            }
        }
        for profile in profiles.into_iter().filter(|p| p.details.is_none()) {
            let id = profile.prints.person_id;
            match tx.delete_person_dir(&id) {
                Ok(()) | Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
            index.remove_person(tx, &id)?;
        }
        if let Some(merge) = &self.merge
            && !redo
        {
            merge.clean_keep(&lib)?;
        }
        self.retrying = false;
        Ok(())
    }
}
