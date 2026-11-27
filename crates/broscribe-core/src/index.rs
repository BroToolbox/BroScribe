//! Derived SQLite index (design §7.1): full-text search over utterances and
//! meetings, people, speakers, the review queue, and utterance windows for
//! semantic search (vectors in a separate cache, see [`vectors`]).
//!
//! The index lives in `<lib>/.index/broscribe.sqlite` and is derived only:
//! [`Index::rebuild`] restores it from the library files. It runs in WAL mode
//! with a 5 s busy timeout, so readers never block the writer and never take
//! the library lock.
//!
//! - **Writes** need a [`WriteTx`] as proof that the library lock is held, and
//!   each one runs in a single SQLite transaction.
//! - **Reads** (CLI, MCP) open a short-lived connection with
//!   [`Index::open_readonly`] per request; a reader keeps its snapshot until
//!   its transaction ends, even across a rebuild.
//! - Speaker names are resolved at query time from the `people` table, so
//!   renaming a person never leaves stale names behind.
//! - **Windows** ([`windows`]) are written with their meeting; their vectors
//!   live in `.index/vectors.sqlite`, which read-only connections `ATTACH`
//!   when present. Keyword search never needs it.
//! - **Reads** that run several statements (hybrid search, meetings with
//!   their tags, people with aliases, actions and decisions with names) run
//!   them in one read transaction, so they all see one snapshot. Read-only
//!   connections never checkpoint on close: closing a reader leaves the
//!   database and its `-wal` file as they were (no change events of its own).
//! - **Health and pending repair**: before a write changes a meeting's or
//!   person's files ([`WriteTx::save_meeting`], [`WriteTx::save_person`]…),
//!   it lists them durably in `.index/pending.json`; a successful upsert or
//!   removal in the same write takes them off again when the write ends. A
//!   write that failed or crashed in between leaves them listed, and a
//!   rebuild leaves listed what it could not read.
//!   [`Index::health`] is the one read-only verdict on whether the index is
//!   current: the database answers searches (it is not missing, incomplete,
//!   of another layout or upgraded in place since its last rebuild) and
//!   nothing is listed. [`Index::repair_pending`] acts on that verdict.
//! - **Pages**: meetings, search and actions are also read in
//!   keyset pages whose opaque [`PageCursor`] is bound to the query and to
//!   the index's identity: a random instance renewed when the index is
//!   created, rebuilt or upgraded, and a revision every committed write
//!   moves (metadata-only upserts, repairs and removals included). A cursor
//!   from before any of them is `cursor_stale`, never a page that skips or
//!   repeats rows.
//! - **Person relations** (`person_meetings`, layout 8): each meeting's
//!   named speakers, attributed lines, linked participants and the targets
//!   of its live decisions awaiting review, so
//!   [`Index::person_meeting_candidates`] lists a person's meetings without
//!   reading every file; the files still decide. An index upgraded from an
//!   older layout refuses that read until a rebuild backfilled it.

mod enrichment;
mod page;
mod schema;
mod search;
pub mod vectors;
pub mod windows;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

pub use enrichment::{ActionItem, ActionQuery, DecisionItem, DecisionQuery, TemplateHit};
pub use page::{
    CURSOR_MISMATCH, CURSOR_STALE, CursorRequest, INVALID_CURSOR, IndexRevision, MAX_CURSOR_LEN,
    MAX_PAGE_SIZE, Page, PageCursor,
};
pub use schema::INDEX_SCHEMA_VERSION;
pub use search::{
    MeetingHit, MeetingOrder, MetadataMatch, PendingWindow, PersonMeetingCandidate, PersonRelation,
    PersonSummary, QueryVector, ReviewItem, SearchHit, SearchMode, SearchQuery, fts_query,
};
pub use vectors::{VECTORS_DB, VectorCache};
pub use windows::{Window, meeting_window_revision, meeting_windows, window_revision};

use crate::schema::{Meeting, Person};
use crate::{Error, Library, MeetingId, PersonId, Result, WriteTx, fsutil, paths};

/// File in `.index/` listing what [`Index::repair_pending`] must re-index.
pub const PENDING_REPAIR: &str = "pending.json";

/// How long a connection waits for a SQLite lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A connection to the library's index.
pub struct Index {
    conn: Connection,
    /// Opened by [`Index::open_readonly`]: writes are refused.
    read_only: bool,
    /// The vector cache is attached as schema `vec` (read-only).
    vectors: bool,
    /// The layout's version: [`INDEX_SCHEMA_VERSION`], or a migratable
    /// older one (4 to 7) when a read-only open could not upgrade it.
    /// Queries that need a newer layout check it and return nothing below.
    version: u32,
}

/// What [`Index::rebuild`] indexed.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RebuildReport {
    pub meetings: usize,
    pub people: usize,
    pub utterances: usize,
    pub windows: usize,
    /// Vector-cache entries deleted because no window uses their text.
    pub vectors_pruned: usize,
    /// Files that could not be read; the rest of the library is indexed.
    /// They stay pending ([`Index::health`]) until a repair indexes them.
    pub skipped: Vec<SkippedFile>,
}

pub use crate::library::SkippedFile;

/// `.index/pending.json`: the meetings and people whose files changed
/// without (yet) the matching index update.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct PendingRepair {
    /// Only a full rebuild repairs the index: the list could not be read
    /// ([`read_pending`]), or a rebuild skipped a meeting folder (its
    /// `meeting.json` unreadable, or a duplicate meeting ID).
    #[serde(default)]
    rebuild: bool,
    #[serde(default)]
    meetings: BTreeSet<MeetingId>,
    #[serde(default)]
    people: BTreeSet<PersonId>,
}

impl PendingRepair {
    fn is_empty(&self) -> bool {
        !self.rebuild && self.meetings.is_empty() && self.people.is_empty()
    }

    fn contains(&self, item: &IndexItem) -> bool {
        self.rebuild
            || match item {
                IndexItem::Meeting(id) => self.meetings.contains(id),
                IndexItem::Person(id) => self.people.contains(id),
            }
    }

    fn insert(&mut self, item: &IndexItem) {
        match item {
            IndexItem::Meeting(id) => self.meetings.insert(id.clone()),
            IndexItem::Person(id) => self.people.insert(id.clone()),
        };
    }

    fn remove(&mut self, item: &IndexItem) {
        match item {
            IndexItem::Meeting(id) => self.meetings.remove(id),
            IndexItem::Person(id) => self.people.remove(id),
        };
    }

    /// Also lists everything `other` lists.
    fn extend(&mut self, other: &PendingRepair) {
        self.rebuild |= other.rebuild;
        self.meetings.extend(other.meetings.iter().cloned());
        self.people.extend(other.people.iter().cloned());
    }
}

/// How the index database stands, as [`Index::health`] reads it without
/// opening it for writing, upgrading it or creating anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexDatabase {
    /// Complete, of this layout, and rebuilt since any in-place upgrade:
    /// [`Index::open_readonly`] serves it.
    Ready,
    /// No database, and no meeting or person to index: nothing is missing.
    NotNeeded,
    /// No database while the library holds meetings or people.
    Missing,
    /// Created alongside existing meetings or people (or without a schema)
    /// and never rebuilt: [`Index::open_readonly`] refuses it.
    Incomplete,
    /// A layout this version cannot read.
    Incompatible,
    /// An older layout (4 to 7) still to upgrade, or one upgraded in place
    /// and not rebuilt since: its older rows lack what this layout holds
    /// (e.g. unnamed speaker labels, person relations).
    Outdated,
}

impl IndexDatabase {
    /// Only a full [`Index::rebuild`] makes it serve every search again.
    pub fn needs_rebuild(self) -> bool {
        !matches!(self, Self::Ready | Self::NotNeeded)
    }
}

/// The one verdict on whether the index matches the library files and
/// answers searches ([`Index::health`]): for the agent's startup and
/// maintenance, `doctor`, `library check`, diagnostics and every repair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IndexHealth {
    pub database: IndexDatabase,
    /// Only a full rebuild makes the index current: the database needs one
    /// ([`IndexDatabase::needs_rebuild`]), the pending list is unreadable,
    /// or the last rebuild skipped a meeting folder.
    pub rebuild: bool,
    /// Listed in `.index/pending.json`: their files changed without (yet)
    /// the matching index update, e.g. after a crash between the two.
    pub meetings: BTreeSet<MeetingId>,
    pub people: BTreeSet<PersonId>,
}

impl IndexHealth {
    /// Every write reached a database that serves searches (or there is
    /// nothing to index): nothing to repair.
    pub fn is_current(&self) -> bool {
        !self.rebuild && self.meetings.is_empty() && self.people.is_empty()
    }
}

/// A library item with index rows.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum IndexItem {
    Meeting(MeetingId),
    Person(PersonId),
}

/// What [`Index::repair_pending`] did.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RepairReport {
    /// Set when [`IndexHealth::rebuild`] asked for it and the index was
    /// rebuilt from the files.
    pub rebuilt: Option<RebuildReport>,
    /// Meetings re-indexed from their files.
    pub meetings: usize,
    /// People re-indexed from their files.
    pub people: usize,
    /// Meetings and people whose files are gone: their rows were removed.
    pub removed: usize,
    /// Files that could not be read; they stay pending.
    pub skipped: Vec<SkippedFile>,
}

impl Index {
    /// Path of the index database inside the library at `lib_root`.
    pub fn path(lib_root: &Path) -> PathBuf {
        lib_root.join(paths::INDEX_DB)
    }

    /// Opens the index for writing, creating the database and its schema
    /// when missing. A schema created alongside existing meetings or people
    /// remains incomplete (hidden from read-only callers, and
    /// [`IndexDatabase::Incomplete`]) until [`Index::rebuild`]. The previous
    /// schema version is upgraded in place; any other version requires a
    /// rebuild.
    pub fn open(lib_root: &Path) -> Result<Self> {
        let mut conn = open_rw(lib_root)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        match schema::stored_version(&tx)? {
            None => {
                schema::create(&tx)?;
                let lib = Library::open_existing(lib_root)?;
                schema::set_complete(&tx, !has_indexable_files(&lib)?)?;
            }
            Some(v) if schema::is_migratable(v) => schema::migrate(&tx, v).map_err(db)?,
            Some(v) => {
                check_version(v)?;
                schema::ensure_identity(&tx)?;
            }
        }
        tx.commit().map_err(db)?;
        Ok(Self {
            conn,
            read_only: false,
            vectors: false,
            version: INDEX_SCHEMA_VERSION,
        })
    }

    /// Opens an existing index for reading only (`query_only`). Meant to be
    /// short-lived: one per request. The vector cache, when present and
    /// readable, is attached read-only for semantic search; without it
    /// keyword search works the same. An older migratable layout (4 to 7) is
    /// upgraded in place first; when the database cannot be written it is
    /// read as it is, and the queries that need a newer layout return
    /// nothing (e.g. [`Index::resolved_models`] below 5, template results
    /// and [`Index::search_templates`] below 6) or less (unnamed speakers
    /// show as `Speaker N` below 7).
    pub fn open_readonly(lib_root: &Path) -> Result<Self> {
        let path = Self::path(lib_root);
        if !path.is_file() {
            return Err(missing_index());
        }
        let mut conn = open_reader(&path)?;
        let version = match schema::stored_version(&conn)? {
            None => return Err(missing_index()),
            Some(v) if schema::is_migratable(v) => migrate_if_writable(&mut conn, v)?,
            Some(v) => {
                check_version(v)?;
                v
            }
        };
        if !schema::is_complete(&conn)? {
            return Err(missing_index());
        }
        let vectors = attach_vectors(&conn, lib_root);
        conn.execute_batch("PRAGMA query_only = ON").map_err(db)?;
        Ok(Self {
            conn,
            read_only: true,
            vectors,
            version,
        })
    }

    /// Whether the vector cache is attached (read-only connections only).
    pub fn vectors_attached(&self) -> bool {
        self.vectors
    }

    /// Rebuilds the index from the library files, in place and inside one
    /// SQLite transaction: recreates the schema when the stored version
    /// differs, deletes every row (FTS included) and repopulates from
    /// `people/` and the meeting folders (`.queue/` is never read). Needs no
    /// text model: windows are derived from the transcripts, and afterwards
    /// vector-cache entries no window uses any more are pruned (vectors for
    /// new windows are added by the pipeline, outside the lock).
    ///
    /// The database and its `-wal`/`-shm` files are never renamed or deleted,
    /// so open readers keep their snapshot until they finish. Works when
    /// `.index/` was deleted. Unreadable files are skipped and reported, and
    /// stay listed for [`Index::repair_pending`] (never current until they
    /// are indexed): an unreadable person by its ID (the folder's, which
    /// [`Library::load_person`] reads only when the file holds that ID), and
    /// a skipped meeting folder as a full rebuild, as nothing read proves
    /// which meeting an unreadable `meeting.json` holds (its folder name may
    /// not) and a repair by ID finds only one of a duplicate ID's folders.
    /// They are listed before the commit makes the database complete, so an
    /// interruption between the two never loses them, and everything else
    /// listed comes off only after it.
    pub fn rebuild(tx: &WriteTx<'_>) -> Result<RebuildReport> {
        let lib = tx.library();
        let mut conn = open_rw(lib.root())?;
        let t = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        match schema::stored_version(&t)? {
            Some(INDEX_SCHEMA_VERSION) => {
                schema::clear(&t)?;
                schema::renew_identity(&t).map_err(db)?;
            }
            None => schema::create(&t)?,
            Some(_) => {
                schema::drop_all(&t)?;
                schema::create(&t)?;
            }
        }

        let mut report = RebuildReport::default();
        // What this rebuild could not index: still pending afterwards.
        let mut unindexed = PendingRepair::default();
        for id in person_ids_on_disk(lib)? {
            match lib.load_person(&id) {
                Ok(person) => {
                    write_person(&t, &person)?;
                    report.people += 1;
                }
                Err(e) => {
                    report.skipped.push(SkippedFile {
                        path: lib.person_dir(&id).join(paths::PERSON_MD),
                        error: e.to_string(),
                    });
                    unindexed.insert(&IndexItem::Person(id));
                }
            }
        }

        let mut seen = HashSet::new();
        for dir in lib.list_meeting_dirs()? {
            let path = dir.join(paths::MEETING_JSON);
            let meeting = match lib.load_meeting_at(&dir) {
                Ok(m) => m,
                Err(e) => {
                    unindexed.rebuild = true;
                    report.skipped.push(SkippedFile {
                        path,
                        error: e.to_string(),
                    });
                    continue;
                }
            };
            if !seen.insert(meeting.id.clone()) {
                // A repair by ID would find only one of the folders.
                unindexed.rebuild = true;
                report.skipped.push(SkippedFile {
                    path,
                    error: format!("duplicate meeting ID {}", meeting.id),
                });
                continue;
            }
            report.windows += write_meeting(&t, &meeting, &rel_dir(lib, &dir))?;
            report.meetings += 1;
            report.utterances += meeting.segments.len();
        }
        // Listed before the marks that make the database complete and
        // current are committed: interrupted in between, the list still
        // names what was skipped (and an unreadable one still asks for a
        // rebuild).
        if !unindexed.is_empty() {
            let before = read_pending(lib.root())?;
            let mut listed = before.clone();
            listed.extend(&unindexed);
            if listed != before {
                write_pending(lib.root(), &listed)?;
            }
        }
        schema::clear_rebuild_required(&t)?;
        schema::set_complete(&t, true)?;
        t.commit().map_err(db)?;
        // Everything else was just read from the files: only what was
        // skipped stays pending.
        write_pending(lib.root(), &unindexed)?;
        tx.index_forget_all();

        if VectorCache::path(lib.root()).is_file() {
            let referenced = text_hashes(&conn)?;
            report.vectors_pruned = VectorCache::open(lib.root())?.prune(tx, &referenced)?;
        }
        Ok(report)
    }

    /// Whether the index is current with the library files and answers
    /// searches, and what must be repaired when it is not. Reads only, takes
    /// no lock, creates nothing: a missing database is never created, and
    /// an existing one is neither upgraded nor written. Without a database
    /// it looks for an indexable meeting folder or person (no file is read);
    /// with one it reads only its layout marks.
    ///
    /// - `database`: missing (with files to index), incomplete, of another
    ///   or an older layout, or upgraded in place and not rebuilt since all
    ///   ask for a full rebuild; ready, or not needed for a library with
    ///   nothing to index, do not.
    /// - `meetings`, `people`: listed in `.index/pending.json` because a
    ///   write changed their files without (yet) updating the index, or a
    ///   rebuild could not read a person.
    /// - `rebuild`: the database asks for it, the list is unreadable, or a
    ///   rebuild skipped a meeting folder.
    pub fn health(lib_root: &Path) -> Result<IndexHealth> {
        // Skipped inputs are recorded before a rebuild publishes a ready database.
        // Never combine that new database with an older empty repair list.
        let database = database_state(lib_root)?;
        let pending = read_pending(lib_root)?;
        Ok(IndexHealth {
            database,
            rebuild: pending.rebuild || database.needs_rebuild(),
            meetings: pending.meetings,
            people: pending.people,
        })
    }

    /// Makes the index current as [`Index::health`] reads it under the
    /// lock: rebuilds the whole index when it asks for that (a deleted or
    /// recreated incomplete database included; afterwards only what the
    /// rebuild skipped stays listed), else re-indexes what is listed from
    /// the library files: people first (meetings name them), then meetings;
    /// a listed item whose files are gone loses its rows. Items whose files
    /// cannot be read stay listed and are reported. Nothing to do when it
    /// is current.
    pub fn repair_pending(tx: &WriteTx<'_>) -> Result<RepairReport> {
        let root = tx.root();
        let health = Self::health(root)?;
        let mut report = RepairReport::default();
        if health.is_current() {
            return Ok(report);
        }
        if health.rebuild {
            report.rebuilt = Some(Self::rebuild(tx)?);
            return Ok(report);
        }
        let lib = tx.library();
        let mut index = Self::open(root)?;
        let mut repaired = BTreeSet::new();
        for id in &health.people {
            match lib.load_person(id) {
                Ok(person) => {
                    index.upsert_person(tx, &person)?;
                    report.people += 1;
                }
                Err(Error::NotFound(_)) => {
                    index.remove_person(tx, id)?;
                    report.removed += 1;
                }
                Err(e) => {
                    report.skipped.push(SkippedFile {
                        path: lib.person_dir(id).join(paths::PERSON_MD),
                        error: e.to_string(),
                    });
                    continue;
                }
            }
            repaired.insert(IndexItem::Person(id.clone()));
        }
        for id in &health.meetings {
            match tx.load_meeting(id) {
                Ok(meeting) => {
                    index.upsert_meeting(tx, &meeting)?;
                    report.meetings += 1;
                }
                Err(Error::NotFound(_)) => {
                    index.remove_meeting(tx, id)?;
                    report.removed += 1;
                }
                Err(e) => {
                    report.skipped.push(SkippedFile {
                        path: root.join(id.as_str()),
                        error: e.to_string(),
                    });
                    continue;
                }
            }
            repaired.insert(IndexItem::Meeting(id.clone()));
        }
        clear_pending(root, &repaired)?;
        tx.index_forget(&repaired);
        Ok(report)
    }

    /// Replaces every row of the meeting (speakers, tags, participants,
    /// windows, utterances, FTS) in one transaction. Windows, utterances
    /// and their full-text rows are kept as they are when the transcript
    /// rows they would be rewritten with are identical (a change of title,
    /// tags, participants or enrichment only).
    pub fn upsert_meeting(&mut self, tx: &WriteTx<'_>, meeting: &Meeting) -> Result<()> {
        let dir = rel_dir(tx.library(), &meeting_dir(tx, meeting)?);
        let t = self.write_tx()?;
        write_meeting(&t, meeting, &dir)?;
        schema::bump_revision(&t)?;
        t.commit().map_err(db)?;
        tx.index_synced(&IndexItem::Meeting(meeting.id.clone()));
        Ok(())
    }

    /// Removes the meeting's rows; `false` when it was not indexed.
    pub fn remove_meeting(&mut self, tx: &WriteTx<'_>, id: &MeetingId) -> Result<bool> {
        let t = self.write_tx()?;
        let removed = delete_meeting_rows(&t, id.as_str())?;
        schema::bump_revision(&t)?;
        t.commit().map_err(db)?;
        tx.index_synced(&IndexItem::Meeting(id.clone()));
        Ok(removed)
    }

    /// Drops text-vector cache entries no search window uses any more (after
    /// a meeting was removed); returns how many went.
    pub fn prune_vectors(&mut self, tx: &WriteTx<'_>) -> Result<usize> {
        if !VectorCache::path(tx.root()).is_file() {
            return Ok(0);
        }
        let referenced = text_hashes(&self.conn)?;
        VectorCache::open(tx.root())?.prune(tx, &referenced)
    }

    /// Replaces the person's rows and refreshes the meetings that mention them.
    pub fn upsert_person(&mut self, tx: &WriteTx<'_>, person: &Person) -> Result<()> {
        // The hue is the one the person's file keeps (it never changes), so
        // an incremental update indexes what a rebuild would.
        let mut person = person.clone();
        if let Ok(stored) = tx.library().load_person(&person.id) {
            person.hue = Some(stored.hue());
        }
        let person = &person;
        let t = self.write_tx()?;
        write_person(&t, person)?;
        refresh_person_meetings(&t, person.id.as_str())?;
        schema::bump_revision(&t)?;
        t.commit().map_err(db)?;
        tx.index_synced(&IndexItem::Person(person.id.clone()));
        Ok(())
    }

    /// Removes the person's rows; their speakers then show as `Speaker N`, as
    /// `meeting.md` does for unknown people. `false` when not indexed.
    pub fn remove_person(&mut self, tx: &WriteTx<'_>, id: &PersonId) -> Result<bool> {
        let t = self.write_tx()?;
        let removed = delete_person_rows(&t, id.as_str())?;
        refresh_person_meetings(&t, id.as_str())?;
        schema::bump_revision(&t)?;
        t.commit().map_err(db)?;
        tx.index_synced(&IndexItem::Person(id.clone()));
        Ok(removed)
    }

    /// Runs `f` inside one read transaction, so every statement it runs
    /// sees the same snapshot even while a writer commits; inside a
    /// transaction already open on this connection `f` simply runs in it.
    fn read_snapshot<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if !self.conn.is_autocommit() {
            return f();
        }
        let snapshot = self.conn.unchecked_transaction().map_err(db)?;
        let out = f()?;
        snapshot.commit().map_err(db)?;
        Ok(out)
    }

    fn write_tx(&mut self) -> Result<rusqlite::Transaction<'_>> {
        if self.read_only {
            return Err(Error::InvalidInput("the index was opened read-only".into()));
        }
        self.conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)
    }
}

/// Maps a SQLite error to a BroScribe error (`library_busy` on lock timeout).
fn db(e: rusqlite::Error) -> Error {
    match e.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => {
            Error::LibraryBusy(format!("search index ({e})"))
        }
        _ => Error::Internal(format!("index: {e}")),
    }
}

fn missing_index() -> Error {
    Error::NotFound("search index (run `broscribe index rebuild`)".into())
}

fn check_version(v: u32) -> Result<()> {
    if v == INDEX_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(Error::Schema(format!(
            "index schema version {v} differs from {INDEX_SCHEMA_VERSION}; \
             run `broscribe index rebuild`"
        )))
    }
}

/// Upgrades a migratable index (`kept`, its stored version) in place in an
/// IMMEDIATE transaction (re-checking the version inside it: another
/// connection may have upgraded it meanwhile) and returns the version now
/// readable. When the database cannot be written (read-only file or
/// connection, or a writer holding it past the busy timeout) the stored
/// version is kept.
fn migrate_if_writable(conn: &mut Connection, kept: u32) -> Result<u32> {
    let cannot_write = |e: &rusqlite::Error| {
        matches!(
            e.sqlite_error_code(),
            Some(ErrorCode::ReadOnly | ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        )
    };
    let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(e) if cannot_write(&e) => return Ok(kept),
        Err(e) => return Err(db(e)),
    };
    // Dropping `tx` on an early return rolls the upgrade back.
    let version = match schema::stored_version(&tx)? {
        None => return Err(missing_index()),
        Some(v) if schema::is_migratable(v) => match schema::migrate(&tx, v) {
            Ok(()) => INDEX_SCHEMA_VERSION,
            Err(e) if cannot_write(&e) => return Ok(kept),
            Err(e) => return Err(db(e)),
        },
        Some(v) => {
            check_version(v)?;
            v
        }
    };
    match tx.commit() {
        Ok(()) => Ok(version),
        Err(e) if cannot_write(&e) => Ok(kept),
        Err(e) => Err(db(e)),
    }
}

/// Attaches the vector cache read-only as schema `vec` when it exists and
/// holds the expected layout; `false` (nothing attached) otherwise, so a
/// missing or unreadable cache only disables semantic search.
fn attach_vectors(conn: &Connection, lib_root: &Path) -> bool {
    let path = VectorCache::path(lib_root);
    if !path.is_file() {
        return false;
    }
    if conn
        .execute("ATTACH DATABASE ?1 AS vec", [vectors::readonly_uri(&path)])
        .is_err()
    {
        return false;
    }
    let version: rusqlite::Result<String> = conn.query_row(
        "SELECT value FROM vec.meta WHERE key = 'schema_version'",
        [],
        |r| r.get(0),
    );
    let tables: rusqlite::Result<i64> = conn.query_row(
        "SELECT count(*) FROM vec.sqlite_master WHERE type = 'table' AND name = 'vectors'",
        [],
        |r| r.get(0),
    );
    let ok = version.is_ok_and(|v| v == vectors::CACHE_SCHEMA_VERSION.to_string())
        && tables.is_ok_and(|n| n == 1);
    if !ok {
        let _ = conn.execute_batch("DETACH DATABASE vec");
    }
    ok
}

/// Every distinct window text hash in the index.
fn text_hashes(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT text_sha256 FROM windows")
        .map_err(db)?;
    stmt.query_map([], |r| r.get(0))
        .map_err(db)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db)
}

/// A connection to the existing database at `path` for readers: read-write
/// without CREATE (a plain read-only connection cannot always attach to a
/// WAL database whose `-shm` or `-wal` file is missing; callers set
/// `query_only` once done upgrading). URI names are enabled for the
/// read-only `ATTACH` of the vector cache only.
fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(db)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(db)?;
    // Closing the last connection would otherwise checkpoint the WAL into
    // the database and delete the `-wal` file: file changes the library
    // watcher reports as index changes, which make views read again.
    // Writers checkpoint when they close.
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .map_err(db)?;
    Ok(conn)
}

/// How the index database stands ([`Index::health`]). Never creates,
/// upgrades or writes it: a reader connection with `query_only` reads the
/// layout marks. Without a database, a library with nothing to index needs
/// none.
fn database_state(lib_root: &Path) -> Result<IndexDatabase> {
    let path = Index::path(lib_root);
    if !path.is_file() {
        let indexable = match Library::open_existing(lib_root) {
            Ok(lib) => has_indexable_files(&lib)?,
            // No people, meeting year or index folder: nothing to index.
            Err(Error::NotFound(_)) => false,
            Err(e) => return Err(e),
        };
        return Ok(if indexable {
            IndexDatabase::Missing
        } else {
            IndexDatabase::NotNeeded
        });
    }
    let conn = open_reader(&path)?;
    conn.execute_batch("PRAGMA query_only = ON").map_err(db)?;
    Ok(match schema::stored_version(&conn)? {
        None => IndexDatabase::Incomplete,
        Some(v) if schema::is_migratable(v) => IndexDatabase::Outdated,
        Some(INDEX_SCHEMA_VERSION) if !schema::is_complete(&conn)? => IndexDatabase::Incomplete,
        Some(INDEX_SCHEMA_VERSION) if schema::rebuild_required(&conn)? => IndexDatabase::Outdated,
        Some(INDEX_SCHEMA_VERSION) => IndexDatabase::Ready,
        Some(_) => IndexDatabase::Incompatible,
    })
}

/// Whether the library holds a meeting folder or a person the index must
/// hold (folder names and file presence only; nothing is read).
fn has_indexable_files(lib: &Library) -> Result<bool> {
    Ok(!lib.list_meeting_dirs()?.is_empty() || !person_ids_on_disk(lib)?.is_empty())
}

/// A writable connection in WAL mode, creating `.index/` and the file if needed.
fn open_rw(lib_root: &Path) -> Result<Connection> {
    let dir = lib_root.join(paths::INDEX_DIR);
    fsutil::create_dir_durable(&dir)?;
    let conn = Connection::open_with_flags(
        Index::path(lib_root),
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(db)?;
    conn.busy_timeout(BUSY_TIMEOUT).map_err(db)?;
    conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))
        .map_err(db)?;
    conn.execute_batch("PRAGMA synchronous = NORMAL")
        .map_err(db)?;
    Ok(conn)
}

/// Case folding for name, alias, e-mail and tag matching (as in `library`).
fn fold(s: &str) -> String {
    s.trim().to_lowercase()
}

/// The snake_case name serde gives a unit enum variant.
fn enum_str<T: Serialize>(v: &T) -> Result<String> {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        Ok(other) => Err(Error::Internal(format!("not a unit variant: {other}"))),
        Err(e) => Err(Error::Internal(e.to_string())),
    }
}

/// The meeting's folder: the path its ID and creation data give, else the
/// transaction's lookup ([`WriteTx::meeting_dir`]).
fn meeting_dir(tx: &WriteTx<'_>, meeting: &Meeting) -> Result<PathBuf> {
    let lib = tx.library();
    let guess = lib.root().join(paths::meeting_rel_dir(
        &meeting.started_at,
        meeting.title.as_deref(),
        &meeting.id,
    ));
    if guess.join(paths::MEETING_JSON).is_file() {
        return Ok(guess);
    }
    tx.meeting_dir(&meeting.id)
}

fn pending_path(lib_root: &Path) -> PathBuf {
    lib_root.join(paths::INDEX_DIR).join(PENDING_REPAIR)
}

/// `.index/pending.json`; empty when missing, a full rebuild when unreadable.
fn read_pending(lib_root: &Path) -> Result<PendingRepair> {
    match fsutil::read_json::<PendingRepair>(&pending_path(lib_root)) {
        Ok(p) => Ok(p),
        Err(Error::NotFound(_)) => Ok(PendingRepair::default()),
        Err(Error::Schema(_)) => Ok(PendingRepair {
            rebuild: true,
            ..PendingRepair::default()
        }),
        Err(e) => Err(e),
    }
}

/// Writes `.index/pending.json` (durably), or removes it when empty.
fn write_pending(lib_root: &Path, pending: &PendingRepair) -> Result<()> {
    let path = pending_path(lib_root);
    if pending.is_empty() {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io_at(&path, e)),
        };
    }
    fsutil::create_dir_durable(&lib_root.join(paths::INDEX_DIR))?;
    fsutil::write_json_atomic(&path, pending)
}

/// Lists `item` in `.index/pending.json` before its files change (no write
/// when it is listed already). Under the library lock.
pub(crate) fn mark_pending(lib_root: &Path, item: &IndexItem) -> Result<()> {
    let mut pending = read_pending(lib_root)?;
    if pending.contains(item) {
        return Ok(());
    }
    pending.insert(item);
    write_pending(lib_root, &pending)
}

/// Takes `items`, whose rows now match their files, off
/// `.index/pending.json`. A list asking for a rebuild (unreadable, or left
/// by one) is left for it. Under the library lock.
pub(crate) fn clear_pending(lib_root: &Path, items: &BTreeSet<IndexItem>) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut pending = read_pending(lib_root)?;
    if pending.rebuild {
        return Ok(());
    }
    let before = pending.clone();
    for item in items {
        pending.remove(item);
    }
    if pending == before {
        return Ok(());
    }
    write_pending(lib_root, &pending)
}

/// `dir` relative to the library root, with `/` separators.
fn rel_dir(lib: &Library, dir: &Path) -> String {
    let rel = dir.strip_prefix(lib.root()).unwrap_or(dir);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// IDs of `people/<id>/` folders holding a `person.md`, sorted.
fn person_ids_on_disk(lib: &Library) -> Result<Vec<PersonId>> {
    let dir = lib.people_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io_at(&dir, e)),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(&dir, e))?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| PersonId::parse(n).ok())
        else {
            continue;
        };
        if entry.path().join(paths::PERSON_MD).is_file() {
            ids.push(id);
        }
    }
    ids.sort();
    Ok(ids)
}

/// Deletes every row of meeting `id`; `true` when it was indexed.
fn delete_meeting_rows(conn: &Connection, id: &str) -> Result<bool> {
    delete_transcript_rows(conn, id)?;
    delete_meeting_level_rows(conn, id)
}

/// Deletes the meeting's windows, utterances and their full-text rows.
fn delete_transcript_rows(conn: &Connection, id: &str) -> Result<()> {
    // External-content FTS: remove the indexed text before the rows go.
    conn.execute(
        "INSERT INTO utterances_fts(utterances_fts, rowid, text)
         SELECT 'delete', id, text FROM utterances WHERE meeting_id = ?1",
        [id],
    )
    .map_err(db)?;
    for table in ["utterances", "windows"] {
        conn.execute(&format!("DELETE FROM {table} WHERE meeting_id = ?1"), [id])
            .map_err(db)?;
    }
    Ok(())
}

/// Deletes the meeting's rows other than its transcript's; `true` when it
/// was indexed.
fn delete_meeting_level_rows(conn: &Connection, id: &str) -> Result<bool> {
    for table in [
        "speakers",
        "tags",
        "participants",
        "meetings_fts",
        "person_meetings",
    ] {
        conn.execute(&format!("DELETE FROM {table} WHERE meeting_id = ?1"), [id])
            .map_err(db)?;
    }
    enrichment::delete_rows(conn, id)?;
    let removed = conn
        .execute("DELETE FROM meetings WHERE id = ?1", [id])
        .map_err(db)?;
    Ok(removed > 0)
}

/// A `windows` row of a meeting, without its ID and meeting.
#[derive(Debug, PartialEq)]
struct WindowRow {
    speaker: Option<String>,
    person: Option<String>,
    first: i64,
    last: i64,
    start: f64,
    end: f64,
    text_sha256: String,
}

/// An `utterances` row of a meeting, without its row ID and meeting.
#[derive(Debug, PartialEq)]
struct UtteranceRow {
    segment: i64,
    speaker: Option<String>,
    person: Option<String>,
    start: f64,
    end: f64,
    text: String,
    window: String,
}

/// Whether meeting `id`'s stored windows (by ID) and utterances (in the
/// order they were written) are exactly `windows` and `utterances`: then
/// their full-text rows are current too.
fn transcript_rows_match(
    conn: &Connection,
    id: &str,
    windows: &BTreeMap<String, WindowRow>,
    utterances: &[UtteranceRow],
) -> Result<bool> {
    let mut stmt = conn
        .prepare(
            "SELECT id, speaker_id, person_id, first_segment, last_segment, start_s, end_s,
                    text_sha256
             FROM windows WHERE meeting_id = ?1 ORDER BY id",
        )
        .map_err(db)?;
    let stored = stmt
        .query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                WindowRow {
                    speaker: r.get(1)?,
                    person: r.get(2)?,
                    first: r.get(3)?,
                    last: r.get(4)?,
                    start: r.get(5)?,
                    end: r.get(6)?,
                    text_sha256: r.get(7)?,
                },
            ))
        })
        .map_err(db)?;
    let mut expected = windows.iter();
    for row in stored {
        let (wid, w) = row.map_err(db)?;
        match expected.next() {
            Some((e_id, e)) if *e_id == wid && *e == w => {}
            _ => return Ok(false),
        }
    }
    if expected.next().is_some() {
        return Ok(false);
    }
    let mut stmt = conn
        .prepare(
            "SELECT segment_id, speaker_id, person_id, start_s, end_s, text, window_id
             FROM utterances WHERE meeting_id = ?1 ORDER BY id",
        )
        .map_err(db)?;
    let stored = stmt
        .query_map([id], |r| {
            Ok(UtteranceRow {
                segment: r.get(0)?,
                speaker: r.get(1)?,
                person: r.get(2)?,
                start: r.get(3)?,
                end: r.get(4)?,
                text: r.get(5)?,
                window: r.get(6)?,
            })
        })
        .map_err(db)?;
    let mut expected = utterances.iter();
    for row in stored {
        if expected.next() != Some(&row.map_err(db)?) {
            return Ok(false);
        }
    }
    Ok(expected.next().is_none())
}

/// Writes every row of the meeting; returns its window count. The
/// transcript rows (windows, utterances, their full text) are rewritten
/// only when they differ from what is stored.
fn write_meeting(conn: &Connection, m: &Meeting, dir: &str) -> Result<usize> {
    let id = m.id.as_str();
    // A line correction's person, else the named speaker's (confirmed or
    // auto): the same attribution `meeting.md` shows.
    let attribution = crate::ops::effective_attribution(m);
    let windows = windows::attributed_windows(m, &attribution);
    let mut window_rows: BTreeMap<String, WindowRow> = BTreeMap::new();
    let mut window_of: HashMap<u32, &str> = HashMap::new();
    for w in &windows {
        let (Some(first), Some(last)) = (w.segment_ids.first(), w.segment_ids.last()) else {
            continue;
        };
        window_rows.insert(
            w.id.clone(),
            WindowRow {
                speaker: w.speaker.map(|s| s.to_string()),
                person: w.person_id.as_ref().map(|p| p.as_str().to_owned()),
                first: i64::from(*first),
                last: i64::from(*last),
                start: w.start,
                end: w.end,
                text_sha256: w.text_sha256.clone(),
            },
        );
        for seg in &w.segment_ids {
            window_of.insert(*seg, &w.id);
        }
    }
    let utterance_rows: Vec<UtteranceRow> = m
        .segments
        .iter()
        .zip(&attribution)
        .map(|(seg, who)| UtteranceRow {
            segment: i64::from(seg.id),
            speaker: seg
                .speaker
                .map(|s| crate::ops::effective_speaker(m, s).to_string()),
            person: who.person_id.as_ref().map(|p| p.as_str().to_owned()),
            start: seg.start,
            end: seg.end,
            text: seg.text.clone(),
            window: window_of
                .get(&seg.id)
                .copied()
                .unwrap_or_default()
                .to_owned(),
        })
        .collect();
    let keep_transcript = transcript_rows_match(conn, id, &window_rows, &utterance_rows)?;
    if !keep_transcript {
        delete_transcript_rows(conn, id)?;
    }
    delete_meeting_level_rows(conn, id)?;

    let mut languages: Vec<&str> = Vec::new();
    for span in &m.languages {
        if !languages.contains(&span.lang.as_str()) {
            languages.push(&span.lang);
        }
    }
    let language = (!languages.is_empty()).then(|| languages.join(","));
    let recording_state = m
        .recording
        .as_ref()
        .map(|r| enum_str(&r.state))
        .transpose()?;
    conn.execute(
        "INSERT INTO meetings(id, dir, title, started_at, started_unix, duration_s, source,
                              app, app_fold, language, recording_state, processing_status,
                              window_revision, setup)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            dir,
            m.title,
            m.started_at.to_rfc3339(),
            m.started_at.timestamp(),
            m.duration_s,
            enum_str(&m.source)?,
            m.app,
            m.app.as_deref().map(fold).filter(|a| !a.is_empty()),
            language,
            recording_state,
            enum_str(&m.processing.status)?,
            window_revision(&windows),
            m.setup,
        ],
    )
    .map_err(db)?;

    // One row per shown speaker: a group is its kept speaker's identity with
    // the group's talk time; grouped members get no row of their own. Its
    // label is what it shows as while unnamed (`Unknown (mic)`, `Speaker N`).
    for s in crate::ops::effective_speakers(m) {
        conn.execute(
            "INSERT OR REPLACE INTO speakers(meeting_id, speaker_id, person_id, assignment, score,
                                             suggested_person_id, suggested_score, talk_s,
                                             track, label)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                s.id.to_string(),
                s.person_id.as_ref().map(PersonId::as_str),
                enum_str(&s.assignment)?,
                s.score.map(f64::from),
                s.suggestion.as_ref().map(|x| x.person_id.as_str()),
                s.suggestion.as_ref().map(|x| f64::from(x.score)),
                crate::ops::effective_talk_s(m, s.id),
                s.track,
                crate::render::unnamed_speaker_label(m, s.id),
            ],
        )
        .map_err(db)?;
    }

    // Displayed tags: user ∪ enrichment − removed (plan §10.3).
    for tag in &m.effective_tags() {
        let folded = fold(tag);
        if folded.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO tags(meeting_id, tag, tag_fold) VALUES (?1, ?2, ?3)",
            params![id, tag.trim(), folded],
        )
        .map_err(db)?;
    }

    for p in &m.participants {
        conn.execute(
            "INSERT INTO participants(meeting_id, name, name_fold, email, email_fold,
                                      person_id, is_self)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                p.name,
                p.name.as_deref().map(fold),
                p.email,
                p.email.as_deref().map(fold),
                p.person_id.as_ref().map(PersonId::as_str),
                p.is_self,
            ],
        )
        .map_err(db)?;
    }

    // The people the meeting relates to, for `Index::person_meeting_candidates`:
    // named shown speakers, attributed lines, linked participants and the
    // targets of live decisions awaiting review (a candidate, never a
    // name the transcript shows).
    let mut relations: BTreeSet<(&PersonId, PersonRelation)> = BTreeSet::new();
    for s in crate::ops::effective_speakers(m) {
        if let Some(p) = s.named_person() {
            relations.insert((p, PersonRelation::Speaker));
        }
    }
    for a in &attribution {
        if let Some(p) = a.line_person() {
            relations.insert((p, PersonRelation::Line));
        }
    }
    for p in m.participants.iter().filter_map(|p| p.person_id.as_ref()) {
        relations.insert((p, PersonRelation::Participant));
    }
    for r in m.live_attributions.iter().filter(|r| r.needs_review()) {
        if let Some(p) = &r.person_id {
            relations.insert((p, PersonRelation::LiveReview));
        }
    }
    for (person, relation) in relations {
        conn.execute(
            "INSERT INTO person_meetings(person_id, meeting_id, relation) VALUES (?1, ?2, ?3)",
            params![person.as_str(), id, enum_str(&relation)?],
        )
        .map_err(db)?;
    }

    if !keep_transcript {
        for (wid, w) in &window_rows {
            conn.execute(
                "INSERT OR REPLACE INTO windows(id, meeting_id, speaker_id, person_id,
                                                first_segment, last_segment, start_s, end_s,
                                                text_sha256)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    wid,
                    id,
                    w.speaker,
                    w.person,
                    w.first,
                    w.last,
                    w.start,
                    w.end,
                    w.text_sha256,
                ],
            )
            .map_err(db)?;
        }
        for u in &utterance_rows {
            conn.execute(
                "INSERT INTO utterances(meeting_id, segment_id, speaker_id, person_id,
                                        start_s, end_s, text, window_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id, u.segment, u.speaker, u.person, u.start, u.end, u.text, u.window,
                ],
            )
            .map_err(db)?;
        }
        conn.execute(
            "INSERT INTO utterances_fts(rowid, text)
             SELECT id, text FROM utterances WHERE meeting_id = ?1",
            [id],
        )
        .map_err(db)?;
    }

    refresh_meeting_fts(conn, id)?;
    enrichment::write_rows(conn, m)?;
    Ok(windows.len())
}

/// Rewrites the meeting's `meetings_fts` row from the other tables: title,
/// tags, participants (names, e-mails, linked people) and named speakers.
fn refresh_meeting_fts(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM meetings_fts WHERE meeting_id = ?1", [id])
        .map_err(db)?;
    let title: Option<String> =
        match conn.query_row("SELECT title FROM meetings WHERE id = ?1", [id], |r| {
            r.get(0)
        }) {
            Ok(t) => t,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
            Err(e) => return Err(db(e)),
        };
    let tags = collect_strings(
        conn,
        "SELECT tag FROM tags WHERE meeting_id = ?1 ORDER BY tag_fold",
        id,
    )?;
    // Fixed order (participants as listed, then people by name), so the
    // text is the same however the rows were written.
    let mut participants = collect_strings(
        conn,
        "SELECT coalesce(name, '') || ' ' || coalesce(email, '') FROM participants
         WHERE meeting_id = ?1 ORDER BY rowid",
        id,
    )?;
    participants.extend(collect_strings(
        conn,
        "SELECT name FROM people WHERE id IN (
             SELECT person_id FROM participants WHERE meeting_id = ?1
             UNION
             SELECT person_id FROM speakers
             WHERE meeting_id = ?1 AND assignment IN ('confirmed', 'auto')
             UNION
             SELECT person_id FROM utterances
             WHERE meeting_id = ?1 AND person_id IS NOT NULL
         )
         ORDER BY name_fold, id",
        id,
    )?);
    conn.execute(
        "INSERT INTO meetings_fts(meeting_id, title, tags, participants)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            id,
            title.unwrap_or_default(),
            tags.join(" "),
            participants.join(" ")
        ],
    )
    .map_err(db)?;
    Ok(())
}

fn collect_strings(conn: &Connection, sql: &str, id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(sql).map_err(db)?;
    stmt.query_map([id], |r| r.get(0))
        .map_err(db)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db)
}

/// Refreshes `meetings_fts` and `enrichment_fts` for every meeting that
/// names or lists the person, attributes a line to them, or refers to them
/// in its enrichment or template-result rows (`{{<person ID>}}`, see
/// `enrichment::write_rows`). Writers only (current layout).
fn refresh_person_meetings(conn: &Connection, person: &str) -> Result<()> {
    let ids = collect_strings(
        conn,
        "SELECT meeting_id FROM speakers WHERE person_id = ?1
         UNION
         SELECT meeting_id FROM participants WHERE person_id = ?1
         UNION
         SELECT meeting_id FROM utterances WHERE person_id = ?1
         UNION
         SELECT meeting_id FROM enrichments WHERE instr(summary, '{{' || ?1 || '}}') > 0
         UNION
         SELECT meeting_id FROM decisions
         WHERE instr(text, '{{' || ?1 || '}}') > 0 OR by_name = '{{' || ?1 || '}}'
         UNION
         SELECT meeting_id FROM actions
         WHERE instr(text, '{{' || ?1 || '}}') > 0 OR owner_name = '{{' || ?1 || '}}'
         UNION
         SELECT meeting_id FROM template_results WHERE instr(markdown, '{{' || ?1 || '}}') > 0",
        person,
    )?;
    for id in ids {
        refresh_meeting_fts(conn, &id)?;
        enrichment::refresh_fts(conn, &id)?;
    }
    Ok(())
}

fn delete_person_rows(conn: &Connection, id: &str) -> Result<bool> {
    conn.execute("DELETE FROM person_emails WHERE person_id = ?1", [id])
        .map_err(db)?;
    conn.execute("DELETE FROM person_aliases WHERE person_id = ?1", [id])
        .map_err(db)?;
    let removed = conn
        .execute("DELETE FROM people WHERE id = ?1", [id])
        .map_err(db)?;
    Ok(removed > 0)
}

fn write_person(conn: &Connection, p: &Person) -> Result<()> {
    let id = p.id.as_str();
    delete_person_rows(conn, id)?;
    conn.execute(
        "INSERT INTO people(id, name, name_fold, is_self, needs_review, hue)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            p.name,
            fold(&p.name),
            p.is_self,
            p.needs_review,
            p.hue()
        ],
    )
    .map_err(db)?;
    for email in &p.emails {
        conn.execute(
            "INSERT INTO person_emails(person_id, email, email_fold) VALUES (?1, ?2, ?3)",
            params![id, email, fold(email)],
        )
        .map_err(db)?;
    }
    for alias in &p.aliases {
        conn.execute(
            "INSERT INTO person_aliases(person_id, alias, alias_fold) VALUES (?1, ?2, ?3)",
            params![id, alias, fold(alias)],
        )
        .map_err(db)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
