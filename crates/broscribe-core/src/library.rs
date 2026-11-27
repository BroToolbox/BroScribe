//! The library folder: meetings, people and voiceprints on disk.
//!
//! Reads never lock. A read that loads many meetings lists the meeting
//! folders once ([`MeetingLookup`]) and still reads each `meeting.json` as
//! it is when loaded. Every write goes through [`Library::write`], which
//! holds the single-writer lock for one transaction ([`WriteTx`]).
//!
//! A transaction remembers what it looked up, as hints it re-validates
//! against the files before use: meeting folders by ID (one folder scan per
//! transaction instead of one per lookup) and the people lookup used to
//! render `meeting.md` (re-read only when a `person.md` changed). It also
//! lists the meetings and people it changes in the index's pending-repair
//! list before changing them (see [`crate::index::Index::repair_pending`]).

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use crate::index::IndexItem;
use crate::lock::{LibraryLock, LockGuard};
use crate::process::ProcessProbe;
use crate::render::{
    MeetingMdIdentity, PeopleLookup, parse_person_md, read_meeting_md_identity, render_meeting_md,
    render_pending_meeting_md, render_person_md,
};
use crate::schema::{
    self, Meeting, Person, RecordingState, SCHEMA_VERSION, VOICEPRINTS_SCHEMA_VERSION, Voiceprint,
    Voiceprints,
};
use crate::{Error, MeetingId, PersonId, Result, fsutil, paths};

#[cfg(test)]
thread_local! {
    /// Test hook: the next [`WriteTx::save_meeting`] on this thread stops
    /// right after writing `meeting.json`, as a crash there would.
    static FAIL_AFTER_MEETING_JSON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A file that could not be read and was left out of a listing.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct SkippedFile {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Clone, Debug)]
pub struct Library {
    root: PathBuf,
    lock_wait: Duration,
    // Library clones moved into background writers retain their profile admission.
    _mode_lease: Option<std::sync::Arc<crate::maintenance::ModeLease>>,
}

impl Library {
    /// Opens (and creates if needed) the library at `root`. For commands that
    /// write. Read-only commands should use [`Library::open_existing`] so a
    /// mistyped path does not create an empty library.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for dir in [paths::PEOPLE_DIR, paths::QUEUE_DIR, paths::INDEX_DIR] {
            fsutil::create_dir_durable(&root.join(dir))?;
        }
        Ok(Self::at(root))
    }

    /// Opens an existing library without creating anything; `not_found` when
    /// `root` has neither `people/`, a meeting tree (`YYYY/`) nor `.index/`.
    /// Read-only commands should use this instead of [`Library::open`].
    pub fn open_existing(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let exists = root.join(paths::PEOPLE_DIR).is_dir()
            || root.join(paths::INDEX_DIR).is_dir()
            || subdirs(&root)?.iter().any(|d| is_digits(d, 4));
        if exists {
            Ok(Self::at(root))
        } else {
            Err(Error::NotFound(format!("library {}", root.display())))
        }
    }

    fn at(root: PathBuf) -> Self {
        Self {
            root,
            lock_wait: LibraryLock::DEFAULT_WAIT,
            _mode_lease: None,
        }
    }

    /// Keeps normal-profile admission through every clone, including a clone
    /// made after coercion to `&Library` by a background-writing helper.
    pub fn with_mode_lease(
        mut self,
        admission: std::sync::Arc<crate::maintenance::ModeLease>,
    ) -> Result<Self> {
        admission.require_normal()?;
        self._mode_lease = Some(admission);
        Ok(self)
    }

    /// How long [`Library::write`] waits for the lock (default 5 s).
    pub fn with_lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Runs `f` while holding the library lock (waits up to the lock wait, then
    /// `library_busy`). Never nest calls: the lock is per open file handle.
    /// When `f` ends (with a result or an error), the meetings and people it
    /// changed and then indexed come off the index's pending-repair list.
    pub fn write<T>(&self, f: impl FnOnce(&mut WriteTx<'_>) -> Result<T>) -> Result<T> {
        self.write_checked(|| Ok(()), f)
    }

    /// Checks request admission after acquiring the writer lock, before any
    /// index bootstrap or durable mutation. `check` must only inspect the
    /// caller's cancellation/admission state; it must not acquire other locks.
    /// Once admitted, the transaction and its durable follow-ups may finish.
    pub fn write_checked<T>(
        &self,
        check: impl FnOnce() -> Result<()>,
        f: impl FnOnce(&mut WriteTx<'_>) -> Result<T>,
    ) -> Result<T> {
        let guard = LibraryLock::acquire(&self.root, self.lock_wait)?;
        check()?;
        // Bootstrap before the first mutation: opening after the first meeting
        // is saved would mistake a new library for a deleted-index recovery.
        // Existing meeting files still make a recreated index incomplete.
        if !crate::index::Index::path(&self.root).is_file() {
            crate::index::Index::open(&self.root)?;
        }
        let mut tx = WriteTx {
            lib: self,
            _guard: guard,
            state: Mutex::new(TxState::default()),
            review_edit: None,
        };
        let out = f(&mut tx);
        tx.finish_index();
        out
    }

    // ---- meetings (read) ----

    /// Every meeting folder (`YYYY/MM/<name>/` containing `meeting.json`), sorted.
    pub fn list_meeting_dirs(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for year in subdirs(&self.root)? {
            if !is_digits(&year, 4) {
                continue;
            }
            for month in subdirs(&year)? {
                if !is_digits(&month, 2) {
                    continue;
                }
                for dir in subdirs(&month)? {
                    if dir.join(paths::MEETING_JSON).is_file() {
                        out.push(dir);
                    }
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// The meeting IDs the meeting folders' names end with (`-<6 hex>`),
    /// sorted and deduplicated, without reading any `meeting.json`; loading
    /// one ([`Self::load_meeting`]) verifies it.
    pub fn list_meeting_ids(&self) -> Result<Vec<MeetingId>> {
        let mut out: Vec<MeetingId> = self
            .list_meeting_dirs()?
            .iter()
            .filter_map(|dir| {
                let name = dir.file_name()?.to_str()?;
                let (_, hex) = name.rsplit_once('-')?;
                MeetingId::parse(&format!("m-{hex}")).ok()
            })
            .collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// The folder of meeting `id` (matched by the short ID suffix, verified
    /// against `meeting.json`).
    pub fn meeting_dir(&self, id: &MeetingId) -> Result<PathBuf> {
        let suffix = format!("-{}", id.short_hex());
        for dir in self.list_meeting_dirs()? {
            let matches_name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix));
            if matches_name && self.load_meeting_at(&dir)?.id == *id {
                return Ok(dir);
            }
        }
        Err(Error::NotFound(format!("meeting {id}")))
    }

    pub fn meeting_exists(&self, id: &MeetingId) -> Result<bool> {
        match self.meeting_dir(id) {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn load_meeting(&self, id: &MeetingId) -> Result<Meeting> {
        self.load_meeting_at(&self.meeting_dir(id)?)
    }

    /// Reads `meeting.json`, migrated in memory to the current schema;
    /// `schema` error if a newer BroScribe wrote it. Speaker groups that break
    /// an invariant (a hand-edited file) are ignored with a review note,
    /// never an error (`ops::sanitize_overlay`).
    pub fn load_meeting_at(&self, dir: &Path) -> Result<Meeting> {
        let mut meeting =
            schema::migrate_meeting(fsutil::read_json(&dir.join(paths::MEETING_JSON))?)?;
        crate::ops::sanitize_overlay(&mut meeting);
        Ok(meeting)
    }

    /// A lookup of meetings by ID for one read operation that loads many
    /// (see [`MeetingLookup`]): lists the meeting folders once, now.
    pub fn meeting_lookup(&self) -> Result<MeetingLookup<'_>> {
        Ok(MeetingLookup {
            lib: self,
            dirs: dirs_by_hex(self.list_meeting_dirs()?),
        })
    }

    /// A random meeting ID not used in this library: no meeting folder ends
    /// with it and no deletion of a meeting with it is pending (its resume
    /// would delete the new meeting).
    pub fn new_meeting_id(&self) -> Result<MeetingId> {
        let used: Vec<String> = self
            .list_meeting_dirs()?
            .iter()
            .filter_map(|d| d.file_name()?.to_str().map(str::to_owned))
            .collect();
        loop {
            let id = MeetingId::random();
            let suffix = format!("-{}", id.short_hex());
            if !used.iter().any(|n| n.ends_with(&suffix))
                && !crate::lifecycle::meeting_deleted(self, &id)
            {
                return Ok(id);
            }
        }
    }

    /// Path of the meeting's recording while it is available.
    pub fn meeting_audio_path(&self, meeting: &Meeting) -> Result<Option<PathBuf>> {
        let Some(rec) = meeting.recording.as_ref() else {
            return Ok(None);
        };
        if rec.state != RecordingState::Available {
            return Ok(None);
        }
        if rec.path.is_absolute() {
            return Ok(Some(rec.path.clone()));
        }
        Ok(Some(self.meeting_dir(&meeting.id)?.join(&rec.path)))
    }

    // ---- people (read) ----

    pub fn people_dir(&self) -> PathBuf {
        self.root.join(paths::PEOPLE_DIR)
    }

    pub fn person_dir(&self, id: &PersonId) -> PathBuf {
        self.people_dir().join(id.as_str())
    }

    /// Every readable person with a `person.md`, sorted by ID. Unreadable or
    /// invalid files are skipped and logged; see
    /// [`Library::list_people_with_skipped`].
    pub fn list_people(&self) -> Result<Vec<Person>> {
        let (people, skipped) = self.list_people_with_skipped()?;
        for s in &skipped {
            tracing::warn!(path = %s.path.display(), "skipping unreadable person file");
        }
        Ok(people)
    }

    /// Every readable person, sorted by ID, plus the `person.md` files that
    /// could not be read or parsed.
    pub fn list_people_with_skipped(&self) -> Result<(Vec<Person>, Vec<SkippedFile>)> {
        let mut out = Vec::new();
        let mut skipped = Vec::new();
        for dir in subdirs(&self.people_dir())? {
            let Some(id) = dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| PersonId::parse(n).ok())
            else {
                continue;
            };
            let path = dir.join(paths::PERSON_MD);
            if path.is_file() {
                match self.load_person(&id) {
                    Ok(p) => out.push(p),
                    Err(e) => skipped.push(SkippedFile {
                        path,
                        error: e.to_string(),
                    }),
                }
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok((out, skipped))
    }

    pub fn load_person(&self, id: &PersonId) -> Result<Person> {
        Ok(self.load_person_with_notes(id)?.0)
    }

    pub fn load_person_with_notes(&self, id: &PersonId) -> Result<(Person, String)> {
        let path = self.person_dir(id).join(paths::PERSON_MD);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(Error::NotFound(format!("person {id}")));
            }
            Err(e) => return Err(Error::io_at(&path, e)),
        };
        let (person, notes) = parse_person_md(&text)?;
        if person.id != *id {
            return Err(Error::Schema(format!(
                "{} holds person {}",
                path.display(),
                person.id
            )));
        }
        Ok((person, notes))
    }

    /// A random person ID not used in this library.
    pub fn new_person_id(&self) -> PersonId {
        loop {
            let id = PersonId::random();
            if !self.person_dir(&id).exists()
                && !crate::ops::merge_redirect_path(self.root(), &id).exists()
            {
                return id;
            }
        }
    }

    /// The person's voiceprints; empty when the file does not exist; `schema`
    /// error if a newer BroScribe wrote it.
    pub fn load_voiceprints(&self, id: &PersonId) -> Result<Voiceprints> {
        let path = self.person_dir(id).join(paths::VOICEPRINTS_JSON);
        match fsutil::read_json::<Voiceprints>(&path) {
            Ok(v) => {
                schema::check_voiceprints_schema_version(v.schema_version)?;
                Ok(v)
            }
            Err(Error::NotFound(_)) => Ok(Voiceprints::empty(id.clone())),
            Err(e) => Err(e),
        }
    }

    /// Case-insensitive e-mail match.
    pub fn find_person_by_email(&self, email: &str) -> Result<Option<Person>> {
        let email = fold(email);
        if email.is_empty() {
            return Ok(None);
        }
        Ok(self
            .list_people()?
            .into_iter()
            .find(|p| p.emails.iter().any(|e| fold(e) == email)))
    }

    /// Case-folded match on the name or an alias.
    pub fn find_person_by_name(&self, name: &str) -> Result<Option<Person>> {
        let name = fold(name);
        if name.is_empty() {
            return Ok(None);
        }
        Ok(self
            .list_people()?
            .into_iter()
            .find(|p| fold(&p.name) == name || p.aliases.iter().any(|a| fold(a) == name)))
    }

    /// The person flagged `is_self` ("Me"), if any.
    pub fn self_person(&self) -> Result<Option<Person>> {
        Ok(self.list_people()?.into_iter().find(|p| p.is_self))
    }

    pub fn people_lookup(&self) -> Result<PeopleLookup> {
        Ok(PeopleLookup::new(self.list_people()?))
    }

    /// The folders the library writes atomically into, where interrupted
    /// writes leave temporaries ([`fsutil::orphan_temps`]): the library
    /// root (the re-resolution checkpoint), each meeting folder and its
    /// `templates/`, each `people/<id>/`, the forget tombstones
    /// (`people/.forgotten`), the live-decision sidecars (`.live/`) and
    /// `.index/`, all written under the library lock. Read only: takes no
    /// lock and creates nothing (listed folders may be missing).
    pub fn temp_dirs(&self) -> Result<Vec<PathBuf>> {
        let mut dirs = vec![self.root.clone()];
        for dir in self.list_meeting_dirs()? {
            dirs.push(dir.join(paths::MEETING_TEMPLATES_DIR));
            dirs.push(dir);
        }
        dirs.extend(subdirs(&self.people_dir())?);
        dirs.push(self.root.join(crate::ops::FORGOTTEN_DIR));
        dirs.push(self.root.join(crate::live_identity::LIVE_DIR));
        dirs.push(self.root.join(paths::INDEX_DIR));
        Ok(dirs)
    }
}

/// Meeting folders by ID, listed once for one read operation that loads many
/// meetings ([`Library::meeting_lookup`]), so each load does not scan every
/// meeting folder again as [`Library::load_meeting`] does. Takes no lock.
///
/// Only the folder listing is kept: every [`Self::load`] reads the
/// meeting's `meeting.json` as it is then, so writes made while the lookup
/// is held are seen. Meetings created after the listing are not found;
/// take a new lookup for each operation.
pub struct MeetingLookup<'a> {
    lib: &'a Library,
    dirs: HashMap<String, Vec<PathBuf>>,
}

impl MeetingLookup<'_> {
    /// The meeting as [`Library::load_meeting`] reads it: the listed
    /// folders whose names end with its short ID are read in order, and
    /// only one whose `meeting.json` holds this ID is returned (an
    /// unreadable candidate is an error, as there). `not_found` when none
    /// does, including when its folder was removed since the listing.
    pub fn load(&self, id: &MeetingId) -> Result<Meeting> {
        Ok(self.find(id)?.1)
    }

    /// [`Self::load`] with the folder the meeting was read from.
    pub fn find(&self, id: &MeetingId) -> Result<(PathBuf, Meeting)> {
        for dir in self.dirs.get(id.short_hex()).into_iter().flatten() {
            if !dir.join(paths::MEETING_JSON).is_file() {
                continue;
            }
            let meeting = self.lib.load_meeting_at(dir)?;
            if meeting.id == *id {
                return Ok((dir.clone(), meeting));
            }
        }
        Err(Error::NotFound(format!("meeting {id}")))
    }
}

/// One write transaction: holds the library lock until dropped. All mutating
/// operations live here.
pub struct WriteTx<'a> {
    lib: &'a Library,
    /// Held, never read: the lock lasts as long as the transaction.
    _guard: LockGuard,
    state: Mutex<TxState>,
    pub(crate) review_edit: Option<crate::ops::ReviewCapture>,
}

/// What a transaction remembers between its operations.
#[derive(Default)]
struct TxState {
    /// Meeting folders by the short hex ID their names end with, sorted as
    /// [`Library::list_meeting_dirs`] lists them; `None` until the first
    /// lookup or after a lookup found them out of date.
    meeting_dirs: Option<HashMap<String, Vec<PathBuf>>>,
    /// The people lookup with the `person.md` files it was read from.
    people: Option<(PeopleStamp, PeopleLookup)>,
    /// Items this transaction listed in the index's pending-repair list.
    index_marked: BTreeSet<IndexItem>,
    /// Marked items indexed after their last change in this transaction.
    index_synced: BTreeSet<IndexItem>,
}

/// Each `person.md`'s path, size and modification time, sorted.
type PeopleStamp = Vec<(PathBuf, u64, Option<SystemTime>)>;

impl WriteTx<'_> {
    pub fn root(&self) -> &Path {
        self.lib.root()
    }

    /// The read side, for lookups inside the transaction.
    pub fn library(&self) -> &Library {
        self.lib
    }

    fn state(&self) -> MutexGuard<'_, TxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The folder of meeting `id`, as [`Library::meeting_dir`] finds it,
    /// without scanning every meeting folder on each call: the folders are
    /// listed once per transaction, and a listing that no longer finds the
    /// meeting is taken again before `not_found`. The folder found is
    /// always verified against its `meeting.json`.
    pub fn meeting_dir(&self, id: &MeetingId) -> Result<PathBuf> {
        Ok(self.find_meeting(id)?.0)
    }

    /// The meeting as [`Library::load_meeting`] reads it, found as
    /// [`Self::meeting_dir`] finds it.
    pub fn load_meeting(&self, id: &MeetingId) -> Result<Meeting> {
        Ok(self.find_meeting(id)?.1)
    }

    fn find_meeting(&self, id: &MeetingId) -> Result<(PathBuf, Meeting)> {
        let hex = id.short_hex();
        let mut fresh = false;
        loop {
            let candidates = {
                let mut st = self.state();
                if st.meeting_dirs.is_none() {
                    st.meeting_dirs = Some(dirs_by_hex(self.lib.list_meeting_dirs()?));
                    fresh = true;
                }
                st.meeting_dirs
                    .as_ref()
                    .and_then(|m| m.get(hex))
                    .cloned()
                    .unwrap_or_default()
            };
            for dir in candidates {
                if !dir.join(paths::MEETING_JSON).is_file() {
                    continue;
                }
                let meeting = self.lib.load_meeting_at(&dir)?;
                if meeting.id == *id {
                    return Ok((dir, meeting));
                }
            }
            if fresh {
                return Err(Error::NotFound(format!("meeting {id}")));
            }
            self.state().meeting_dirs = None;
        }
    }

    /// Every readable person, as [`Library::people_lookup`] reads them; read
    /// again only when a `person.md` changed since this transaction last
    /// read them.
    pub fn people_lookup(&self) -> Result<PeopleLookup> {
        let stamp = people_stamp(&self.lib.people_dir())?;
        let mut st = self.state();
        if let Some((seen, lookup)) = &st.people
            && *seen == stamp
        {
            return Ok(lookup.clone());
        }
        let lookup = self.lib.people_lookup()?;
        st.people = Some((stamp, lookup.clone()));
        Ok(lookup)
    }

    /// Lists `item` in the index's pending-repair list before its files
    /// change (once per transaction).
    pub(crate) fn index_changed(&self, item: IndexItem) -> Result<()> {
        let mut st = self.state();
        st.index_synced.remove(&item);
        if !st.index_marked.contains(&item) {
            crate::index::mark_pending(self.root(), &item)?;
            st.index_marked.insert(item);
        }
        Ok(())
    }

    /// `item`'s index rows now match its files: when the write ends it
    /// comes off the pending-repair list this transaction put it on.
    pub(crate) fn index_synced(&self, item: &IndexItem) {
        let mut st = self.state();
        if st.index_marked.contains(item) {
            st.index_synced.insert(item.clone());
        }
    }

    /// `items` came off the pending-repair list (a repair).
    pub(crate) fn index_forget(&self, items: &BTreeSet<IndexItem>) {
        let mut st = self.state();
        st.index_marked.retain(|i| !items.contains(i));
        st.index_synced.retain(|i| !items.contains(i));
    }

    /// A rebuild rewrote the repair list to only the inputs it skipped.
    pub(crate) fn index_forget_all(&self) {
        let mut st = self.state();
        st.index_marked.clear();
        st.index_synced.clear();
    }

    /// Takes what this transaction indexed off the pending-repair list. A
    /// failure leaves them listed: a later repair re-indexes them, which
    /// is redundant but harmless.
    fn finish_index(&self) {
        let synced = std::mem::take(&mut self.state().index_synced);
        if let Err(e) = crate::index::clear_pending(self.root(), &synced) {
            tracing::warn!(
                code = e.code(),
                "could not update the index's pending-repair list"
            );
        }
    }

    /// Creates the meeting's folder; fails if its ID is already used.
    pub fn create_meeting_dir(&mut self, meeting: &Meeting) -> Result<PathBuf> {
        match self.find_meeting(&meeting.id) {
            Ok(_) => return Err(Error::AlreadyExists(format!("meeting {}", meeting.id))),
            Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        let rel =
            paths::meeting_rel_dir(&meeting.started_at, meeting.title.as_deref(), &meeting.id);
        let dir = self.lib.root.join(rel);
        let parent = dir.parent().unwrap_or(&self.lib.root);
        fsutil::create_dir_durable(parent)?;
        fs::create_dir(&dir).map_err(|e| match e.kind() {
            ErrorKind::AlreadyExists => Error::AlreadyExists(dir.display().to_string()),
            _ => Error::io_at(&dir, e),
        })?;
        fsutil::sync_dir(parent)?;
        if let Some(dirs) = self.state().meeting_dirs.as_mut() {
            let found = dirs.entry(meeting.id.short_hex().to_owned()).or_default();
            found.push(dir.clone());
            found.sort();
        }
        Ok(dir)
    }

    /// Writes `meeting.json` (always at this crate's schema version), then
    /// re-renders `meeting.md` and `templates/<id>.md` for each template
    /// result, removing the copies of results that are gone (all atomic).
    ///
    /// `meeting.md` vouches for the meeting's identity to lookups that cannot
    /// read `meeting.json` (refusal-only, see
    /// [`crate::render::parse_meeting_md_identity`]). When this save changes
    /// an identity the current `meeting.md` vouches for (e.g. a manifest
    /// adopting a file import gives it a `source_id`), that file is first
    /// replaced, durably, by one marked `pending` that vouches for nothing,
    /// so an interruption before the complete `meeting.md` is written never
    /// leaves the old identity as proof. A save that keeps the identity
    /// writes `meeting.md` once.
    ///
    /// An invalid speaker grouping overlay is never persisted: `internal`
    /// error, nothing written.
    pub fn save_meeting(&mut self, meeting: &Meeting) -> Result<()> {
        crate::ops::check_meeting_write(self.lib, meeting)?;
        crate::ops::check_overlay(meeting, None).map_err(|e| {
            Error::Internal(format!("{}: invalid speaker grouping: {e}", meeting.id))
        })?;
        let dir = self.meeting_dir(&meeting.id).or_else(|e| match e {
            // A folder created but not yet holding meeting.json.
            Error::NotFound(_) => {
                let rel = paths::meeting_rel_dir(
                    &meeting.started_at,
                    meeting.title.as_deref(),
                    &meeting.id,
                );
                let dir = self.lib.root.join(rel);
                if dir.is_dir() { Ok(dir) } else { Err(e) }
            }
            e => Err(e),
        })?;
        if let Some(edit) = &mut self.review_edit {
            edit.meeting(self.lib, &meeting.id, &dir)?;
        }
        let fixed;
        let meeting = if meeting.schema_version == SCHEMA_VERSION {
            meeting
        } else {
            fixed = Meeting {
                schema_version: SCHEMA_VERSION,
                ..meeting.clone()
            };
            &fixed
        };
        let people = self.people_lookup()?;
        let md = render_meeting_md(meeting, &people)?;
        let md_path = dir.join(paths::MEETING_MD);
        let vouches_falsely = match read_meeting_md_identity(&md_path) {
            Ok(Some(old)) => !old.holds_for(&MeetingMdIdentity::of(meeting)),
            // Vouches for nothing already.
            Ok(None) => false,
            Err(e) if e.kind() == ErrorKind::NotFound => false,
            // It may vouch for the old identity once readable again.
            Err(_) => true,
        };
        self.index_changed(IndexItem::Meeting(meeting.id.clone()))?;
        if vouches_falsely {
            // The meeting as it still is, unless that cannot be read or
            // rendered.
            let pending = self
                .lib
                .load_meeting_at(&dir)
                .and_then(|old| render_pending_meeting_md(&old, &people))
                .or_else(|_| render_pending_meeting_md(meeting, &people))?;
            fsutil::write_atomic(&md_path, pending.as_bytes())?;
        }
        fsutil::write_json_atomic(&dir.join(paths::MEETING_JSON), meeting)?;
        #[cfg(test)]
        if FAIL_AFTER_MEETING_JSON.replace(false) {
            return Err(Error::Internal("failpoint after meeting.json".into()));
        }
        fsutil::write_atomic(&md_path, md.as_bytes())?;
        write_template_files(&dir, meeting, &people)
    }

    /// Re-reads the meeting under the lock, applies `f`, bumps `updated_at`
    /// and saves it.
    pub fn update_meeting(
        &mut self,
        id: &MeetingId,
        f: impl FnOnce(&mut Meeting) -> Result<()>,
    ) -> Result<Meeting> {
        let mut meeting = self.load_meeting(id)?;
        f(&mut meeting)?;
        meeting.updated_at = schema::now();
        self.save_meeting(&meeting)?;
        Ok(meeting)
    }

    /// Writes `person.md`, keeping existing notes.
    pub fn save_person(&mut self, person: &Person) -> Result<()> {
        let notes = match self.lib.load_person_with_notes(&person.id) {
            Ok((_, notes)) => notes,
            Err(Error::NotFound(_)) => String::new(),
            Err(e) => return Err(e),
        };
        self.save_person_with_notes(person, &notes)
    }

    pub fn save_person_with_notes(&mut self, person: &Person, notes: &str) -> Result<()> {
        crate::ops::check_profile_write(self.lib, &person.id)?;
        self.capture_review_person(&person.id)?;
        let mut person = person.clone();
        person.hue = Some(self.hue_to_store(&person)?);
        let text = render_person_md(&person, notes)?;
        let dir = self.lib.person_dir(&person.id);
        self.index_changed(IndexItem::Person(person.id.clone()))?;
        fsutil::create_dir_durable(&dir)?;
        let saved = fsutil::write_atomic(&dir.join(paths::PERSON_MD), text.as_bytes());
        self.state().people = None;
        saved
    }

    /// The hue `person`'s file keeps: the one already stored (a hue never
    /// changes, whatever the caller passes), the derived one for a file
    /// written before hues existed, or the least-used hue for a new person.
    fn hue_to_store(&self, person: &Person) -> Result<u8> {
        match self.lib.load_person(&person.id) {
            Ok(stored) => Ok(stored.hue()),
            Err(Error::NotFound(_)) => match person.hue {
                Some(hue @ 1..=crate::schema::PERSON_HUES) => Ok(hue),
                _ => Ok(crate::schema::least_used_hue(&self.lib.list_people()?)),
            },
            // An unreadable file being replaced: its hue can't be read, so
            // keep the one every reader derives for this ID.
            Err(_) => Ok(person.hue()),
        }
    }

    /// Removes `people/<id>/` with its notes and voiceprints.
    pub fn delete_person_dir(&mut self, id: &PersonId) -> Result<()> {
        self.capture_review_person(id)?;
        let dir = self.lib.person_dir(id);
        if !dir.exists() {
            return Err(Error::NotFound(format!("person {id}")));
        }
        self.index_changed(IndexItem::Person(id.clone()))?;
        let removed = match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                Err(Error::NotFound(format!("person {id}")))
            }
            Err(e) => Err(Error::io_at(&dir, e)),
        };
        self.state().people = None;
        removed
    }

    /// Removes the temporary files interrupted atomic writes left in
    /// [`Library::temp_dirs`] ([`fsutil::sweep_orphan_temps`]). Returns how
    /// many were removed.
    pub fn sweep_orphan_temps(
        &mut self,
        probe: &dyn ProcessProbe,
        min_age: Duration,
    ) -> Result<usize> {
        let mut removed = 0;
        for dir in self.lib.temp_dirs()? {
            removed += fsutil::sweep_orphan_temps(&dir, probe, min_age)?;
        }
        Ok(removed)
    }

    /// Writes `voiceprints.json`, always at this crate's voiceprints schema version.
    pub fn save_voiceprints(&mut self, voiceprints: &Voiceprints) -> Result<()> {
        crate::ops::check_profile_write(self.lib, &voiceprints.person_id)?;
        self.capture_review_person(&voiceprints.person_id)?;
        let dir = self.lib.person_dir(&voiceprints.person_id);
        fsutil::create_dir_durable(&dir)?;
        let fixed;
        let voiceprints = if voiceprints.schema_version == VOICEPRINTS_SCHEMA_VERSION {
            voiceprints
        } else {
            fixed = Voiceprints {
                schema_version: VOICEPRINTS_SCHEMA_VERSION,
                ..voiceprints.clone()
            };
            &fixed
        };
        fsutil::write_json_atomic(&dir.join(paths::VOICEPRINTS_JSON), voiceprints)
    }

    fn capture_review_person(&mut self, id: &PersonId) -> Result<()> {
        if let Some(edit) = &mut self.review_edit {
            edit.person(self.lib, id)?;
        }
        Ok(())
    }

    /// Adds voiceprints to a person, replacing those with the same ID.
    pub fn upsert_voiceprints(
        &mut self,
        person: &PersonId,
        prints: Vec<Voiceprint>,
    ) -> Result<Voiceprints> {
        let mut vps = self.lib.load_voiceprints(person)?;
        vps.upsert(prints);
        self.save_voiceprints(&vps)?;
        Ok(vps)
    }
}

/// Writes `<meeting>/templates/<id>.md` for each template result and removes
/// copies whose result is gone; the folder exists only while results do.
fn write_template_files(dir: &Path, meeting: &Meeting, people: &PeopleLookup) -> Result<()> {
    let folder = dir.join(paths::MEETING_TEMPLATES_DIR);
    let keep: Vec<String> = meeting
        .template_results
        .iter()
        .filter(|r| crate::templates::validate_id(&r.template_id).is_ok())
        .map(|r| format!("{}.md", r.template_id))
        .collect();
    if !keep.is_empty() {
        fsutil::create_dir_durable(&folder)?;
        for r in &meeting.template_results {
            if crate::templates::validate_id(&r.template_id).is_err() {
                continue;
            }
            let text = crate::render::render_template_file(meeting, r, people);
            fsutil::write_atomic(
                &folder.join(format!("{}.md", r.template_id)),
                text.as_bytes(),
            )?;
        }
    }
    let entries = match fs::read_dir(&folder) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io_at(&folder, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(&folder, e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let derived = name
            .strip_suffix(".md")
            .is_some_and(|id| crate::templates::validate_id(id).is_ok());
        if derived && !keep.iter().any(|k| k == name) {
            let path = entry.path();
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io_at(&path, e)),
            }
        }
    }
    if keep.is_empty() {
        // Only when nothing else was put there.
        let _ = fs::remove_dir(&folder);
    }
    Ok(())
}

/// Case folding for name and e-mail matching.
fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}

fn is_digits(path: &Path, len: usize) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.len() == len && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Meeting folders by the short hex ID their names end with (`-<6 hex>`),
/// each list in the order given.
fn dirs_by_hex(dirs: Vec<PathBuf>) -> HashMap<String, Vec<PathBuf>> {
    let mut out: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for dir in dirs {
        let hex = dir
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.rsplit_once('-'))
            .map(|(_, hex)| hex.to_owned());
        if let Some(hex) = hex {
            out.entry(hex).or_default().push(dir);
        }
    }
    out
}

/// What the `person.md` files under `people` look like now ([`PeopleStamp`]).
fn people_stamp(people: &Path) -> Result<PeopleStamp> {
    let mut out = Vec::new();
    for dir in subdirs(people)? {
        let path = dir.join(paths::PERSON_MD);
        match fs::metadata(&path) {
            Ok(m) if m.is_file() => out.push((path, m.len(), m.modified().ok())),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io_at(&path, e)),
        }
    }
    out.sort();
    Ok(out)
}

/// Child folders of `dir`, skipping hidden ones; empty if `dir` is missing.
fn subdirs(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(dir, e))?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if entry
            .file_type()
            .map_err(|e| Error::io_at(&entry.path(), e))?
            .is_dir()
        {
            out.push(entry.path());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SpeakerId;
    use crate::schema::tests::sample_meeting;
    use crate::schema::{
        Assignment, Strength, VoiceprintKind, VoiceprintOrigin, VoiceprintSource, encode_vector,
        voiceprint_id,
    };

    fn ana() -> Person {
        let mut p = Person::new(PersonId::parse("p-a1a1a1").unwrap(), "Ana Example");
        p.emails.push("Ana@Example.com".into());
        p.aliases.push("Aninha".into());
        p
    }

    fn vp(person: &PersonId, start: f64) -> Voiceprint {
        Voiceprint {
            id: voiceprint_id(
                person,
                "fake",
                VoiceprintOrigin::Meeting {
                    id: &MeetingId::parse("m-7f3a2c").unwrap(),
                },
                start,
                start + 2.0,
                VoiceprintKind::Hint,
            ),
            model: "fake".into(),
            dim: 2,
            vector_b64: encode_vector(&[0.6, 0.8]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Hint,
                meeting_id: MeetingId::parse("m-7f3a2c").ok(),
                sha256: None,
                start,
                end: start + 2.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        }
    }

    #[test]
    fn create_save_load_and_update_rerenders() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = sample_meeting();
        lib.write(|tx| {
            let dir = tx.create_meeting_dir(&m)?;
            assert!(dir.ends_with("2026/11/2026-11-13-1400-roadmap-review-7f3a2c"));
            tx.save_meeting(&m)
        })
        .unwrap();
        assert_eq!(lib.list_meeting_dirs().unwrap().len(), 1);
        assert_eq!(lib.load_meeting(&m.id).unwrap(), m);
        let dir = lib.meeting_dir(&m.id).unwrap();
        let md = fs::read_to_string(dir.join(paths::MEETING_MD)).unwrap();
        assert!(md.contains("] Speaker 1: Hello there."));

        // Naming a person re-renders meeting.md on the next save.
        lib.write(|tx| {
            tx.save_person(&ana())?;
            tx.update_meeting(&m.id, |m| {
                let s = m.speaker_mut(SpeakerId::new(2).unwrap()).unwrap();
                s.person_id = Some(PersonId::parse("p-a1a1a1").unwrap());
                s.assignment = Assignment::Auto;
                Ok(())
            })
        })
        .unwrap();
        let md = fs::read_to_string(dir.join(paths::MEETING_MD)).unwrap();
        assert!(md.contains("[00:00:01] Ana Example: Hello there."));
        assert!(md.contains("[01:01:01] Ana Example: Yes."));
        assert!(lib.load_meeting(&m.id).unwrap().updated_at >= m.updated_at);

        let err = lib.write(|tx| tx.create_meeting_dir(&m)).unwrap_err();
        assert_eq!(err.code(), "already_exists");
        assert_eq!(
            lib.load_meeting(&MeetingId::parse("m-000000").unwrap())
                .unwrap_err()
                .code(),
            "not_found"
        );
        assert_eq!(
            lib.meeting_audio_path(&m).unwrap(),
            Some(dir.join("audio.m4a"))
        );
        assert_ne!(lib.new_meeting_id().unwrap(), m.id);
    }

    #[test]
    fn update_and_upsert_in_one_transaction() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = sample_meeting();
        let p = ana();
        lib.write(|tx| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)?;
            tx.save_person(&p)
        })
        .unwrap();
        lib.write(|tx| {
            tx.update_meeting(&m.id, |m| {
                m.tags.push("decision".into());
                Ok(())
            })?;
            tx.upsert_voiceprints(&p.id, vec![vp(&p.id, 1.0), vp(&p.id, 5.0)])?;
            tx.upsert_voiceprints(&p.id, vec![vp(&p.id, 1.0)])
        })
        .unwrap();
        assert_eq!(
            lib.load_meeting(&m.id).unwrap().tags,
            vec!["decision".to_owned()]
        );
        assert_eq!(lib.load_voiceprints(&p.id).unwrap().embeddings.len(), 2);
    }

    #[test]
    fn second_writer_waits_then_is_busy() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = {
            let lib = lib.clone();
            std::thread::spawn(move || {
                lib.write(|_| {
                    held_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap();
            })
        };
        held_rx.recv().unwrap();
        let other = Library::open(tmp.path())
            .unwrap()
            .with_lock_wait(Duration::from_millis(150));
        let start = std::time::Instant::now();
        let err = std::thread::spawn(move || other.write(|_| Ok(())).unwrap_err())
            .join()
            .unwrap();
        assert_eq!(err.code(), "library_busy");
        assert!(start.elapsed() >= Duration::from_millis(150));
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        lib.write(|_| Ok(())).unwrap();
    }

    #[test]
    fn people_lookup_by_email_name_and_self() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let mut me = Person::new(lib.new_person_id(), "Bruno Sample");
        me.is_self = true;
        lib.write(|tx| {
            tx.save_person_with_notes(&ana(), "Prefers mornings.\n")?;
            tx.save_person(&me)
        })
        .unwrap();
        assert_eq!(lib.list_people().unwrap().len(), 2);
        assert_eq!(
            lib.find_person_by_email("ana@example.COM")
                .unwrap()
                .unwrap()
                .name,
            "Ana Example"
        );
        assert_eq!(
            lib.find_person_by_name("  ANA example ")
                .unwrap()
                .unwrap()
                .id,
            ana().id
        );
        assert_eq!(
            lib.find_person_by_name("aninha").unwrap().unwrap().id,
            ana().id
        );
        assert!(lib.find_person_by_name("Nobody").unwrap().is_none());
        assert_eq!(lib.self_person().unwrap().unwrap().id, me.id);

        // save_person keeps notes.
        let mut a = ana();
        a.aliases.push("A.".into());
        lib.write(|tx| tx.save_person(&a)).unwrap();
        let (back, notes) = lib.load_person_with_notes(&a.id).unwrap();
        assert!(back.hue.is_some(), "saving stores the hue");
        a.hue = back.hue;
        assert_eq!(back, a);
        assert_eq!(notes, "Prefers mornings.\n");

        lib.write(|tx| tx.delete_person_dir(&a.id)).unwrap();
        assert_eq!(lib.load_person(&a.id).unwrap_err().code(), "not_found");
        assert!(lib.load_voiceprints(&a.id).unwrap().embeddings.is_empty());
    }

    #[test]
    fn an_identity_change_interrupted_before_meeting_md_leaves_it_vouching_for_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        // A file import (no source_id) and an unrelated healthy meeting.
        let mut adopted = sample_meeting();
        let mut healthy = sample_meeting();
        healthy.id = lib.new_meeting_id().unwrap();
        healthy.recording.as_mut().unwrap().sha256 = "cd".repeat(32);
        healthy.source_id = Some("healthy-source".into());
        lib.write(|tx| {
            for m in [&adopted, &healthy] {
                tx.create_meeting_dir(m)?;
                tx.save_meeting(m)?;
            }
            Ok(())
        })
        .unwrap();
        let md_of = |m: &Meeting| {
            read_meeting_md_identity(&lib.meeting_dir(&m.id).unwrap().join(paths::MEETING_MD))
                .unwrap()
        };
        assert_eq!(md_of(&adopted), Some(MeetingMdIdentity::of(&adopted)));
        assert_eq!(md_of(&healthy), Some(MeetingMdIdentity::of(&healthy)));
        let interrupted = |m: &Meeting| {
            FAIL_AFTER_MEETING_JSON.set(true);
            let error = lib.write(|tx| tx.save_meeting(m)).unwrap_err();
            assert!(error.to_string().contains("failpoint"), "{error}");
            assert!(!FAIL_AFTER_MEETING_JSON.get());
        };

        // A change that keeps the identity writes meeting.md once: stopped
        // after meeting.json, the old file still vouches, truthfully.
        adopted.duration_s += 60.0;
        interrupted(&adopted);
        assert_eq!(
            lib.load_meeting(&adopted.id).unwrap().duration_s,
            adopted.duration_s
        );
        assert_eq!(md_of(&adopted), Some(MeetingMdIdentity::of(&adopted)));

        // A manifest adopts it: a source_id reaches meeting.json, the
        // interruption comes before meeting.md. The file must not vouch for
        // the old identity, not even its recording hash.
        let before = MeetingMdIdentity::of(&adopted);
        adopted.source_id = Some("manifest-source".into());
        assert!(!before.holds_for(&MeetingMdIdentity::of(&adopted)));
        interrupted(&adopted);
        assert_eq!(
            lib.load_meeting(&adopted.id).unwrap().source_id.as_deref(),
            Some("manifest-source")
        );
        let dir = lib.meeting_dir(&adopted.id).unwrap();
        let md = fs::read_to_string(dir.join(paths::MEETING_MD)).unwrap();
        assert!(md.contains("pending: true"), "{md}");
        assert_eq!(md_of(&adopted), None);
        // Its unrelated neighbour keeps vouching for itself.
        assert_eq!(md_of(&healthy), Some(MeetingMdIdentity::of(&healthy)));

        // A pending file is replaced only by a complete save.
        lib.write(|tx| tx.save_meeting(&adopted)).unwrap();
        assert_eq!(md_of(&adopted), Some(MeetingMdIdentity::of(&adopted)));

        // A meeting.md an earlier version wrote vouches only for the
        // recording hash: keeping it needs no pending step, changing it does.
        let legacy = MeetingMdIdentity {
            provenance: None,
            ..MeetingMdIdentity::of(&adopted)
        };
        assert!(legacy.holds_for(&MeetingMdIdentity::of(&adopted)));
        let mut other = adopted.clone();
        other.recording.as_mut().unwrap().sha256 = "ef".repeat(32);
        assert!(!legacy.holds_for(&MeetingMdIdentity::of(&other)));
    }

    #[test]
    fn newer_schema_is_not_loaded_or_updated() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = sample_meeting();
        lib.write(|tx| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)
        })
        .unwrap();
        // Saving always writes this crate's version.
        let mut old = m.clone();
        old.schema_version = 0;
        lib.write(|tx| tx.save_meeting(&old)).unwrap();
        assert_eq!(
            lib.load_meeting(&m.id).unwrap().schema_version,
            SCHEMA_VERSION
        );

        let path = lib.meeting_dir(&m.id).unwrap().join(paths::MEETING_JSON);
        let mut v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        v["schema_version"] = (SCHEMA_VERSION + 1).into();
        v["future_field"] = "kept".into();
        let newer = serde_json::to_string_pretty(&v).unwrap();
        fs::write(&path, &newer).unwrap();

        let err = lib.load_meeting(&m.id).unwrap_err();
        assert_eq!(err.code(), "schema");
        assert!(err.to_string().contains("newer BroScribe"), "{err}");
        let err = lib
            .write(|tx| tx.update_meeting(&m.id, |_| Ok(())))
            .unwrap_err();
        assert_eq!(err.code(), "schema");
        assert_eq!(fs::read_to_string(&path).unwrap(), newer);
        // Nor overwritten by a plain save.
        assert_eq!(
            lib.write(|tx| tx.save_meeting(&m)).unwrap_err().code(),
            "schema"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), newer);

        let vp = lib.person_dir(&ana().id).join(paths::VOICEPRINTS_JSON);
        fs::create_dir_all(vp.parent().unwrap()).unwrap();
        fs::write(&vp, r#"{"schema_version": 9, "person_id": "p-a1a1a1"}"#).unwrap();
        assert_eq!(
            lib.load_voiceprints(&ana().id).unwrap_err().code(),
            "schema"
        );
    }

    #[test]
    fn people_get_the_least_used_hue_and_never_change_it() {
        use crate::schema::{PERSON_HUES, derived_hue};
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let ids: Vec<PersonId> = (0..9)
            .map(|n| PersonId::parse(&format!("p-{n}{n}{n}{n}{n}{n}")).unwrap())
            .collect();
        for (n, id) in ids.iter().take(8).enumerate() {
            lib.write(|tx| tx.save_person(&Person::new(id.clone(), format!("Synthetic {n}"))))
                .unwrap();
        }
        let hues: Vec<u8> = ids[..8]
            .iter()
            .map(|id| lib.load_person(id).unwrap().hue.unwrap())
            .collect();
        // Each new person takes the least-used hue, the lowest on a tie.
        assert_eq!(hues, [1, 2, 3, 4, 5, 6, 7, 1]);

        // Saving again with another hue, or none, keeps the stored one.
        let mut first = lib.load_person(&ids[0]).unwrap();
        first.hue = Some(PERSON_HUES);
        first.name = "Synthetic renamed".into();
        lib.write(|tx| tx.save_person(&first)).unwrap();
        lib.write(|tx| tx.save_person(&Person::new(ids[0].clone(), "Synthetic again")))
            .unwrap();
        assert_eq!(lib.load_person(&ids[0]).unwrap().hue, Some(1));

        // A file written before hues existed reads with the derived hue,
        // and keeps it when saved.
        let legacy = Person::new(ids[8].clone(), "Synthetic legacy");
        fs::create_dir_all(lib.person_dir(&ids[8])).unwrap();
        fs::write(
            lib.person_dir(&ids[8]).join(paths::PERSON_MD),
            render_person_md(&legacy, "").unwrap(),
        )
        .unwrap();
        let read = lib.load_person(&ids[8]).unwrap();
        assert_eq!(read.hue, None);
        assert_eq!(read.hue(), derived_hue(&ids[8]));
        lib.write(|tx| tx.save_person(&read)).unwrap();
        assert_eq!(
            lib.load_person(&ids[8]).unwrap().hue,
            Some(derived_hue(&ids[8]))
        );
    }

    #[test]
    fn unreadable_person_does_not_break_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let mut bo = Person::new(PersonId::parse("p-b2b2b2").unwrap(), "Bo Example");
        lib.write(|tx| tx.save_person(&bo)).unwrap();
        // The first person in an empty library takes the first hue.
        bo.hue = Some(1);
        // The meeting's speaker 1 is linked to ana, whose file is corrupt.
        let bad = lib.person_dir(&ana().id).join(paths::PERSON_MD);
        fs::create_dir_all(bad.parent().unwrap()).unwrap();
        fs::write(&bad, "---\nnot: [valid\n").unwrap();

        let (people, skipped) = lib.list_people_with_skipped().unwrap();
        assert_eq!(people, vec![bo.clone()]);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].path, bad);
        assert_eq!(lib.list_people().unwrap(), vec![bo.clone()]);
        assert_eq!(
            lib.find_person_by_name("bo example").unwrap().unwrap().id,
            bo.id
        );
        assert!(lib.self_person().unwrap().is_none());

        let m = sample_meeting();
        lib.write(|tx| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)
        })
        .unwrap();
        let md =
            fs::read_to_string(lib.meeting_dir(&m.id).unwrap().join(paths::MEETING_MD)).unwrap();
        assert!(md.contains("Speaker 1: Hello there."), "{md}");
    }

    #[test]
    fn open_existing_does_not_create() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib");
        assert_eq!(
            Library::open_existing(&root).unwrap_err().code(),
            "not_found"
        );
        assert!(!root.exists());
        fs::create_dir_all(root.join("2026/11")).unwrap();
        Library::open_existing(&root).unwrap();
        assert!(!root.join(paths::PEOPLE_DIR).exists());
        let other = tmp.path().join("other");
        Library::open(&other).unwrap();
        Library::open_existing(&other).unwrap();
    }

    /// A process probe where only `.0` (this process) is alive.
    struct Probe(u32);

    impl ProcessProbe for Probe {
        fn is_alive(&self, pid: u32) -> bool {
            pid == self.0
        }
        fn current_pid(&self) -> u32 {
            self.0
        }
    }

    #[test]
    fn transaction_lookups_follow_changes_made_during_the_write() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let mut m = sample_meeting();
        let ana = ana();
        lib.write(|tx| {
            let dir = tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)?;
            assert_eq!(tx.meeting_dir(&m.id)?, dir);
            // Another step of the same write moves the folder.
            let moved = dir.with_file_name("2026-11-13-1400-renamed-7f3a2c");
            fs::rename(&dir, &moved).unwrap();
            assert_eq!(tx.meeting_dir(&m.id)?, moved);
            assert_eq!(tx.load_meeting(&m.id)?, lib.load_meeting(&m.id)?);
            let missing = MeetingId::parse("m-000000")?;
            assert_eq!(tx.meeting_dir(&missing).unwrap_err().code(), "not_found");

            // meeting.md names a person once their person.md exists, however
            // it was written.
            let s = m.speaker_mut(SpeakerId::new(2).unwrap()).unwrap();
            s.person_id = Some(ana.id.clone());
            s.assignment = Assignment::Auto;
            tx.save_meeting(&m)?;
            let md = || fs::read_to_string(moved.join(paths::MEETING_MD)).unwrap();
            assert!(!md().contains("Ana Example"), "{}", md());
            let person_dir = lib.person_dir(&ana.id);
            fs::create_dir_all(&person_dir).unwrap();
            fs::write(
                person_dir.join(paths::PERSON_MD),
                render_person_md(&ana, "")?,
            )
            .unwrap();
            tx.save_meeting(&m)?;
            assert!(md().contains("Ana Example: Hello there."), "{}", md());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn meeting_lookup_reads_current_files_and_only_matching_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let mut m = sample_meeting();
        let dir = lib
            .write(|tx| {
                let dir = tx.create_meeting_dir(&m)?;
                tx.save_meeting(&m)?;
                Ok(dir)
            })
            .unwrap();
        // Folders named for an ID whose meeting.json holds another meeting:
        // one listed before the real folder of `m`, one for an absent ID.
        let mut other = sample_meeting();
        other.id = MeetingId::parse("m-0a0b0c").unwrap();
        let ghost = MeetingId::parse("m-0d0e0f").unwrap();
        for hex in [m.id.short_hex(), ghost.short_hex()] {
            let decoy = dir.with_file_name(format!("0000-decoy-{hex}"));
            fs::create_dir(&decoy).unwrap();
            fsutil::write_json_atomic(&decoy.join(paths::MEETING_JSON), &other).unwrap();
        }

        let lookup = lib.meeting_lookup().unwrap();
        assert_eq!(lookup.load(&m.id).unwrap(), m);
        for id in [&ghost, &other.id] {
            assert_eq!(lookup.load(id).unwrap_err().code(), "not_found");
            assert_eq!(lib.load_meeting(id).unwrap_err().code(), "not_found");
        }

        // A write made while the lookup is held is read back, never a copy
        // from before it.
        m.title = Some("Roadmap review, revised".into());
        lib.write(|tx| tx.save_meeting(&m)).unwrap();
        assert_eq!(lookup.load(&m.id).unwrap(), m);
        assert_eq!(
            lookup.load(&m.id).unwrap(),
            lib.load_meeting(&m.id).unwrap()
        );
    }

    #[test]
    fn sweep_removes_only_temporary_files_of_dead_writers() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let m = sample_meeting();
        let dir = lib
            .write(|tx| {
                let dir = tx.create_meeting_dir(&m)?;
                tx.save_meeting(&m)?;
                Ok(dir)
            })
            .unwrap();
        let dead = dir.join(".meeting.json.tmp-4242-0000000a");
        let own = dir.join(".meeting.md.tmp-77-0000000b");
        let user = dir.join("notes.tmp-4242-0000000a");
        for f in [&dead, &own, &user] {
            fs::write(f, b"x").unwrap();
        }
        let removed = lib
            .write(|tx| tx.sweep_orphan_temps(&Probe(77), Duration::ZERO))
            .unwrap();
        assert_eq!(removed, 1);
        assert!(!dead.exists() && own.exists() && user.exists());
        assert_eq!(lib.load_meeting(&m.id).unwrap(), m);
    }
}
