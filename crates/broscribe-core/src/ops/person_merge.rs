//! Review and consolidate person records across the library.
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{
    identity_review::{review_min, review_revision},
    person_names, save_and_index,
};
use crate::{
    Error, Library, MeetingId, PersonId, Result, WriteTx,
    config::Thresholds,
    fsutil, identity,
    index::Index,
    learning, live_identity,
    schema::{
        Meeting, Person, Strength, VoiceprintOrigin, VoiceprintSource, Voiceprints, voiceprint_id,
    },
};

const PENDING: &str = ".merge-pending.json";
const REDIRECTS: &str = ".merge-redirects";
pub(super) const SEPARATE: &str = ".merge-exclusions.json";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PersonPair(pub PersonId, pub PersonId);

impl PersonPair {
    pub fn new(a: PersonId, b: PersonId) -> Self {
        if a <= b { Self(a, b) } else { Self(b, a) }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersonDuplicate {
    pub pair: PersonPair,
    pub names: [String; 2],
    pub samples: [Option<VoiceprintSource>; 2],
    pub score: f32,
    pub meeting_count: usize,
    pub keep: PersonId,
    pub keep_locked: bool,
    pub revision: String,
}

pub(super) fn file(lib: &Library, name: &str) -> PathBuf {
    lib.root().join(crate::paths::PEOPLE_DIR).join(name)
}

pub(super) fn read_optional<T: serde::de::DeserializeOwned>(
    lib: &Library,
    name: &str,
) -> Result<Option<T>> {
    match fsutil::read_json(&file(lib, name)) {
        Ok(value) => Ok(Some(value)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

struct Profile {
    person: Person,
    meetings: BTreeSet<MeetingId>,
    voice: Vec<f32>,
    sample: Option<VoiceprintSource>,
    revision: String,
}

pub fn person_duplicates(
    lib: &Library,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<Vec<PersonDuplicate>> {
    let index = match Index::open_readonly(lib.root()) {
        Ok(index) => index,
        Err(Error::NotFound(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let excluded: BTreeSet<PersonPair> = read_optional(lib, SEPARATE)?.unwrap_or_default();
    let pending = pending_person_merge(lib)?;
    let mut profiles = Vec::new();
    for entry in index.list_people()? {
        if pending
            .as_ref()
            .is_some_and(|p| p.keep == entry.id || p.source == entry.id)
        {
            continue;
        }
        let (person, notes) = match lib.load_person_with_notes(&entry.id) {
            Ok(profile) => profile,
            Err(Error::NotFound(_)) => continue,
            Err(e) => return Err(e),
        };
        let prints = lib.load_voiceprints(&person.id)?;
        let mut voices = Vec::new();
        let mut sample = None;
        for print in prints
            .embeddings
            .iter()
            .filter(|p| p.model == model && p.strength == Strength::Confirmed)
        {
            if let Some(vector) = identity::usable_vector(&print.vector_b64, print.dim)?
                && vector.iter().any(|x| *x != 0.0)
            {
                voices.push(vector);
                if sample.is_none()
                    && print.source.end > print.source.start
                    && (print.source.meeting_id.is_some()
                        || (print.source.kind == crate::schema::VoiceprintKind::Enrollment
                            && print.source.sha256.is_some()))
                {
                    sample = Some(print.source.clone());
                }
            }
        }
        let Some(voice) = identity::mean_normalized(&voices) else {
            continue;
        };
        if !voice.iter().any(|x| *x != 0.0) {
            continue;
        }
        let meetings = index
            .person_meeting_candidates(&person.id)?
            .into_iter()
            .filter(|candidate| {
                candidate.relations.iter().any(|r| {
                    matches!(
                        r,
                        crate::index::PersonRelation::Speaker | crate::index::PersonRelation::Line
                    )
                })
            })
            .map(|candidate| candidate.meeting_id)
            .collect();
        let revision = review_revision(&(&person, notes, prints))?;
        profiles.push(Profile {
            person,
            meetings,
            voice,
            sample,
            revision,
        });
    }
    profiles.sort_by(|a, b| a.person.id.cmp(&b.person.id));
    let mut pairs = Vec::new();
    let min = review_min(thresholds);
    for (i, a) in profiles.iter().enumerate() {
        for b in &profiles[i + 1..] {
            if a.voice.len() != b.voice.len() || (a.person.is_self && b.person.is_self) {
                continue;
            }
            let score = identity::cosine(&a.voice, &b.voice);
            let pair = PersonPair::new(a.person.id.clone(), b.person.id.clone());
            if score < min || excluded.contains(&pair) {
                continue;
            }
            let keep = if b.person.is_self
                || (!a.person.is_self
                    && ((a.person.needs_review && !b.person.needs_review)
                        || (a.person.needs_review == b.person.needs_review
                            && b.meetings.len() > a.meetings.len())))
            {
                &b.person.id
            } else {
                &a.person.id
            };
            pairs.push(PersonDuplicate {
                pair,
                names: [
                    super::review_person_name(&a.person),
                    super::review_person_name(&b.person),
                ],
                samples: [a.sample.clone(), b.sample.clone()],
                score,
                meeting_count: a.meetings.union(&b.meetings).count(),
                keep: keep.clone(),
                keep_locked: a.person.is_self || b.person.is_self,
                revision: review_revision(&(
                    model,
                    &a.revision,
                    &b.revision,
                    &a.meetings,
                    &b.meetings,
                ))?,
            });
        }
    }
    pairs.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.meeting_count.cmp(&a.meeting_count))
            .then_with(|| a.pair.cmp(&b.pair))
    });
    Ok(pairs)
}

pub fn dismiss_person_duplicate(tx: &mut WriteTx<'_>, pair: &PersonPair) -> Result<()> {
    let lib = tx.library().clone();
    let pair = PersonPair::new(pair.0.clone(), pair.1.clone());
    if pair.0 == pair.1 {
        return Err(Error::InvalidInput("Select two different people.".into()));
    }
    lib.load_person(&pair.0)?;
    lib.load_person(&pair.1)?;
    let mut excluded: BTreeSet<PersonPair> = read_optional(&lib, SEPARATE)?.unwrap_or_default();
    if let Some(edit) = &mut tx.review_edit {
        edit.exclusions(&lib)?;
    }
    excluded.insert(pair);
    fsutil::write_json_atomic(&file(&lib, SEPARATE), &excluded)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergePeople {
    pub keep: PersonId,
    pub source: PersonId,
    pub revision: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingMerge {
    request: MergePeople,
    /// Meetings whose identity JSON may already be written even if the
    /// rendered Markdown or index did not finish.
    affected: BTreeSet<MeetingId>,
    cleanup: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MergePeopleOutcome {
    pub keep: PersonId,
    pub source: PersonId,
    pub meetings_changed: usize,
}

pub fn pending_person_merge(lib: &Library) -> Result<Option<MergePeople>> {
    Ok(read_optional::<PendingMerge>(lib, PENDING)?.map(|merge| merge.request))
}

/// Resolves attendance recorded before a merge. A restored profile (Undo)
/// takes precedence over its saved redirect, and a missing target is unlinked.
pub fn resolve_merged_person(lib: &Library, source: &PersonId) -> Result<Option<Person>> {
    let mut id = source.clone();
    let mut visited = HashSet::new();
    while visited.insert(id.clone()) {
        match lib.load_person(&id) {
            Ok(person) => return Ok(Some(person)),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        id = match fsutil::read_json(&merge_redirect_path(lib.root(), &id)) {
            Ok(keep) => keep,
            Err(Error::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
    }
    Ok(None)
}

pub(crate) fn merge_redirect_path(root: &Path, source: &PersonId) -> PathBuf {
    root.join(crate::paths::PEOPLE_DIR)
        .join(REDIRECTS)
        .join(format!("{source}.json"))
}

/// Write the redirect before deleting the source, under the writer lock.
pub(super) fn save_merge_redirect(
    tx: &WriteTx<'_>,
    source: &PersonId,
    keep: &PersonId,
) -> Result<()> {
    fsutil::create_dir_durable(&file(tx.library(), REDIRECTS))?;
    fsutil::write_json_atomic(&merge_redirect_path(tx.root(), source), keep)
}

/// Undo removes the redirect before the source can be forgotten again.
pub(super) fn remove_merge_redirect(tx: &WriteTx<'_>, source: &PersonId) -> Result<()> {
    let path = merge_redirect_path(tx.root(), source);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io_at(&path, e)),
    }
    match fsutil::sync_dir(&file(tx.library(), REDIRECTS)) {
        Err(Error::Io { source, .. }) if source.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Apply the reviewed merge or resume the exact operation that was interrupted.
pub fn merge_people(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    request: &MergePeople,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<MergePeopleOutcome> {
    let lib = tx.library().clone();
    let path = file(&lib, PENDING);
    let existing = read_optional::<PendingMerge>(&lib, PENDING)?;
    // A resumed merge may already have published part of its original state.
    if let Some(edit) = &mut tx.review_edit {
        edit.incomplete_baseline = existing.is_some();
        if existing.is_none() {
            edit.merge(&lib, request)?;
        }
    }
    let mut pending = if let Some(pending) = existing {
        if pending.request != *request {
            return Err(Error::InvalidInput(
                "Finish the pending profile merge first.".into(),
            ));
        }
        pending
    } else {
        let pair = PersonPair::new(request.keep.clone(), request.source.clone());
        if !person_duplicates(&lib, model, thresholds)?
            .iter()
            .any(|candidate| candidate.pair == pair && candidate.revision == request.revision)
        {
            return Err(Error::InvalidInput(
                "This duplicate suggestion changed. Review it again.".into(),
            ));
        }
        PendingMerge {
            request: request.clone(),
            affected: BTreeSet::new(),
            cleanup: false,
        }
    };

    if !pending.cleanup {
        let (mut keep, keep_notes) = lib.load_person_with_notes(&request.keep)?;
        let (source, source_notes) = lib.load_person_with_notes(&request.source)?;
        if source.is_self {
            return Err(Error::InvalidInput(
                "Keep your own profile when merging these people.".into(),
            ));
        }
        let source_prints = lib.load_voiceprints(&request.source)?;
        let mut prints = lib.load_voiceprints(&request.keep)?;
        if source_prints.person_id != source.id || prints.person_id != keep.id {
            return Err(Error::InvalidInput(
                "Voice samples belong to a different profile.".into(),
            ));
        }
        let enrollment_copies = plan_enrollment_copies(&lib, request)?;
        let source_fence = live_identity::forget_fence(&lib, source.id.as_str())?;
        let keep_fence = live_identity::forget_fence(&lib, keep.id.as_str())?;

        // Prepare every canonical meeting before making changes. An
        // unreadable meeting may hold a reference to the source profile.
        let mut meetings = Vec::new();
        for dir in lib.list_meeting_dirs()? {
            let mut meeting = lib.load_meeting_at(&dir)?;
            let changed =
                remap_meeting(&mut meeting, &source.id, &keep, &source_fence, &keep_fence);
            if changed {
                super::check_overlay(&meeting, None).map_err(|e| {
                    Error::InvalidInput(format!(
                        "Review the conflicting speaker decisions in {} before merging: {e}",
                        meeting.id
                    ))
                })?;
                pending.affected.insert(meeting.id.clone());
            }
            if pending.affected.contains(&meeting.id) {
                meetings.push(meeting);
            }
        }
        fsutil::write_json_atomic(&path, &pending)?;

        append_unique(
            &mut keep.aliases,
            std::iter::once(source.name).chain(source.aliases),
            &keep.name,
        );
        append_unique(&mut keep.emails, source.emails, "");
        keep.needs_review &= source.needs_review;
        let notes = if source_notes.trim().is_empty() || keep_notes.ends_with(&source_notes) {
            keep_notes
        } else if keep_notes.trim().is_empty() {
            source_notes
        } else {
            format!("{}\n\n{}", keep_notes.trim_end(), source_notes)
        };
        merge_voiceprints(&mut prints, source_prints);
        let models = prints
            .embeddings
            .iter()
            .map(|p| p.model.clone())
            .collect::<BTreeSet<_>>();
        learning::request_all(tx, models)?;
        for (source, dest) in enrollment_copies {
            let bytes = fs::read(&source).map_err(|e| Error::io_at(&source, e))?;
            fsutil::create_dir_durable(dest.parent().expect("enrollment file has a parent"))?;
            fsutil::write_atomic_with_mode(&dest, &bytes, Some(0o600))?;
        }
        tx.save_person_with_notes(&keep, &notes)?;
        tx.save_voiceprints(&prints)?;
        index.upsert_person(tx, &keep)?;

        let names = person_names(&lib)?;
        for mut meeting in meetings {
            live_identity::refresh_review_notes(&mut meeting, &names);
            super::guard_automatic_names(&mut meeting);
            super::refresh_identity_conflict_notes(&lib, &mut meeting)?;
            save_and_index(tx, index, &mut meeting)?;
        }
        live_identity::replace_person_in_sidecars(
            tx,
            &request.source,
            &request.keep,
            &source_fence,
            &keep_fence,
        )?;
        remap_exclusions(&lib, request)?;
        pending.cleanup = true;
        fsutil::write_json_atomic(&path, &pending)?;
    }

    save_merge_redirect(tx, &request.source, &request.keep)?;
    match tx.delete_person_dir(&request.source) {
        Ok(()) | Err(Error::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    let people = lib.root().join(crate::paths::PEOPLE_DIR);
    fsutil::sync_dir(&people)?;
    index.remove_person(tx, &request.source)?;
    fs::remove_file(&path).map_err(|e| Error::io_at(&path, e))?;
    fsutil::sync_dir(&people)?;
    Ok(MergePeopleOutcome {
        keep: request.keep.clone(),
        source: request.source.clone(),
        meetings_changed: pending.affected.len(),
    })
}

fn append_unique(target: &mut Vec<String>, incoming: impl IntoIterator<Item = String>, own: &str) {
    let mut seen: HashSet<_> = target.iter().map(|v| super::fold(v)).collect();
    seen.insert(super::fold(own));
    target.extend(
        incoming
            .into_iter()
            .filter(|v| !v.trim().is_empty() && seen.insert(super::fold(v))),
    );
}

fn merge_voiceprints(keep: &mut Voiceprints, source: Voiceprints) {
    for mut print in source.embeddings {
        if let Some(origin) = VoiceprintOrigin::of_source(&print.source) {
            print.id = voiceprint_id(
                &keep.person_id,
                &print.model,
                origin,
                print.source.start,
                print.source.end,
                print.source.kind,
            );
        }
        if let Some(existing) = keep.embeddings.iter_mut().find(|p| p.id == print.id) {
            if existing.strength == Strength::Weak && print.strength == Strength::Confirmed {
                *existing = print;
            }
        } else {
            keep.embeddings.push(print);
        }
    }
}

fn plan_enrollment_copies(lib: &Library, request: &MergePeople) -> Result<Vec<(PathBuf, PathBuf)>> {
    let from = lib.person_dir(&request.source).join("enrollment");
    let to = lib.person_dir(&request.keep).join("enrollment");
    let entries = match fs::read_dir(&from) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(&from, e)),
    };
    let mut copies = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(&from, e))?;
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|e| Error::io_at(&path, e))?
            .is_file()
        {
            continue;
        }
        let bytes = fs::read(&path).map_err(|e| Error::io_at(&path, e))?;
        let dest = to.join(entry.file_name());
        match fs::read(&dest) {
            Ok(existing) if existing == bytes => {}
            Ok(_) => {
                return Err(Error::InvalidInput(
                    "The profiles have conflicting enrollment files.".into(),
                ));
            }
            Err(e) if e.kind() == ErrorKind::NotFound => copies.push((path, dest)),
            Err(e) => return Err(Error::io_at(&dest, e)),
        }
    }
    Ok(copies)
}

/// The source may already have been copied and be awaiting deletion.
pub(crate) fn check_profile_write(lib: &Library, person: &PersonId) -> Result<()> {
    if pending_person_merge(lib)?.is_some_and(|merge| merge.source == *person) {
        return Err(source_write_busy());
    }
    Ok(())
}

pub(crate) fn check_meeting_write(lib: &Library, meeting: &Meeting) -> Result<()> {
    if let Some(merge) = read_optional::<PendingMerge>(lib, PENDING)?
        && merge.cleanup
        && (super::meeting_references_person(meeting, &merge.request.source)
            || meeting
                .live_attributions
                .iter()
                .any(|turn| turn.person_id.as_ref() == Some(&merge.request.source)))
    {
        return Err(source_write_busy());
    }
    Ok(())
}

fn source_write_busy() -> Error {
    Error::LibraryBusy(
        "Finish the pending profile merge before editing or linking the source profile.".into(),
    )
}

pub(super) fn check_forget(lib: &Library, person: &PersonId) -> Result<()> {
    if pending_person_merge(lib)?
        .is_some_and(|merge| merge.source == *person || merge.keep == *person)
    {
        return Err(Error::InvalidInput(
            "Finish their pending profile merge before forgetting this person or voice.".into(),
        ));
    }
    Ok(())
}

pub(super) fn forget_exclusions(lib: &Library, person: &PersonId) -> Result<()> {
    let Some(mut pairs) = read_optional::<BTreeSet<PersonPair>>(lib, SEPARATE)? else {
        return Ok(());
    };
    let before = pairs.len();
    pairs.retain(|pair| pair.0 != *person && pair.1 != *person);
    if pairs.len() != before {
        fsutil::write_json_atomic(&file(lib, SEPARATE), &pairs)?;
    }
    Ok(())
}

fn remap_exclusions(lib: &Library, request: &MergePeople) -> Result<()> {
    let Some(old) = read_optional::<BTreeSet<PersonPair>>(lib, SEPARATE)? else {
        return Ok(());
    };
    let new: BTreeSet<_> = old
        .into_iter()
        .filter_map(|PersonPair(a, b)| {
            let a = if a == request.source {
                request.keep.clone()
            } else {
                a
            };
            let b = if b == request.source {
                request.keep.clone()
            } else {
                b
            };
            (a != b).then(|| PersonPair::new(a, b))
        })
        .collect();
    fsutil::write_json_atomic(&file(lib, SEPARATE), &new)
}

fn remap_meeting(
    meeting: &mut Meeting,
    source: &PersonId,
    keep: &Person,
    source_fence: &str,
    keep_fence: &str,
) -> bool {
    let replace = |person: &mut PersonId| {
        if person == source {
            *person = keep.id.clone();
            true
        } else {
            false
        }
    };
    let mut changed = false;
    for speaker in &mut meeting.speakers {
        let mut touched = speaker.person_id.as_mut().is_some_and(&replace);
        if let Some(promotion) = &mut speaker.live_promotion {
            touched |= replace(&mut promotion.person_id);
        }
        if let Some(conflict) = &mut speaker.identity_conflict {
            touched |= conflict.candidate.as_mut().is_some_and(&replace);
            for line in &mut conflict.lines {
                touched |= replace(&mut line.person_id);
            }
        }
        if let Some(suggestion) = &mut speaker.suggestion {
            touched |= replace(&mut suggestion.person_id);
        }
        for person in &mut speaker.rejected_person_ids {
            touched |= replace(person);
        }
        if !touched {
            continue;
        }
        changed = true;
        speaker.rejected_person_ids.sort();
        speaker.rejected_person_ids.dedup();
        if let Some(person) = speaker.named_person().cloned() {
            if speaker.assignment == crate::schema::Assignment::Confirmed {
                speaker
                    .rejected_person_ids
                    .retain(|rejected| *rejected != person);
            } else if speaker.rejected_person_ids.contains(&person) {
                speaker.person_id = None;
                speaker.assignment = crate::schema::Assignment::None;
                speaker.score = None;
            }
        } else if speaker
            .suggestion
            .as_ref()
            .is_some_and(|s| speaker.rejected_person_ids.contains(&s.person_id))
        {
            speaker.suggestion = None;
            speaker.assignment = crate::schema::Assignment::None;
            speaker.score = None;
        }
    }
    for participant in &mut meeting.participants {
        if participant.person_id.as_mut().is_some_and(&replace) {
            participant.is_self = keep.is_self;
            changed = true;
        }
    }
    for correction in &mut meeting.utterance_people {
        changed |= replace(&mut correction.person_id);
    }
    for review in &mut meeting.live_attributions {
        if review.person_id.as_mut().is_some_and(&replace) {
            live_identity::remap_forget_fence(&mut review.forget_fence, source_fence, keep_fence);
            changed = true;
        }
    }
    for generated in meeting.generated_mut() {
        for person in generated.line_people_mut().values_mut() {
            changed |= replace(person);
        }
    }
    if changed {
        super::mark_generated_stale(meeting);
    }
    changed
}
