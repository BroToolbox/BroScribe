//! Read commands: search, show, transcript, people. They take no lock and
//! read the index through a short-lived read-only connection.

use broscribe_core::index::{Index, MeetingHit, SearchHit, SearchMode, SearchQuery};
use broscribe_core::render::{NameStyle, format_hms, speaker_label};
use broscribe_core::schema::{Assignment, GroupOrigin, Strength};
use broscribe_core::transcript::{self, TranscriptFilter, TranscriptSegment};
use broscribe_core::{Error, PersonId, Result, SpeakerId, ops};
use broscribe_integrations::{
    AccountRef, DirectoryPerson, IntegrationError, IntegrationId, MicrosoftAccount, PublicError,
};
use broscribe_pipeline::vectors;
use serde_json::{Value, json};
use std::path::PathBuf;

use super::{meeting_id, speaker_id};
use crate::args::parse_clock;
use crate::cli::{MeetingsArgs, SearchArgs, SearchInArg, SearchModeArg, TranscriptArgs};
use crate::query::{
    SearchFilters, enrichment_view, page_request, page_value, provenance, read_stdin, resolve_at,
    search_query, template_hits,
};
use crate::{Ctx, to_value};

/// `search --query-stdin` / `meetings --query-stdin` input: the query and
/// textual filters, kept out of argv by app callers.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchStdin {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    person: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    topic: Option<String>,
}

pub(super) fn search(ctx: &mut Ctx<'_>, mut a: SearchArgs) -> Result<()> {
    if a.query_stdin {
        let input: SearchStdin = read_stdin("search")?;
        a.query = input.query;
        a.person = input.person;
        a.tag = input.tag.or(a.tag);
        a.topic = input.topic.or(a.topic);
    }
    if a.scope == SearchInArg::Template {
        return search_templates(ctx, &a);
    }
    let now = ctx.deps.clock.now();
    let page = page_request(a.page.page_size, a.page.cursor.as_deref(), now)?;
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let mut q = search_query(
        &SearchFilters {
            query: a.query.as_deref(),
            person: a.person.as_deref(),
            tag: a.tag.as_deref(),
            topic: a.topic.as_deref(),
            app: a.app.as_deref(),
            source: a.source.map(Into::into),
            since: a.since.as_deref(),
            until: a.until.as_deref(),
            meeting: a.meeting.as_deref(),
            // A page is bounded by its size; `--limit` belongs to unpaged reads.
            limit: page.is_none().then_some(a.limit),
        },
        resolve_at(page.as_ref(), now),
    )?;
    if q.text.is_none()
        && a.person.is_none()
        && a.tag.is_none()
        && a.topic.is_none()
        && a.app.is_none()
        && a.source.is_none()
        && a.since.is_none()
        && a.until.is_none()
        && a.meeting.is_none()
    {
        return Err(Error::InvalidInput(
            "give words to find or at least one filter (--person, --tag, --topic, --app, --source, --since, --until, --meeting)".into(),
        ));
    }
    let note = prepare_utterance_query(ctx, &config, &lib, &mut q, a.mode)?;
    if let Some(n) = &note {
        ctx.note(format!("note: {n}"));
    }
    let index = Index::open_readonly(lib.root())?;
    let hit_value = |h: &SearchHit| -> Result<Value> {
        let mut v = to_value(h)?;
        v["timestamp"] = json!(format_hms(h.start));
        Ok(v)
    };
    let (hits, mut value) = match &page {
        None => {
            let hits = index.search(&q)?;
            let results = hits.iter().map(hit_value).collect::<Result<Vec<_>>>()?;
            (hits, json!({ "results": results }))
        }
        Some(request) => {
            let mut hits = Vec::new();
            let value = page_value("results", index.search_page(&q, request)?, |h| {
                let v = hit_value(&h)?;
                hits.push(h);
                Ok(v)
            })?;
            (hits, value)
        }
    };
    value["query"] = json!(a.query);
    value["mode"] = json!(q.mode);
    value["note"] = json!(note);
    let next = next_cursor(&value);
    ctx.emit(value, |out| {
        if hits.is_empty() {
            writeln!(out, "No results.")?;
        }
        for h in &hits {
            writeln!(
                out,
                "{}  {}  {}  [{}] {}: {}",
                h.meeting_id,
                h.started_at.format("%Y-%m-%d %H:%M"),
                NameStyle::Plain.apply(h.meeting_title.as_deref().unwrap_or("(untitled)")),
                format_hms(h.start),
                NameStyle::Plain.apply(&h.speaker_name),
                h.snippet
            )?;
        }
        write_next_page(out, next.as_deref())
    })
}

fn prepare_utterance_query(
    ctx: &Ctx<'_>,
    config: &broscribe_core::config::Config,
    lib: &broscribe_core::Library,
    q: &mut SearchQuery,
    mode: Option<SearchModeArg>,
) -> Result<Option<String>> {
    let requested = mode.and_then(|mode| match mode {
        SearchModeArg::Auto => None,
        SearchModeArg::Keyword => Some(SearchMode::Keyword),
        SearchModeArg::Semantic => Some(SearchMode::Semantic),
        SearchModeArg::Hybrid => Some(SearchMode::Hybrid),
    });
    // Ranking by meaning needs words; filters alone list by keyword rules.
    let note = if q.text.is_some() && requested != Some(SearchMode::Keyword) {
        let model = vectors::text_model(
            config,
            ctx.data_dir(),
            ctx.deps.secrets.as_ref(),
            &ctx.deps.execution,
        )?;
        vectors::prepare_search(lib, &model, q, requested)?
    } else {
        q.mode = SearchMode::Keyword;
        None
    };
    Ok(note)
}

/// The `next_cursor` of a page value ([`page_value`]); `None` on the last
/// page and for unpaged reads.
fn next_cursor(value: &Value) -> Option<String> {
    value["next_cursor"].as_str().map(str::to_owned)
}

/// Human output's pointer to the next page, when there is one.
fn write_next_page(out: &mut dyn std::io::Write, next: Option<&str>) -> std::io::Result<()> {
    match next {
        Some(cursor) => writeln!(out, "More: run again with --cursor {cursor}"),
        None => Ok(()),
    }
}

/// `search --in template`: template results matching every word, keyword
/// only (results are not embedded) and by words alone.
fn search_templates(ctx: &mut Ctx<'_>, a: &SearchArgs) -> Result<()> {
    if matches!(
        a.mode,
        Some(SearchModeArg::Semantic | SearchModeArg::Hybrid)
    ) {
        return Err(Error::InvalidInput(
            "search --in template is keyword only (template results have no vectors)".into(),
        ));
    }
    if a.person.is_some()
        || a.tag.is_some()
        || a.topic.is_some()
        || a.app.is_some()
        || a.source.is_some()
        || a.since.is_some()
        || a.until.is_some()
        || a.meeting.is_some()
        || a.page.page_size.is_some()
    {
        return Err(Error::InvalidInput(
            "search --in template takes words and --limit only (no paging)".into(),
        ));
    }
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let hits = template_hits(&lib, a.query.as_deref(), a.limit)?;
    let value = json!({
        "query": a.query,
        "in": "template",
        "mode": SearchMode::Keyword,
        "results": to_value(&hits)?,
    });
    ctx.emit(value, |out| {
        if hits.is_empty() {
            writeln!(out, "No results.")?;
        }
        for h in &hits {
            writeln!(
                out,
                "{}  {} ({}): {}",
                h.meeting_id, h.template_name, h.template_id, h.snippet
            )?;
        }
        Ok(())
    })
}

/// `meetings`: meetings whose title, tags or participants match, newest
/// first; without words or filters, the most recent ones.
pub(super) fn meetings(ctx: &mut Ctx<'_>, mut a: MeetingsArgs) -> Result<()> {
    if a.query_stdin {
        let input: SearchStdin = read_stdin("meetings")?;
        a.query = input.query;
        a.person = input.person;
        a.tag = input.tag.or(a.tag);
        a.topic = input.topic.or(a.topic);
    }
    let now = ctx.deps.clock.now();
    let page = page_request(a.page.page_size, a.page.cursor.as_deref(), now)?;
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let mut q = search_query(
        &SearchFilters {
            query: a.query.as_deref(),
            person: a.person.as_deref(),
            tag: a.tag.as_deref(),
            topic: a.topic.as_deref(),
            app: a.app.as_deref(),
            source: a.source.map(Into::into),
            since: a.since.as_deref(),
            until: a.until.as_deref(),
            meeting: None,
            limit: page.is_none().then_some(a.limit),
        },
        resolve_at(page.as_ref(), now),
    )?;
    let index = Index::open_readonly(lib.root())?;
    q.meeting_order = a.order.map(Into::into).unwrap_or_default();
    let naming = ops::review_naming_counts(&lib)?;
    let meeting_row = |h: &MeetingHit| -> Result<Value> {
        let mut row = to_value(h)?;
        row["speakers_need_name"] = json!(naming.get(&h.meeting_id).copied().unwrap_or(0));
        Ok(row)
    };
    let (hits, mut value): (Vec<MeetingHit>, Value) = match &page {
        None => {
            let hits = index.search_meetings(&q)?;
            let rows = hits.iter().map(meeting_row).collect::<Result<Vec<_>>>()?;
            (hits, json!({ "meetings": rows }))
        }
        Some(request) => {
            let mut hits = Vec::new();
            let value = page_value("meetings", index.search_meetings_page(&q, request)?, |h| {
                let v = meeting_row(&h)?;
                hits.push(h);
                Ok(v)
            })?;
            (hits, value)
        }
    };
    let transcript = if a.include_transcripts {
        let rows = library_transcript_hits(ctx, &config, &lib, &index, &q, &hits, a.limit)?;
        let rows = rows
            .into_iter()
            .map(|(meeting, hit)| {
                let mut row = meeting_row(&meeting)?;
                row["match"] = to_value(&hit)?;
                Ok(row)
            })
            .collect::<Result<Vec<_>>>()?;
        value["transcript_matches"] = json!(rows);
        rows
    } else {
        Vec::new()
    };
    let next = next_cursor(&value);
    ctx.emit(value, |out| {
        if hits.is_empty() && transcript.is_empty() {
            writeln!(out, "No meetings.")?;
        }
        for h in &hits {
            let tags = if h.tags.is_empty() {
                String::new()
            } else {
                format!("  [{}]", h.tags.join(", "))
            };
            let speakers = naming
                .get(&h.meeting_id)
                .map_or_else(String::new, |n| format!("  {n} speaker(s) need a name"));
            writeln!(
                out,
                "{}  {}  {}{tags}{speakers}",
                h.meeting_id,
                h.started_at.format("%Y-%m-%d %H:%M"),
                h.title.as_deref().unwrap_or("(untitled)"),
            )?;
        }
        if !transcript.is_empty() {
            writeln!(out, "In transcripts:")?;
            for row in transcript {
                writeln!(
                    out,
                    "{}  {}  [{}] {}",
                    row["meeting_id"].as_str().unwrap_or(""),
                    row["title"].as_str().unwrap_or("(untitled)"),
                    format_hms(row["match"]["start"].as_f64().unwrap_or_default()),
                    row["match"]["snippet"].as_str().unwrap_or("")
                )?;
            }
        }
        write_next_page(out, next.as_deref())
    })
}

pub(super) fn filters(ctx: &mut Ctx<'_>) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let names = broscribe_core::detected_apps::names(ctx.data_dir());
    let (tags, apps) = match Index::open_readonly(lib.root()) {
        Ok(index) => (index.meeting_tags()?, index.meeting_apps()?),
        Err(Error::NotFound(_)) if !Index::path(lib.root()).exists() => (Vec::new(), Vec::new()),
        Err(e) => return Err(e),
    };
    let tag_rows: Vec<_> = tags
        .iter()
        .map(|(tag, count)| json!({"tag": tag, "meetings": count}))
        .collect();
    let app_rows: Vec<_> = apps
        .iter()
        .map(|id| json!({"id": id, "name": names.get(id).unwrap_or(id)}))
        .collect();
    ctx.emit(json!({"tags": tag_rows, "apps": app_rows}), |out| {
        writeln!(out, "Tags:")?;
        for (tag, count) in tags {
            writeln!(out, "  {tag} ({count} meetings)")?;
        }
        writeln!(out, "Apps:")?;
        for id in apps {
            writeln!(out, "  {} ({id})", names.get(&id).unwrap_or(&id))?;
        }
        Ok(())
    })
}

fn library_transcript_hits(
    ctx: &mut Ctx<'_>,
    config: &broscribe_core::config::Config,
    lib: &broscribe_core::Library,
    index: &Index,
    query: &SearchQuery,
    metadata: &[MeetingHit],
    limit: usize,
) -> Result<Vec<(MeetingHit, SearchHit)>> {
    if query.text.is_none() || metadata.len() >= limit {
        return Ok(Vec::new());
    }
    let mut q = SearchQuery {
        limit: None,
        ..query.clone()
    };
    let mut filters = SearchQuery {
        text: None,
        limit: None,
        ..query.clone()
    };
    let mut seen: std::collections::BTreeSet<_> =
        metadata.iter().map(|m| m.meeting_id.clone()).collect();
    if let Some(note) = prepare_utterance_query(ctx, config, lib, &mut q, None)? {
        ctx.note(format!("note: {note}"));
    }
    let mut request = page_request(Some(100), None, ctx.deps.clock.now())?.expect("page size set");
    let mut rows = Vec::new();
    loop {
        let page = index.search_page(&q, &request)?;
        for hit in page.items {
            if seen.insert(hit.meeting_id.clone()) {
                filters.meeting = Some(hit.meeting_id.clone());
                if let Some(meeting) = index.search_meetings(&filters)?.into_iter().next() {
                    rows.push((meeting, hit));
                    if rows.len() + metadata.len() == limit {
                        return Ok(rows);
                    }
                }
            }
        }
        let Some(cursor) = page.next_cursor else {
            return Ok(rows);
        };
        request.after = Some(cursor);
    }
}

fn assignment_str(a: Assignment) -> &'static str {
    match a {
        Assignment::Confirmed => "confirmed",
        Assignment::Auto => "auto",
        Assignment::Suggested => "suggested",
        Assignment::None => "none",
    }
}

/// How a speaker group came to be, for human output: `auto 0.93` or
/// `merged by you`.
pub(crate) fn group_origin(origin: &GroupOrigin) -> String {
    match origin {
        GroupOrigin::Auto { score } => format!("auto {score:.2}"),
        GroupOrigin::User => "merged by you".to_owned(),
    }
}

pub(super) fn show(ctx: &mut Ctx<'_>, meeting: &str) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let id = meeting_id(meeting)?;
    let m = lib.load_meeting(&id)?;
    let dir = lib.meeting_dir(&id)?;
    let people = lib.people_lookup()?;
    let tags = m.effective_tags();
    let speakers: Vec<Value> = m
        .speakers
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": speaker_label(&m, Some(s.id), &people),
                "person_id": s.person_id,
                "assignment": s.assignment,
                "revision": ops::speaker_revision(&m, s),
                "score": s.score,
                "suggestion": s.suggestion.as_ref().map(|g| json!({
                    "person_id": g.person_id,
                    "name": people.display_name(&g.person_id),
                    "score": g.score,
                })),
                "talk_s": s.talk_s,
                "merged_into": ops::is_grouped_member(&m, s.id)
                    .then(|| ops::effective_speaker(&m, s.id)),
                "review_dismissed": ops::review_dismissed(&m, s.id),
                "live_promotion": s.live_promotion,
                "live_suggestion_lines": s.live_suggestion_lines,
                "identity_conflict": s.identity_conflict,
            })
        })
        .collect();
    let value = json!({
        "id": m.id,
        "title": m.title,
        "source": m.source,
        "source_id": m.source_id,
        "app": m.app,
        "started_at": m.started_at,
        "duration_s": m.duration_s,
        "languages": m.languages,
        "language_effective": m.language_effective,
        "participants": m.participants,
        "tags": tags,
        "user_tags": m.tags,
        "tags_removed": m.tags_removed,
        "bookmarks": m.bookmarks,
        "notes": m.notes,
        "recording": m.recording,
        "speakers": speakers,
        "speaker_groups": m.speaker_groups,
        "not_same": m.not_same,
        "segments": m.segments.len(),
        "models": m.models,
        "processing": m.processing,
        "review_notes": m.review_notes,
        "live_attributions": m.live_attributions,
        "enrichment": enrichment_view(&m),
        "path": dir,
    });
    ctx.emit(value, |out| {
        writeln!(
            out,
            "{}  {}",
            m.id,
            m.title.as_deref().unwrap_or("(untitled)")
        )?;
        writeln!(out, "started:  {}", m.started_at.to_rfc3339())?;
        writeln!(out, "duration: {}", format_hms(m.duration_s))?;
        if !m.languages.is_empty() {
            let langs: Vec<&str> = m.languages.iter().map(|l| l.lang.as_str()).collect();
            writeln!(out, "language: {}", langs.join(", "))?;
        }
        if let Some(note) = &m.language_effective {
            writeln!(out, "          {note}")?;
        }
        if !tags.is_empty() {
            writeln!(out, "tags:     {}", tags.join(", "))?;
        }
        if !m.tags.is_empty() {
            writeln!(out, "added tags: {}", m.tags.join(", "))?;
        }
        if !m.tags_removed.is_empty() {
            writeln!(out, "removed tags: {}", m.tags_removed.join(", "))?;
        }
        if let Some(e) = &m.enrichment
            && !e.tags.is_empty()
        {
            writeln!(out, "summary tags: {}", e.tags.join(", "))?;
        }
        if let Some(app) = &m.app {
            writeln!(out, "app:      {app}")?;
        }
        if let Some(r) = &m.recording {
            writeln!(out, "audio:    {} ({:?})", r.path.display(), r.state)?;
        }
        writeln!(out, "folder:   {}", dir.display())?;
        writeln!(out, "segments: {}", m.segments.len())?;
        if let Some(e) = &m.enrichment {
            let model = match (&e.model, e.model_reported) {
                (Some(model), true) => model.clone(),
                (Some(model), false) => format!("{model} (requested)"),
                (None, _) => "provider default model".to_owned(),
            };
            let stale = if ops::enrichment_is_stale(&m) {
                "  (stale)"
            } else {
                ""
            };
            writeln!(
                out,
                "summary:  {} · {model} · {}{stale}",
                e.provider,
                e.created_at.to_rfc3339()
            )?;
            if let Some(instructions) = &e.instructions {
                writeln!(out, "summary instructions: {instructions}")?;
            }
        }
        writeln!(out, "speakers:")?;
        for s in &m.speakers {
            let mut line = format!(
                "  {}  {}  [{}]  talk {}",
                s.id,
                speaker_label(&m, Some(s.id), &people),
                assignment_str(s.assignment),
                format_hms(s.talk_s)
            );
            if let Some(g) = &s.suggestion {
                line.push_str(&format!(
                    "  suggestion: {} ({:.2})",
                    people
                        .display_name(&g.person_id)
                        .unwrap_or(g.person_id.as_str()),
                    g.score
                ));
            }
            if let Some(n) = s
                .live_suggestion_lines
                .or_else(|| s.live_promotion.as_ref().map(|p| p.line_count))
            {
                line.push_str(&format!(
                    "  named while recording on {n} {}",
                    if n == 1 { "line" } else { "lines" }
                ));
            }
            if ops::is_grouped_member(&m, s.id) {
                line.push_str(&format!(
                    "  merged into {}",
                    ops::effective_speaker(&m, s.id)
                ));
            }
            if ops::review_dismissed(&m, s.id) {
                line.push_str("  dismissed from review (can't tell)");
            }
            writeln!(out, "{line}")?;
        }
        for g in &m.speaker_groups {
            let members: Vec<String> = g.members.iter().map(ToString::to_string).collect();
            writeln!(
                out,
                "  {}: {} (+ {}, {})",
                g.id,
                g.kept,
                members.join(", "),
                group_origin(&g.origin)
            )?;
        }
        if !m.review_notes.is_empty() {
            writeln!(out, "needs review:")?;
            for n in &m.review_notes {
                writeln!(out, "  {}", n.text)?;
            }
        }
        Ok(())
    })
}

/// One transcript segment as `transcript --json` shows it: the core
/// projection's values raw (names as stored), with the provenance
/// ([`provenance`]) MCP `get_transcript` reports.
fn segment_value(s: &TranscriptSegment<'_>) -> Value {
    let mut v = json!({
        "id": s.segment_id,
        "timestamp": format_hms(s.start),
        "start": s.start,
        "end": s.end,
        "speaker": s.speaker,
        "speaker_name": s.speaker_label,
        "text": s.text,
    });
    if let Some(original) = s.original_speaker {
        v["original_speaker"] = json!(original);
    }
    provenance(s, &mut v);
    v
}

/// `transcript <meeting> [--speaker S|person|label] [--from] [--to]`: the
/// core transcript projection (order, attribution, labels and filters are
/// `broscribe_core::transcript`'s). JSON lists segments (the IDs `transcript attribute`
/// takes); text shows merged lines with names written plain.
pub(super) fn transcript(ctx: &mut Ctx<'_>, a: TranscriptArgs) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let id = meeting_id(a.meeting.as_deref().unwrap_or_default())?;
    let m = lib.load_meeting(&id)?;
    let people = lib.people_lookup()?;
    let filter = TranscriptFilter {
        who: a
            .speaker
            .as_deref()
            .map(|s| transcript::resolve_who(&lib, &m, &people, s))
            .transpose()?,
        from_s: a.from.as_deref().map(parse_clock).transpose()?,
        to_s: a.to.as_deref().map(parse_clock).transpose()?,
    };
    let segments = transcript::transcript(&m, &people, &filter);
    if a.find_stdin {
        let query = ctx.input_text("transcript --find-stdin")?;
        let query = broscribe_core::names::fold_search_text(query.trim());
        let lines = transcript::merge_lines(&segments);
        let matches: Vec<_> = lines
            .iter()
            .filter(|line| transcript::line_matches(line, &query))
            .collect();
        return ctx.emit(
            json!({
                "meeting_id": m.id,
                "matches": matches.iter().map(|line| json!({
                    "segment_ids": line.segment_ids,
                    "start": line.start,
                    "end": line.end,
                    "speaker": line.speaker,
                    "speaker_name": line.label,
                    "overlapping_speech": line.overlapping_speech,
                    "text": line.text,
                })).collect::<Vec<_>>(),
            }),
            |out| {
                for line in matches {
                    writeln!(out, "{}", line.display(NameStyle::Plain))?;
                }
                Ok(())
            },
        );
    }
    let value = json!({
        "meeting_id": m.id,
        "title": m.title,
        "started_at": m.started_at,
        "segments": segments.iter().map(segment_value).collect::<Vec<_>>(),
        "bookmarks": m.bookmarks,
        "notes": m.notes,
    });
    ctx.emit(value, |out| {
        for line in transcript::merge_lines(&segments) {
            writeln!(out, "{}", line.display(NameStyle::Plain))?;
        }
        Ok(())
    })
}

pub(super) fn people(ctx: &mut Ctx<'_>, query: Option<&str>) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let fold = broscribe_core::names::fold_search_text;
    let query = fold(query.unwrap_or("").trim());
    let people = Index::open_readonly(lib.root())?
        .list_people()?
        .into_iter()
        .filter(|p| {
            std::iter::once(p.name.as_str())
                .chain(std::iter::once(p.id.as_str()))
                .chain(p.aliases.iter().chain(&p.emails).map(String::as_str))
                .any(|value| fold(value).contains(&query))
        })
        .collect::<Vec<_>>();
    let (rows, counts): (Vec<Value>, Vec<usize>) = crate::query::people_rows(&lib, &people)?
        .into_iter()
        .unzip();
    ctx.emit(json!({ "people": rows }), |out| {
        if people.is_empty() {
            writeln!(out, "No people yet.")?;
        }
        for (p, count) in people.iter().zip(&counts) {
            let me = if p.is_self { " (me)" } else { "" };
            let emails = if p.emails.is_empty() {
                String::new()
            } else {
                format!("  <{}>", p.emails.join(", "))
            };
            writeln!(
                out,
                "{}  {}{me}{emails}  {} meetings, talk {}, {count} voice samples",
                p.id,
                p.name,
                p.meetings,
                format_hms(p.talk_s)
            )?;
        }
        Ok(())
    })
}

/// Default and largest `--limit` of `speakers matches` and MCP
/// `speaker_voice_matches`.
const MATCHES_DEFAULT: usize = 3;
const MATCHES_MAX: usize = 10;

/// `speakers matches` / MCP `speaker_voice_matches`: the people closest to
/// one speaker's voice (design §6.4), as the app's "Name…" picker lists them.
#[derive(serde::Serialize)]
pub(crate) struct VoiceMatches {
    meeting_id: broscribe_core::MeetingId,
    speaker_id: SpeakerId,
    model: String,
    matches: Vec<VoiceMatchRow>,
}

#[derive(serde::Serialize)]
struct VoiceMatchRow {
    person_id: broscribe_core::PersonId,
    name: Option<String>,
    /// Raw similarity, not a probability.
    score: f32,
    likely: bool,
    current_suggestion: bool,
}

pub(crate) fn voice_matches(
    lib: &broscribe_core::Library,
    config: &broscribe_core::config::Config,
    meeting: &str,
    speaker: &str,
    limit: Option<usize>,
) -> Result<VoiceMatches> {
    let limit = limit.unwrap_or(MATCHES_DEFAULT);
    if !(1..=MATCHES_MAX).contains(&limit) {
        return Err(Error::InvalidInput(format!(
            "limit must be 1 to {MATCHES_MAX}, got {limit}"
        )));
    }
    let mid = meeting_id(meeting)?;
    let sid = speaker_id(speaker)?;
    let m = lib.load_meeting(&mid)?;
    let model = config.profile.speaker_embedding.clone();
    let people = ops::load_person_samples(lib, &model)?;
    let roster = m.roster_person_ids();
    // A grouped member ranks as its kept speaker, without any member's
    // rejected people: the ranking equals resolution. Unknown speaker:
    // `not_found`.
    let matches = ops::voice_matches(
        &people,
        &m,
        sid,
        Some(&roster),
        config.thresholds(&model),
        &model,
        limit,
    )?;
    let names = lib.people_lookup()?;
    let matches = matches
        .into_iter()
        .map(|v| VoiceMatchRow {
            name: names.display_name(&v.person_id).map(str::to_owned),
            person_id: v.person_id,
            score: v.score,
            likely: v.likely,
            current_suggestion: v.current_suggestion,
        })
        .collect();
    Ok(VoiceMatches {
        meeting_id: mid,
        speaker_id: sid,
        model,
        matches,
    })
}

pub(super) fn speaker_matches(
    ctx: &mut Ctx<'_>,
    lib: &broscribe_core::Library,
    config: &broscribe_core::config::Config,
    meeting: &str,
    speaker: &str,
    limit: Option<usize>,
) -> Result<()> {
    let out = voice_matches(lib, config, meeting, speaker, limit)?;
    ctx.emit(to_value(&out)?, |w| {
        if out.matches.is_empty() {
            writeln!(
                w,
                "No voice matches for {} {}.",
                out.meeting_id, out.speaker_id
            )?;
        }
        for v in &out.matches {
            let likely = if v.likely { "  likely" } else { "" };
            let current = if v.current_suggestion {
                "  (current suggestion)"
            } else {
                ""
            };
            writeln!(
                w,
                "{}  {}  voice {:.2}{likely}{current}",
                v.person_id,
                v.name.as_deref().unwrap_or("(unnamed)"),
                v.score
            )?;
        }
        Ok(())
    })
}

/// `speakers duplicates` / MCP `speaker_duplicates`: pairs of speakers shown
/// in one meeting that may be one over-split voice (design §6.4), as the
/// app's "Possible duplicates" lists them. Empty without a calibrated
/// `merge` threshold.
#[derive(serde::Serialize)]
pub(crate) struct Duplicates {
    meeting_id: broscribe_core::MeetingId,
    model: String,
    /// The calibrated thresholds bounding the suggestions: similarity at
    /// least `merge` and below `auto_merge` (merged automatically).
    merge: Option<f32>,
    auto_merge: Option<f32>,
    duplicates: Vec<DuplicateRow>,
}

#[derive(serde::Serialize)]
struct DuplicateRow {
    /// The speaker a merge keeps.
    a: SpeakerId,
    a_name: String,
    /// The speaker that sounds like `a`.
    b: SpeakerId,
    b_name: String,
    /// Similarity of the two voices (a group's: all its speakers).
    score: f32,
}

pub(crate) fn speaker_duplicates(
    lib: &broscribe_core::Library,
    config: &broscribe_core::config::Config,
    meeting: &str,
) -> Result<Duplicates> {
    let mid = meeting_id(meeting)?;
    let m = lib.load_meeting(&mid)?;
    let model = config.profile.speaker_embedding.clone();
    let thresholds = config.thresholds(&model);
    let proven = ops::caption_proven(lib, &m)?;
    let people = lib.people_lookup()?;
    let duplicates = ops::duplicate_suggestions(&m, &model, thresholds, &proven)?
        .into_iter()
        .map(|d| DuplicateRow {
            a_name: speaker_label(&m, Some(d.a), &people),
            a: d.a,
            b_name: speaker_label(&m, Some(d.b), &people),
            b: d.b,
            score: d.score,
        })
        .collect();
    Ok(Duplicates {
        meeting_id: mid,
        merge: thresholds.and_then(|t| t.merge),
        auto_merge: thresholds.and_then(|t| t.auto_merge),
        model,
        duplicates,
    })
}

pub(super) fn duplicates(
    ctx: &mut Ctx<'_>,
    lib: &broscribe_core::Library,
    config: &broscribe_core::config::Config,
    meeting: &str,
) -> Result<()> {
    let out = speaker_duplicates(lib, config, meeting)?;
    ctx.emit(to_value(&out)?, |w| {
        if out.merge.is_none() {
            writeln!(
                w,
                "No duplicates suggested: {} has no calibrated merge threshold.",
                out.model
            )?;
        } else if out.duplicates.is_empty() {
            writeln!(w, "No possible duplicates in {}.", out.meeting_id)?;
        }
        for d in &out.duplicates {
            writeln!(
                w,
                "{} ({}) sounds like {} ({})  {:.2}",
                d.b, d.b_name, d.a, d.a_name, d.score
            )?;
        }
        Ok(())
    })
}

/// `people show <person>`: [`ops::person_view`], the projection MCP
/// `get_person` returns too. Its meetings come from the index's person
/// relations, never a scan of every meeting; an index without them is
/// refused (`schema`: rebuild it).
pub(super) fn person(ctx: &mut Ctx<'_>, reference: &str) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let p = ops::find_person(&lib, reference)?
        .ok_or_else(|| Error::NotFound(format!("person {}", reference.trim())))?;
    let view = ops::person_view(&lib, &Index::open_readonly(lib.root())?, &p.id)?;
    ctx.emit(to_value(&view)?, |out| {
        let p = &view.person;
        let me = if p.is_self { " (me)" } else { "" };
        writeln!(out, "{}  {}{me}", p.id, NameStyle::Plain.apply(&p.name))?;
        if !p.aliases.is_empty() {
            let aliases: Vec<String> = p
                .aliases
                .iter()
                .map(|a| NameStyle::Plain.apply(a))
                .collect();
            writeln!(out, "aliases: {}", aliases.join(", "))?;
        }
        if !p.emails.is_empty() {
            writeln!(out, "e-mails: {}", p.emails.join(", "))?;
        }
        writeln!(out, "voiceprints:")?;
        if view.voiceprints.is_empty() {
            writeln!(out, "  none")?;
        }
        for v in &view.voiceprints {
            let strength = match v.strength {
                Strength::Confirmed => "confirmed",
                Strength::Weak => "weak",
            };
            writeln!(out, "  {}: {} {} ({strength})", v.model, v.count, v.kind)?;
        }
        writeln!(out, "meetings:")?;
        if view.meetings.is_empty() {
            writeln!(out, "  none")?;
        }
        for m in &view.meetings {
            let mut line = format!(
                "  {}  {}  {}",
                m.meeting_id,
                m.title.as_deref().unwrap_or("(untitled)"),
                format_hms(m.talk_s)
            );
            if m.lines > 0 {
                line.push_str(&format!("  {} corrected line(s)", m.lines));
            }
            if !m.live_review.is_empty() {
                line.push_str(&format!(
                    "  {} live decision(s) to review",
                    m.live_review.len()
                ));
            }
            writeln!(out, "{line}")?;
        }
        if !view.notes.trim().is_empty() {
            writeln!(out, "\n{}", view.notes.trim_end())?;
        }
        Ok(())
    })
}

/// The accounts a directory command uses: the named one (its errors are the
/// command's), or every signed-in account (each one's errors are reported
/// beside the others' results, never aborting them). System calendars have no
/// people directory or photos: they are never asked, and naming them fails.
fn directory_accounts(
    service: &broscribe_integrations::Integrations,
    integration: Option<crate::cli::IntegrationArg>,
) -> Result<Vec<IntegrationId>> {
    match integration.map(IntegrationId::from) {
        Some(id) if id.is_native_calendar() => Err(Error::Unsupported(
            "native calendars have no people directory; name a directory account instead".into(),
        )),
        Some(id) => Ok(vec![id]),
        None => Ok(active_accounts(
            &service.statuses().map_err(super::integrations::core)?,
        )),
    }
}

fn active_accounts(statuses: &[broscribe_integrations::IntegrationStatus]) -> Vec<IntegrationId> {
    statuses
        .iter()
        .filter(|s| !s.integration.is_native_calendar())
        .filter(|s| {
            s.state.status == broscribe_integrations::AccountStatus::Active
                || !s.microsoft_accounts.is_empty()
        })
        .map(|s| s.integration)
        .collect()
}

/// A provider or Microsoft account a directory or photo read could not use, as
/// its `errors[]` entry: `{integration, code, message}`, and for a Microsoft
/// account also its `account`, `generation` and `account_label`. The failure is
/// the sanitized [`PublicError`], never raw provider text or a local path.
#[derive(Debug, serde::Serialize)]
struct AccountFailure {
    integration: IntegrationId,
    #[serde(flatten)]
    account: Option<FailedAccount>,
    #[serde(flatten)]
    error: PublicError,
}

/// The Microsoft account an [`AccountFailure`] belongs to, as the account list
/// read it.
#[derive(Debug, serde::Serialize)]
struct FailedAccount {
    account: AccountRef,
    generation: u64,
    account_label: Option<String>,
}

impl AccountFailure {
    fn provider(integration: IntegrationId, error: &IntegrationError) -> Self {
        Self {
            integration,
            account: None,
            error: error.public(),
        }
    }

    fn account(
        integration: IntegrationId,
        account: MicrosoftAccount,
        error: &IntegrationError,
    ) -> Self {
        Self {
            integration,
            account: Some(FailedAccount {
                account: account.account,
                generation: account.generation,
                account_label: account.identity.display_name,
            }),
            error: error.public(),
        }
    }
}

/// Human output of the failures: `integration[/account]: message` each.
fn write_account_errors(
    out: &mut dyn std::io::Write,
    errors: &[AccountFailure],
) -> std::io::Result<()> {
    for failure in errors {
        write!(out, "{}", failure.integration)?;
        if let Some(failed) = &failure.account {
            write!(out, "/{}", failed.account.account_id)?;
        }
        writeln!(out, ": {}", failure.error.message)?;
    }
    Ok(())
}

/// Each account's result, and the failure of each account that failed.
#[derive(Debug)]
struct PerAccount<T> {
    done: Vec<(IntegrationId, T)>,
    errors: Vec<AccountFailure>,
}

/// Runs `each` per account; with an explicit account its error fails the
/// command, otherwise it becomes an `errors[]` entry and the rest continue.
fn per_account<T>(
    ids: &[IntegrationId],
    explicit: bool,
    mut each: impl FnMut(IntegrationId) -> broscribe_integrations::Result<T>,
) -> Result<PerAccount<T>> {
    let mut done = Vec::new();
    let mut errors = Vec::new();
    for &id in ids {
        match each(id) {
            Ok(value) => done.push((id, value)),
            Err(error) if explicit => return Err(super::integrations::core(error)),
            Err(error) => errors.push(AccountFailure::provider(id, &error)),
        }
    }
    Ok(PerAccount { done, errors })
}

/// One directory match under its provider; a Microsoft account's match
/// carries its account and generation in `person.source`.
#[derive(Debug, serde::Serialize)]
struct DirectoryHit {
    integration: IntegrationId,
    person: DirectoryPerson,
}

/// `people lookup` output, `{results, errors}`: every account's matches
/// beside the failures of the accounts that could not be searched.
#[derive(Debug, serde::Serialize)]
struct DirectoryLookup {
    results: Vec<DirectoryHit>,
    errors: Vec<AccountFailure>,
}

impl DirectoryLookup {
    fn found(&mut self, integration: IntegrationId, people: Vec<DirectoryPerson>) {
        self.results
            .extend(people.into_iter().map(|person| DirectoryHit {
                integration,
                person,
            }));
    }

    /// `integration[/account]  name  email` per match, then the failures.
    fn write_human(&self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        for hit in &self.results {
            write!(out, "{}", hit.integration)?;
            if let Some(source) = &hit.person.source {
                write!(out, "/{}", source.account.account_id)?;
            }
            writeln!(
                out,
                "  {}  {}",
                hit.person.name.as_deref().unwrap_or(""),
                hit.person.email.as_deref().unwrap_or("")
            )?;
        }
        write_account_errors(out, &self.errors)
    }
}

/// Account-isolated directory result used by the CLI JSON boundary.
fn directory_lookup(
    service: &broscribe_integrations::Integrations,
    query: &str,
    integration: Option<crate::cli::IntegrationArg>,
    account: Option<&str>,
) -> Result<DirectoryLookup> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::InvalidInput("directory query is empty".into()));
    }
    let account = account
        .map(super::integrations::qualified_account)
        .transpose()?;
    let mut lookup = DirectoryLookup {
        results: Vec::new(),
        errors: Vec::new(),
    };
    if let Some(account) = account {
        let id = IntegrationId::parse(account.integration.as_str())
            .map_err(super::integrations::core)?;
        if integration.is_some_and(|selected| IntegrationId::from(selected) != id) {
            return Err(Error::InvalidInput(
                "--integration and --account disagree".into(),
            ));
        }
        let result = service
            .account_directory(&account, query)
            .map_err(super::integrations::core)?;
        lookup.found(id, result.people);
        return Ok(lookup);
    }
    for id in directory_accounts(service, integration)? {
        if id == IntegrationId::MicrosoftGraph {
            match service.list_accounts(Some(id)) {
                Ok(accounts) => {
                    for account in accounts {
                        match service.account_directory(&account.account, query) {
                            Ok(result) => lookup.found(id, result.people),
                            Err(error) => lookup
                                .errors
                                .push(AccountFailure::account(id, account, &error)),
                        }
                    }
                }
                Err(error) => lookup.errors.push(AccountFailure::provider(id, &error)),
            }
        } else {
            match service.directory(id, query) {
                Ok(people) => lookup.found(id, people),
                Err(error) => lookup.errors.push(AccountFailure::provider(id, &error)),
            }
        }
    }
    Ok(lookup)
}

/// Explicit lookup never creates a local identity.
pub(super) fn lookup_people(
    ctx: &mut Ctx<'_>,
    query: &str,
    integration: Option<crate::cli::IntegrationArg>,
    account: Option<&str>,
) -> Result<()> {
    let service = super::integrations::service(ctx)?;
    let lookup = directory_lookup(&service, query, integration, account)?;
    ctx.emit(to_value(&lookup)?, |out| lookup.write_human(out))
}

/// A photo cached from `integration`'s directory.
#[derive(Debug, serde::Serialize)]
struct CachedPhoto {
    integration: IntegrationId,
    path: PathBuf,
}

/// `people photo` output, `{person_id, photos, errors}`: the photos the
/// accounts cached beside the failures of the accounts that could not.
#[derive(Debug, serde::Serialize)]
struct PersonPhotos {
    person_id: PersonId,
    photos: Vec<CachedPhoto>,
    errors: Vec<AccountFailure>,
}

impl PersonPhotos {
    fn new(person_id: PersonId, cached: PerAccount<Option<PathBuf>>) -> Self {
        Self {
            person_id,
            photos: cached
                .done
                .into_iter()
                .filter_map(|(integration, path)| {
                    path.map(|path| CachedPhoto { integration, path })
                })
                .collect(),
            errors: cached.errors,
        }
    }

    /// Each cached photo's path, then the failures.
    fn write_human(&self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        for photo in &self.photos {
            writeln!(out, "{}", photo.path.display())?;
        }
        write_account_errors(out, &self.errors)
    }
}

/// Cache only exact local e-mail matches, never fuzzy display-name matches.
/// Only signed-in accounts are asked; one account's failure leaves the others.
pub(super) fn person_photo(
    ctx: &mut Ctx<'_>,
    reference: &str,
    account: Option<&str>,
) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let person = ops::find_person(&lib, reference)?
        .ok_or_else(|| Error::NotFound(format!("person {}", reference.trim())))?;
    let service = super::integrations::service(ctx)?;
    let account = account
        .map(super::integrations::qualified_account)
        .transpose()?;
    let ids = if let Some(account) = &account {
        vec![
            IntegrationId::parse(account.integration.as_str())
                .map_err(super::integrations::core)?,
        ]
    } else {
        // A photo mutates local identity state: never choose a corporate account implicitly.
        vec![IntegrationId::Google]
    };
    let cached = per_account(&ids, account.is_some(), |id| {
        for email in &person.emails {
            let people = match &account {
                Some(account) => service.account_directory(account, email)?.people,
                None => service.directory(id, email)?,
            };
            for remote in people {
                if !remote
                    .email
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case(email))
                {
                    continue;
                }
                let photo = if let Some(source) = &remote.source {
                    service.cache_account_photo(
                        &source.account,
                        source.generation,
                        person.id.as_str(),
                        &remote,
                        &|_| lib.load_person(&person.id).is_ok(),
                    )?
                } else {
                    service.cache_photo(id, person.id.as_str(), &remote, &|_| {
                        lib.load_person(&person.id).is_ok()
                    })?
                };
                if let Some(path) = photo {
                    return Ok(Some(path));
                }
            }
        }
        Ok(None)
    })?;
    let photos = PersonPhotos::new(person.id, cached);
    ctx.emit(to_value(&photos)?, |out| photos.write_human(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_integrations::{AccountState, AccountStatus, IntegrationStatus};

    #[test]
    fn directory_json_preserves_success_and_qualifies_another_accounts_failure() {
        use broscribe_integrations::{Endpoints, Integrations, IntegrationsConfig, account_dir};
        use broscribe_platform::{
            SecretStore as _,
            fakes::{FakeBrowserLauncher, FakeSecretStore},
        };
        use std::{
            io::{Read as _, Write as _},
            net::TcpListener,
            sync::Arc,
            time::{Duration, Instant},
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let start = Instant::now();
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && start.elapsed() < Duration::from_secs(5) =>
                        {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("synthetic directory server: {error}"),
                    }
                };
                // A stream accepted from a non-blocking listener inherits that mode
                // on some platforms (macOS): reads block, bounded by the timeout.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                }
                let header = String::from_utf8(header).unwrap();
                assert!(
                    header.starts_with("GET /directory/users?"),
                    "Only directory reads, never authentication: {header}"
                );
                let body = r#"{"value":[{"id":"synthetic-person","displayName":"Synthetic Person","mail":"synthetic@example.invalid"}]}"#;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let secrets = Arc::new(FakeSecretStore::default());
        let browser = Arc::new(FakeBrowserLauncher::default());
        let blocked = "11111111111111111111111111111111";
        let successful = "22222222222222222222222222222222";
        let dir = account_dir(temp.path(), IntegrationId::MicrosoftGraph);
        std::fs::create_dir_all(&dir).unwrap();
        let accounts = [blocked, successful].iter().map(|handle| {
            let key = format!("integration/microsoft-graph/accounts/{handle}/tokens/33333333333333333333333333333333");
            secrets.set(&key, r#"{"access_token":"synthetic-access","refresh_token":"synthetic-refresh","expires_at":4102444800,"scope":"User.Read Calendars.Read User.ReadBasic.All"}"#).unwrap();
            json!({"view":{
                "account":{"integration":"microsoft-graph","account_id":handle},
                "identity":{"authority_host":"127.0.0.1","resource_tenant_id":"synthetic-tenant","object_id":handle,
                    "client_id":"synthetic-client","verified_addresses":[]},
                "generation":5,"selection_generation":1,"status":"active",
                "access_error":if *handle == blocked {Some("conditional_access_blocked")} else {None},
                "granted_scopes":["User.Read","Calendars.Read","User.ReadBasic.All"],"selected_calendars":[],
                "calendar":"available","directory":"available","shared_calendars":"not_requested",
                "microsoft365_groups":"not_requested","target_results":[]
            },"secret_key":key,"authorized_scopes":["User.Read","Calendars.Read","User.ReadBasic.All"]})
        }).collect::<Vec<_>>();
        broscribe_core::fsutil::write_json_atomic(
            &dir.join("accounts.json"),
            &json!({
                "version":1,"accounts":accounts,"pending_secrets":[],"attempts":[],
                "legacy_quarantined":false,"legacy_checked":true
            }),
        )
        .unwrap();
        let config_text = "[integration.microsoft-graph]\nclient_id='synthetic-client'\n";
        std::fs::write(temp.path().join("config.toml"), config_text).unwrap();
        let config = IntegrationsConfig::parse(config_text).unwrap();
        let service = Integrations::new(
            temp.path(),
            config,
            secrets,
            browser.clone(),
            Arc::new(|_| {}),
        )
        .unwrap()
        .with_endpoints(
            IntegrationId::MicrosoftGraph,
            Endpoints {
                authorize: format!("{base}/authorize"),
                token: format!("{base}/token"),
                api: format!("{base}/api"),
                directory_api: format!("{base}/directory"),
                revoke: None,
            },
        );
        let blocked_error = IntegrationError::ConditionalAccessBlocked.public();
        for provider in [None, Some(crate::cli::IntegrationArg::MicrosoftGraph)] {
            let lookup = directory_lookup(&service, "Synthetic", provider, None).unwrap();
            // `people lookup --json` emits exactly this value.
            let output = to_value(&lookup).unwrap();
            assert_eq!(output.as_object().unwrap().len(), 2, "{output}");
            assert_eq!(output["results"].as_array().unwrap().len(), 1, "{output}");
            let hit = &output["results"][0];
            assert_eq!(hit.as_object().unwrap().len(), 2, "{output}");
            assert_eq!(hit["integration"], "microsoft-graph");
            assert_eq!(hit["person"]["remote_id"], "synthetic-person");
            assert_eq!(hit["person"]["name"], "Synthetic Person");
            assert_eq!(hit["person"]["email"], "synthetic@example.invalid");
            assert_eq!(
                hit["person"]["source"],
                json!({
                    "account": {"integration": "microsoft-graph", "account_id": successful},
                    "generation": 5,
                })
            );
            // The healthy account's result survives; the blocked account is
            // qualified by account, generation and label beside its finite error.
            assert_eq!(
                output["errors"],
                json!([{
                    "integration": "microsoft-graph",
                    "account": {"integration": "microsoft-graph", "account_id": blocked},
                    "generation": 5,
                    "account_label": null,
                    "code": "conditional_access_blocked",
                    "message": blocked_error.message,
                }])
            );
            let mut human = Vec::new();
            lookup.write_human(&mut human).unwrap();
            assert_eq!(
                String::from_utf8(human).unwrap(),
                format!(
                    "microsoft-graph/{successful}  Synthetic Person  synthetic@example.invalid\n\
                     microsoft-graph/{blocked}: {}\n",
                    blocked_error.message
                )
            );
        }
        let error = directory_lookup(
            &service,
            "Synthetic",
            None,
            Some(&format!("microsoft-graph/{blocked}")),
        )
        .unwrap_err();
        assert_eq!(error.code(), "conditional_access_blocked");
        assert!(browser.opened.lock().unwrap().is_empty());
        server.join().unwrap();
    }

    fn status(integration: IntegrationId, status: AccountStatus) -> IntegrationStatus {
        IntegrationStatus {
            integration,
            configured: true,
            state: AccountState {
                status,
                ..Default::default()
            },
            admin_controlled_scopes: Vec::new(),
            system_calendar: None,
            microsoft_accounts: Vec::new(),
            legacy_microsoft: None,
            client_secret_stored: None,
        }
    }

    #[test]
    fn implicit_directory_uses_only_signed_in_accounts_and_keeps_going() {
        let ids = active_accounts(&[
            status(IntegrationId::SystemCalendar, AccountStatus::Active),
            status(IntegrationId::MicrosoftGraph, AccountStatus::ReauthRequired),
            status(IntegrationId::Google, AccountStatus::Active),
        ]);
        // An active native-calendar account is never a directory or photo source.
        assert_eq!(ids, [IntegrationId::Google]);
        let both = [IntegrationId::MicrosoftGraph, IntegrationId::Google];
        let each = |id| {
            if id == IntegrationId::MicrosoftGraph {
                Err(IntegrationError::Transient("synthetic outage".into()))
            } else {
                Ok(1)
            }
        };
        let outcome = per_account(&both, false, each).unwrap();
        assert_eq!(outcome.done, [(IntegrationId::Google, 1)]);
        assert_eq!(
            to_value(&outcome.errors).unwrap(),
            json!([{
                "integration": "microsoft-graph",
                "code": "transient",
                "message": IntegrationError::Transient("synthetic outage".into()).public().message,
            }])
        );
        // A named account's failure is the command's failure.
        assert!(per_account(&both, true, each).is_err());
    }

    #[test]
    fn photo_output_keeps_cached_photos_beside_sanitized_account_failures() {
        let private = std::path::Path::new("/synthetic/private/state.json");
        let failure =
            || IntegrationError::Core(Error::io_at(private, std::io::Error::other("synthetic")));
        let path = PathBuf::from("/synthetic/photos/p-0a1b2c.jpg");
        let cached = per_account(
            &[IntegrationId::MicrosoftGraph, IntegrationId::Google],
            false,
            |id| match id {
                IntegrationId::MicrosoftGraph => Err(failure()),
                _ => Ok(Some(path.clone())),
            },
        )
        .unwrap();
        let photos = PersonPhotos::new(PersonId::parse("p-0a1b2c").unwrap(), cached);
        let sanitized = failure().public();
        // `people photo --json` emits exactly this value.
        let output = to_value(&photos).unwrap();
        assert_eq!(
            output,
            json!({
                "person_id": "p-0a1b2c",
                "photos": [{"integration": "google", "path": path}],
                "errors": [{
                    "integration": "microsoft-graph",
                    "code": sanitized.code,
                    "message": sanitized.message,
                }],
            })
        );
        assert!(
            !output.to_string().contains("/synthetic/private"),
            "{output}"
        );
        let mut human = Vec::new();
        photos.write_human(&mut human).unwrap();
        let human = String::from_utf8(human).unwrap();
        assert_eq!(
            human,
            format!(
                "{}\nmicrosoft-graph: {}\n",
                path.display(),
                sanitized.message
            )
        );
        assert!(!human.contains("/synthetic/private"), "{human}");
        // An account that found no photo adds no entry and no failure.
        let none = per_account(&[IntegrationId::Google], false, |_| Ok(None)).unwrap();
        let photos = PersonPhotos::new(PersonId::parse("p-0a1b2c").unwrap(), none);
        assert_eq!(
            to_value(&photos).unwrap(),
            json!({"person_id": "p-0a1b2c", "photos": [], "errors": []})
        );
    }
}
