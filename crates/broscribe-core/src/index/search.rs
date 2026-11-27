//! Reads: keyword, semantic and hybrid search, meetings, review queue,
//! people, and the windows still missing a vector.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, FixedOffset};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::page::{
    CursorRequest, Dir, Page, PageKey, PageKind, QueryFingerprint, after_sql, invalid_key, real,
    unpaged_limit,
};
use super::vectors::{dot_blob, normalize};
use super::windows::window_text;
use super::{Index, db, enum_str, fold, text_hashes};
use crate::schema::{Assignment, Source, sha256_hex};
use crate::{Error, MeetingId, PersonId, Result, SpeakerId};

/// Marks around matched terms in snippets.
pub(super) const MARK_START: &str = "**";
pub(super) const MARK_END: &str = "**";
pub(super) const ELLIPSIS: &str = "…";
/// Tokens per snippet.
pub(super) const SNIPPET_TOKENS: u32 = 16;
/// Reciprocal-rank-fusion constant (plan §3).
const RRF_K: f64 = 60.0;
/// Segments taken from the keyword leg of a hybrid search.
const KEYWORD_CANDIDATES: usize = 1000;
/// Windows taken from the semantic leg.
const SEMANTIC_CANDIDATES: usize = 200;

/// How [`Index::search`] ranks utterances.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// FTS5 bm25 over single utterances.
    #[default]
    Keyword,
    /// Cosine similarity of the query embedding with window vectors.
    Semantic,
    /// Reciprocal-rank fusion (k = 60) of both, at window level.
    Hybrid,
}

/// The query text embedded by the text model whose vectors are searched.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryVector {
    /// Vector-cache key of the model (registry ID or `provider:model`).
    pub model: String,
    pub vector: Vec<f32>,
}

/// The date order of unranked meeting lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MeetingOrder {
    #[default]
    Newest,
    Oldest,
}

impl MeetingOrder {
    fn date_order(self) -> &'static str {
        match self {
            Self::Newest => " ORDER BY m.started_unix DESC, m.id",
            Self::Oldest => " ORDER BY m.started_unix ASC, m.id",
        }
    }

    fn date_direction(self) -> Dir {
        match self {
            Self::Newest => Dir::Desc,
            Self::Oldest => Dir::Asc,
        }
    }
}

/// Filters shared by [`Index::search`] and [`Index::search_meetings`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchQuery {
    /// Free text; every term must match (diacritics and case ignored). FTS
    /// syntax is not interpreted. Text without any searchable term matches
    /// nothing in keyword mode.
    pub text: Option<String>,
    /// A person ID, name, alias or e-mail (case-insensitive).
    pub person: Option<String>,
    /// A tag (case-insensitive).
    pub tag: Option<String>,
    /// Meetings with an enrichment topic containing this (case-insensitive).
    pub topic: Option<String>,
    /// Meetings that started at or after this time.
    pub since: Option<DateTime<FixedOffset>>,
    /// Meetings that started before this time.
    pub until: Option<DateTime<FixedOffset>>,
    pub meeting: Option<MeetingId>,
    /// The meeting's app (case-insensitive).
    pub app: Option<String>,
    /// Imported or recorded meetings.
    pub source: Option<Source>,
    /// At most this many results; `None` for all.
    pub limit: Option<usize>,
    /// Ranking of [`Index::search`]; ignored by [`Index::search_meetings`].
    pub mode: SearchMode,
    /// Date order of meeting results without a text query.
    pub meeting_order: MeetingOrder,
    /// Embedding of `text`, required by [`SearchMode::Semantic`] and used by
    /// [`SearchMode::Hybrid`] when present.
    pub query_vector: Option<QueryVector>,
}

/// One matching utterance.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchHit {
    pub meeting_id: MeetingId,
    pub meeting_title: Option<String>,
    pub started_at: DateTime<FixedOffset>,
    pub segment_id: u32,
    pub speaker_id: Option<SpeakerId>,
    /// As in `meeting.md`: the attributed person (line correction, else named
    /// speaker), else the shown speaker's unnamed label (`Unknown (mic)`,
    /// `Speaker N`), or `Unknown` without a speaker.
    pub speaker_name: String,
    pub start: f64,
    pub end: f64,
    /// Text around the match with terms in `**…**`; the whole text when the
    /// query has no text or the segment was found semantically only.
    pub snippet: String,
    /// Semantic mode: cosine similarity; hybrid mode: fused RRF score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// One matching meeting.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MeetingHit {
    pub meeting_id: MeetingId,
    pub title: Option<String>,
    pub started_at: DateTime<FixedOffset>,
    pub duration_s: f64,
    pub source: Source,
    pub app: Option<String>,
    pub tags: Vec<String>,
    pub processing_status: crate::schema::ProcessingStatus,
    pub participant_count: u32,
    /// Matched title, tags or participants, when the query has text.
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_match: Option<MetadataMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_person: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataMatch {
    Title,
    Tags,
    People,
}

/// A window text that has no vector yet for the model asked about.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingWindow {
    /// The first meeting (by ID) with a window holding this text.
    pub meeting_id: MeetingId,
    pub text_sha256: String,
    pub text: String,
}

/// A speaker awaiting confirmation, including named caption-only identities needing review.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReviewItem {
    pub meeting_id: MeetingId,
    pub meeting_title: Option<String>,
    pub started_at: DateTime<FixedOffset>,
    pub speaker_id: SpeakerId,
    pub talk_s: f64,
    pub assignment: Assignment,
    /// The auto-named person, if any.
    pub person_id: Option<PersonId>,
    pub person_name: Option<String>,
    pub score: Option<f32>,
    pub suggested_person_id: Option<PersonId>,
    pub suggested_name: Option<String>,
    pub suggested_score: Option<f32>,
}

/// A person with the meetings and talk time they are named in
/// (`confirmed` or `auto` speakers).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PersonSummary {
    pub id: PersonId,
    pub name: String,
    pub is_self: bool,
    pub aliases: Vec<String>,
    pub emails: Vec<String>,
    /// The person's hue, 1 to [`crate::schema::PERSON_HUES`] (see
    /// [`crate::schema::Person::hue`]).
    pub hue: u8,
    pub meetings: u32,
    pub talk_s: f64,
}

/// How a meeting relates to a person ([`Index::person_meeting_candidates`]),
/// from its files when it was last indexed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersonRelation {
    /// A shown speaker is named as them (confirmed or auto).
    Speaker,
    /// A line correction attributes a line to them.
    Line,
    /// A participant is linked to them.
    Participant,
    /// A live decision awaiting review targets them: a review item, not
    /// an attribution or a voice.
    LiveReview,
}

/// A meeting related to a person, to be checked against its files.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PersonMeetingCandidate {
    pub meeting_id: MeetingId,
    pub started_at: DateTime<FixedOffset>,
    /// Sorted, never empty.
    pub relations: Vec<PersonRelation>,
}

/// Turns user text into a safe FTS5 query: each whitespace-separated term is
/// quoted (inner `"` doubled) and the terms are joined by `AND`, so operators,
/// `*`, `-`, `^`, column filters and parentheses are plain text. Terms without
/// a letter or digit are dropped; `None` when nothing is left.
pub fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split_whitespace()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" AND "))
}

/// SQL display name of person alias `p` (NULL when unnamed), as
/// `PeopleLookup::display_name`.
fn display_name_sql(p: &str) -> String {
    format!("CASE WHEN trim({p}.name) <> '' THEN {p}.name WHEN {p}.is_self = 1 THEN 'Me' END")
}

/// SQL for the IDs of the people matching bound parameter `n` (folded).
fn person_ids_sql(n: usize) -> String {
    format!(
        "SELECT id FROM people WHERE id = ?{n} OR name_fold = ?{n}
         OR id IN (SELECT person_id FROM person_aliases WHERE alias_fold = ?{n})
         OR id IN (SELECT person_id FROM person_emails WHERE email_fold = ?{n})"
    )
}

/// Index layout from which `speakers` rows hold their unnamed label.
const LABELS_VERSION: u32 = 7;

/// Index layout from which `person_meetings` exists.
const RELATIONS_VERSION: u32 = 8;

/// Index layout from which `people` rows hold their stored hue.
const PERSON_HUES_VERSION: u32 = 9;

/// The utterance columns every hit query selects first (`u`, `m`, `p`
/// aliases, plus `s` for the utterance's shown speaker when `labels`; see
/// [`hit_joins`]), followed by the snippet or text column. An unnamed
/// speaker shows as its stored label (`Unknown (mic)`, `Speaker N`); rows
/// without one (older layouts) as `Speaker N`.
fn hit_columns(labels: bool) -> String {
    let name = display_name_sql("p");
    let fallback = "'Speaker ' || substr(u.speaker_id, 2)";
    let unnamed = if labels {
        format!("coalesce(nullif(s.label, ''), {fallback})")
    } else {
        fallback.to_owned()
    };
    format!(
        "SELECT m.id, m.title, m.started_at, u.segment_id, u.speaker_id,
                coalesce({name}, CASE WHEN u.speaker_id IS NULL THEN 'Unknown'
                                      ELSE {unnamed} END),
                u.start_s, u.end_s, u.window_id, "
    )
}

/// The joins [`hit_columns`] needs after `utterances u`.
fn hit_joins(labels: bool) -> &'static str {
    if labels {
        " JOIN meetings m ON m.id = u.meeting_id
          LEFT JOIN people p ON p.id = u.person_id
          LEFT JOIN speakers s ON s.meeting_id = u.meeting_id AND s.speaker_id = u.speaker_id"
    } else {
        " JOIN meetings m ON m.id = u.meeting_id
          LEFT JOIN people p ON p.id = u.person_id"
    }
}

/// One row of [`hit_columns`] plus the snippet.
type RawHit = (
    String,
    Option<String>,
    String,
    i64,
    Option<String>,
    String,
    f64,
    f64,
    String,
    String,
);

fn raw_hit(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawHit> {
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
    ))
}

/// The hit and its window ID.
fn to_hit(raw: RawHit) -> Result<(SearchHit, String)> {
    let (id, title, started, seg, spk, name, start, end, window, snippet) = raw;
    Ok((
        SearchHit {
            meeting_id: meeting_id(&id)?,
            meeting_title: title,
            started_at: timestamp(&started)?,
            segment_id: as_u32(seg)?,
            speaker_id: spk.as_deref().map(speaker_id).transpose()?,
            speaker_name: name,
            start,
            end,
            snippet,
            score: None,
        },
        window,
    ))
}

/// The meeting columns every meeting query selects first: ID, title,
/// start, duration, source, app and snippet.
type MeetingRaw = (
    String,
    Option<String>,
    String,
    f64,
    String,
    Option<String>,
    Option<String>,
    String,
    u32,
    Option<String>,
    Option<String>,
);

const MEETING_COLUMNS: &str = "m.id, m.title, m.started_at, m.duration_s, m.source, m.app,
    m.processing_status, (SELECT count(*) FROM participants p WHERE p.meeting_id = m.id)";
const MATCH_COLUMN: &str = "CASE
    WHEN highlight(meetings_fts, 1, '[', ']') != meetings_fts.title THEN 'title'
    WHEN highlight(meetings_fts, 3, '[', ']') != meetings_fts.participants THEN 'people'
    WHEN highlight(meetings_fts, 2, '[', ']') != meetings_fts.tags THEN 'tags' END";
const MATCHED_PEOPLE: &str = "highlight(meetings_fts, 3, char(31), char(31))";

fn meeting_raw(r: &rusqlite::Row<'_>) -> rusqlite::Result<MeetingRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(8)?,
        r.get(6)?,
        r.get(7)?,
        r.get(9)?,
        r.get(10)?,
    ))
}

/// The hit of a meeting row, with its tags read through `tags`
/// ([`Index::tags_stmt`]).
fn meeting_hit(
    conn: &Connection,
    tags: &mut rusqlite::Statement<'_>,
    raw: MeetingRaw,
) -> Result<MeetingHit> {
    let (
        id,
        title,
        started,
        duration_s,
        source,
        app,
        snippet,
        processing_status,
        participant_count,
        metadata_match,
        people,
    ) = raw;
    let tags = tags
        .query_map([&id], |r| r.get(0))
        .map_err(db)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db)?;
    Ok(MeetingHit {
        meeting_id: meeting_id(&id)?,
        title,
        started_at: timestamp(&started)?,
        duration_s,
        source: parse_enum(source)?,
        app,
        tags,
        snippet,
        processing_status: parse_enum(processing_status)?,
        participant_count,
        metadata_match: metadata_match.map(parse_enum).transpose()?,
        matched_person: people
            .and_then(|people| people.contains('\u{1f}').then_some(people))
            .map(|people| matched_person(conn, &id, &people))
            .transpose()?
            .flatten(),
    })
}

fn matched_person(conn: &Connection, meeting: &str, people: &str) -> Result<Option<String>> {
    use crate::names::folded_name;
    let matches: Vec<_> = people
        .split('\u{1f}')
        .skip(1)
        .step_by(2)
        .map(folded_name)
        .filter(|term| !term.is_empty())
        .collect();
    let mut statement = conn
        .prepare_cached(
            "SELECT coalesce(nullif(p.name, ''), nullif(t.name, ''), t.email, ''),
                coalesce(t.name, '') || ' ' || coalesce(t.email, '')
         FROM participants t LEFT JOIN people p ON p.id = t.person_id
         WHERE t.meeting_id = ?1
         UNION ALL
         SELECT p.name, p.name FROM people p WHERE p.id IN (
             SELECT person_id FROM participants WHERE meeting_id = ?1
             UNION SELECT person_id FROM speakers WHERE meeting_id = ?1
                AND assignment IN ('confirmed', 'auto')
             UNION SELECT person_id FROM utterances WHERE meeting_id = ?1
         )",
        )
        .map_err(db)?;
    let mut candidates = statement.query([meeting]).map_err(db)?;
    let mut best = None;
    let mut best_score = 0;
    while let Some(row) = candidates.next().map_err(db)? {
        let name: String = row.get(0).map_err(db)?;
        let key: String = row.get(1).map_err(db)?;
        let key = folded_name(&format!("{name} {key}"));
        let score = matches
            .iter()
            .filter(|term| key.contains(term.as_str()))
            .count();
        if score > best_score && !name.is_empty() {
            best_score = score;
            best = Some(name);
        }
    }
    Ok(best)
}

/// Query text under construction with its positional parameters.
#[derive(Default)]
struct Sql {
    text: String,
    params: Vec<Value>,
}

impl Sql {
    fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            params: Vec::new(),
        }
    }

    fn push(&mut self, s: &str) {
        self.text.push_str(s);
    }

    /// Binds a value and returns its `?N` index.
    fn bind(&mut self, v: impl Into<Value>) -> usize {
        self.params.push(v.into());
        self.params.len()
    }

    /// Filters on meeting alias `m`: tag, topic, dates, meeting ID, app and source.
    fn meeting_filters(&mut self, q: &SearchQuery) -> Result<()> {
        if let Some(tag) = &q.tag {
            let n = self.bind(fold(tag));
            self.push(&format!(
                " AND EXISTS (SELECT 1 FROM tags t WHERE t.meeting_id = m.id AND t.tag_fold = ?{n})"
            ));
        }
        if let Some(topic) = q.topic.as_deref().map(fold).filter(|t| !t.is_empty()) {
            let n = self.bind(topic);
            self.push(&format!(
                " AND EXISTS (SELECT 1 FROM topics t WHERE t.meeting_id = m.id
                      AND instr(t.topic_fold, ?{n}) > 0)"
            ));
        }
        if let Some(since) = &q.since {
            let n = self.bind(since.timestamp());
            self.push(&format!(" AND m.started_unix >= ?{n}"));
        }
        if let Some(until) = &q.until {
            let n = self.bind(until.timestamp());
            self.push(&format!(" AND m.started_unix < ?{n}"));
        }
        if let Some(id) = &q.meeting {
            let n = self.bind(id.as_str().to_owned());
            self.push(&format!(" AND m.id = ?{n}"));
        }
        if let Some(app) = &q.app {
            let n = self.bind(fold(app));
            self.push(&format!(" AND m.app_fold = ?{n}"));
        }
        if let Some(source) = &q.source {
            let n = self.bind(enum_str(source)?);
            self.push(&format!(" AND m.source = ?{n}"));
        }
        Ok(())
    }

    /// Filters on the named person of utterance or window alias `alias`.
    fn person_filter(&mut self, q: &SearchQuery, alias: &str) {
        if let Some(person) = &q.person {
            let n = self.bind(fold(person));
            self.push(&format!(
                " AND {alias}.person_id IN ({})",
                person_ids_sql(n)
            ));
        }
    }

    /// Filters meeting alias `m` on the person: named speakers,
    /// participants (linked person, name or e-mail) and people a line is
    /// attributed to.
    fn meeting_person_filter(&mut self, q: &SearchQuery) {
        if let Some(person) = &q.person {
            let n = self.bind(fold(person));
            let ids = person_ids_sql(n);
            self.push(&format!(
                " AND (m.id IN (SELECT meeting_id FROM speakers
                               WHERE assignment IN ('confirmed', 'auto') AND person_id IN ({ids}))
                       OR m.id IN (SELECT meeting_id FROM participants
                                   WHERE person_id IN ({ids}) OR email_fold = ?{n} OR name_fold = ?{n})
                       OR m.id IN (SELECT meeting_id FROM utterances WHERE person_id IN ({ids})))"
            ));
        }
    }

    fn limit(&mut self, limit: Option<usize>) {
        if let Some(limit) = limit {
            let n = self.bind(i64::try_from(limit).unwrap_or(i64::MAX));
            self.push(&format!(" LIMIT ?{n}"));
        }
    }
}

/// The query's FTS expression: `Ok(None)` without text, `Err(())` when the
/// text holds no searchable term (nothing can match).
fn text_match(q: &SearchQuery) -> std::result::Result<Option<String>, ()> {
    match q.text.as_deref() {
        None => Ok(None),
        Some(t) => fts_query(t).map(Some).ok_or(()),
    }
}

fn bad_row(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Schema(format!("index row holds an invalid {what}: {e}"))
}

fn meeting_id(s: &str) -> Result<MeetingId> {
    MeetingId::parse(s).map_err(|e| bad_row("meeting ID", e))
}

fn person_id(s: Option<String>) -> Result<Option<PersonId>> {
    s.map(|s| PersonId::parse(&s).map_err(|e| bad_row("person ID", e)))
        .transpose()
}

fn speaker_id(s: &str) -> Result<SpeakerId> {
    SpeakerId::parse(s).map_err(|e| bad_row("speaker ID", e))
}

fn timestamp(s: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(s).map_err(|e| bad_row("timestamp", e))
}

fn parse_enum<T: DeserializeOwned>(s: String) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s)).map_err(|e| bad_row("value", e))
}

fn as_u32(n: i64) -> Result<u32> {
    u32::try_from(n).map_err(|e| bad_row("number", e))
}

/// A window's standing in the fused ranking.
#[derive(Default)]
struct Fused {
    score: f64,
    keyword_rank: Option<usize>,
    semantic_rank: Option<usize>,
    cosine: Option<f32>,
    /// The best keyword segment of the window, when it has one.
    hit: Option<SearchHit>,
}

impl Fused {
    fn best_rank(&self) -> usize {
        self.keyword_rank
            .into_iter()
            .chain(self.semantic_rank)
            .min()
            .unwrap_or(usize::MAX)
    }
}

/// A window's place in the fused ranking: higher score first, then better
/// best rank, then windows with a keyword hit, then window ID (unique).
#[derive(Clone, Debug)]
struct FusedKey {
    score: f64,
    best_rank: usize,
    keyword: bool,
    window: String,
}

impl FusedKey {
    fn cmp_rank(&self, other: &Self) -> Ordering {
        other
            .score
            .total_cmp(&self.score)
            .then(self.best_rank.cmp(&other.best_rank))
            .then(other.keyword.cmp(&self.keyword))
            .then_with(|| self.window.cmp(&other.window))
    }

    fn page_key(&self) -> PageKey {
        PageKey::Window {
            score: self.score.to_bits(),
            best: u64::try_from(self.best_rank).unwrap_or(u64::MAX),
            keyword: self.keyword,
            window: self.window.clone(),
        }
    }
}

/// Reciprocal-rank fusion of the keyword leg (segment hits with their
/// window, best first) and the semantic leg (windows by cosine) at window
/// level, best first.
fn fuse(keyword: Vec<(SearchHit, String)>, semantic: Vec<(String, f32)>) -> Vec<(FusedKey, Fused)> {
    let mut windows: HashMap<String, Fused> = HashMap::new();
    let mut rank = 0;
    for (hit, window) in keyword {
        let entry = windows.entry(window).or_default();
        if entry.keyword_rank.is_some() {
            continue; // a better segment of this window ranked already
        }
        rank += 1;
        entry.keyword_rank = Some(rank);
        entry.score += 1.0 / (RRF_K + rank as f64);
        entry.hit = Some(hit);
    }
    for (i, (window, cosine)) in semantic.into_iter().enumerate() {
        let entry = windows.entry(window).or_default();
        entry.semantic_rank = Some(i + 1);
        entry.score += 1.0 / (RRF_K + (i + 1) as f64);
        entry.cosine = Some(cosine);
    }
    let mut ranked: Vec<(FusedKey, Fused)> = windows
        .into_iter()
        .map(|(window, fused)| {
            let key = FusedKey {
                score: fused.score,
                best_rank: fused.best_rank(),
                keyword: fused.keyword_rank.is_some(),
                window,
            };
            (key, fused)
        })
        .collect();
    ranked.sort_by(|(a, _), (b, _)| a.cmp_rank(b));
    ranked
}

/// Feeds the filters of `q` to a page cursor's query fingerprint, as the
/// queries apply them: text as its FTS expression (`""` when it holds no
/// searchable term), names, tags, topics and apps folded, dates as instants.
fn fingerprint_filters(fp: &mut QueryFingerprint, q: &SearchQuery) -> Result<()> {
    let text = text_match(q).unwrap_or_else(|()| Some(String::new()));
    fp.text("text", text.as_deref());
    fp.text("person", q.person.as_deref().map(fold).as_deref());
    fp.text("tag", q.tag.as_deref().map(fold).as_deref());
    fp.text(
        "topic",
        q.topic
            .as_deref()
            .map(fold)
            .filter(|t| !t.is_empty())
            .as_deref(),
    );
    fp.int("since", q.since.map(|t| t.timestamp()));
    fp.int("until", q.until.map(|t| t.timestamp()));
    fp.text("meeting", q.meeting.as_ref().map(MeetingId::as_str));
    fp.text("app", q.app.as_deref().map(fold).as_deref());
    let source = q.source.as_ref().map(enum_str).transpose()?;
    fp.text("source", source.as_deref());
    Ok(())
}

impl Index {
    /// Searches utterances.
    ///
    /// - [`SearchMode::Keyword`]: utterances matching the text, best match
    ///   first (bm25, then newest meeting).
    /// - [`SearchMode::Semantic`]: windows closest to `query_vector` (cosine),
    ///   each returned as its longest segment; needs `query_vector`.
    /// - [`SearchMode::Hybrid`]: reciprocal-rank fusion (k = 60) of the keyword
    ///   ranking (segment hits mapped to their window, ranked by their best
    ///   segment) and the semantic ranking, at window level; each window is
    ///   returned as its best keyword segment, else its longest segment.
    ///   Without `query_vector` or the vector cache, only the keyword leg
    ///   counts.
    ///
    /// Filters apply to both legs. Without text, every mode lists utterances
    /// newest meeting first in transcript order.
    pub fn search(&self, q: &SearchQuery) -> Result<Vec<SearchHit>> {
        self.read_snapshot(|| self.search_in_snapshot(q))
    }

    fn search_in_snapshot(&self, q: &SearchQuery) -> Result<Vec<SearchHit>> {
        if q.mode == SearchMode::Keyword || q.text.is_none() {
            let Ok(text) = text_match(q) else {
                return Ok(Vec::new());
            };
            return Ok(self
                .keyword_hits(q, text.as_deref(), q.limit)?
                .into_iter()
                .map(|(h, _)| h)
                .collect());
        }
        let mut ranked = self.fused_ranking(q)?;
        if let Some(limit) = q.limit {
            ranked.truncate(limit);
        }
        let mut out = Vec::with_capacity(ranked.len());
        for (key, fused) in ranked {
            out.extend(self.fused_hit(q, &key.window, fused)?);
        }
        Ok(out)
    }

    /// One page of [`Index::search`]: the same rows in the same order (ties
    /// broken by utterance row, or by window ID in the fused ranking), read
    /// in one snapshot. Semantic and hybrid pages are also bound to the
    /// vector cache they ranked with. The query's `limit` must be unset.
    pub fn search_page(&self, q: &SearchQuery, page: &CursorRequest) -> Result<Page<SearchHit>> {
        unpaged_limit(q.limit)?;
        let fused = q.mode != SearchMode::Keyword && q.text.is_some();
        self.read_page(
            PageKind::Search,
            page,
            fused && q.query_vector.is_some(),
            |fp| {
                fingerprint_filters(fp, q)?;
                if fused {
                    let mode = enum_str(&q.mode)?;
                    fp.text("mode", Some(mode.as_str()));
                    fp.text("model", q.query_vector.as_ref().map(|v| v.model.as_str()));
                    if let Some(v) = &q.query_vector {
                        fp.floats("vector", &v.vector);
                    }
                }
                Ok(())
            },
            |after, take| {
                if fused {
                    self.fused_page_rows(q, after, take)
                } else {
                    self.keyword_page_rows(q, after, take)
                }
            },
        )
    }

    /// Up to `take` keyword (or, without text, listing) hits after `after`.
    /// With text, the matches' sort keys are ranked first and snippets are
    /// made only for the rows of the page.
    fn keyword_page_rows(
        &self,
        q: &SearchQuery,
        after: Option<&PageKey>,
        take: usize,
    ) -> Result<Vec<(SearchHit, PageKey)>> {
        let Ok(text) = text_match(q) else {
            return Ok(Vec::new());
        };
        let labels = self.version >= LABELS_VERSION;
        let Some(text) = text else {
            let mut sql = Sql::new(hit_columns(labels));
            sql.push("u.text, m.started_unix, u.id FROM utterances u");
            sql.push(hit_joins(labels));
            sql.push(" WHERE 1 = 1");
            sql.person_filter(q, "u");
            sql.meeting_filters(q)?;
            match after {
                None => {}
                Some(PageKey::Utterance {
                    started,
                    meeting,
                    start,
                    segment,
                    row,
                }) => {
                    let condition = after_sql(
                        vec![
                            ("m.started_unix", Dir::Desc, Value::Integer(*started)),
                            ("m.id", Dir::Asc, Value::Text(meeting.as_str().to_owned())),
                            ("u.start_s", Dir::Asc, real(*start)),
                            ("u.segment_id", Dir::Asc, Value::Integer(*segment)),
                            ("u.id", Dir::Asc, Value::Integer(*row)),
                        ],
                        |v| sql.bind(v),
                    );
                    sql.push(&format!(" AND {condition}"));
                }
                Some(_) => return Err(invalid_key()),
            }
            sql.push(" ORDER BY m.started_unix DESC, m.id, u.start_s, u.segment_id, u.id");
            sql.limit(Some(take));
            let rows: Vec<(RawHit, i64, i64)> = {
                let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
                stmt.query_map(params_from_iter(sql.params.iter()), |r| {
                    Ok((raw_hit(r)?, r.get(10)?, r.get(11)?))
                })
                .map_err(db)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db)?
            };
            return rows
                .into_iter()
                .map(|(raw, started, row)| {
                    let (hit, _) = to_hit(raw)?;
                    let key = PageKey::Utterance {
                        started,
                        meeting: hit.meeting_id.clone(),
                        start: hit.start.to_bits(),
                        segment: i64::from(hit.segment_id),
                        row,
                    };
                    Ok((hit, key))
                })
                .collect();
        };

        // bm25 is ranked inside a materialized CTE: the keyset condition
        // then compares plain values, never the FTS function itself.
        let mut sql = Sql::new(
            "WITH hits AS MATERIALIZED (
                 SELECT u.id AS uid, bm25(utterances_fts) AS score, m.started_unix AS started,
                        m.id AS mid, u.start_s AS st, u.segment_id AS seg
                 FROM utterances_fts
                 JOIN utterances u ON u.id = utterances_fts.rowid
                 JOIN meetings m ON m.id = u.meeting_id
                 WHERE 1 = 1",
        );
        let n = sql.bind(text.clone());
        sql.push(&format!(" AND utterances_fts MATCH ?{n}"));
        sql.person_filter(q, "u");
        sql.meeting_filters(q)?;
        sql.push(") SELECT uid, score, started, mid, st, seg FROM hits WHERE 1 = 1");
        match after {
            None => {}
            Some(PageKey::RankedUtterance {
                score,
                started,
                meeting,
                start,
                segment,
                row,
            }) => {
                let condition = after_sql(
                    vec![
                        ("score", Dir::Asc, real(*score)),
                        ("started", Dir::Desc, Value::Integer(*started)),
                        ("mid", Dir::Asc, Value::Text(meeting.as_str().to_owned())),
                        ("st", Dir::Asc, real(*start)),
                        ("seg", Dir::Asc, Value::Integer(*segment)),
                        ("uid", Dir::Asc, Value::Integer(*row)),
                    ],
                    |v| sql.bind(v),
                );
                sql.push(&format!(" AND {condition}"));
            }
            Some(_) => return Err(invalid_key()),
        }
        sql.push(" ORDER BY score, started DESC, mid, st, seg, uid");
        sql.limit(Some(take));
        type Key = (i64, f64, i64, String, f64, i64);
        let keys: Vec<Key> = {
            let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
            stmt.query_map(params_from_iter(sql.params.iter()), |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?
        };
        let mut stmt = self
            .conn
            .prepare(&format!(
                "{}snippet(utterances_fts, 0, '{MARK_START}', '{MARK_END}', '{ELLIPSIS}', {SNIPPET_TOKENS})
                 FROM utterances_fts JOIN utterances u ON u.id = utterances_fts.rowid{}
                 WHERE utterances_fts MATCH ?1 AND utterances_fts.rowid = ?2",
                hit_columns(labels),
                hit_joins(labels),
            ))
            .map_err(db)?;
        let mut out = Vec::with_capacity(keys.len());
        for (uid, score, started, mid, start, segment) in keys {
            let raw = stmt
                .query_row(params![text, uid], raw_hit)
                .optional()
                .map_err(db)?
                .ok_or_else(|| {
                    Error::Internal("index: a ranked utterance left its snapshot".into())
                })?;
            let (hit, _) = to_hit(raw)?;
            let key = PageKey::RankedUtterance {
                score: score.to_bits(),
                started,
                meeting: meeting_id(&mid)?,
                start: start.to_bits(),
                segment,
                row: uid,
            };
            out.push((hit, key));
        }
        Ok(out)
    }

    /// Up to `take` hits of the fused (semantic or hybrid) ranking after
    /// `after`.
    fn fused_page_rows(
        &self,
        q: &SearchQuery,
        after: Option<&PageKey>,
        take: usize,
    ) -> Result<Vec<(SearchHit, PageKey)>> {
        let after = match after {
            None => None,
            Some(PageKey::Window {
                score,
                best,
                keyword,
                window,
            }) => Some(FusedKey {
                score: f64::from_bits(*score),
                best_rank: usize::try_from(*best).map_err(|_| invalid_key())?,
                keyword: *keyword,
                window: window.clone(),
            }),
            Some(_) => return Err(invalid_key()),
        };
        let mut out = Vec::new();
        for (key, fused) in self.fused_ranking(q)? {
            if out.len() == take {
                break;
            }
            if after
                .as_ref()
                .is_some_and(|a| key.cmp_rank(a) != Ordering::Greater)
            {
                continue;
            }
            if let Some(hit) = self.fused_hit(q, &key.window, fused)? {
                out.push((hit, key.page_key()));
            }
        }
        Ok(out)
    }

    /// Every window of the semantic or hybrid ranking, best first.
    fn fused_ranking(&self, q: &SearchQuery) -> Result<Vec<(FusedKey, Fused)>> {
        let semantic = match (&q.query_vector, q.mode) {
            (Some(v), _) => self.semantic_windows(q, v)?,
            (None, SearchMode::Semantic) => {
                return Err(Error::InvalidInput(
                    "semantic search needs the query's text embedding".into(),
                ));
            }
            (None, _) => Vec::new(),
        };
        let keyword = match (q.mode, text_match(q)) {
            (SearchMode::Hybrid, Ok(Some(text))) => {
                self.keyword_hits(q, Some(&text), Some(KEYWORD_CANDIDATES))?
            }
            _ => Vec::new(),
        };
        Ok(fuse(keyword, semantic))
    }

    /// Keyword (or, without text, listing) hits with their window IDs.
    fn keyword_hits(
        &self,
        q: &SearchQuery,
        text: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<(SearchHit, String)>> {
        let labels = self.version >= LABELS_VERSION;
        let mut sql = Sql::new(hit_columns(labels));
        if text.is_some() {
            sql.push(&format!(
                "snippet(utterances_fts, 0, '{MARK_START}', '{MARK_END}', '{ELLIPSIS}', {SNIPPET_TOKENS})
                 FROM utterances_fts JOIN utterances u ON u.id = utterances_fts.rowid"
            ));
        } else {
            sql.push("u.text FROM utterances u");
        }
        sql.push(hit_joins(labels));
        sql.push(" WHERE 1 = 1");
        if let Some(text) = text {
            let n = sql.bind(text.to_owned());
            sql.push(&format!(" AND utterances_fts MATCH ?{n}"));
        }
        sql.person_filter(q, "u");
        sql.meeting_filters(q)?;
        if text.is_some() {
            sql.push(" ORDER BY bm25(utterances_fts), m.started_unix DESC, m.id, u.start_s, u.segment_id, u.id");
        } else {
            sql.push(" ORDER BY m.started_unix DESC, m.id, u.start_s, u.segment_id, u.id");
        }
        sql.limit(limit);

        let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
        let rows: Vec<RawHit> = stmt
            .query_map(params_from_iter(sql.params.iter()), raw_hit)
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?;
        rows.into_iter().map(to_hit).collect()
    }

    /// Windows passing the filters whose cached vector for the query's model
    /// is closest to the query, best first, with their cosine similarity.
    /// Empty without the vector cache.
    fn semantic_windows(&self, q: &SearchQuery, v: &QueryVector) -> Result<Vec<(String, f32)>> {
        if !self.vectors {
            return Ok(Vec::new());
        }
        let mut query = v.vector.clone();
        normalize(&mut query);
        if query.is_empty() || query.iter().any(|x| !x.is_finite()) {
            return Ok(Vec::new());
        }
        let mut sql = Sql::new(
            "SELECT w.id, v.vector FROM windows w
             JOIN meetings m ON m.id = w.meeting_id
             JOIN vec.vectors v ON v.model = ?1 AND v.text_sha256 = w.text_sha256
             WHERE 1 = 1",
        );
        sql.bind(v.model.clone());
        sql.person_filter(q, "w");
        sql.meeting_filters(q)?;

        let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
        let mut rows = stmt
            .query(params_from_iter(sql.params.iter()))
            .map_err(db)?;
        let mut scored: Vec<(String, f32)> = Vec::new();
        while let Some(row) = rows.next().map_err(db)? {
            let blob = row
                .get_ref(1)
                .map_err(db)?
                .as_blob()
                .map_err(|e| bad_row("vector", e))?;
            if let Some(score) = dot_blob(blob, &query) {
                scored.push((row.get(0).map_err(db)?, score));
            }
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(SEMANTIC_CANDIDATES);
        Ok(scored)
    }

    /// The hit a fused window is shown as: its best keyword segment, else
    /// its longest segment; scored by cosine (semantic) or fused RRF score.
    fn fused_hit(&self, q: &SearchQuery, window: &str, fused: Fused) -> Result<Option<SearchHit>> {
        let hit = match fused.hit {
            Some(hit) => Some(hit),
            None => self.longest_segment(window)?,
        };
        Ok(hit.map(|mut hit| {
            hit.score = Some(match q.mode {
                SearchMode::Semantic => f64::from(fused.cosine.unwrap_or_default()),
                _ => fused.score,
            });
            hit
        }))
    }

    /// The window's longest segment (its whole text as the snippet).
    fn longest_segment(&self, window: &str) -> Result<Option<SearchHit>> {
        let labels = self.version >= LABELS_VERSION;
        let sql = format!(
            "{}u.text FROM utterances u{}
             WHERE u.window_id = ?1
             ORDER BY (u.end_s - u.start_s) DESC, u.start_s, u.segment_id
             LIMIT 1",
            hit_columns(labels),
            hit_joins(labels),
        );
        let raw = self
            .conn
            .query_row(&sql, [window], raw_hit)
            .optional()
            .map_err(db)?;
        raw.map(|r| to_hit(r).map(|(h, _)| h)).transpose()
    }

    /// Whether the attached vector cache holds any vector of `model`.
    pub fn has_vectors(&self, model: &str) -> Result<bool> {
        if !self.vectors {
            return Ok(false);
        }
        self.conn
            .prepare("SELECT 1 FROM vec.vectors WHERE model = ?1 LIMIT 1")
            .and_then(|mut s| s.exists([model]))
            .map_err(db)
    }

    /// The meeting's indexed window-hash revision, `None` when not indexed.
    pub fn window_revision(&self, id: &MeetingId) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT window_revision FROM meetings WHERE id = ?1",
                [id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(db)
    }

    /// Every distinct window text hash in the index.
    pub fn text_hashes(&self) -> Result<HashSet<String>> {
        text_hashes(&self.conn)
    }

    /// Distinct non-empty window texts (of one meeting, or all) without a
    /// vector for `model` in the attached cache; every text when the cache
    /// is not attached. Texts are rebuilt from the utterances with
    /// [`window_text`] and checked against their stored hash.
    pub fn pending_windows(
        &self,
        model: &str,
        meeting: Option<&MeetingId>,
    ) -> Result<Vec<PendingWindow>> {
        let mut sql = Sql::new(
            "SELECT w.meeting_id, w.id, w.text_sha256, u.text FROM windows w
             JOIN utterances u ON u.window_id = w.id",
        );
        let empty = sql.bind(sha256_hex(b""));
        sql.push(&format!(" WHERE w.text_sha256 <> ?{empty}"));
        if let Some(id) = meeting {
            let n = sql.bind(id.as_str().to_owned());
            sql.push(&format!(" AND w.meeting_id = ?{n}"));
        }
        if self.vectors {
            let n = sql.bind(model.to_owned());
            sql.push(&format!(
                " AND NOT EXISTS (SELECT 1 FROM vec.vectors v
                                  WHERE v.model = ?{n} AND v.text_sha256 = w.text_sha256)"
            ));
        }
        sql.push(" ORDER BY w.meeting_id, w.id, u.start_s, u.segment_id");

        type Raw = (String, String, String, String);
        let rows: Vec<Raw> = {
            let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
            stmt.query_map(params_from_iter(sql.params.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?
        };

        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut i = 0;
        while i < rows.len() {
            let (meeting, window, hash, _) = &rows[i];
            let mut j = i;
            while j < rows.len() && rows[j].1 == *window {
                j += 1;
            }
            let text = window_text(rows[i..j].iter().map(|r| r.3.as_str()));
            // A window whose utterances no longer give its hash (duplicate
            // segment IDs) is skipped rather than cached under a wrong key.
            if sha256_hex(text.as_bytes()) == *hash && seen.insert(hash.clone()) {
                out.push(PendingWindow {
                    meeting_id: meeting_id(meeting)?,
                    text_sha256: hash.clone(),
                    text,
                });
            }
            i = j;
        }
        Ok(out)
    }

    /// Distinct non-empty meeting apps, sorted case-insensitively.
    pub fn meeting_apps(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT min(app) FROM meetings
                 WHERE app IS NOT NULL AND trim(app) <> ''
                 GROUP BY app_fold ORDER BY app_fold",
            )
            .map_err(db)?;
        stmt.query_map([], |r| r.get(0))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)
    }

    /// Tags in use and the number of meetings carrying each tag.
    pub fn meeting_tags(&self) -> Result<Vec<(String, u32)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT min(tag), count(DISTINCT meeting_id) FROM tags
             GROUP BY tag_fold ORDER BY tag_fold",
            )
            .map_err(db)?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)
    }

    /// Meetings whose title, tags or participants match the text, with the
    /// same filters as [`Index::search`]. The person filter matches named
    /// speakers, participants (linked person, name or e-mail) and people a
    /// line is attributed to.
    pub fn search_meetings(&self, q: &SearchQuery) -> Result<Vec<MeetingHit>> {
        self.read_snapshot(|| self.search_meetings_in_snapshot(q))
    }

    fn search_meetings_in_snapshot(&self, q: &SearchQuery) -> Result<Vec<MeetingHit>> {
        let Ok(text) = text_match(q) else {
            return Ok(Vec::new());
        };
        let mut sql = Sql::new(format!("SELECT {MEETING_COLUMNS}, "));
        if text.is_some() {
            sql.push(&format!(
                "snippet(meetings_fts, -1, '{MARK_START}', '{MARK_END}', '{ELLIPSIS}', {SNIPPET_TOKENS}), {MATCH_COLUMN}, {MATCHED_PEOPLE}
                 FROM meetings_fts JOIN meetings m ON m.id = meetings_fts.meeting_id"
            ));
        } else {
            sql.push("NULL, NULL, NULL FROM meetings m");
        }
        sql.push(" WHERE 1 = 1");
        if let Some(text) = &text {
            let n = sql.bind(text.clone());
            sql.push(&format!(" AND meetings_fts MATCH ?{n}"));
        }
        sql.meeting_person_filter(q);
        sql.meeting_filters(q)?;
        if text.is_some() {
            sql.push(" ORDER BY bm25(meetings_fts), m.started_unix DESC, m.id, meetings_fts.rowid");
        } else {
            sql.push(q.meeting_order.date_order());
        }
        sql.limit(q.limit);

        let rows: Vec<MeetingRaw> = {
            let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
            stmt.query_map(params_from_iter(sql.params.iter()), meeting_raw)
                .map_err(db)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db)?
        };
        let mut tags = self.tags_stmt()?;
        rows.into_iter()
            .map(|raw| meeting_hit(&self.conn, &mut tags, raw))
            .collect()
    }

    /// One page of [`Index::search_meetings`]: the same meetings in the same
    /// order (ties broken by full-text row), read in one snapshot. The
    /// query's `limit` must be unset; `mode` and `query_vector` are ignored
    /// as there.
    pub fn search_meetings_page(
        &self,
        q: &SearchQuery,
        page: &CursorRequest,
    ) -> Result<Page<MeetingHit>> {
        unpaged_limit(q.limit)?;
        self.read_page(
            PageKind::Meetings,
            page,
            false,
            |fp| {
                if q.text.is_none() && q.meeting_order == MeetingOrder::Oldest {
                    fp.text("order", Some("oldest"));
                }
                fingerprint_filters(fp, q)
            },
            |after, take| self.meeting_page_rows(q, after, take),
        )
    }

    /// Up to `take` meetings after `after`. With text, the matches' sort
    /// keys are ranked first and snippets are made only for the page.
    fn meeting_page_rows(
        &self,
        q: &SearchQuery,
        after: Option<&PageKey>,
        take: usize,
    ) -> Result<Vec<(MeetingHit, PageKey)>> {
        let Ok(text) = text_match(q) else {
            return Ok(Vec::new());
        };
        let mut tags = self.tags_stmt()?;
        let Some(text) = text else {
            let mut sql = Sql::new(format!(
                "SELECT {MEETING_COLUMNS}, NULL, NULL, NULL, m.started_unix FROM meetings m WHERE 1 = 1",
            ));
            sql.meeting_person_filter(q);
            sql.meeting_filters(q)?;
            match after {
                None => {}
                Some(PageKey::Meeting { started, id }) => {
                    let condition = after_sql(
                        vec![
                            (
                                "m.started_unix",
                                q.meeting_order.date_direction(),
                                Value::Integer(*started),
                            ),
                            ("m.id", Dir::Asc, Value::Text(id.as_str().to_owned())),
                        ],
                        |v| sql.bind(v),
                    );
                    sql.push(&format!(" AND {condition}"));
                }
                Some(_) => return Err(invalid_key()),
            }
            sql.push(q.meeting_order.date_order());
            sql.limit(Some(take));
            let rows: Vec<(MeetingRaw, i64)> = {
                let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
                stmt.query_map(params_from_iter(sql.params.iter()), |r| {
                    Ok((meeting_raw(r)?, r.get(11)?))
                })
                .map_err(db)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db)?
            };
            return rows
                .into_iter()
                .map(|(raw, started)| {
                    let hit = meeting_hit(&self.conn, &mut tags, raw)?;
                    let key = PageKey::Meeting {
                        started,
                        id: hit.meeting_id.clone(),
                    };
                    Ok((hit, key))
                })
                .collect();
        };

        // bm25 is ranked inside a materialized CTE: the keyset condition
        // then compares plain values, never the FTS function itself.
        let mut sql = Sql::new(
            "WITH hits AS MATERIALIZED (
                 SELECT meetings_fts.rowid AS fts_row, bm25(meetings_fts) AS score,
                        m.started_unix AS started, m.id AS mid
                 FROM meetings_fts JOIN meetings m ON m.id = meetings_fts.meeting_id
                 WHERE 1 = 1",
        );
        let n = sql.bind(text.clone());
        sql.push(&format!(" AND meetings_fts MATCH ?{n}"));
        sql.meeting_person_filter(q);
        sql.meeting_filters(q)?;
        sql.push(") SELECT fts_row, score, started, mid FROM hits WHERE 1 = 1");
        match after {
            None => {}
            Some(PageKey::RankedMeeting {
                score,
                started,
                id,
                row,
            }) => {
                let condition = after_sql(
                    vec![
                        ("score", Dir::Asc, real(*score)),
                        ("started", Dir::Desc, Value::Integer(*started)),
                        ("mid", Dir::Asc, Value::Text(id.as_str().to_owned())),
                        ("fts_row", Dir::Asc, Value::Integer(*row)),
                    ],
                    |v| sql.bind(v),
                );
                sql.push(&format!(" AND {condition}"));
            }
            Some(_) => return Err(invalid_key()),
        }
        sql.push(" ORDER BY score, started DESC, mid, fts_row");
        sql.limit(Some(take));
        let keys: Vec<(i64, f64, i64, String)> = {
            let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
            stmt.query_map(params_from_iter(sql.params.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?
        };
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {MEETING_COLUMNS},
                        snippet(meetings_fts, -1, '{MARK_START}', '{MARK_END}', '{ELLIPSIS}', {SNIPPET_TOKENS}), {MATCH_COLUMN}, {MATCHED_PEOPLE}
                 FROM meetings_fts JOIN meetings m ON m.id = meetings_fts.meeting_id
                 WHERE meetings_fts MATCH ?1 AND meetings_fts.rowid = ?2"
            ))
            .map_err(db)?;
        let mut out = Vec::with_capacity(keys.len());
        for (row, score, started, mid) in keys {
            let raw = stmt
                .query_row(params![text, row], meeting_raw)
                .optional()
                .map_err(db)?
                .ok_or_else(|| {
                    Error::Internal("index: a ranked meeting left its snapshot".into())
                })?;
            let key = PageKey::RankedMeeting {
                score: score.to_bits(),
                started,
                id: meeting_id(&mid)?,
                row,
            };
            out.push((meeting_hit(&self.conn, &mut tags, raw)?, key));
        }
        Ok(out)
    }

    fn tags_stmt(&self) -> Result<rusqlite::Statement<'_>> {
        self.conn
            .prepare("SELECT tag FROM tags WHERE meeting_id = ?1 ORDER BY tag_fold")
            .map_err(db)
    }

    /// Speakers needing a decision, most talk time first. Trusted automatic
    /// names need no manual confirmation; identity placeholders still do.
    pub fn review_queue(&self, limit: Option<usize>) -> Result<Vec<ReviewItem>> {
        let person = display_name_sql("pp");
        let suggested = display_name_sql("sp");
        let mut sql = Sql::new(format!(
            "SELECT s.meeting_id, m.title, m.started_at, s.speaker_id, s.talk_s, s.assignment,
                    s.person_id, {person}, s.score,
                    s.suggested_person_id, {suggested}, s.suggested_score
             FROM speakers s
             JOIN meetings m ON m.id = s.meeting_id
             LEFT JOIN people pp ON pp.id = s.person_id
             LEFT JOIN people sp ON sp.id = s.suggested_person_id
             WHERE s.assignment NOT IN ('confirmed', 'auto')
                OR pp.needs_review = 1
                OR (s.assignment = 'auto' AND pp.id IS NULL)
             ORDER BY s.talk_s DESC, m.started_unix DESC, s.meeting_id, s.speaker_id"
        ));
        sql.limit(limit);

        type Raw = (
            String,
            Option<String>,
            String,
            String,
            f64,
            String,
            Option<String>,
            Option<String>,
            Option<f64>,
            Option<String>,
            Option<String>,
            Option<f64>,
        );
        let mut stmt = self.conn.prepare(&sql.text).map_err(db)?;
        let rows: Vec<Raw> = stmt
            .query_map(params_from_iter(sql.params.iter()), |r| {
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
                ))
            })
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?;
        rows.into_iter()
            .map(
                |(
                    id,
                    title,
                    started,
                    spk,
                    talk_s,
                    assignment,
                    pid,
                    pname,
                    score,
                    sid,
                    sname,
                    sscore,
                )| {
                    Ok(ReviewItem {
                        meeting_id: meeting_id(&id)?,
                        meeting_title: title,
                        started_at: timestamp(&started)?,
                        speaker_id: speaker_id(&spk)?,
                        talk_s,
                        assignment: parse_enum(assignment)?,
                        person_id: person_id(pid)?,
                        person_name: pname,
                        score: score.map(|s| s as f32),
                        suggested_person_id: person_id(sid)?,
                        suggested_name: sname,
                        suggested_score: sscore.map(|s| s as f32),
                    })
                },
            )
            .collect()
    }

    /// Every indexed person by name, with meeting count and talk time.
    pub fn list_people(&self) -> Result<Vec<PersonSummary>> {
        self.read_snapshot(|| self.list_people_in_snapshot())
    }

    fn list_people_in_snapshot(&self) -> Result<Vec<PersonSummary>> {
        type Raw = (String, String, bool, i64, f64, i64);
        let hue = if self.version >= PERSON_HUES_VERSION {
            "p.hue"
        } else {
            "0"
        };
        let rows: Vec<Raw> = {
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT p.id, p.name, p.is_self,
                            (SELECT count(DISTINCT s.meeting_id) FROM speakers s
                             WHERE s.person_id = p.id AND s.assignment IN ('confirmed', 'auto')),
                            (SELECT coalesce(sum(s.talk_s), 0.0) FROM speakers s
                             WHERE s.person_id = p.id AND s.assignment IN ('confirmed', 'auto')),
                            {hue}
                     FROM people p ORDER BY p.name_fold, p.id",
                ))
                .map_err(db)?;
            stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?
        };
        let mut aliases = self
            .conn
            .prepare("SELECT alias FROM person_aliases WHERE person_id = ?1 ORDER BY rowid")
            .map_err(db)?;
        let mut emails = self
            .conn
            .prepare("SELECT email FROM person_emails WHERE person_id = ?1 ORDER BY rowid")
            .map_err(db)?;
        rows.into_iter()
            .map(|(id, name, is_self, meetings, talk_s, hue)| {
                let aliases = aliases
                    .query_map([&id], |r| r.get(0))
                    .map_err(db)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(db)?;
                let emails = emails
                    .query_map([&id], |r| r.get(0))
                    .map_err(db)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(db)?;
                let id = PersonId::parse(&id).map_err(|e| bad_row("person ID", e))?;
                // 0: a row indexed before hues (layout 8), whose person had
                // no stored hue either, so the derived one is theirs.
                let hue = match u8::try_from(hue) {
                    Ok(hue @ 1..=crate::schema::PERSON_HUES) => hue,
                    _ => crate::schema::derived_hue(&id),
                };
                Ok(PersonSummary {
                    id,
                    name,
                    is_self,
                    aliases,
                    emails,
                    hue,
                    meetings: as_u32(meetings)?,
                    talk_s,
                })
            })
            .collect()
    }

    /// Every indexed meeting, newest first.
    pub fn meeting_ids(&self) -> Result<Vec<MeetingId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM meetings ORDER BY started_unix DESC, id")
            .map_err(db)?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .map_err(db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db)?;
        ids.iter().map(|s| meeting_id(s)).collect()
    }

    /// The meetings person `id` is related to, newest first (then by ID),
    /// each with how: a candidate list for reading the meetings' files,
    /// which decide (an index row never names anyone by itself).
    /// [`PersonRelation::LiveReview`] only says a live decision awaiting
    /// review targets the person, never that a line or speaker is theirs.
    ///
    /// Refused (`schema`, "run `broscribe index rebuild`") while the index
    /// holds rows written before it recorded relations: a layout older than
    /// 8 that a read-only open could not upgrade, or one upgraded in place
    /// and not rebuilt since ([`Index::health`] reports it
    /// [`super::IndexDatabase::Outdated`] and asks for the rebuild). An empty
    /// answer is then never passed off as current.
    pub fn person_meeting_candidates(&self, id: &PersonId) -> Result<Vec<PersonMeetingCandidate>> {
        self.read_snapshot(|| {
            if self.version < RELATIONS_VERSION || super::schema::rebuild_required(&self.conn)? {
                return Err(Error::Schema(
                    "the index predates its person relations; run `broscribe index rebuild`".into(),
                ));
            }
            let rows: Vec<(String, String, String)> = {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT r.meeting_id, m.started_at, r.relation
                         FROM person_meetings r JOIN meetings m ON m.id = r.meeting_id
                         WHERE r.person_id = ?1
                         ORDER BY m.started_unix DESC, r.meeting_id",
                    )
                    .map_err(db)?;
                stmt.query_map([id.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                    .map_err(db)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(db)?
            };
            let mut out: Vec<PersonMeetingCandidate> = Vec::new();
            for (meeting, started, relation) in rows {
                let relation: PersonRelation = parse_enum(relation)?;
                match out.last_mut() {
                    Some(last) if last.meeting_id.as_str() == meeting => {
                        last.relations.push(relation);
                    }
                    _ => out.push(PersonMeetingCandidate {
                        meeting_id: meeting_id(&meeting)?,
                        started_at: timestamp(&started)?,
                        relations: vec![relation],
                    }),
                }
            }
            for candidate in &mut out {
                candidate.relations.sort();
            }
            Ok(out)
        })
    }
}
