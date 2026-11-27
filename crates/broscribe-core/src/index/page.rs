//! Keyset pages of index reads: [`Index::search_meetings_page`],
//! [`Index::search_page`] and [`Index::list_actions_page`].
//!
//! - Every page is read in one SQLite snapshot, in the same order as the
//!   unpaged read with a total tie-break (row ID, window ID or item order),
//!   and continues strictly after the last row of the page before: no
//!   offsets, no truncated page passed off as complete. A page holds
//!   `page_size` rows unless it is the last one, which has no
//!   `next_cursor`.
//! - A [`PageCursor`] is opaque: URL-safe base64 of a small versioned
//!   record holding the kind of read, the index identity it was read at
//!   ([`IndexRevision`]; for semantic and hybrid search also the vector
//!   cache's), a fingerprint of the query, the instant the first page
//!   resolved its relative date filters at ([`CursorRequest::anchor`]) and
//!   the last row's sort key. It never holds query text, names or snippets:
//!   the fingerprint is a SHA-256 keyed by a secret that never leaves the
//!   index.
//! - A continuation resolves its relative date filters (`30d`) at the
//!   anchor its cursor carries, not at its own time, so every page reads the
//!   same date window however much later it is asked for.
//! - A cursor read after any committed index write, a rebuild, an upgrade,
//!   or a deleted and recreated index (or, for semantic and hybrid search,
//!   a changed vector cache) is [`CURSOR_STALE`]; a cursor of another query,
//!   anchor or kind of read is [`CURSOR_MISMATCH`]; one that does not decode
//!   is [`INVALID_CURSOR`]. Callers start again from the first page.

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::types::Value;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Index, schema};
use crate::schema::hex;
use crate::{Error, MeetingId, Result};

/// The largest page a paged read returns.
pub const MAX_PAGE_SIZE: usize = 500;
/// The longest cursor accepted (cursors issued are far shorter).
pub const MAX_CURSOR_LEN: usize = 1024;
/// Error code: the index (or vector cache) changed since the cursor was issued.
pub const CURSOR_STALE: &str = "cursor_stale";
/// Error code: the cursor belongs to another query or kind of read.
pub const CURSOR_MISMATCH: &str = "cursor_mismatch";
/// Error code: the cursor is not one this index issued.
pub const INVALID_CURSOR: &str = "invalid_cursor";

/// Version of the encoded cursor record (2: with the first page's anchor).
const CURSOR_FORMAT: u8 = 2;
/// Hex digits of an index or vector-cache instance.
const INSTANCE_HEX: usize = 32;
/// Hex digits of a query fingerprint.
const QUERY_HEX: usize = 64;
/// Hex digits of a window ID (`windows::window_id`).
const WINDOW_HEX: usize = 32;

/// Which index state a read saw.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRevision {
    /// Random ID, new whenever the index is created (also after `.index/`
    /// was deleted), rebuilt or upgraded in place. Empty for an index no
    /// writer has opened since identities were introduced.
    pub instance: String,
    /// Moves with every committed index write.
    pub revision: u64,
}

/// An opaque position in a paged read, from [`Page::next_cursor`]. Its text
/// is URL-safe and at most [`MAX_CURSOR_LEN`] bytes; [`PageCursor::parse`]
/// checks its shape, the read checks what it is bound to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PageCursor {
    text: String,
    /// The anchor of the read's first page, decoded from `text`.
    anchor: Option<DateTime<Utc>>,
}

impl PageCursor {
    /// A cursor given back by a caller; `invalid_cursor` when it does not
    /// decode to a cursor record.
    pub fn parse(s: &str) -> Result<Self> {
        Self::try_from(s.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The instant the read's first page resolved its relative date filters
    /// at ([`CursorRequest::anchor`]): a continuation resolves them at this
    /// instant again. `None` when that page had no anchor.
    pub fn anchor(&self) -> Option<DateTime<Utc>> {
        self.anchor
    }
}

impl fmt::Display for PageCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl FromStr for PageCursor {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl TryFrom<String> for PageCursor {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        let anchor = CursorState::decode(&s)?.anchor;
        Ok(Self { text: s, anchor })
    }
}

impl From<PageCursor> for String {
    fn from(c: PageCursor) -> String {
        c.text
    }
}

/// Which page to read: the first one, or the one after `after`, and the
/// instant the query's relative date filters were resolved at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorRequest {
    /// Rows per page, 1 to [`MAX_PAGE_SIZE`].
    pub page_size: usize,
    /// The previous page's [`Page::next_cursor`]; `None` for the first page.
    pub after: Option<PageCursor>,
    /// The instant the caller resolved the query's relative date filters
    /// (`since 30d`) at. A first page sets it ([`CursorRequest::first_at`]);
    /// every cursor after that page carries it and [`CursorRequest::after`]
    /// takes it back, so the caller resolves those filters at the same
    /// instant on every page and the date window never moves. It is part of
    /// the cursor's query: a continuation at another anchor is
    /// [`CURSOR_MISMATCH`]. `None` for a caller that resolves no relative
    /// filters (it gives instants).
    pub anchor: Option<DateTime<Utc>>,
}

impl CursorRequest {
    /// A first page of a query without relative date filters.
    pub fn first(page_size: usize) -> Self {
        Self {
            page_size,
            after: None,
            anchor: None,
        }
    }

    /// A first page of a query whose relative date filters were resolved at
    /// `anchor` (the caller's clock when it asked).
    pub fn first_at(page_size: usize, anchor: DateTime<Utc>) -> Self {
        Self {
            page_size,
            after: None,
            anchor: Some(anchor),
        }
    }

    /// The page after `cursor`, at the anchor of its first page
    /// ([`PageCursor::anchor`]).
    pub fn after(cursor: PageCursor, page_size: usize) -> Self {
        Self {
            page_size,
            anchor: cursor.anchor(),
            after: Some(cursor),
        }
    }
}

/// One page of a paged read.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Where the next page starts; `None` on the last page.
    pub next_cursor: Option<PageCursor>,
    /// The index state the page was read at.
    pub revision: IndexRevision,
}

/// The kind of paged read a cursor belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PageKind {
    Meetings,
    Search,
    Actions,
}

/// The sort key of a page's last row. Floats are kept as their bits, so
/// the next page compares against exactly the value the index returned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub(super) enum PageKey {
    /// Meetings without text: newest first.
    Meeting { started: i64, id: MeetingId },
    /// Meetings matched by text: bm25, newest first, `meetings_fts` row.
    RankedMeeting {
        score: u64,
        started: i64,
        id: MeetingId,
        row: i64,
    },
    /// Utterances listed without text: newest meeting first, transcript
    /// order, utterance row.
    Utterance {
        started: i64,
        meeting: MeetingId,
        start: u64,
        segment: i64,
        row: i64,
    },
    /// Utterances matched by keyword: bm25, then as [`PageKey::Utterance`].
    RankedUtterance {
        score: u64,
        started: i64,
        meeting: MeetingId,
        start: u64,
        segment: i64,
        row: i64,
    },
    /// A window of the fused (semantic or hybrid) ranking.
    Window {
        score: u64,
        best: u64,
        keyword: bool,
        window: String,
    },
    /// Actions: newest meeting first, meeting order.
    Action {
        started: i64,
        meeting: MeetingId,
        ord: i64,
    },
}

impl PageKey {
    fn is_valid(&self) -> bool {
        let finite = |bits: &u64| f64::from_bits(*bits).is_finite();
        let segment = |s: &i64| u32::try_from(*s).is_ok();
        match self {
            PageKey::Meeting { .. } => true,
            PageKey::RankedMeeting { score, .. } => finite(score),
            PageKey::Utterance {
                start, segment: s, ..
            } => finite(start) && segment(s),
            PageKey::RankedUtterance {
                score,
                start,
                segment: s,
                ..
            } => finite(score) && finite(start) && segment(s),
            PageKey::Window {
                score,
                best,
                window,
                ..
            } => finite(score) && *best >= 1 && is_hex(window, WINDOW_HEX),
            PageKey::Action { ord, .. } => *ord >= 0,
        }
    }
}

/// The record a [`PageCursor`] encodes.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorState {
    v: u8,
    kind: PageKind,
    index: IndexRevision,
    vectors: Option<IndexRevision>,
    query: String,
    anchor: Option<DateTime<Utc>>,
    key: PageKey,
}

impl CursorState {
    fn encode(&self) -> Result<PageCursor> {
        let json =
            serde_json::to_vec(self).map_err(|e| Error::Internal(format!("page cursor: {e}")))?;
        let text = URL_SAFE_NO_PAD.encode(json);
        if text.len() > MAX_CURSOR_LEN {
            return Err(Error::Internal("page cursor longer than its bound".into()));
        }
        Ok(PageCursor {
            text,
            anchor: self.anchor,
        })
    }

    fn decode(text: &str) -> Result<Self> {
        if text.is_empty() || text.len() > MAX_CURSOR_LEN {
            return Err(invalid("its length is out of bounds"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(text)
            .map_err(|_| invalid("it is not URL-safe base64"))?;
        let state: Self =
            serde_json::from_slice(&bytes).map_err(|_| invalid("it is not a page cursor"))?;
        let identity_ok =
            |r: &IndexRevision| r.instance.is_empty() || is_hex(&r.instance, INSTANCE_HEX);
        if state.v != CURSOR_FORMAT
            || !identity_ok(&state.index)
            || !state.vectors.as_ref().is_none_or(identity_ok)
            || !is_hex(&state.query, QUERY_HEX)
            || !state.key.is_valid()
        {
            return Err(invalid("its fields are malformed"));
        }
        Ok(state)
    }
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn invalid(why: &str) -> Error {
    Error::Service {
        code: INVALID_CURSOR,
        message: format!("invalid page cursor: {why}"),
    }
}

/// A cursor whose fingerprint matches but whose key does not fit the query.
pub(super) fn invalid_key() -> Error {
    invalid("its sort key does not fit this query")
}

fn stale() -> Error {
    Error::Service {
        code: CURSOR_STALE,
        message:
            "the search index changed since this page was read; read again from the first page"
                .into(),
    }
}

fn mismatch() -> Error {
    Error::Service {
        code: CURSOR_MISMATCH,
        message: "this page cursor belongs to another query".into(),
    }
}

/// Paged reads take their size from the [`CursorRequest`], never from the
/// query's own `limit`.
pub(super) fn unpaged_limit(limit: Option<usize>) -> Result<()> {
    match limit {
        None => Ok(()),
        Some(_) => Err(Error::InvalidInput(
            "a paged read takes its size from the page request; leave the query's limit unset"
                .into(),
        )),
    }
}

/// The parts of a query that decide its rows and their order, hashed with
/// the index's cursor secret. Each field is named and length-prefixed, so
/// different queries never feed the same bytes.
pub(super) struct QueryFingerprint(Sha256);

impl QueryFingerprint {
    fn new(secret: &str, kind: PageKind) -> Self {
        let mut fp = Self(Sha256::new());
        fp.0.update(b"broscribe page query\0");
        fp.field("secret", Some(secret.as_bytes()));
        fp.field(
            "kind",
            Some(match kind {
                PageKind::Meetings => b"meetings".as_slice(),
                PageKind::Search => b"search".as_slice(),
                PageKind::Actions => b"actions".as_slice(),
            }),
        );
        fp
    }

    pub(super) fn text(&mut self, name: &str, value: Option<&str>) {
        self.field(name, value.map(str::as_bytes));
    }

    pub(super) fn int(&mut self, name: &str, value: Option<i64>) {
        let bytes = value.map(i64::to_le_bytes);
        self.field(name, bytes.as_ref().map(<[u8; 8]>::as_slice));
    }

    pub(super) fn floats(&mut self, name: &str, values: &[f32]) {
        let bytes: Vec<u8> = values
            .iter()
            .flat_map(|v| v.to_bits().to_le_bytes())
            .collect();
        self.field(name, Some(&bytes));
    }

    fn field(&mut self, name: &str, value: Option<&[u8]>) {
        let put = |h: &mut Sha256, b: &[u8]| {
            h.update((b.len() as u64).to_le_bytes());
            h.update(b);
        };
        put(&mut self.0, name.as_bytes());
        match value {
            None => self.0.update([0u8]),
            Some(v) => {
                self.0.update([1u8]);
                put(&mut self.0, v);
            }
        }
    }

    fn finish(self) -> String {
        hex(&self.0.finalize())
    }
}

/// Sort direction of one sort-key column.
#[derive(Clone, Copy)]
pub(super) enum Dir {
    Asc,
    Desc,
}

/// SQL condition true for the rows sorting strictly after the cursor row
/// in the order `cols` gives: each column's SQL expression, direction and
/// the cursor row's value. `bind` binds a value and returns its `?N`.
pub(super) fn after_sql(
    cols: Vec<(&str, Dir, Value)>,
    mut bind: impl FnMut(Value) -> usize,
) -> String {
    let mut condition: Option<String> = None;
    for (expr, dir, value) in cols.into_iter().rev() {
        let n = bind(value);
        let op = match dir {
            Dir::Asc => '>',
            Dir::Desc => '<',
        };
        condition = Some(match condition {
            None => format!("{expr} {op} ?{n}"),
            Some(rest) => format!("({expr} {op} ?{n} OR ({expr} = ?{n} AND {rest}))"),
        });
    }
    format!("({})", condition.unwrap_or_else(|| "1 = 1".into()))
}

/// A float sort-key value as SQL compares it.
pub(super) fn real(bits: u64) -> Value {
    Value::Real(f64::from_bits(bits))
}

impl Index {
    /// The index state this connection reads now. Inside a read snapshot
    /// it is the snapshot's; a later write or rebuild changes it.
    pub fn read_revision(&self) -> Result<IndexRevision> {
        self.read_snapshot(|| Ok(self.identity()?.0))
    }

    /// The index identity and cursor secret in the current snapshot.
    fn identity(&self) -> Result<(IndexRevision, String)> {
        let (instance, secret, revision) = schema::identity(&self.conn, "main")?;
        Ok((IndexRevision { instance, revision }, secret))
    }

    /// The attached vector cache's identity; `None` without one.
    fn vector_identity(&self) -> Result<Option<IndexRevision>> {
        if !self.vectors {
            return Ok(None);
        }
        let (instance, _, revision) = schema::identity(&self.conn, "vec")?;
        Ok(Some(IndexRevision { instance, revision }))
    }

    /// Reads one page in one snapshot: checks the request's cursor against
    /// the current index (and, with `uses_vectors`, vector-cache) identity
    /// and the query's `fingerprint` (with the request's anchor), then has
    /// `read` return at most `take` rows after the cursor's key, each with
    /// its own key. One row more than the page size is read, so a full page
    /// has a next cursor exactly when another row follows; that cursor
    /// carries the request's anchor on.
    pub(super) fn read_page<T>(
        &self,
        kind: PageKind,
        request: &CursorRequest,
        uses_vectors: bool,
        fingerprint: impl FnOnce(&mut QueryFingerprint) -> Result<()>,
        read: impl FnOnce(Option<&PageKey>, usize) -> Result<Vec<(T, PageKey)>>,
    ) -> Result<Page<T>> {
        let size = request.page_size;
        if !(1..=MAX_PAGE_SIZE).contains(&size) {
            return Err(Error::InvalidInput(format!(
                "page size must be 1 to {MAX_PAGE_SIZE}, not {size}"
            )));
        }
        let after = request
            .after
            .as_ref()
            .map(|c| CursorState::decode(c.as_str()))
            .transpose()?;
        self.read_snapshot(|| {
            let (index, secret) = self.identity()?;
            let vectors = if uses_vectors {
                self.vector_identity()?
            } else {
                None
            };
            let mut fp = QueryFingerprint::new(&secret, kind);
            let anchor = request
                .anchor
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Nanos, true));
            fp.text("anchor", anchor.as_deref());
            fingerprint(&mut fp)?;
            let query = fp.finish();
            if let Some(state) = &after {
                if state.kind != kind {
                    return Err(mismatch());
                }
                if state.index != index || state.vectors != vectors {
                    return Err(stale());
                }
                if state.query != query {
                    return Err(mismatch());
                }
            }
            let mut rows = read(after.as_ref().map(|s| &s.key), size + 1)?;
            let next_cursor = if rows.len() > size {
                rows.truncate(size);
                let key = rows
                    .last()
                    .map(|(_, key)| key.clone())
                    .ok_or_else(|| Error::Internal("empty page with more rows".into()))?;
                let state = CursorState {
                    v: CURSOR_FORMAT,
                    kind,
                    index: index.clone(),
                    vectors,
                    query,
                    anchor: request.anchor,
                    key,
                };
                Some(state.encode()?)
            } else {
                None
            };
            Ok(Page {
                items: rows.into_iter().map(|(item, _)| item).collect(),
                next_cursor,
                revision: index,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> CursorState {
        CursorState {
            v: CURSOR_FORMAT,
            kind: PageKind::Search,
            index: IndexRevision {
                instance: "0".repeat(INSTANCE_HEX),
                revision: 7,
            },
            vectors: None,
            query: "a".repeat(QUERY_HEX),
            anchor: Some(
                DateTime::parse_from_rfc3339("2026-11-14T12:00:00.123456789-03:00")
                    .unwrap()
                    .to_utc(),
            ),
            key: PageKey::RankedUtterance {
                score: (-1.25f64).to_bits(),
                started: 1_700_000_000,
                meeting: MeetingId::parse("m-000001").unwrap(),
                start: 4.5f64.to_bits(),
                segment: 3,
                row: 12,
            },
        }
    }

    #[test]
    fn cursors_round_trip_exactly_within_their_bound() {
        let cursor = state().encode().unwrap();
        assert!(cursor.as_str().len() <= MAX_CURSOR_LEN);
        assert!(
            cursor
                .as_str()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(CursorState::decode(cursor.as_str()).unwrap(), state());
        let parsed = PageCursor::parse(cursor.as_str()).unwrap();
        assert_eq!(parsed, cursor);
        assert_eq!(
            parsed.anchor().unwrap().to_rfc3339(),
            "2026-11-14T15:00:00.123456789+00:00"
        );
        let json = serde_json::to_string(&cursor).unwrap();
        assert_eq!(serde_json::from_str::<PageCursor>(&json).unwrap(), cursor);
    }

    #[test]
    fn malformed_cursors_are_invalid() {
        let encode = |value: serde_json::Value| URL_SAFE_NO_PAD.encode(value.to_string());
        let good = serde_json::to_value(state()).unwrap();
        let with = |field: &str, value: serde_json::Value| {
            let mut v = good.clone();
            v[field] = value;
            encode(v)
        };
        let mut nan_key = good.clone();
        nan_key["key"]["score"] = serde_json::json!(f64::NAN.to_bits());
        let mut bad_meeting = good.clone();
        bad_meeting["key"]["meeting"] = serde_json::json!("../x");
        let mut extra = good.clone();
        extra["extra"] = serde_json::json!(1);
        for text in [
            String::new(),
            "not base64!".into(),
            "A".repeat(MAX_CURSOR_LEN + 1),
            URL_SAFE_NO_PAD.encode("{}"),
            with("v", serde_json::json!(1)),
            with("v", serde_json::json!(3)),
            with("anchor", serde_json::json!("yesterday")),
            with("anchor", serde_json::json!(1_700_000_000)),
            with("query", serde_json::json!("short")),
            with("kind", serde_json::json!("people")),
            with(
                "index",
                serde_json::json!({"instance": "XYZ", "revision": 1}),
            ),
            encode(nan_key),
            encode(bad_meeting),
            encode(extra),
        ] {
            let err = PageCursor::parse(&text).unwrap_err();
            assert_eq!(err.code(), INVALID_CURSOR, "{text}");
        }
    }
}
