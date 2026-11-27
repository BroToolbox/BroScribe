//! Meeting export (design §4, §11): the transcript and everything generated
//! for a meeting (summary, decisions, action items, template results) as
//! Markdown, plain text or JSON, with current names substituted.
//!
//! - Names follow the meeting view: effective (grouped) speakers, line
//!   corrections, and `{{S#}}` / `{{P#}}` placeholders resolved through the
//!   `line_people` of the content they belong to. Markdown escapes them and
//!   plain text keeps them on one line ([`NameStyle`]); JSON carries them as
//!   stored. Transcript text and generated prose stay verbatim.
//! - Stale content is marked "(predates transcript changes)".
//! - Requested sections the meeting does not have are left out and listed
//!   in [`ExportDoc::omitted`].
//! - [`write_export`] writes atomically with mode 0600: an export holds a
//!   transcript.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Value, json};

use crate::render::{NameStyle, PeopleLookup, enrichment_who, format_hms, substitute_speakers_as};
use crate::schema::{Action, ActionStatus, Meeting, TemplateResult};
use crate::{Error, Result, fsutil};

/// The marker of stale content.
pub const STALE_MARK: &str = "(predates transcript changes)";

/// One part of an export.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ExportSection {
    Transcript,
    Summary,
    Decisions,
    Actions,
    Notes,
    /// The result of the template with this ID.
    Template(String),
}

impl ExportSection {
    /// `transcript`, `summary`, `decisions`, `actions` or `template:<id>`.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        Ok(match s {
            "transcript" => Self::Transcript,
            "summary" => Self::Summary,
            "decisions" => Self::Decisions,
            "actions" => Self::Actions,
            "notes" => Self::Notes,
            _ => match s.strip_prefix("template:") {
                Some(id) => {
                    crate::templates::validate_id(id)?;
                    Self::Template(id.to_owned())
                }
                None => {
                    return Err(Error::InvalidInput(format!(
                        "unknown export section {s:?} (transcript, summary, decisions, \
                         actions, notes or template:<id>)"
                    )));
                }
            },
        })
    }

    /// The name [`Self::parse`] reads.
    pub fn name(&self) -> String {
        match self {
            Self::Transcript => "transcript".into(),
            Self::Summary => "summary".into(),
            Self::Decisions => "decisions".into(),
            Self::Actions => "actions".into(),
            Self::Notes => "notes".into(),
            Self::Template(id) => format!("template:{id}"),
        }
    }
}

/// The file format of an export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    #[default]
    Markdown,
    Text,
    Json,
}

impl ExportFormat {
    /// `md`, `txt` or `json` (also `markdown`, `text`).
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "md" | "markdown" => Ok(Self::Markdown),
            "txt" | "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(Error::InvalidInput(format!(
                "unknown export format {other:?} (md, txt or json)"
            ))),
        }
    }

    /// The file extension: `md`, `txt` or `json`.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Text => "txt",
            Self::Json => "json",
        }
    }
}

/// A rendered export.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExportDoc {
    pub content: String,
    /// The sections included, in output order (names as
    /// [`ExportSection::name`]).
    pub sections: Vec<String>,
    /// Requested sections the meeting does not have.
    pub omitted: Vec<String>,
}

/// The sections `meeting` can export, in output order: summary, decisions
/// and action items when the enrichment has them, each template result,
/// then the transcript when it has lines. The export form offers these and
/// checks them all by default.
pub fn available_sections(meeting: &Meeting) -> Vec<ExportSection> {
    let mut out = Vec::new();
    if let Some(e) = &meeting.enrichment {
        if !e.summary.trim().is_empty() {
            out.push(ExportSection::Summary);
        }
        if !e.decisions.is_empty() {
            out.push(ExportSection::Decisions);
        }
        if !e.actions.is_empty() {
            out.push(ExportSection::Actions);
        }
    }
    if !meeting.bookmarks.is_empty() || !meeting.notes.is_empty() {
        out.push(ExportSection::Notes);
    }
    for r in &meeting.template_results {
        out.push(ExportSection::Template(r.template_id.clone()));
    }
    if meeting.segments.iter().any(|s| !s.text.trim().is_empty()) {
        out.push(ExportSection::Transcript);
    }
    out
}

/// Renders the export of `meeting`: the requested `sections` it has, in the
/// order of [`available_sections`] (duplicates ignored), in `format`;
/// `timestamps` adds `[hh:mm:ss]` to transcript lines and decisions, and an
/// `At` column to the action table.
pub fn render_export(
    meeting: &Meeting,
    people: &PeopleLookup,
    sections: &[ExportSection],
    format: ExportFormat,
    timestamps: bool,
) -> Result<ExportDoc> {
    let available = available_sections(meeting);
    let included: Vec<ExportSection> = available
        .iter()
        .filter(|s| sections.contains(s))
        .cloned()
        .collect();
    let mut omitted: Vec<String> = Vec::new();
    for s in sections {
        let name = s.name();
        if !available.contains(s) && !omitted.contains(&name) {
            omitted.push(name);
        }
    }
    let content = match format {
        ExportFormat::Markdown => markdown(meeting, people, &included, timestamps),
        ExportFormat::Text => text(meeting, people, &included, timestamps),
        ExportFormat::Json => {
            let v = json_value(meeting, people, &included, timestamps);
            let mut s = serde_json::to_string_pretty(&v)
                .map_err(|e| Error::Internal(format!("serialising export: {e}")))?;
            s.push('\n');
            s
        }
    };
    Ok(ExportDoc {
        content,
        sections: included.iter().map(ExportSection::name).collect(),
        omitted,
    })
}

/// The destination's existing parent, resolved through symlinks. A bare
/// filename is relative to the working directory.
pub fn outside_library_parent(library: &Path, file: &Path) -> Option<PathBuf> {
    let library = library.canonicalize().ok()?;
    let parent = file.parent()?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent = parent.canonicalize().ok()?;
    (parent.is_dir() && !parent.starts_with(library)).then_some(parent)
}

/// Writes an export to `path` atomically, readable by its owner only (mode
/// 0600 where the platform has Unix permissions).
pub fn write_export(path: &Path, text: &str) -> Result<()> {
    fsutil::write_atomic_with_mode(path, text.as_bytes(), Some(0o600))
}

/// The meeting's action items as a Markdown table `| Status | Action |
/// Owner | Due | At |`, names substituted and escaped; a user-set action
/// the latest summary no longer produced is marked. Empty without actions.
pub fn actions_table_markdown(meeting: &Meeting, people: &PeopleLookup) -> String {
    actions_table(meeting, people, true)
}

fn title(meeting: &Meeting) -> &str {
    meeting
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("Untitled meeting")
}

fn when(meeting: &Meeting) -> String {
    let minutes = (meeting.duration_s / 60.0).round() as u64;
    format!(
        "{} · {} min",
        meeting.started_at.format("%Y-%m-%d %H:%M"),
        minutes
    )
}

fn status_word(s: ActionStatus) -> &'static str {
    match s {
        ActionStatus::Open => "Open",
        ActionStatus::Done => "Done",
        ActionStatus::Dropped => "Dropped",
    }
}

fn template_title(r: &TemplateResult) -> String {
    let name = r.template_name.trim();
    if name.is_empty() {
        r.template_id.clone()
    } else {
        name.to_owned()
    }
}

/// A Markdown table cell: one line, unescaped pipes escaped (a name
/// rendered as [`NameStyle::Markdown`] arrives with its pipes escaped).
fn cell(s: &str) -> String {
    let line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(line.len());
    let mut escaped = false;
    for c in line.chars() {
        if c == '|' && !escaped {
            out.push('\\');
        }
        escaped = c == '\\' && !escaped;
        out.push(c);
    }
    out
}

struct ActionRow {
    status: &'static str,
    text: String,
    owner: String,
    due: String,
    at: String,
    stale: bool,
}

fn action_rows(
    meeting: &Meeting,
    people: &PeopleLookup,
    actions: &[Action],
    style: NameStyle,
) -> Vec<ActionRow> {
    let lp = meeting
        .enrichment
        .as_ref()
        .map(|e| e.line_people.clone())
        .unwrap_or_default();
    actions
        .iter()
        .map(|a| ActionRow {
            status: status_word(a.status),
            text: substitute_speakers_as(meeting, &lp, a.text.trim(), people, style),
            owner: enrichment_who(
                meeting,
                &lp,
                a.owner_speaker,
                a.owner_name.as_deref(),
                people,
            )
            .map(|n| style.apply(&n))
            .unwrap_or_default(),
            due: a.due.map(|d| d.to_string()).unwrap_or_default(),
            at: format_hms(a.at_s),
            stale: a.stale,
        })
        .collect()
}

fn actions_table(meeting: &Meeting, people: &PeopleLookup, timestamps: bool) -> String {
    let Some(e) = &meeting.enrichment else {
        return String::new();
    };
    if e.actions.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    if timestamps {
        out.push_str("| Status | Action | Owner | Due | At |\n|---|---|---|---|---|\n");
    } else {
        out.push_str("| Status | Action | Owner | Due |\n|---|---|---|---|\n");
    }
    for r in action_rows(meeting, people, &e.actions, NameStyle::Markdown) {
        let mut text = cell(&r.text);
        if r.stale {
            text.push_str(" (no longer in the latest summary)");
        }
        out.push_str(&format!(
            "| {} | {} | {} | {} |",
            r.status,
            text,
            cell(&r.owner),
            r.due
        ));
        if timestamps {
            out.push_str(&format!(" {} |", r.at));
        }
        out.push('\n');
    }
    out
}

/// Decision lines (`text (who)`) with their times, names in `style`.
fn decision_lines(
    meeting: &Meeting,
    people: &PeopleLookup,
    style: NameStyle,
) -> Vec<(String, String)> {
    let Some(e) = &meeting.enrichment else {
        return Vec::new();
    };
    let lp = &e.line_people;
    e.decisions
        .iter()
        .map(|d| {
            let mut line = substitute_speakers_as(meeting, lp, d.text.trim(), people, style);
            if let Some(who) =
                enrichment_who(meeting, lp, d.by_speaker, d.by_name.as_deref(), people)
            {
                line.push_str(&format!(" ({})", style.apply(&who)));
            }
            (format_hms(d.at_s), line)
        })
        .collect()
}

fn summary_text(meeting: &Meeting, people: &PeopleLookup, style: NameStyle) -> String {
    meeting.enrichment.as_ref().map_or_else(String::new, |e| {
        substitute_speakers_as(meeting, &e.line_people, e.summary.trim(), people, style)
    })
}

fn write_notes(out: &mut String, meeting: &Meeting, style: NameStyle) {
    use std::fmt::Write as _;

    let mut entries: Vec<_> = meeting
        .bookmarks
        .iter()
        .map(|b| (b.at_s, "Bookmark", b.label.as_deref().unwrap_or("")))
        .chain(
            meeting
                .notes
                .iter()
                .map(|n| (n.at_s, "Note", n.text.as_str())),
        )
        .collect();
    entries.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (at_s, kind, text) in entries {
        let time = format_hms(at_s);
        let short_time = time.strip_prefix("00:").unwrap_or(&time);
        if text.trim().is_empty() {
            let _ = writeln!(out, "- {short_time} · {kind}");
        } else {
            let _ = writeln!(out, "- {short_time} · {kind} · {}", style.apply(text));
        }
    }
}

fn markdown(
    meeting: &Meeting,
    people: &PeopleLookup,
    sections: &[ExportSection],
    timestamps: bool,
) -> String {
    let stale = crate::ops::enrichment_is_stale(meeting);
    let md = NameStyle::Markdown;
    let mut out = format!("# {}\n\n{}\n", md.apply(title(meeting)), when(meeting));
    for s in sections {
        out.push('\n');
        match s {
            ExportSection::Summary => {
                out.push_str("## Summary\n\n");
                if stale {
                    out.push_str(&format!("*{STALE_MARK}*\n\n"));
                }
                out.push_str(&summary_text(meeting, people, md));
                out.push('\n');
            }
            ExportSection::Decisions => {
                out.push_str("## Decisions\n\n");
                if stale {
                    out.push_str(&format!("*{STALE_MARK}*\n\n"));
                }
                for (at, line) in decision_lines(meeting, people, md) {
                    if timestamps {
                        out.push_str(&format!("- [{at}] {line}\n"));
                    } else {
                        out.push_str(&format!("- {line}\n"));
                    }
                }
            }
            ExportSection::Actions => {
                out.push_str("## Action items\n\n");
                if stale {
                    out.push_str(&format!("*{STALE_MARK}*\n\n"));
                }
                out.push_str(&actions_table(meeting, people, timestamps));
            }
            ExportSection::Notes => {
                out.push_str("## Bookmarks and notes\n\n");
                write_notes(&mut out, meeting, md);
            }
            ExportSection::Template(id) => {
                let Some(r) = meeting.template_result(id) else {
                    continue;
                };
                out.push_str(&format!("## {}\n\n", md.apply(&template_title(r))));
                if crate::ops::template_is_stale(meeting, r) {
                    out.push_str(&format!("*{STALE_MARK}*\n\n"));
                }
                out.push_str(&crate::render::template_result_md(meeting, r, people));
                out.push('\n');
            }
            ExportSection::Transcript => {
                out.push_str("## Transcript\n\n");
                for line in transcript_lines(meeting, people, timestamps, md) {
                    out.push_str(&line);
                    out.push_str("  \n");
                }
            }
        }
    }
    out
}

/// Transcript lines (`[hh:mm:ss] Name: text`), names in `style`; the text
/// is the transcript's own.
pub fn transcript_lines(
    meeting: &Meeting,
    people: &PeopleLookup,
    timestamps: bool,
    style: NameStyle,
) -> Vec<String> {
    let attribution = crate::ops::effective_attribution(meeting);
    crate::render::transcript_turns(meeting, &attribution, people)
        .into_iter()
        .map(|t| {
            let name = t.display_name(style);
            if timestamps {
                format!("[{}] {name}: {}", format_hms(t.start), t.text)
            } else {
                format!("{name}: {}", t.text)
            }
        })
        .collect()
}

fn heading(out: &mut String, title: &str) {
    out.push_str(&title.to_uppercase());
    out.push('\n');
    out.push_str(&"=".repeat(title.chars().count()));
    out.push_str("\n\n");
}

fn text(
    meeting: &Meeting,
    people: &PeopleLookup,
    sections: &[ExportSection],
    timestamps: bool,
) -> String {
    let stale = crate::ops::enrichment_is_stale(meeting);
    let plain = NameStyle::Plain;
    let mut out = format!("{}\n{}\n", plain.apply(title(meeting)), when(meeting));
    for s in sections {
        out.push('\n');
        match s {
            ExportSection::Summary => {
                heading(&mut out, "Summary");
                if stale {
                    out.push_str(&format!("{STALE_MARK}\n\n"));
                }
                out.push_str(&summary_text(meeting, people, plain));
                out.push('\n');
            }
            ExportSection::Decisions => {
                heading(&mut out, "Decisions");
                if stale {
                    out.push_str(&format!("{STALE_MARK}\n\n"));
                }
                for (at, line) in decision_lines(meeting, people, plain) {
                    if timestamps {
                        out.push_str(&format!("- [{at}] {line}\n"));
                    } else {
                        out.push_str(&format!("- {line}\n"));
                    }
                }
            }
            ExportSection::Actions => {
                heading(&mut out, "Action items");
                if stale {
                    out.push_str(&format!("{STALE_MARK}\n\n"));
                }
                let actions = meeting
                    .enrichment
                    .as_ref()
                    .map_or(&[][..], |e| e.actions.as_slice());
                for r in action_rows(meeting, people, actions, plain) {
                    let mut details = vec![r.status.to_owned()];
                    if !r.owner.is_empty() {
                        details.push(r.owner.clone());
                    }
                    if !r.due.is_empty() {
                        details.push(format!("due {}", r.due));
                    }
                    if timestamps {
                        details.push(format!("at {}", r.at));
                    }
                    if r.stale {
                        details.push("no longer in the latest summary".into());
                    }
                    out.push_str(&format!("- {} ({})\n", r.text, details.join(", ")));
                }
            }
            ExportSection::Notes => {
                heading(&mut out, "Bookmarks and notes");
                write_notes(&mut out, meeting, plain);
            }
            ExportSection::Template(id) => {
                let Some(r) = meeting.template_result(id) else {
                    continue;
                };
                heading(&mut out, &plain.apply(&template_title(r)));
                if crate::ops::template_is_stale(meeting, r) {
                    out.push_str(&format!("{STALE_MARK}\n\n"));
                }
                out.push_str(&substitute_speakers_as(
                    meeting,
                    &r.line_people,
                    r.markdown.trim(),
                    people,
                    plain,
                ));
                out.push('\n');
            }
            ExportSection::Transcript => {
                heading(&mut out, "Transcript");
                for line in transcript_lines(meeting, people, timestamps, plain) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }
    }
    out
}

fn json_value(
    meeting: &Meeting,
    people: &PeopleLookup,
    sections: &[ExportSection],
    timestamps: bool,
) -> Value {
    let stale = crate::ops::enrichment_is_stale(meeting);
    let mut out = serde_json::Map::new();
    out.insert(
        "meeting".into(),
        json!({
            "id": meeting.id,
            "title": meeting.title,
            "started_at": meeting.started_at.to_rfc3339(),
            "duration_s": meeting.duration_s,
            "app": meeting.app,
            "source": meeting.source,
            "tags": meeting.effective_tags(),
            "user_tags": meeting.tags,
            "tags_removed": meeting.tags_removed,
            "summary_tags": meeting.enrichment.as_ref().map(|e| &e.tags),
        }),
    );
    let mut templates = Vec::new();
    for s in sections {
        match s {
            ExportSection::Summary => {
                let text = summary_text(meeting, people, NameStyle::Raw);
                out.insert("summary".into(), json!({
                    "text": text,
                    "stale": stale,
                    "instructions": meeting.enrichment.as_ref().and_then(|e| e.instructions.as_deref()),
                }));
            }
            ExportSection::Decisions => {
                let lp = meeting
                    .enrichment
                    .as_ref()
                    .map(|e| e.line_people.clone())
                    .unwrap_or_default();
                let items: Vec<Value> = meeting
                    .enrichment
                    .iter()
                    .flat_map(|e| &e.decisions)
                    .map(|d| {
                        let text =
                            substitute_speakers_as(meeting, &lp, d.text.trim(), people, NameStyle::Raw);
                        let mut v = json!({
                            "id": d.id,
                            "text": text,
                            "by": enrichment_who(meeting, &lp, d.by_speaker, d.by_name.as_deref(), people),
                        });
                        if timestamps {
                            v["at_s"] = json!(d.at_s);
                        }
                        v
                    })
                    .collect();
                out.insert(
                    "decisions".into(),
                    json!({ "items": items, "stale": stale }),
                );
            }
            ExportSection::Actions => {
                let actions = meeting
                    .enrichment
                    .as_ref()
                    .map_or(&[][..], |e| e.actions.as_slice());
                let items: Vec<Value> = actions
                    .iter()
                    .zip(action_rows(meeting, people, actions, NameStyle::Raw))
                    .map(|(a, r)| {
                        let mut v = json!({
                            "id": a.id,
                            "status": a.status,
                            "text": r.text,
                            "owner": (!r.owner.is_empty()).then_some(r.owner),
                            "due": a.due.map(|d| d.to_string()),
                            "no_longer_in_summary": a.stale,
                        });
                        if timestamps {
                            v["at_s"] = json!(a.at_s);
                        }
                        v
                    })
                    .collect();
                out.insert("actions".into(), json!({ "items": items, "stale": stale }));
            }
            ExportSection::Notes => {
                out.insert("bookmarks".into(), json!(meeting.bookmarks));
                out.insert("notes".into(), json!(meeting.notes));
            }
            ExportSection::Template(id) => {
                if let Some(r) = meeting.template_result(id) {
                    templates.push(json!({
                        "template_id": r.template_id,
                        "template_name": r.template_name,
                        "provider": r.provider,
                        "model": r.model,
                        "created_at": r.created_at.to_rfc3339(),
                        "stale": crate::ops::template_is_stale(meeting, r),
                        "markdown": crate::render::template_result_md(meeting, r, people),
                    }));
                }
            }
            ExportSection::Transcript => {
                let attribution = crate::ops::effective_attribution(meeting);
                let lines: Vec<Value> =
                    crate::render::transcript_turns(meeting, &attribution, people)
                        .into_iter()
                        .map(|t| {
                            let mut v = json!({
                                "speaker": t.name,
                                "person_id": t.person_id,
                                "text": t.text,
                            });
                            if t.overlapping_speech {
                                v["overlapping_speech"] = json!(true);
                            }
                            if timestamps {
                                v["start_s"] = json!(t.start);
                                v["end_s"] = json!(t.end);
                            }
                            v
                        })
                        .collect();
                out.insert("transcript".into(), Value::Array(lines));
            }
        }
    }
    if !templates.is_empty() {
        out.insert("templates".into(), Value::Array(templates));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests;
