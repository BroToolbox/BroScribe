//! Filters → core queries, shared by the CLI read commands and the MCP read
//! tools so both answer the same question the same way. Each front end keeps
//! its own defaults (limits, guards) and output shape.

use std::path::Path;

use chrono::{DateTime, FixedOffset, Local, Utc};

use broscribe_core::config::Config;
use broscribe_core::export::{self, ExportDoc, ExportFormat, ExportSection};
use broscribe_core::index::{
    ActionQuery, CursorRequest, DecisionQuery, Index, MAX_PAGE_SIZE, Page, PageCursor, SearchQuery,
    TemplateHit,
};
use broscribe_core::ops;
use broscribe_core::render;
use broscribe_core::schema::{ActionStatus, Meeting};
use broscribe_core::templates::{self, Template};
use broscribe_core::transcript::TranscriptSegment;
use broscribe_core::{Error, Library, MeetingId, Result};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::args::parse_time_bound_at;

/// Adds who said one transcript segment, as core's projection decides it
/// ([`broscribe_core::transcript`]), to a front end's row: `person_id`,
/// `person_name`, `attribution` (`"line"` or `"speaker"`), the shown
/// speaker's `assignment` and, for a line correction made from a live
/// decision, `live_decision`.
pub(crate) fn provenance(s: &TranscriptSegment<'_>, row: &mut Value) {
    row["person_id"] = json!(s.person_id);
    row["person_name"] = json!(s.person_name);
    row["attribution"] = json!(s.attribution.as_str());
    row["assignment"] = json!(s.assignment);
    if s.overlapping_speech {
        row["overlapping_speech"] = json!(true);
    }
    if let Some(decision) = s.live_decision {
        row["live_decision"] = json!(decision);
    }
}

/// The page a CLI or MCP read asks for: `None` without `page_size` (an
/// unpaged read). `page_size` is 1 to [`MAX_PAGE_SIZE`]; `cursor` needs it
/// and must be one a page returned (`invalid_cursor` otherwise). A first
/// page is anchored at `now`, the instant its relative date filters resolve
/// at; a continuation keeps its first page's anchor ([`resolve_at`]).
pub(crate) fn page_request(
    page_size: Option<usize>,
    cursor: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<CursorRequest>> {
    let Some(size) = page_size else {
        return match cursor {
            Some(_) => Err(Error::InvalidInput(
                "a cursor continues a paged read: give page_size too".into(),
            )),
            None => Ok(None),
        };
    };
    if !(1..=MAX_PAGE_SIZE).contains(&size) {
        return Err(Error::InvalidInput(format!(
            "page_size must be 1 to {MAX_PAGE_SIZE}"
        )));
    }
    Ok(Some(match cursor {
        Some(c) => CursorRequest::after(PageCursor::parse(c)?, size),
        None => CursorRequest::first_at(size, now),
    }))
}

/// The instant a read resolves its relative date filters (`30d`, `12h`)
/// at: a continued page's first-page anchor, so every page of one read sees
/// the same date window however much later it is asked for; otherwise
/// `now`.
pub(crate) fn resolve_at(page: Option<&CursorRequest>, now: DateTime<Utc>) -> DateTime<Utc> {
    page.and_then(|p| p.anchor).unwrap_or(now)
}

/// One page as the CLI and MCP show it: its rows under `key`, the index
/// `revision` it was read at and, unless it is the last page,
/// `next_cursor`.
pub(crate) fn page_value<T>(
    key: &str,
    page: Page<T>,
    row: impl FnMut(T) -> Result<Value>,
) -> Result<Value> {
    let rows = page
        .items
        .into_iter()
        .map(row)
        .collect::<Result<Vec<_>>>()?;
    let mut value = serde_json::Map::new();
    value.insert(key.to_owned(), Value::Array(rows));
    value.insert("revision".into(), json!(page.revision));
    if let Some(cursor) = page.next_cursor {
        value.insert("next_cursor".into(), json!(cursor));
    }
    Ok(Value::Object(value))
}

/// Filters of a meeting or utterance search, as given (unparsed).
#[derive(Debug, Default)]
pub(crate) struct SearchFilters<'a> {
    pub query: Option<&'a str>,
    pub person: Option<&'a str>,
    pub tag: Option<&'a str>,
    pub topic: Option<&'a str>,
    pub app: Option<&'a str>,
    pub source: Option<broscribe_core::schema::Source>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub meeting: Option<&'a str>,
    pub limit: Option<usize>,
}

/// Filters of an action-item listing, as given.
#[derive(Debug, Default)]
pub(crate) struct ActionFilters<'a> {
    /// `me`, or a person ID, name, alias or e-mail.
    pub owner: Option<&'a str>,
    pub status: Option<ActionStatus>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub meeting: Option<&'a str>,
    pub query: Option<&'a str>,
    pub limit: Option<usize>,
}

/// Filters of a decision listing, as given.
#[derive(Debug, Default)]
pub(crate) struct DecisionFilters<'a> {
    pub topic: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub meeting: Option<&'a str>,
    pub query: Option<&'a str>,
    pub limit: Option<usize>,
}

/// Query text, trimmed; blank text is no text.
pub(crate) fn text(query: Option<&str>) -> Option<String> {
    query
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// People-list rows (CLI `people`, MCP `list_people`): each person's index
/// summary plus `voice_samples`, the count the app's People list shows. An
/// unreadable voiceprint file counts as none and is logged by its code.
pub(crate) fn people_rows(
    lib: &Library,
    people: &[broscribe_core::index::PersonSummary],
) -> Result<Vec<(Value, usize)>> {
    people
        .iter()
        .map(|p| {
            let count = lib.load_voiceprints(&p.id).map(|v| v.embeddings.len());
            if let Err(e) = &count {
                tracing::warn!(code = e.code(), "reading voiceprints");
            }
            let count = count.unwrap_or_default();
            let mut row = serde_json::to_value(p).map_err(|e| Error::Internal(e.to_string()))?;
            row["voice_samples"] = json!(count);
            Ok((row, count))
        })
        .collect()
}

/// Who produced the meeting's enrichment, for `show` and `meeting_view`:
/// `model` is the one the provider reported when `model_reported`, else the
/// one requested (`null`: the provider's default); `null` without an
/// enrichment.
pub(crate) fn enrichment_view(m: &Meeting) -> Value {
    m.enrichment.as_ref().map_or(Value::Null, |e| {
        json!({
            "provider": e.provider,
            "instructions": e.instructions,
            "tags": e.tags,
            "topics": e.topics,
            "model": e.model,
            "requested_model": e.requested_model,
            "model_reported": e.model_reported,
            "created_at": e.created_at,
            "can_restore": m.previous_enrichment.is_some(),
            "stale": broscribe_core::ops::enrichment_is_stale(m),
        })
    })
}

/// Template definitions (built-in and user, each with `auto` when it runs
/// after each meeting's summary) for `templates list` and `list_templates`;
/// with `meeting`, also that meeting's stored results, including those whose
/// definition was deleted since (`defined: false`).
pub(crate) fn templates_view(
    data_dir: &Path,
    config: &Config,
    meeting: Option<&Meeting>,
) -> Result<Value> {
    let defined = templates::list(data_dir)?;
    let rows: Vec<Value> = defined
        .iter()
        .map(|t| template_row(t, config))
        .collect::<Result<_>>()?;
    let mut value = json!({ "templates": rows });
    if let Some(m) = meeting {
        let results: Vec<Value> = m
            .template_results
            .iter()
            .map(|r| {
                json!({
                    "template_id": r.template_id,
                    "template_name": r.template_name,
                    "created_at": r.created_at,
                    "stale": ops::template_is_stale(m, r),
                    "defined": defined.iter().any(|t| t.id == r.template_id),
                })
            })
            .collect();
        value["meeting_id"] = json!(m.id);
        value["results"] = json!(results);
    }
    Ok(value)
}

/// One template definition as `templates list|show` and `list_templates`
/// show it.
pub(crate) fn template_row(t: &Template, config: &Config) -> Result<Value> {
    let mut v = crate::to_value(t)?;
    v["auto"] = json!(config.templates.auto.contains(&t.id));
    Ok(v)
}

/// A meeting's result for one template (`templates result`,
/// `get_template_result`): its Markdown with names filled in through the
/// result's own line attribution, and who produced it. A result outlives
/// its definition.
pub(crate) fn template_result_view(
    lib: &Library,
    meeting: &MeetingId,
    template_id: &str,
) -> Result<Value> {
    let m = lib.load_meeting(meeting)?;
    let r = m
        .template_results
        .iter()
        .find(|r| r.template_id == template_id)
        .ok_or_else(|| {
            Error::NotFound(format!(
                "a result of template {template_id:?} in meeting {meeting}"
            ))
        })?;
    let people = lib.people_lookup()?;
    Ok(json!({
        "meeting_id": m.id,
        "template_id": r.template_id,
        "template_name": r.template_name,
        "instructions_sha256": r.instructions_sha256,
        "provider": r.provider,
        "model": r.model,
        "requested_model": r.requested_model,
        "model_reported": r.model_reported,
        "created_at": r.created_at,
        "can_restore": m.previous_template_results.contains_key(template_id),
        "stale": ops::template_is_stale(&m, r),
        // Markdown: substituted names are escaped so a name can never open
        // a heading, list, link or HTML; the template's own prose is kept.
        "markdown": render::substitute_speakers_as(
            &m,
            &r.line_people,
            &r.markdown,
            &people,
            render::NameStyle::Markdown,
        ),
    }))
}

/// Template results matching every word (keyword only), for `search --in
/// template` and `search_templates`. Words are required: there is no
/// listing of every result.
pub(crate) fn template_hits(
    lib: &Library,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<TemplateHit>> {
    let words = text(query)
        .ok_or_else(|| Error::InvalidInput("give words to find in template results".into()))?;
    Index::open_readonly(lib.root())?.search_templates(&words, Some(limit))
}

/// The export of `export` and `export_meeting`: `include` names sections
/// (`transcript`, `summary`, `decisions`, `actions`, `template:<id>`);
/// empty means every section the meeting has.
pub(crate) fn export_doc(
    lib: &Library,
    meeting: &MeetingId,
    include: &[String],
    format: ExportFormat,
    timestamps: bool,
) -> Result<ExportDoc> {
    let m = lib.load_meeting(meeting)?;
    let sections = if include.is_empty() {
        export::available_sections(&m)
    } else {
        include
            .iter()
            .map(|s| ExportSection::parse(s.trim()))
            .collect::<Result<Vec<_>>>()?
    };
    export::render_export(&m, &lib.people_lookup()?, &sections, format, timestamps)
}

/// A `since`/`until` filter resolved at `at` (relative ages count back from
/// it; dates and times are absolute).
fn bound(
    value: Option<&str>,
    end_of_day: bool,
    at: DateTime<Utc>,
) -> Result<Option<DateTime<FixedOffset>>> {
    let now = at.with_timezone(&Local).fixed_offset();
    value
        .map(|s| parse_time_bound_at(s, end_of_day, now))
        .transpose()
}

fn meeting(value: Option<&str>) -> Result<Option<MeetingId>> {
    value.map(crate::cmd::meeting_id).transpose()
}

/// The [`SearchQuery`] of both `search`/`meetings` and `search_utterances`/
/// `search_meetings` (mode and query vector are the caller's), its relative
/// dates resolved at `at` ([`resolve_at`]).
pub(crate) fn search_query(f: &SearchFilters<'_>, at: DateTime<Utc>) -> Result<SearchQuery> {
    Ok(SearchQuery {
        text: text(f.query),
        person: f.person.map(str::to_owned),
        tag: f.tag.map(str::to_owned),
        topic: f.topic.map(str::to_owned),
        app: f.app.map(str::to_owned),
        source: f.source,
        since: bound(f.since, false, at)?,
        until: bound(f.until, true, at)?,
        meeting: meeting(f.meeting)?,
        limit: f.limit,
        ..SearchQuery::default()
    })
}

/// The [`ActionQuery`] of `actions` and `list_action_items`, its relative
/// dates resolved at `at` ([`resolve_at`]); resolving the owner reads the
/// people files.
pub(crate) fn action_query(
    lib: &Library,
    f: &ActionFilters<'_>,
    at: DateTime<Utc>,
) -> Result<ActionQuery> {
    Ok(ActionQuery {
        owner: f
            .owner
            .map(|o| crate::cmd::actions::owner_id(lib, o))
            .transpose()?,
        status: f.status,
        since: bound(f.since, false, at)?,
        until: bound(f.until, true, at)?,
        meeting: meeting(f.meeting)?,
        text: text(f.query),
        limit: f.limit,
    })
}

/// The [`DecisionQuery`] of `decisions` and `list_decisions`, its relative
/// dates resolved at `at`.
pub(crate) fn decision_query(f: &DecisionFilters<'_>, at: DateTime<Utc>) -> Result<DecisionQuery> {
    Ok(DecisionQuery {
        topic: f.topic.map(str::to_owned),
        since: bound(f.since, false, at)?,
        until: bound(f.until, true, at)?,
        meeting: meeting(f.meeting)?,
        text: text(f.query),
        limit: f.limit,
    })
}

/// One JSON object read from stdin (`--query-stdin`), so query text and
/// names never appear in argv. `T` refuses unknown fields.
pub(crate) fn read_stdin<T: DeserializeOwned>(command: &str) -> Result<T> {
    serde_json::from_reader(std::io::stdin().lock())
        .map_err(|e| Error::InvalidInput(format!("{command} input on stdin: {e}")))
}

/// Stdin read whole, untrimmed and unvalidated: the one raw reader the text
/// transports build on, so sensitive text never appears in argv. A read
/// failure is `invalid_input` naming `command`.
pub(crate) fn read_stdin_whole(command: &str) -> Result<String> {
    use std::io::Read;
    let mut text = String::new();
    std::io::stdin()
        .lock()
        .read_to_string(&mut text)
        .map_err(|e| Error::InvalidInput(format!("{command} input on stdin: {e}")))?;
    Ok(text)
}

/// Text read whole from stdin (`--instructions-stdin`, `instructions set
/// --stdin`), trailing whitespace removed, so instructions never appear in
/// argv. Empty text is returned as such: for a run it means "none".
pub(crate) fn read_stdin_text(command: &str) -> Result<String> {
    checked_instructions(read_stdin_whole(command)?)
}

pub(crate) fn checked_instructions(mut text: String) -> Result<String> {
    text.truncate(text.trim_end().len());
    broscribe_core::config::validate_instructions(&text)?;
    Ok(text)
}

/// A title read whole from stdin (`title --stdin`, `record title --stdin`),
/// so titles never appear in argv; checked as [`checked_title`].
pub(crate) fn read_stdin_title(command: &str) -> Result<String> {
    checked_title(&read_stdin_whole(command)?)
}

/// The core's title rule (trimmed, one non-empty line, bounded length),
/// applied before the agent or the library is asked.
pub(crate) fn checked_title(text: &str) -> Result<String> {
    broscribe_core::ops::validate_title(text)
}
