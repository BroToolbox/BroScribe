//! Index DDL and schema-version handling.

use rusqlite::{Connection, OptionalExtension};

use super::db;
use crate::{Error, Result};

/// Version of the SQL layout below; a mismatch requires `index rebuild`,
/// except the versions [`is_migratable`] accepts, which [`migrate`]
/// upgrades in place.
pub const INDEX_SCHEMA_VERSION: u32 = 9;

/// The oldest layout [`migrate`] upgrades in place, step by step:
/// - 4 → 5 added two `enrichments` columns, which every pre-5 row has at
///   their defaults (no `requested_model`, `model_reported = 0`);
/// - 5 → 6 added the `template_results` table. No meeting file held
///   template results before 6, so there is nothing to backfill;
/// - 6 → 7 added each shown speaker's `track` and unnamed `label`
///   (`render::unnamed_speaker_label`, e.g. `Unknown (mic)`) and the
///   meeting's `setup`. Rows written before 7 have them empty (they show
///   `Speaker N`, as before), so this step marks the index
///   [`rebuild_required`] until a rebuild rewrites every row;
/// - 7 → 8 added `person_meetings`, the meetings each person is related
///   to. Meetings indexed before 8 have no rows there, so this step marks
///   the index [`rebuild_required`] too, and
///   `Index::person_meeting_candidates` refuses to answer until a rebuild
///   backfilled it;
/// - 8 → 9 added each person's `hue`. Rows written before 9 hold 0, which
///   reads as the hue derived from the ID: no person had a stored hue
///   before 9, so that is the hue their file gives too, and no rebuild is
///   needed.
pub(super) const OLDEST_MIGRATABLE: u32 = 4;

/// Whether [`migrate`] upgrades layout `v` in place.
pub(super) fn is_migratable(v: u32) -> bool {
    (OLDEST_MIGRATABLE..INDEX_SCHEMA_VERSION).contains(&v)
}

const DDL: &str = "
CREATE TABLE meta(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE meetings(
    id TEXT PRIMARY KEY,
    dir TEXT NOT NULL,
    title TEXT,
    started_at TEXT NOT NULL,
    started_unix INTEGER NOT NULL,
    duration_s REAL NOT NULL,
    source TEXT NOT NULL,
    app TEXT,
    app_fold TEXT,
    language TEXT,
    recording_state TEXT,
    processing_status TEXT NOT NULL,
    window_revision TEXT NOT NULL,
    setup TEXT
);
CREATE INDEX meetings_started ON meetings(started_unix);
CREATE INDEX meetings_app ON meetings(app_fold);
CREATE TABLE speakers(
    meeting_id TEXT NOT NULL,
    speaker_id TEXT NOT NULL,
    person_id TEXT,
    assignment TEXT NOT NULL,
    score REAL,
    suggested_person_id TEXT,
    suggested_score REAL,
    talk_s REAL NOT NULL,
    track TEXT NOT NULL DEFAULT '',
    label TEXT NOT NULL DEFAULT '',
    PRIMARY KEY(meeting_id, speaker_id)
) WITHOUT ROWID;
CREATE INDEX speakers_person ON speakers(person_id);
CREATE TABLE people(
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    name_fold TEXT NOT NULL,
    is_self INTEGER NOT NULL,
    needs_review INTEGER NOT NULL,
    hue INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX people_name ON people(name_fold);
CREATE TABLE person_emails(
    person_id TEXT NOT NULL,
    email TEXT NOT NULL,
    email_fold TEXT NOT NULL
);
CREATE INDEX person_emails_person ON person_emails(person_id);
CREATE INDEX person_emails_email ON person_emails(email_fold);
CREATE TABLE person_aliases(
    person_id TEXT NOT NULL,
    alias TEXT NOT NULL,
    alias_fold TEXT NOT NULL
);
CREATE INDEX person_aliases_person ON person_aliases(person_id);
CREATE INDEX person_aliases_alias ON person_aliases(alias_fold);
CREATE TABLE participants(
    meeting_id TEXT NOT NULL,
    name TEXT,
    name_fold TEXT,
    email TEXT,
    email_fold TEXT,
    person_id TEXT,
    is_self INTEGER NOT NULL
);
CREATE INDEX participants_meeting ON participants(meeting_id);
CREATE INDEX participants_person ON participants(person_id);
CREATE TABLE tags(
    meeting_id TEXT NOT NULL,
    tag TEXT NOT NULL,
    tag_fold TEXT NOT NULL,
    PRIMARY KEY(meeting_id, tag_fold)
) WITHOUT ROWID;
CREATE INDEX tags_tag ON tags(tag_fold);
CREATE TABLE windows(
    id TEXT PRIMARY KEY,
    meeting_id TEXT NOT NULL,
    speaker_id TEXT,
    person_id TEXT,
    first_segment INTEGER NOT NULL,
    last_segment INTEGER NOT NULL,
    start_s REAL NOT NULL,
    end_s REAL NOT NULL,
    text_sha256 TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX windows_meeting ON windows(meeting_id, start_s);
CREATE INDEX windows_person ON windows(person_id);
CREATE INDEX windows_text ON windows(text_sha256);
CREATE TABLE utterances(
    id INTEGER PRIMARY KEY,
    meeting_id TEXT NOT NULL,
    segment_id INTEGER NOT NULL,
    speaker_id TEXT,
    person_id TEXT,
    start_s REAL NOT NULL,
    end_s REAL NOT NULL,
    text TEXT NOT NULL,
    window_id TEXT NOT NULL
);
CREATE INDEX utterances_meeting ON utterances(meeting_id, start_s);
CREATE INDEX utterances_person ON utterances(person_id);
CREATE INDEX utterances_window ON utterances(window_id);
CREATE VIRTUAL TABLE utterances_fts USING fts5(
    text,
    content='utterances',
    content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE meetings_fts USING fts5(
    meeting_id UNINDEXED,
    title,
    tags,
    participants,
    tokenize='unicode61 remove_diacritics 2'
);
CREATE TABLE enrichments(
    meeting_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    model TEXT,
    created_at TEXT NOT NULL,
    stale INTEGER NOT NULL,
    revision TEXT NOT NULL,
    summary TEXT NOT NULL,
    requested_model TEXT,
    model_reported INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE TABLE topics(
    meeting_id TEXT NOT NULL,
    topic TEXT NOT NULL,
    topic_fold TEXT NOT NULL,
    UNIQUE(meeting_id, topic_fold)
);
CREATE INDEX topics_topic ON topics(topic_fold);
CREATE TABLE keywords(
    meeting_id TEXT NOT NULL,
    keyword TEXT NOT NULL,
    keyword_fold TEXT NOT NULL,
    UNIQUE(meeting_id, keyword_fold)
);
CREATE INDEX keywords_keyword ON keywords(keyword_fold);
CREATE TABLE decisions(
    meeting_id TEXT NOT NULL,
    decision_id TEXT NOT NULL,
    ord INTEGER NOT NULL,
    text TEXT NOT NULL,
    at_s REAL NOT NULL,
    by_speaker TEXT,
    by_name TEXT,
    PRIMARY KEY(meeting_id, decision_id)
) WITHOUT ROWID;
CREATE TABLE actions(
    meeting_id TEXT NOT NULL,
    action_id TEXT NOT NULL,
    ord INTEGER NOT NULL,
    text TEXT NOT NULL,
    at_s REAL NOT NULL,
    owner_speaker TEXT,
    owner_name TEXT,
    owner_name_fold TEXT,
    due TEXT,
    status TEXT NOT NULL,
    status_source TEXT NOT NULL,
    status_updated_at TEXT,
    stale INTEGER NOT NULL,
    PRIMARY KEY(meeting_id, action_id)
) WITHOUT ROWID;
CREATE INDEX actions_status ON actions(status);
CREATE VIRTUAL TABLE enrichment_fts USING fts5(
    meeting_id UNINDEXED,
    kind UNINDEXED,
    item_id UNINDEXED,
    text,
    tokenize='unicode61 remove_diacritics 2'
);
CREATE TABLE template_results(
    meeting_id TEXT NOT NULL,
    template_id TEXT NOT NULL,
    template_name TEXT NOT NULL,
    stale INTEGER NOT NULL,
    revision TEXT NOT NULL,
    markdown TEXT NOT NULL,
    PRIMARY KEY(meeting_id, template_id)
) WITHOUT ROWID;
CREATE TABLE person_meetings(
    person_id TEXT NOT NULL,
    meeting_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    PRIMARY KEY(person_id, meeting_id, relation)
) WITHOUT ROWID;
CREATE INDEX person_meetings_meeting ON person_meetings(meeting_id);
";

/// Plain tables emptied by a rebuild (the FTS tables are reset separately).
const DATA_TABLES: &[&str] = &[
    "meetings",
    "speakers",
    "people",
    "person_emails",
    "person_aliases",
    "participants",
    "tags",
    "windows",
    "utterances",
    "enrichments",
    "topics",
    "keywords",
    "decisions",
    "actions",
    "template_results",
    "person_meetings",
];

/// The stored schema version: `None` for an empty database, `Some(0)` for
/// tables without a readable version.
pub(super) fn stored_version(conn: &Connection) -> Result<Option<u32>> {
    let objects: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )
        .map_err(db)?;
    if objects == 0 {
        return Ok(None);
    }
    let has_meta: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |r| r.get(0),
        )
        .map_err(db)?;
    if has_meta == 0 {
        return Ok(Some(0));
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(db)?;
    Ok(Some(value.and_then(|v| v.parse().ok()).unwrap_or(0)))
}

/// Creates every table in an empty database and records the version.
pub(super) fn create(conn: &Connection) -> Result<()> {
    conn.execute_batch(DDL).map_err(db)?;
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)",
        [INDEX_SCHEMA_VERSION.to_string()],
    )
    .map_err(db)?;
    renew_identity(conn).map_err(db)
}

/// Gives the index a new identity: a random `instance`, a new
/// `cursor_secret` (keys the query fingerprints of page cursors, never
/// leaves the index) and the next `revision`. Runs in the write transaction
/// that creates, rebuilds or upgrades the layout, so every cursor issued
/// before it is stale.
pub(super) fn renew_identity(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('instance', ?1), ('cursor_secret', ?2)",
        [
            format!("{:032x}", rand::random::<u128>()),
            format!(
                "{:032x}{:032x}",
                rand::random::<u128>(),
                rand::random::<u128>()
            ),
        ],
    )?;
    next_revision(conn)
}

/// Gives an index written before identities existed one (writers only).
pub(super) fn ensure_identity(conn: &Connection) -> Result<()> {
    let present: i64 = conn
        .query_row(
            "SELECT count(*) FROM meta WHERE key = 'instance'",
            [],
            |r| r.get(0),
        )
        .map_err(db)?;
    if present == 0 {
        renew_identity(conn).map_err(db)?;
    }
    Ok(())
}

/// Counts one write: runs inside every transaction that changes index rows,
/// so the revision moves exactly with committed writes.
pub(super) fn bump_revision(conn: &Connection) -> Result<()> {
    next_revision(conn).map_err(db)
}

fn next_revision(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('revision', '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
        [],
    )?;
    Ok(())
}

/// The identity stored in the `meta` of database `name` (`main`, or `vec`
/// for the attached vector cache): `(instance, cursor_secret, revision)`,
/// empty and 0 for what was written before identities existed.
pub(super) fn identity(conn: &Connection, name: &str) -> Result<(String, String, u64)> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT key, value FROM {name}.meta
             WHERE key IN ('instance', 'cursor_secret', 'revision')"
        ))
        .map_err(db)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(db)?;
    let mut out = (String::new(), String::new(), 0);
    for row in rows {
        match row.map_err(db)? {
            (key, value) if key == "instance" => out.0 = value,
            (key, value) if key == "cursor_secret" => out.1 = value,
            (_, value) => {
                out.2 = value.parse().map_err(|_| {
                    Error::Schema(format!("index revision {value:?} is not a number"))
                })?;
            }
        }
    }
    Ok(out)
}

/// Upgrades layout `from` (see [`is_migratable`]) to
/// [`INDEX_SCHEMA_VERSION`] one step at a time, keeping every row and the
/// completeness mark: the same result as a rebuild. Runs inside the
/// caller's write transaction.
pub(super) fn migrate(conn: &Connection, from: u32) -> rusqlite::Result<()> {
    if from <= 4 {
        conn.execute_batch(
            "ALTER TABLE enrichments ADD COLUMN requested_model TEXT;
             ALTER TABLE enrichments ADD COLUMN model_reported INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    if from <= 5 {
        conn.execute_batch(
            "CREATE TABLE template_results(
                 meeting_id TEXT NOT NULL,
                 template_id TEXT NOT NULL,
                 template_name TEXT NOT NULL,
                 stale INTEGER NOT NULL,
                 revision TEXT NOT NULL,
                 markdown TEXT NOT NULL,
                 PRIMARY KEY(meeting_id, template_id)
             ) WITHOUT ROWID;",
        )?;
    }
    if from <= 6 {
        conn.execute_batch(
            "ALTER TABLE speakers ADD COLUMN track TEXT NOT NULL DEFAULT '';
             ALTER TABLE speakers ADD COLUMN label TEXT NOT NULL DEFAULT '';
             ALTER TABLE meetings ADD COLUMN setup TEXT;",
        )?;
    }
    if from <= 7 {
        conn.execute_batch(
            "CREATE TABLE person_meetings(
                 person_id TEXT NOT NULL,
                 meeting_id TEXT NOT NULL,
                 relation TEXT NOT NULL,
                 PRIMARY KEY(person_id, meeting_id, relation)
             ) WITHOUT ROWID;
             CREATE INDEX person_meetings_meeting ON person_meetings(meeting_id);",
        )?;
        // The rows written before hold no labels (below 7) and no person
        // relations (below 8): never pass them off as current. In the same
        // transaction as the upgrade, so the mark is there exactly when the
        // new layout is.
        conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('rebuild_required', ?1)",
            [format!("layout {from} upgraded to {INDEX_SCHEMA_VERSION}")],
        )?;
    }
    if from <= 8 {
        conn.execute_batch("ALTER TABLE people ADD COLUMN hue INTEGER NOT NULL DEFAULT 0;")?;
    }
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
        [INDEX_SCHEMA_VERSION.to_string()],
    )?;
    // Rows read under the older layout may differ from what the same rows
    // read now: cursors issued before the upgrade are stale.
    renew_identity(conn)
}

/// Whether an in-place upgrade left rows written by an older layout that
/// only [`super::Index::rebuild`] makes current.
pub(super) fn rebuild_required(conn: &Connection) -> Result<bool> {
    let mark: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'rebuild_required'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(db)?;
    Ok(mark.is_some())
}

/// Clears the mark of [`rebuild_required`] (inside a rebuild).
pub(super) fn clear_rebuild_required(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM meta WHERE key = 'rebuild_required'", [])
        .map_err(db)?;
    Ok(())
}

/// A newly created index is hidden from readers until its files are accounted for.
pub(super) fn set_complete(conn: &Connection, complete: bool) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('complete', ?1)",
        [if complete { "1" } else { "0" }],
    )
    .map_err(db)?;
    Ok(())
}

pub(super) fn is_complete(conn: &Connection) -> Result<bool> {
    let complete: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'complete'", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(db)?;
    Ok(complete.as_deref() == Some("1"))
}

/// Drops every object (any schema version) so [`create`] can run.
pub(super) fn drop_all(conn: &Connection) -> Result<()> {
    // Views first, then virtual tables (which take their shadow tables with
    // them), then whatever plain tables remain (indexes and triggers follow).
    for (kind, filter) in [
        ("VIEW", "type = 'view'"),
        (
            "TABLE",
            "type = 'table' AND sql LIKE 'CREATE VIRTUAL TABLE%'",
        ),
        ("TABLE", "type = 'table' AND name NOT LIKE 'sqlite_%'"),
    ] {
        let names: Vec<String> = {
            let mut stmt = conn
                .prepare(&format!("SELECT name FROM sqlite_master WHERE {filter}"))
                .map_err(db)?;
            stmt.query_map([], |r| r.get(0))
                .map_err(db)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db)?
        };
        for name in names {
            conn.execute_batch(&format!(
                "DROP {kind} IF EXISTS \"{}\"",
                name.replace('"', "\"\"")
            ))
            .map_err(db)?;
        }
    }
    Ok(())
}

/// Deletes every row, FTS included. `utterances_fts` is external-content, so
/// it is reset with `'delete-all'` rather than `DELETE`.
pub(super) fn clear(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO utterances_fts(utterances_fts) VALUES ('delete-all')",
        [],
    )
    .map_err(db)?;
    conn.execute("DELETE FROM meetings_fts", []).map_err(db)?;
    conn.execute("DELETE FROM enrichment_fts", []).map_err(db)?;
    for table in DATA_TABLES {
        conn.execute(&format!("DELETE FROM {table}"), [])
            .map_err(db)?;
    }
    Ok(())
}
