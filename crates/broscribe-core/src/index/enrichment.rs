//! Generated-content rows (plan M2 §2, §9.5, §10.2, §10.11; design §7.1):
//! the meeting's summary, topics, keywords, decisions and actions, its
//! template results (`template_results`, index version 6), plus
//! `enrichment_fts` (`kind` = `summary`, `decision`, `action` or
//! `template`).
//!
//! The tables keep the prose as stored in `meeting.json` (with `{{S#}}`
//! placeholders), except that line labels (`{{P#}}`, and `by_name` /
//! `owner_name` equal to a label) are stored as `{{<person ID>}}` through the
//! `line_people` of the enrichment or template result the text belongs to
//! (`Unknown` without an entry). The FTS rows and every value returned by
//! the queries carry the prose with the placeholders replaced by current
//! names ([`render::substitute_speakers`]). Names come from the index's own
//! `speakers` and `people` rows, so renaming or naming refreshes them
//! without reading files.

use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset, NaiveDate};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::page::{
    CursorRequest, Dir, Page, PageKey, PageKind, after_sql, invalid_key, unpaged_limit,
};
use super::{Index, db, enum_str, fold, fts_query};
use crate::render::{self, PeopleLookup};
use crate::schema::{ActionStatus, Assignment, Meeting, Person, Source, Speaker, StatusSource};
use crate::{Error, MeetingId, PersonId, Result, SpeakerId};

/// Tables holding one meeting's generated-content rows (FTS excluded).
pub(super) const TABLES: &[&str] = &[
    "enrichments",
    "topics",
    "keywords",
    "decisions",
    "actions",
    "template_results",
];

/// One template result found by [`Index::search_templates`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TemplateHit {
    pub meeting_id: MeetingId,
    /// Always `template`.
    pub kind: &'static str,
    pub template_id: String,
    pub template_name: String,
    /// The matching passage, names substituted, terms marked `**…**`.
    pub snippet: String,
}

/// Filters of [`Index::list_actions`].
#[derive(Clone, Debug, Default)]
pub struct ActionQuery {
    /// Actions owned by this person: the owner speaker is named as them, or
    /// (without a named owner speaker) the owner is a line labelled as them
    /// or the provider's owner name is their name or an alias.
    pub owner: Option<PersonId>,
    pub status: Option<ActionStatus>,
    /// Meetings that started at or after this time.
    pub since: Option<DateTime<FixedOffset>>,
    /// Meetings that started before this time.
    pub until: Option<DateTime<FixedOffset>>,
    pub meeting: Option<MeetingId>,
    /// Words the action text must contain (names substituted).
    pub text: Option<String>,
    pub limit: Option<usize>,
}

/// Filters of [`Index::list_decisions`].
#[derive(Clone, Debug, Default)]
pub struct DecisionQuery {
    /// Meetings with an enrichment topic containing this (case-insensitive).
    pub topic: Option<String>,
    pub since: Option<DateTime<FixedOffset>>,
    pub until: Option<DateTime<FixedOffset>>,
    pub meeting: Option<MeetingId>,
    /// Words the decision text must contain (names substituted).
    pub text: Option<String>,
    pub limit: Option<usize>,
}

/// One action item with names resolved.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ActionItem {
    pub meeting_id: MeetingId,
    pub meeting_title: Option<String>,
    pub started_at: DateTime<FixedOffset>,
    pub action_id: String,
    /// Text with speaker placeholders replaced by current names.
    pub text: String,
    pub owner_speaker: Option<SpeakerId>,
    /// The person the owner speaker is named as, else the line person the
    /// owner label refers to.
    pub owner_person_id: Option<PersonId>,
    /// Name shown for the owner: the speaker's label, else the provider's
    /// owner name; `None` without an owner.
    pub owner: Option<String>,
    pub due: Option<NaiveDate>,
    pub at_s: f64,
    pub status: ActionStatus,
    pub status_source: StatusSource,
    pub status_updated_at: Option<DateTime<FixedOffset>>,
    /// A user-set action the latest enrichment no longer produced.
    pub stale: bool,
    /// The meeting's enrichment predates changes to its transcript
    /// (reprocessing or line attribution).
    pub enrichment_stale: bool,
}

/// One decision with names resolved.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DecisionItem {
    pub meeting_id: MeetingId,
    pub meeting_title: Option<String>,
    pub started_at: DateTime<FixedOffset>,
    pub decision_id: String,
    pub text: String,
    pub by_speaker: Option<SpeakerId>,
    pub by_person_id: Option<PersonId>,
    pub by: Option<String>,
    pub at_s: f64,
    /// The meeting's enrichment topics.
    pub topics: Vec<String>,
    pub enrichment_stale: bool,
}

/// Deletes the meeting's enrichment rows and FTS entries.
pub(super) fn delete_rows(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM enrichment_fts WHERE meeting_id = ?1", [id])
        .map_err(db)?;
    for table in TABLES {
        conn.execute(&format!("DELETE FROM {table} WHERE meeting_id = ?1"), [id])
            .map_err(db)?;
    }
    Ok(())
}

/// Writes the meeting's enrichment and template-result rows (its `speakers`
/// rows must already be written) and their FTS entries. Rows are deleted by
/// the caller first. Only writers call this, and they hold the current
/// layout.
pub(super) fn write_rows(conn: &Connection, m: &Meeting) -> Result<()> {
    let id = m.id.as_str();
    for r in &m.template_results {
        conn.execute(
            "INSERT OR REPLACE INTO template_results(meeting_id, template_id, template_name,
                                                    stale, revision, markdown)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                r.template_id,
                r.template_name,
                crate::ops::template_is_stale(m, r),
                r.revision,
                shown_speakers(m, &line_tokens(&r.line_people, &r.markdown)),
            ],
        )
        .map_err(db)?;
    }
    let Some(e) = &m.enrichment else {
        return refresh_fts(conn, id);
    };
    // Speaker references are stored as the shown speaker (a grouped member's
    // group), whose `speakers` row exists.
    let lp = &e.line_people;
    let shown = |s: Option<SpeakerId>| s.map(|s| crate::ops::effective_speaker(m, s).to_string());
    let prose = |t: &str| shown_speakers(m, &line_tokens(lp, t));
    let name =
        |n: &Option<String>| line_name_token(lp, n.as_deref()).map(|t| shown_speakers(m, &t));
    let summary = prose(&e.summary);
    conn.execute(
        "INSERT INTO enrichments(meeting_id, provider, model, created_at, stale, revision, summary,
                                 requested_model, model_reported)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            e.provider,
            e.model,
            e.created_at.to_rfc3339(),
            crate::ops::enrichment_is_stale(m),
            e.revision,
            summary,
            e.requested_model,
            e.model_reported,
        ],
    )
    .map_err(db)?;
    for (table, column, labels) in [
        ("topics", "topic", &e.topics),
        ("keywords", "keyword", &e.keywords),
    ] {
        for label in labels {
            let folded = fold(label);
            if folded.is_empty() {
                continue;
            }
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO {table}(meeting_id, {column}, {column}_fold)
                     VALUES (?1, ?2, ?3)"
                ),
                params![id, label.trim(), folded],
            )
            .map_err(db)?;
        }
    }
    for (ord, d) in e.decisions.iter().enumerate() {
        conn.execute(
            "INSERT OR REPLACE INTO decisions(meeting_id, decision_id, ord, text, at_s,
                                              by_speaker, by_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                d.id,
                i64::try_from(ord).unwrap_or(i64::MAX),
                prose(&d.text),
                d.at_s,
                shown(d.by_speaker),
                name(&d.by_name),
            ],
        )
        .map_err(db)?;
    }
    for (ord, a) in e.actions.iter().enumerate() {
        let owner_name = name(&a.owner_name);
        conn.execute(
            "INSERT OR REPLACE INTO actions(meeting_id, action_id, ord, text, at_s, owner_speaker,
                                            owner_name, owner_name_fold, due, status,
                                            status_source, status_updated_at, stale)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id,
                a.id,
                i64::try_from(ord).unwrap_or(i64::MAX),
                prose(&a.text),
                a.at_s,
                shown(a.owner_speaker),
                owner_name,
                owner_name.as_deref().map(fold),
                a.due.map(|d| d.format("%Y-%m-%d").to_string()),
                enum_str(&a.status)?,
                enum_str(&a.status_source)?,
                a.status_updated_at.map(|t| t.to_rfc3339()),
                a.stale,
            ],
        )
        .map_err(db)?;
    }
    refresh_fts(conn, id)
}

/// Prose with its `{{S#}}` placeholders rewritten to the shown speaker.
fn shown_speakers(m: &Meeting, text: &str) -> String {
    if m.speaker_groups.is_empty() {
        return text.to_owned();
    }
    render::map_placeholders(text, |inner| {
        let id = SpeakerId::parse(inner).ok()?;
        Some(format!("{{{{{}}}}}", crate::ops::effective_speaker(m, id)))
    })
}

/// `{{<person ID>}}`: how the index stores a line person reference.
fn person_token(person: &PersonId) -> String {
    format!("{{{{{person}}}}}")
}

/// The line person a label stands for as a token, else `Unknown`.
fn label_token(line_people: &BTreeMap<String, PersonId>, label: &str) -> String {
    line_people
        .get(label)
        .map_or_else(|| "Unknown".to_owned(), person_token)
}

/// Prose with its `{{P#}}` placeholders stored as person tokens, through
/// the `line_people` of the content it belongs to.
fn line_tokens(line_people: &BTreeMap<String, PersonId>, text: &str) -> String {
    render::map_placeholders(text, |inner| {
        render::is_line_label(inner).then(|| label_token(line_people, inner))
    })
}

/// A provider name that is a line label, stored as a person token.
fn line_name_token(line_people: &BTreeMap<String, PersonId>, name: Option<&str>) -> Option<String> {
    name.map(|n| {
        if render::is_line_label(n.trim()) {
            label_token(line_people, n.trim())
        } else {
            n.to_owned()
        }
    })
}

/// The person of a stored person token (exactly `{{<person ID>}}`).
fn token_person(s: &str) -> Option<PersonId> {
    s.strip_prefix("{{")
        .and_then(|s| s.strip_suffix("}}"))
        .and_then(|s| PersonId::parse(s).ok())
}

/// Adds every person `text` names by token to `out`.
fn token_people(text: &str, out: &mut Vec<String>) {
    render::map_placeholders(text, |inner| {
        if PersonId::parse(inner).is_ok() && !out.iter().any(|o| o == inner) {
            out.push(inner.to_owned());
        }
        None
    });
}

/// Rewrites the meeting's `enrichment_fts` rows from the stored prose with
/// the speakers' current names (after naming or a person change): the
/// enrichment's summary, decisions and actions, and every template result,
/// with or without an enrichment. Writers only (current layout).
pub(super) fn refresh_fts(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM enrichment_fts WHERE meeting_id = ?1", [id])
        .map_err(db)?;
    let summary: Option<String> = conn
        .query_row(
            "SELECT summary FROM enrichments WHERE meeting_id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db)?;
    let templates: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare("SELECT template_id, markdown FROM template_results WHERE meeting_id = ?1")
            .map_err(db)?;
        stmt.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?
    };
    if summary.is_none() && templates.is_empty() {
        return Ok(());
    }
    let names = Names::load(conn, id, true, true)?;
    let mut rows: Vec<(&str, String, String)> = Vec::new();
    if let Some(s) = summary.filter(|s| !s.trim().is_empty()) {
        rows.push(("summary", String::new(), names.substitute(&s)));
    }
    for (kind, sql) in [
        (
            "decision",
            "SELECT decision_id, text FROM decisions WHERE meeting_id = ?1",
        ),
        (
            "action",
            "SELECT action_id, text FROM actions WHERE meeting_id = ?1",
        ),
    ] {
        let mut stmt = conn.prepare(sql).map_err(db)?;
        let items: Vec<(String, String)> = stmt
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?;
        for (item, text) in items {
            rows.push((kind, item, names.substitute(&text)));
        }
    }
    for (template, markdown) in templates {
        if !markdown.trim().is_empty() {
            rows.push(("template", template, names.substitute(&markdown)));
        }
    }
    for (kind, item, text) in rows {
        conn.execute(
            "INSERT INTO enrichment_fts(meeting_id, kind, item_id, text) VALUES (?1, ?2, ?3, ?4)",
            params![id, kind, item, text],
        )
        .map_err(db)?;
    }
    Ok(())
}

/// A meeting's speakers and the people they are named as or its generated
/// rows name by token, read from the index, for
/// [`render::substitute_speakers`] and [`render::speaker_label`].
struct Names {
    meeting: Meeting,
    people: PeopleLookup,
}

impl Names {
    /// `templates`: also the people template-result rows name (index layout
    /// 6 and later only). `labels`: the speakers' tracks and the meeting's
    /// setup, so unnamed speakers show as `meeting.md` shows them, e.g.
    /// `Unknown (mic)` (layout 7 and later only).
    fn load(conn: &Connection, id: &str, templates: bool, labels: bool) -> Result<Self> {
        let meeting_id = MeetingId::parse(id).map_err(|e| bad_row("meeting ID", e))?;
        let mut meeting = Meeting::new(meeting_id, Source::Import, crate::schema::now());
        let track = if labels { "track" } else { "''" };
        let mut stmt = conn
            .prepare(&format!(
                "SELECT speaker_id, person_id, assignment, {track} FROM speakers
                 WHERE meeting_id = ?1"
            ))
            .map_err(db)?;
        let rows: Vec<(String, Option<String>, String, String)> = stmt
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?;
        for (speaker, person, assignment, track) in rows {
            let mut s = Speaker::new(
                SpeakerId::parse(&speaker).map_err(|e| bad_row("speaker ID", e))?,
                &track,
            );
            s.person_id = person
                .map(|p| PersonId::parse(&p).map_err(|e| bad_row("person ID", e)))
                .transpose()?;
            s.assignment = parse_enum::<Assignment>(assignment)?;
            meeting.speakers.push(s);
        }
        if labels {
            meeting.setup = conn
                .query_row("SELECT setup FROM meetings WHERE id = ?1", [id], |r| {
                    r.get::<_, Option<String>>(0)
                })
                .optional()
                .map_err(db)?
                .flatten();
        }
        let mut stmt = conn
            .prepare(
                "SELECT id, name, is_self FROM people WHERE id IN (
                     SELECT person_id FROM speakers WHERE meeting_id = ?1)",
            )
            .map_err(db)?;
        let mut people: Vec<(String, String, bool)> = stmt
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?;
        // People the generated rows name by token (line references).
        let mut tokens = Vec::new();
        let mut queries = vec![
            "SELECT summary FROM enrichments WHERE meeting_id = ?1",
            "SELECT text FROM decisions WHERE meeting_id = ?1
             UNION ALL SELECT by_name FROM decisions WHERE meeting_id = ?1 AND by_name IS NOT NULL",
            "SELECT text FROM actions WHERE meeting_id = ?1
             UNION ALL SELECT owner_name FROM actions WHERE meeting_id = ?1 AND owner_name IS NOT NULL",
        ];
        if templates {
            queries.push("SELECT markdown FROM template_results WHERE meeting_id = ?1");
        }
        for sql in queries {
            let mut stmt = conn.prepare(sql).map_err(db)?;
            let texts: Vec<String> = stmt
                .query_map([id], |r| r.get(0))
                .map_err(db)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db)?;
            for t in &texts {
                token_people(t, &mut tokens);
            }
        }
        tokens.retain(|t| !people.iter().any(|(p, _, _)| p == t));
        for t in tokens {
            let row = conn
                .query_row(
                    "SELECT id, name, is_self FROM people WHERE id = ?1",
                    [&t],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(db)?;
            people.extend(row);
        }
        let mut list = Vec::with_capacity(people.len());
        for (pid, name, is_self) in people {
            let mut p = Person::new(
                PersonId::parse(&pid).map_err(|e| bad_row("person ID", e))?,
                name,
            );
            p.is_self = is_self;
            list.push(p);
        }
        Ok(Self {
            meeting,
            people: PeopleLookup::new(list),
        })
    }

    /// Stored prose has line labels as person tokens already: no label map.
    fn substitute(&self, text: &str) -> String {
        render::substitute_speakers(&self.meeting, &BTreeMap::new(), text, &self.people)
    }

    /// The person a speaker is named as (`confirmed` or `auto`), else the
    /// line person a stored owner token names.
    fn person(&self, speaker: Option<SpeakerId>, name: Option<&str>) -> Option<PersonId> {
        match speaker.and_then(|s| self.meeting.speaker(s)) {
            Some(s) => s.named_person().cloned(),
            None => name.and_then(token_person),
        }
    }

    /// The speaker's label when it exists in the meeting, else `fallback`
    /// with its placeholders (a line person token) substituted.
    fn label(&self, speaker: Option<SpeakerId>, fallback: Option<String>) -> Option<String> {
        match speaker.filter(|s| self.meeting.speaker(*s).is_some()) {
            Some(s) => Some(render::speaker_label(&self.meeting, Some(s), &self.people)),
            None => fallback
                .filter(|n| !n.trim().is_empty())
                .map(|n| self.substitute(&n)),
        }
    }
}

/// Query text with its positional parameters.
struct Sql {
    text: String,
    params: Vec<Value>,
}

impl Sql {
    fn bind(&mut self, v: impl Into<Value>) -> usize {
        self.params.push(v.into());
        self.params.len()
    }

    fn push(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn meeting_filters(
        &mut self,
        since: Option<&DateTime<FixedOffset>>,
        until: Option<&DateTime<FixedOffset>>,
        meeting: Option<&MeetingId>,
    ) {
        if let Some(since) = since {
            let n = self.bind(since.timestamp());
            self.push(&format!(" AND m.started_unix >= ?{n}"));
        }
        if let Some(until) = until {
            let n = self.bind(until.timestamp());
            self.push(&format!(" AND m.started_unix < ?{n}"));
        }
        if let Some(id) = meeting {
            let n = self.bind(id.as_str().to_owned());
            self.push(&format!(" AND m.id = ?{n}"));
        }
    }

    /// Restricts item alias `x` (kind `kind`) to FTS matches of `text`;
    /// `false` when the text has no searchable term (nothing matches).
    fn text_filter(&mut self, text: Option<&str>, kind: &str, id_column: &str) -> bool {
        let Some(text) = text else {
            return true;
        };
        let Some(q) = fts_query(text) else {
            return false;
        };
        let n = self.bind(q);
        let k = self.bind(kind.to_owned());
        self.push(&format!(
            " AND EXISTS (SELECT 1 FROM enrichment_fts f WHERE f.enrichment_fts MATCH ?{n}
                  AND f.meeting_id = x.meeting_id AND f.kind = ?{k} AND f.item_id = x.{id_column})"
        ));
        true
    }

    fn limit(&mut self, limit: Option<usize>) {
        if let Some(limit) = limit {
            let n = self.bind(i64::try_from(limit).unwrap_or(i64::MAX));
            self.push(&format!(" LIMIT ?{n}"));
        }
    }
}

/// One row of [`action_sql`]: the item's columns, then the meeting's
/// `started_unix` and the item's `ord` (its sort key).
type ActionRow = (
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    f64,
    String,
    String,
    Option<String>,
    bool,
    bool,
    i64,
    i64,
);

/// The action rows `q` selects, without order or limit; `None` when its
/// text holds no searchable term (nothing matches).
fn action_sql(q: &ActionQuery) -> Result<Option<Sql>> {
    let mut sql = Sql {
        text: "SELECT m.id, m.title, m.started_at, x.action_id, x.text, x.owner_speaker,
                      x.owner_name, x.due, x.at_s, x.status, x.status_source,
                      x.status_updated_at, x.stale, e.stale, m.started_unix, x.ord
               FROM actions x
               JOIN meetings m ON m.id = x.meeting_id
               JOIN enrichments e ON e.meeting_id = x.meeting_id
               WHERE 1 = 1"
            .into(),
        params: Vec::new(),
    };
    if let Some(status) = &q.status {
        let n = sql.bind(enum_str(status)?);
        sql.push(&format!(" AND x.status = ?{n}"));
    }
    if let Some(owner) = &q.owner {
        let n = sql.bind(owner.as_str().to_owned());
        sql.push(&format!(
            " AND (EXISTS (SELECT 1 FROM speakers s WHERE s.meeting_id = x.meeting_id
                    AND s.speaker_id = x.owner_speaker AND s.person_id = ?{n}
                    AND s.assignment IN ('confirmed', 'auto'))
               OR (NOT EXISTS (SELECT 1 FROM speakers s WHERE s.meeting_id = x.meeting_id
                    AND s.speaker_id = x.owner_speaker AND s.person_id IS NOT NULL
                    AND s.assignment IN ('confirmed', 'auto'))
                   AND (x.owner_name = '{{{{' || ?{n} || '}}}}'
                        OR x.owner_name_fold IN (
                            SELECT name_fold FROM people WHERE id = ?{n}
                            UNION SELECT alias_fold FROM person_aliases
                            WHERE person_id = ?{n}))))"
        ));
    }
    sql.meeting_filters(q.since.as_ref(), q.until.as_ref(), q.meeting.as_ref());
    if !sql.text_filter(q.text.as_deref(), "action", "action_id") {
        return Ok(None);
    }
    Ok(Some(sql))
}

type DecisionRow = (
    String,
    Option<String>,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    f64,
    bool,
);

impl Index {
    /// What `provider`'s requested models last resolved to: for each
    /// requested model (`None`: the provider's default), the model reported
    /// by the newest enrichment with a reported model. Records without one
    /// (requested model only, or written before models were reported) are
    /// ignored. Empty when a read-only open kept a layout older than 5
    /// (before the reported-model columns).
    pub fn resolved_models(&self, provider: &str) -> Result<BTreeMap<Option<String>, String>> {
        if self.version < 5 {
            return Ok(BTreeMap::new());
        }
        let mut out: BTreeMap<Option<String>, (DateTime<FixedOffset>, String)> = BTreeMap::new();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT requested_model, model, created_at FROM enrichments
                 WHERE provider = ?1 AND model_reported = 1 AND model IS NOT NULL",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map([provider], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(db)?;
        for row in rows {
            let (requested, model, created_at) = row.map_err(db)?;
            // Offsets may differ between rows: compare instants, not text.
            let at = timestamp(&created_at)?;
            match out.get(&requested) {
                Some((newest, _)) if *newest >= at => {}
                _ => {
                    out.insert(requested, (at, model));
                }
            }
        }
        Ok(out.into_iter().map(|(k, (_, m))| (k, m)).collect())
    }

    /// Template results matching `text` (keyword search over their
    /// Markdown with current names), best match first; at most `limit`.
    /// Empty below index layout 6 (a read-only open that could not migrate)
    /// and for text without a searchable term.
    pub fn search_templates(&self, text: &str, limit: Option<usize>) -> Result<Vec<TemplateHit>> {
        if self.version < 6 {
            return Ok(Vec::new());
        }
        let Some(q) = fts_query(text) else {
            return Ok(Vec::new());
        };
        use super::search::{ELLIPSIS, MARK_END, MARK_START, SNIPPET_TOKENS};
        let sql = format!(
            "SELECT f.meeting_id, f.item_id, t.template_name,
                    snippet(enrichment_fts, 3, '{MARK_START}', '{MARK_END}', '{ELLIPSIS}', {SNIPPET_TOKENS})
             FROM enrichment_fts f
             JOIN template_results t ON t.meeting_id = f.meeting_id AND t.template_id = f.item_id
             WHERE enrichment_fts MATCH ?1 AND f.kind = 'template'
             ORDER BY rank, f.meeting_id, f.item_id
             LIMIT ?2"
        );
        let limit = limit.map_or(-1, |l| i64::try_from(l).unwrap_or(i64::MAX));
        let mut stmt = self.conn.prepare(&sql).map_err(db)?;
        let rows: Vec<(String, String, String, String)> = stmt
            .query_map(params![q, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?;
        rows.into_iter()
            .map(|(meeting, template_id, template_name, snippet)| {
                Ok(TemplateHit {
                    meeting_id: MeetingId::parse(&meeting).map_err(|e| bad_row("meeting ID", e))?,
                    kind: "template",
                    template_id,
                    template_name,
                    snippet,
                })
            })
            .collect()
    }

    /// Action items, newest meeting first, in meeting order within one.
    pub fn list_actions(&self, q: &ActionQuery) -> Result<Vec<ActionItem>> {
        self.read_snapshot(|| self.list_actions_in_snapshot(q))
    }

    fn list_actions_in_snapshot(&self, q: &ActionQuery) -> Result<Vec<ActionItem>> {
        let Some(mut sql) = action_sql(q)? else {
            return Ok(Vec::new());
        };
        sql.push(" ORDER BY m.started_unix DESC, m.id, x.ord");
        sql.limit(q.limit);
        let rows = self.action_rows(&sql)?;
        let mut names: Option<(String, Names)> = None;
        rows.into_iter()
            .map(|row| self.action_item(&mut names, row))
            .collect()
    }

    /// One page of [`Index::list_actions`]: the same items in the same
    /// order, read in one snapshot. The query's `limit` must be unset.
    pub fn list_actions_page(
        &self,
        q: &ActionQuery,
        page: &CursorRequest,
    ) -> Result<Page<ActionItem>> {
        unpaged_limit(q.limit)?;
        self.read_page(
            PageKind::Actions,
            page,
            false,
            |fp| {
                fp.text("owner", q.owner.as_ref().map(PersonId::as_str));
                let status = q.status.as_ref().map(enum_str).transpose()?;
                fp.text("status", status.as_deref());
                fp.int("since", q.since.map(|t| t.timestamp()));
                fp.int("until", q.until.map(|t| t.timestamp()));
                fp.text("meeting", q.meeting.as_ref().map(MeetingId::as_str));
                // As the text filter applies it; `""` matches nothing.
                let text = q.text.as_deref().map(|t| fts_query(t).unwrap_or_default());
                fp.text("text", text.as_deref());
                Ok(())
            },
            |after, take| {
                let Some(mut sql) = action_sql(q)? else {
                    return Ok(Vec::new());
                };
                match after {
                    None => {}
                    Some(PageKey::Action {
                        started,
                        meeting,
                        ord,
                    }) => {
                        let condition = after_sql(
                            vec![
                                ("m.started_unix", Dir::Desc, Value::Integer(*started)),
                                ("m.id", Dir::Asc, Value::Text(meeting.as_str().to_owned())),
                                ("x.ord", Dir::Asc, Value::Integer(*ord)),
                            ],
                            |v| sql.bind(v),
                        );
                        sql.push(&format!(" AND {condition}"));
                    }
                    Some(_) => return Err(invalid_key()),
                }
                sql.push(" ORDER BY m.started_unix DESC, m.id, x.ord");
                sql.limit(Some(take));
                let rows = self.action_rows(&sql)?;
                let mut names: Option<(String, Names)> = None;
                rows.into_iter()
                    .map(|row| {
                        let key = PageKey::Action {
                            started: row.14,
                            meeting: MeetingId::parse(&row.0)
                                .map_err(|e| bad_row("meeting ID", e))?,
                            ord: row.15,
                        };
                        Ok((self.action_item(&mut names, row)?, key))
                    })
                    .collect()
            },
        )
    }

    fn action_rows(&self, sql: &Sql) -> Result<Vec<ActionRow>> {
        let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
        stmt.query_map(params_from_iter(sql.params.iter()), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
                r.get(14)?,
                r.get(15)?,
            ))
        })
        .map_err(db)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db)
    }

    /// The item of an [`action_sql`] row, names resolved through `names`
    /// (the previous row's meeting's, reused while rows stay in it).
    fn action_item(
        &self,
        names: &mut Option<(String, Names)>,
        row: ActionRow,
    ) -> Result<ActionItem> {
        let (id, title, started, action_id, text, owner, owner_name, due, at_s) = (
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8,
        );
        let n = meeting_names(self, names, &id)?;
        let owner_speaker = owner
            .as_deref()
            .map(|s| SpeakerId::parse(s).map_err(|e| bad_row("speaker ID", e)))
            .transpose()?;
        Ok(ActionItem {
            meeting_id: MeetingId::parse(&id).map_err(|e| bad_row("meeting ID", e))?,
            meeting_title: title,
            started_at: timestamp(&started)?,
            action_id,
            text: n.substitute(&text),
            owner_speaker,
            owner_person_id: n.person(owner_speaker, owner_name.as_deref()),
            owner: n.label(owner_speaker, owner_name),
            due: due
                .map(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").map_err(|e| bad_row("date", e)))
                .transpose()?,
            at_s,
            status: parse_enum(row.9)?,
            status_source: parse_enum(row.10)?,
            status_updated_at: row.11.as_deref().map(timestamp).transpose()?,
            stale: row.12,
            enrichment_stale: row.13,
        })
    }

    /// Decisions, newest meeting first, in meeting order within one.
    pub fn list_decisions(&self, q: &DecisionQuery) -> Result<Vec<DecisionItem>> {
        self.read_snapshot(|| self.list_decisions_in_snapshot(q))
    }

    fn list_decisions_in_snapshot(&self, q: &DecisionQuery) -> Result<Vec<DecisionItem>> {
        let mut sql = Sql {
            text: "SELECT m.id, m.title, m.started_at, x.decision_id, x.text, x.by_speaker,
                          x.by_name, x.at_s, e.stale
                   FROM decisions x
                   JOIN meetings m ON m.id = x.meeting_id
                   JOIN enrichments e ON e.meeting_id = x.meeting_id
                   WHERE 1 = 1"
                .into(),
            params: Vec::new(),
        };
        if let Some(topic) = q.topic.as_deref().map(fold).filter(|t| !t.is_empty()) {
            let n = sql.bind(topic);
            sql.push(&format!(
                " AND EXISTS (SELECT 1 FROM topics t WHERE t.meeting_id = x.meeting_id
                      AND instr(t.topic_fold, ?{n}) > 0)"
            ));
        }
        sql.meeting_filters(q.since.as_ref(), q.until.as_ref(), q.meeting.as_ref());
        if !sql.text_filter(q.text.as_deref(), "decision", "decision_id") {
            return Ok(Vec::new());
        }
        sql.push(" ORDER BY m.started_unix DESC, m.id, x.ord");
        sql.limit(q.limit);
        let rows: Vec<DecisionRow> = {
            let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
            stmt.query_map(params_from_iter(sql.params.iter()), |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?
        };
        let mut names: Option<(String, Names)> = None;
        let mut topics: Option<(String, Vec<String>)> = None;
        let mut out = Vec::with_capacity(rows.len());
        for (id, title, started, decision_id, text, by, by_name, at_s, stale) in rows {
            let n = meeting_names(self, &mut names, &id)?;
            let by_speaker = by
                .as_deref()
                .map(|s| SpeakerId::parse(s).map_err(|e| bad_row("speaker ID", e)))
                .transpose()?;
            let item = DecisionItem {
                meeting_id: MeetingId::parse(&id).map_err(|e| bad_row("meeting ID", e))?,
                meeting_title: title,
                started_at: timestamp(&started)?,
                decision_id,
                text: n.substitute(&text),
                by_speaker,
                by_person_id: n.person(by_speaker, by_name.as_deref()),
                by: n.label(by_speaker, by_name),
                at_s,
                topics: Vec::new(),
                enrichment_stale: stale,
            };
            let t = match &topics {
                Some((cached, t)) if *cached == id => t.clone(),
                _ => {
                    let mut stmt = self
                        .conn
                        .prepare("SELECT topic FROM topics WHERE meeting_id = ?1 ORDER BY rowid")
                        .map_err(db)?;
                    let t: Vec<String> = stmt
                        .query_map([&id], |r| r.get(0))
                        .map_err(db)?
                        .collect::<rusqlite::Result<_>>()
                        .map_err(db)?;
                    topics = Some((id.clone(), t.clone()));
                    t
                }
            };
            out.push(DecisionItem { topics: t, ..item });
        }
        Ok(out)
    }
}

/// [`Names`] of meeting `id`, reusing the previous row's when it is the same
/// meeting (rows are ordered by meeting).
fn meeting_names<'a>(
    index: &Index,
    cache: &'a mut Option<(String, Names)>,
    id: &str,
) -> Result<&'a Names> {
    if cache.as_ref().is_none_or(|(cached, _)| cached != id) {
        let labels = index.version >= 7;
        *cache = Some((id.to_owned(), Names::load(&index.conn, id, false, labels)?));
    }
    Ok(&cache.as_ref().expect("filled above").1)
}

fn bad_row(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Schema(format!("index row holds an invalid {what}: {e}"))
}

fn timestamp(s: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(s).map_err(|e| bad_row("timestamp", e))
}

fn parse_enum<T: DeserializeOwned>(s: String) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s)).map_err(|e| bad_row("value", e))
}
