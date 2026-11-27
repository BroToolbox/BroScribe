//! Ordinary person edits and their short Undo (design §4 People, UI31).
//!
//! An edit changes only what the user typed — the name, aliases, e-mails and
//! notes of `person.md` — and is fenced by the persisted values it was made
//! against: [`PersonRevision`] holds one SHA-256 per field, and an edit that
//! carries one refuses (`stale_person`) when a field it changes was changed
//! by someone else since. Fields it does not change are never compared, so
//! two editors of different fields both succeed.
//!
//! Each change records one Undo entry in `people/<id>/.edit-undo.json`
//! (mode 0600): the previous values of the changed fields only and the
//! field hashes right after the edit. Undo restores exactly those values,
//! and only while the fields still hold what the edit wrote; it is refused
//! (`undo_unavailable`) once the entry expired, was used, or a restored
//! e-mail now belongs to someone else, and `not_found` once the person was
//! forgotten. The history holds no voice evidence, and it lives in the
//! person's folder, so forget-person deletes it with everything else: no
//! Undo can bring a forgotten person back. Forget-voice and forget-person
//! themselves have no Undo.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset, TimeDelta};
use serde::{Deserialize, Serialize};

use super::{fold, meeting_references_person};
use crate::index::Index;
use crate::render::render_person_md;
use crate::schema::{self, Person, sha256_hex};
use crate::{Error, Library, PersonId, Result, WriteTx, fsutil, names};

/// The Undo history file inside `people/<id>/` (hidden: never a person).
pub const PERSON_UNDO_FILE: &str = ".edit-undo.json";

/// Undo entries kept per person (newest).
pub const PERSON_UNDO_MAX: usize = 8;

/// How long an edit can be undone, in seconds.
pub const PERSON_UNDO_WINDOW_S: i64 = 15 * 60;

/// Longest notes text accepted, in bytes (it is also what one Undo entry
/// may hold).
pub const PERSON_NOTES_MAX_BYTES: usize = 64 * 1024;

const UNDO_SCHEMA_VERSION: u32 = 1;

/// One editable field of a person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonField {
    Name,
    Aliases,
    Emails,
    Notes,
}

impl PersonField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Aliases => "aliases",
            Self::Emails => "emails",
            Self::Notes => "notes",
        }
    }
}

/// The persisted state of a person's editable fields as read: lowercase hex
/// SHA-256 per field, and `revision` over the whole `person.md` (it also
/// changes with "Me" or review flags, which no edit compares).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonRevision {
    pub revision: String,
    pub name: String,
    pub aliases: String,
    pub emails: String,
    pub notes: String,
}

impl PersonRevision {
    /// The revision of `person` with `notes`, as `person.md` stores them.
    pub fn of(person: &Person, notes: &str) -> Result<Self> {
        Ok(Self {
            revision: sha256_hex(render_person_md(person, notes)?.as_bytes()),
            name: sha256_hex(person.name.as_bytes()),
            aliases: list_hash(&person.aliases)?,
            emails: list_hash(&person.emails)?,
            notes: sha256_hex(notes.as_bytes()),
        })
    }

    /// The hash of one field.
    pub fn field(&self, field: PersonField) -> &str {
        match field {
            PersonField::Name => &self.name,
            PersonField::Aliases => &self.aliases,
            PersonField::Emails => &self.emails,
            PersonField::Notes => &self.notes,
        }
    }
}

/// An unambiguous hash of a list (its JSON form).
fn list_hash(list: &[String]) -> Result<String> {
    let json = serde_json::to_vec(list)
        .map_err(|e| Error::Internal(format!("cannot encode a list: {e}")))?;
    Ok(sha256_hex(&json))
}

/// A person as read for editing: the baseline an editor sends back as
/// [`PersonEditSpec::expected`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PersonState {
    pub person: Person,
    pub notes: String,
    pub revision: PersonRevision,
}

/// The persisted person with notes and revision; `not_found` once forgotten.
pub fn person_state(lib: &Library, id: &PersonId) -> Result<PersonState> {
    let (person, notes) = lib.load_person_with_notes(id)?;
    let revision = PersonRevision::of(&person, &notes)?;
    Ok(PersonState {
        person,
        notes,
        revision,
    })
}

/// One edit of a person's name, aliases, e-mails and notes (the JSON the
/// app sends on stdin; unknown fields are refused). A list is either
/// replaced whole (`aliases`, `emails`) or changed by `add_*` / `remove_*`
/// (case-insensitive), never both. Absent fields are left alone and are
/// not compared with `expected`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonEditSpec {
    /// The revision the edit was made against (from [`person_state`]);
    /// `None` edits the values as found now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<PersonRevision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emails: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add_emails: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_emails: Vec<String>,
    /// The new notes; empty clears them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl PersonEditSpec {
    /// The fields this edit addresses (changed or not).
    pub fn fields(&self) -> BTreeSet<PersonField> {
        let mut out = BTreeSet::new();
        if self.name.is_some() {
            out.insert(PersonField::Name);
        }
        if self.aliases.is_some() || !self.add_aliases.is_empty() || !self.remove_aliases.is_empty()
        {
            out.insert(PersonField::Aliases);
        }
        if self.emails.is_some() || !self.add_emails.is_empty() || !self.remove_emails.is_empty() {
            out.insert(PersonField::Emails);
        }
        if self.notes.is_some() {
            out.insert(PersonField::Notes);
        }
        out
    }
}

/// Result of [`edit_person`] and [`undo_person_edit`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PersonEditOutcome {
    pub person: Person,
    /// The persisted revision afterwards: the next edit's `expected`.
    pub revision: PersonRevision,
    pub changed: bool,
    /// The fields whose values changed.
    pub fields: Vec<PersonField>,
    /// Opaque ID that undoes this edit ([`undo_person_edit`]); `None` when
    /// nothing changed, and always for an undo (there is no redo).
    pub undo_id: Option<String>,
    /// Meetings re-rendered because the name or aliases changed.
    pub meetings_rerendered: usize,
}

/// Input for a manual, name-only person without a voiceprint.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewPersonSpec {
    pub name: String,
    #[serde(default)]
    pub emails: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

pub fn add_person(tx: &mut WriteTx<'_>, index: &mut Index, spec: &NewPersonSpec) -> Result<Person> {
    let mut person = Person::new(
        tx.library().new_person_id(),
        names::validate_name(&spec.name)?,
    );
    let emails = edit_list(&[], Some(&spec.emails), &[], &[], |e| {
        validate_email(e).map(str::to_owned)
    })?;
    check_emails_free(tx.library(), &person.id, &person, &emails)?;
    if spec.notes.len() > PERSON_NOTES_MAX_BYTES {
        return Err(Error::InvalidInput(format!(
            "notes longer than {PERSON_NOTES_MAX_BYTES} bytes"
        )));
    }
    person.emails = emails;
    tx.save_person_with_notes(&person, &spec.notes)?;
    let person = tx.library().load_person(&person.id)?;
    index.upsert_person(tx, &person)?;
    Ok(person)
}

/// Applies `spec` to person `id` in this write: the name (validated as
/// [`names::validate_name`]), aliases (each new one validated), e-mails (a
/// new one must look like an address and belong to nobody else) and notes.
/// Voice evidence, "Me" and review flags are never touched. A changed
/// name or alias re-renders every meeting that refers to the person.
///
/// Refused, with nothing written: `not_found` (forgotten or unknown),
/// `invalid_input` (a bad value, or a list both replaced and changed), and
/// `stale_person` when `spec.expected` differs from the persisted hash of
/// a field `spec` addresses. A change records an Undo entry, written before
/// `person.md` so a failed save leaves at most an entry that can never apply.
pub fn edit_person(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    id: &PersonId,
    spec: &PersonEditSpec,
) -> Result<PersonEditOutcome> {
    if spec.aliases.is_some() && !(spec.add_aliases.is_empty() && spec.remove_aliases.is_empty()) {
        return Err(Error::InvalidInput(
            "aliases: replace the list or add/remove entries, not both".into(),
        ));
    }
    if spec.emails.is_some() && !(spec.add_emails.is_empty() && spec.remove_emails.is_empty()) {
        return Err(Error::InvalidInput(
            "emails: replace the list or add/remove entries, not both".into(),
        ));
    }
    let lib = tx.library().clone();
    super::check_profile_write(&lib, id)?;
    let state = person_state(&lib, id)?;
    if let Some(expected) = &spec.expected {
        let stale: Vec<PersonField> = spec
            .fields()
            .into_iter()
            .filter(|f| expected.field(*f) != state.revision.field(*f))
            .collect();
        if !stale.is_empty() {
            return Err(stale_person(id, &stale));
        }
    }

    let before = &state.person;
    let mut next = before.clone();
    let mut notes = state.notes.clone();
    if let Some(name) = &spec.name {
        next.name = names::validate_name(name)?.to_owned();
    }
    next.aliases = edit_list(
        &before.aliases,
        spec.aliases.as_deref(),
        &spec.add_aliases,
        &spec.remove_aliases,
        |a| names::validate_name(a).map(str::to_owned),
    )?;
    next.emails = edit_list(
        &before.emails,
        spec.emails.as_deref(),
        &spec.add_emails,
        &spec.remove_emails,
        |e| validate_email(e).map(str::to_owned),
    )?;
    if let Some(n) = &spec.notes {
        if n.len() > PERSON_NOTES_MAX_BYTES {
            return Err(Error::InvalidInput(format!(
                "notes longer than {PERSON_NOTES_MAX_BYTES} bytes"
            )));
        }
        n.clone_into(&mut notes);
    }
    let fields = changed_fields(before, &state.notes, &next, &notes);
    if fields.is_empty() {
        return Ok(PersonEditOutcome {
            person: state.person,
            revision: state.revision,
            changed: false,
            fields,
            undo_id: None,
            meetings_rerendered: 0,
        });
    }
    check_emails_free(&lib, id, before, &next.emails)?;

    next.hue = Some(before.hue());
    let after = PersonRevision::of(&next, &notes)?;
    let entry = UndoEntry {
        id: format!("pu-{:032x}", rand::random::<u128>()),
        at: schema::now(),
        before: FieldValues::of(before, &state.notes, &fields),
        after: fields
            .iter()
            .map(|f| (*f, after.field(*f).to_owned()))
            .collect(),
    };
    let mut history = read_history(&lib, id).unwrap_or_default();
    history.prune(entry.at);
    history.entries.push(entry.clone());
    let excess = history.entries.len().saturating_sub(PERSON_UNDO_MAX);
    history.entries.drain(..excess);
    write_history(&lib, id, &history)?;

    let meetings_rerendered = save(tx, index, &next, &notes, &fields)?;
    Ok(PersonEditOutcome {
        person: next,
        revision: after,
        changed: true,
        fields,
        undo_id: Some(entry.id),
        meetings_rerendered,
    })
}

/// Undoes the edit `undo_id` names on person `id`: restores the previous
/// values of exactly the fields it changed, in this write, and consumes the
/// entry. Refused, with `person.md` untouched: `not_found` (the person was
/// forgotten: nothing is recreated), `undo_unavailable` (unknown, used or
/// expired entry, unreadable history, or a restored e-mail now someone
/// else's) and `stale_person` (one of those fields changed since the edit).
pub fn undo_person_edit(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    id: &PersonId,
    undo_id: &str,
) -> Result<PersonEditOutcome> {
    let lib = tx.library().clone();
    super::check_profile_write(&lib, id)?;
    let state = person_state(&lib, id)?;
    let mut history = read_history(&lib, id).map_err(|_| undo_unavailable(id, undo_id))?;
    let now = schema::now();
    let pos = history
        .entries
        .iter()
        .position(|e| e.id == undo_id && !e.expired(now))
        .ok_or_else(|| undo_unavailable(id, undo_id))?;
    let entry = history.entries[pos].clone();
    let stale: Vec<PersonField> = entry
        .after
        .iter()
        .filter(|(f, hash)| state.revision.field(**f) != hash.as_str())
        .map(|(f, _)| *f)
        .collect();
    if !stale.is_empty() {
        return Err(stale_person(id, &stale));
    }

    let mut next = state.person.clone();
    let mut notes = state.notes.clone();
    entry.before.restore(&mut next, &mut notes);
    match check_emails_free(&lib, id, &state.person, &next.emails) {
        Ok(()) => {}
        Err(Error::InvalidInput(_)) => return Err(undo_unavailable(id, undo_id)),
        Err(e) => return Err(e),
    }
    let fields = changed_fields(&state.person, &state.notes, &next, &notes);

    history.entries.remove(pos);
    history.prune(now);
    write_history(&lib, id, &history)?;
    let meetings_rerendered = if fields.is_empty() {
        0
    } else {
        next.hue = Some(state.person.hue());
        save(tx, index, &next, &notes, &fields)?
    };
    let revision = PersonRevision::of(&next, &notes)?;
    Ok(PersonEditOutcome {
        person: next,
        revision,
        changed: !fields.is_empty(),
        fields,
        undo_id: None,
        meetings_rerendered,
    })
}

/// Re-renders `meeting.md` (and the index rows) of every meeting that
/// refers to person `id` ([`meeting_references_person`]), e.g. after a
/// rename; returns how many.
pub fn rerender_meetings_of(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    id: &PersonId,
) -> Result<usize> {
    let lib = tx.library().clone();
    let mut n = 0;
    for dir in lib.list_meeting_dirs()? {
        let m = lib.load_meeting_at(&dir)?;
        if meeting_references_person(&m, id) {
            tx.save_meeting(&m)?;
            index.upsert_meeting(tx, &m)?;
            n += 1;
        }
    }
    Ok(n)
}

fn save(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    person: &Person,
    notes: &str,
    fields: &[PersonField],
) -> Result<usize> {
    tx.save_person_with_notes(person, notes)?;
    index.upsert_person(tx, person)?;
    if fields
        .iter()
        .any(|f| matches!(f, PersonField::Name | PersonField::Aliases))
    {
        rerender_meetings_of(tx, index, &person.id)
    } else {
        Ok(0)
    }
}

fn changed_fields(
    before: &Person,
    before_notes: &str,
    after: &Person,
    notes: &str,
) -> Vec<PersonField> {
    let mut out = Vec::new();
    if after.name != before.name {
        out.push(PersonField::Name);
    }
    if after.aliases != before.aliases {
        out.push(PersonField::Aliases);
    }
    if after.emails != before.emails {
        out.push(PersonField::Emails);
    }
    if notes != before_notes {
        out.push(PersonField::Notes);
    }
    out
}

/// `current` replaced by `set`, or with `remove` taken out and `add`
/// appended (case-insensitive, first spelling kept). Only values not
/// already present go through `check`: stored legacy values stay as they
/// are.
fn edit_list(
    current: &[String],
    set: Option<&[String]>,
    add: &[String],
    remove: &[String],
    check: impl Fn(&str) -> Result<String>,
) -> Result<Vec<String>> {
    let has = |list: &[String], x: &str| list.iter().any(|y| fold(y) == fold(x));
    let mut out: Vec<String> = match set {
        Some(_) => Vec::new(),
        None => current
            .iter()
            .filter(|x| !has(remove, x.as_str()))
            .cloned()
            .collect(),
    };
    for value in set.unwrap_or(add) {
        let trimmed = value.trim();
        if trimmed.is_empty() || has(&out, trimmed) {
            continue;
        }
        let kept = current.iter().find(|c| fold(c) == fold(trimmed));
        out.push(match kept {
            Some(c) => c.clone(),
            None => check(trimmed)?,
        });
    }
    Ok(out)
}

fn validate_email(email: &str) -> Result<&str> {
    let e = email.trim();
    let ok = !e.is_empty()
        && e.len() <= 254
        && !e
            .chars()
            .any(|c| c.is_whitespace() || names::is_name_control(c))
        && e.split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty());
    if ok {
        Ok(e)
    } else {
        Err(Error::InvalidInput(format!("not an e-mail: {e:?}")))
    }
}

/// Every e-mail `emails` adds over `before` belongs to no other person.
fn check_emails_free(
    lib: &Library,
    id: &PersonId,
    before: &Person,
    emails: &[String],
) -> Result<()> {
    for e in emails {
        if before.emails.iter().any(|b| fold(b) == fold(e)) {
            continue;
        }
        if let Some(other) = lib.find_person_by_email(e)?
            && other.id != *id
        {
            return Err(Error::InvalidInput(format!(
                "{e} already belongs to person {}",
                other.id
            )));
        }
    }
    Ok(())
}

fn stale_person(id: &PersonId, fields: &[PersonField]) -> Error {
    let names: Vec<&str> = fields.iter().map(|f| f.as_str()).collect();
    Error::Service {
        code: "stale_person",
        message: format!(
            "person {id} changed since it was read ({}); nothing was changed",
            names.join(", ")
        ),
    }
}

fn undo_unavailable(id: &PersonId, undo_id: &str) -> Error {
    Error::Service {
        code: "undo_unavailable",
        message: format!(
            "edit {undo_id:?} of person {id} can no longer be undone; nothing was changed"
        ),
    }
}

// ---- Undo history ----

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct UndoHistory {
    schema_version: u32,
    #[serde(default)]
    entries: Vec<UndoEntry>,
}

impl UndoHistory {
    fn prune(&mut self, now: DateTime<FixedOffset>) {
        self.entries.retain(|e| !e.expired(now));
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct UndoEntry {
    id: String,
    at: DateTime<FixedOffset>,
    /// The values before the edit, of the fields it changed only.
    before: FieldValues,
    /// Hashes of those fields right after the edit.
    after: BTreeMap<PersonField, String>,
}

impl UndoEntry {
    fn expired(&self, now: DateTime<FixedOffset>) -> bool {
        now.signed_duration_since(self.at) > TimeDelta::seconds(PERSON_UNDO_WINDOW_S)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FieldValues {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    aliases: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    emails: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
}

impl FieldValues {
    fn of(person: &Person, notes: &str, fields: &[PersonField]) -> Self {
        let mut out = Self::default();
        for f in fields {
            match f {
                PersonField::Name => out.name = Some(person.name.clone()),
                PersonField::Aliases => out.aliases = Some(person.aliases.clone()),
                PersonField::Emails => out.emails = Some(person.emails.clone()),
                PersonField::Notes => out.notes = Some(notes.to_owned()),
            }
        }
        out
    }

    fn restore(&self, person: &mut Person, notes: &mut String) {
        if let Some(n) = &self.name {
            person.name.clone_from(n);
        }
        if let Some(a) = &self.aliases {
            person.aliases.clone_from(a);
        }
        if let Some(e) = &self.emails {
            person.emails.clone_from(e);
        }
        if let Some(n) = &self.notes {
            notes.clone_from(n);
        }
    }
}

fn history_path(lib: &Library, id: &PersonId) -> PathBuf {
    lib.person_dir(id).join(PERSON_UNDO_FILE)
}

/// The person's Undo history; empty without one. An unreadable or newer
/// history is an error: [`edit_person`] then starts a new one, and
/// [`undo_person_edit`] refuses.
fn read_history(lib: &Library, id: &PersonId) -> Result<UndoHistory> {
    let path = history_path(lib, id);
    match fsutil::read_json::<UndoHistory>(&path) {
        Ok(h) if h.schema_version == UNDO_SCHEMA_VERSION => Ok(h),
        Ok(h) => Err(Error::Schema(format!(
            "{}: undo history version {}",
            path.display(),
            h.schema_version
        ))),
        Err(Error::NotFound(_)) => Ok(UndoHistory {
            schema_version: UNDO_SCHEMA_VERSION,
            entries: Vec::new(),
        }),
        Err(e) => Err(e),
    }
}

/// Writes the history (mode 0600); an empty one removes the file.
fn write_history(lib: &Library, id: &PersonId, history: &UndoHistory) -> Result<()> {
    let path = history_path(lib, id);
    if history.entries.is_empty() {
        return remove_if_present(&path);
    }
    let history = UndoHistory {
        schema_version: UNDO_SCHEMA_VERSION,
        entries: history.entries.clone(),
    };
    let bytes = serde_json::to_vec(&history)
        .map_err(|e| Error::Internal(format!("cannot encode the undo history: {e}")))?;
    fsutil::write_atomic_with_mode(&path, &bytes, Some(0o600))
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_at(path, e)),
    }
}
