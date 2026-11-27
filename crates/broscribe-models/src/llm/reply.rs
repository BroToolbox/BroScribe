//! The replies an LLM must return (plan M2 §2, §9.5, §10.2, §10.11; design
//! §8.4): the enrichment's and a template's JSON schemas, serde types and
//! the explicit checks that the schema cannot express (speaker references
//! that exist in the meeting, timestamps inside the meeting, dates). Prose
//! may name speakers only as `{{S#}}` placeholders and corrected lines'
//! people as the `{{P#}}` labels the prompt sent; those labels are names,
//! never speaker IDs.

use broscribe_core::SpeakerId;
use broscribe_core::render::is_line_label;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::chain::Verdict;

/// Limits that keep a runaway reply out of `meeting.json`.
const MAX_ITEMS: usize = 200;
const MAX_LABELS: usize = 50;
const MAX_TEXT: usize = 2_000;
const MAX_SUMMARY: usize = 20_000;
const MAX_LABEL: usize = 100;
/// Longest template reply (`markdown`), in characters.
pub const MAX_TEMPLATE_MARKDOWN: usize = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyStatus {
    Open,
    Done,
    Dropped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyDecision {
    pub text: String,
    pub at_s: f64,
    pub by_speaker: Option<String>,
    pub by_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyAction {
    pub text: String,
    pub at_s: f64,
    pub owner_speaker: Option<String>,
    pub owner_name: Option<String>,
    /// `YYYY-MM-DD`.
    pub due: Option<String>,
    pub status: ReplyStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrichmentReply {
    pub summary: String,
    pub topics: Vec<String>,
    pub keywords: Vec<String>,
    pub tags: Vec<String>,
    pub decisions: Vec<ReplyDecision>,
    pub actions: Vec<ReplyAction>,
}

/// What the reply is checked against.
#[derive(Clone, Copy, Debug)]
pub struct ReplyContext<'a> {
    /// Speakers of the meeting (or of the chunk's meeting).
    pub speakers: &'a [SpeakerId],
    /// Line labels (`P1`, …) the prompt used for corrected lines: valid only
    /// as `{{P#}}` placeholders and as `by_name` / `owner_name`.
    pub line_labels: &'a [String],
    /// Meeting length; `at_s` must fall inside it (1 s tolerance).
    pub duration_s: f64,
}

fn nullable(ty: &str) -> Value {
    json!({"type": [ty, "null"]})
}

/// JSON schema of [`EnrichmentReply`], strict-mode compatible (every
/// property required, no extra properties, `null` for absent values).
pub fn enrichment_schema() -> Value {
    let labels = json!({
        "type": "array",
        "maxItems": MAX_LABELS,
        "items": {"type": "string", "minLength": 1, "maxLength": MAX_LABEL}
    });
    let text = json!({"type": "string", "minLength": 1, "maxLength": MAX_TEXT});
    let at = json!({"type": "number", "minimum": 0});
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "topics", "keywords", "tags", "decisions", "actions"],
        "properties": {
            "summary": {"type": "string", "maxLength": MAX_SUMMARY},
            "topics": labels,
            "keywords": labels,
            "tags": labels,
            "decisions": {
                "type": "array",
                "maxItems": MAX_ITEMS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["text", "at_s", "by_speaker", "by_name"],
                    "properties": {
                        "text": text,
                        "at_s": at,
                        "by_speaker": nullable("string"),
                        "by_name": nullable("string"),
                    }
                }
            },
            "actions": {
                "type": "array",
                "maxItems": MAX_ITEMS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["text", "at_s", "owner_speaker", "owner_name", "due", "status"],
                    "properties": {
                        "text": text,
                        "at_s": at,
                        "owner_speaker": nullable("string"),
                        "owner_name": nullable("string"),
                        "due": nullable("string"),
                        "status": {"type": "string", "enum": ["open", "done", "dropped"]},
                    }
                }
            }
        }
    })
}

fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let part = &s[r];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (Some(y), Some(m), Some(d)) = (num(0..4), num(5..7), num(8..10)) else {
        return false;
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&d)
}

/// Clears a speaker reference absent from the meeting (a line label is
/// never a speaker); returns whether it did.
fn clear_unknown(r: &mut Option<String>, ctx: &ReplyContext<'_>) -> bool {
    let known = r
        .as_deref()
        .map(|s| SpeakerId::parse(s.trim()).is_ok_and(|id| ctx.speakers.contains(&id)));
    match known {
        Some(false) => {
            *r = None;
            true
        }
        _ => false,
    }
}

/// Clears a name that is a line label the prompt did not send; returns
/// whether it did. Other names stay as written.
fn clear_unknown_label(name: &mut Option<String>, ctx: &ReplyContext<'_>) -> bool {
    let unknown = name
        .as_deref()
        .map(str::trim)
        .is_some_and(|n| is_line_label(n) && !ctx.line_labels.iter().any(|l| l == n));
    if unknown {
        *name = None;
    }
    unknown
}

/// Replaces `{{S#}}` placeholders of absent speakers and `{{P#}}` labels the
/// prompt did not send with plain `S#` / `P#` text (unresolved); returns how
/// many it replaced. Any other `{{…}}` stays as written.
fn clear_unknown_placeholders(text: &mut String, ctx: &ReplyContext<'_>) -> usize {
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    let mut replaced = 0;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        match tail.find("}}") {
            Some(end) => {
                let inner = tail[..end].trim();
                let whole = &rest[start..start + 2 + end + 2];
                match SpeakerId::parse(inner) {
                    Ok(id) if ctx.speakers.contains(&id) => out.push_str(whole),
                    Ok(id) => {
                        out.push_str(&id.to_string());
                        replaced += 1;
                    }
                    Err(_) if is_line_label(inner) => {
                        if ctx.line_labels.iter().any(|l| l == inner) {
                            out.push_str(whole);
                        } else {
                            out.push_str(inner);
                            replaced += 1;
                        }
                    }
                    // Not a speaker or line placeholder: keep as written.
                    Err(_) => out.push_str(whole),
                }
                rest = &tail[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    if replaced > 0 {
        *text = out;
    }
    replaced
}

/// Checks a schema-valid reply. Malformed replies (wrong shape, empty texts,
/// timestamps outside the meeting, invalid dates) are rejected; references to
/// speakers absent from the meeting (a line label used as a speaker counts
/// as one) and line labels the prompt did not send make the reply flawed:
/// retried once, then kept with those references cleared (plan M2 §10.11).
pub fn check_enrichment(value: &Value, ctx: &ReplyContext<'_>) -> Verdict<EnrichmentReply> {
    let mut reply: EnrichmentReply = match serde_json::from_value(value.clone()) {
        Ok(r) => r,
        Err(e) => {
            // serde messages can quote values: keep only the position.
            return Verdict::Reject(format!(
                "reply does not match the enrichment shape (line {}, column {})",
                e.line(),
                e.column()
            ));
        }
    };
    let max_at = ctx.duration_s.max(0.0) + 1.0;
    let at_ok = |at: f64| at.is_finite() && (0.0..=max_at).contains(&at);
    for (i, d) in reply.decisions.iter().enumerate() {
        if d.text.trim().is_empty() {
            return Verdict::Reject(format!("decisions[{i}]: empty text"));
        }
        if !at_ok(d.at_s) {
            return Verdict::Reject(format!("decisions[{i}]: at_s outside the meeting"));
        }
    }
    for (i, a) in reply.actions.iter().enumerate() {
        if a.text.trim().is_empty() {
            return Verdict::Reject(format!("actions[{i}]: empty text"));
        }
        if !at_ok(a.at_s) {
            return Verdict::Reject(format!("actions[{i}]: at_s outside the meeting"));
        }
        if a.due.as_deref().is_some_and(|d| !valid_date(d)) {
            return Verdict::Reject(format!("actions[{i}]: due is not a YYYY-MM-DD date"));
        }
    }
    for (name, list) in [
        ("topics", &reply.topics),
        ("keywords", &reply.keywords),
        ("tags", &reply.tags),
    ] {
        if list.iter().any(|s| s.trim().is_empty()) {
            return Verdict::Reject(format!("{name}: empty entry"));
        }
    }

    let mut issues = Vec::new();
    let n = clear_unknown_placeholders(&mut reply.summary, ctx);
    if n > 0 {
        issues.push(format!("summary: {n} unknown speaker placeholder(s)"));
    }
    for (i, d) in reply.decisions.iter_mut().enumerate() {
        if clear_unknown(&mut d.by_speaker, ctx) {
            issues.push(format!("decisions[{i}].by_speaker: unknown speaker"));
        }
        if clear_unknown_label(&mut d.by_name, ctx) {
            issues.push(format!("decisions[{i}].by_name: unknown line label"));
        }
        let n = clear_unknown_placeholders(&mut d.text, ctx);
        if n > 0 {
            issues.push(format!(
                "decisions[{i}].text: {n} unknown speaker placeholder(s)"
            ));
        }
    }
    for (i, a) in reply.actions.iter_mut().enumerate() {
        if clear_unknown(&mut a.owner_speaker, ctx) {
            issues.push(format!("actions[{i}].owner_speaker: unknown speaker"));
        }
        if clear_unknown_label(&mut a.owner_name, ctx) {
            issues.push(format!("actions[{i}].owner_name: unknown line label"));
        }
        let n = clear_unknown_placeholders(&mut a.text, ctx);
        if n > 0 {
            issues.push(format!(
                "actions[{i}].text: {n} unknown speaker placeholder(s)"
            ));
        }
    }
    if issues.is_empty() {
        Verdict::Accept(reply)
    } else {
        Verdict::Flawed {
            value: reply,
            issues,
        }
    }
}

/// A template reply: one Markdown document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateReply {
    pub markdown: String,
}

/// JSON schema of [`TemplateReply`], strict-mode compatible.
pub fn template_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["markdown"],
        "properties": {
            "markdown": {"type": "string", "minLength": 1, "maxLength": MAX_TEMPLATE_MARKDOWN},
        }
    })
}

/// Checks a schema-valid template reply: blank Markdown is rejected;
/// `{{S#}}` of absent speakers and `{{P#}}` labels the prompt did not send
/// make it flawed (retried once, then kept with them cleared to plain
/// text), as for enrichment.
pub fn check_template(value: &Value, ctx: &ReplyContext<'_>) -> Verdict<TemplateReply> {
    let mut reply: TemplateReply = match serde_json::from_value(value.clone()) {
        Ok(r) => r,
        Err(e) => {
            return Verdict::Reject(format!(
                "reply does not match the template shape (line {}, column {})",
                e.line(),
                e.column()
            ));
        }
    };
    if reply.markdown.trim().is_empty() {
        return Verdict::Reject("markdown: empty".into());
    }
    if reply.markdown.chars().count() > MAX_TEMPLATE_MARKDOWN {
        return Verdict::Reject("markdown: too long".into());
    }
    let n = clear_unknown_placeholders(&mut reply.markdown, ctx);
    if n == 0 {
        Verdict::Accept(reply)
    } else {
        Verdict::Flawed {
            value: reply,
            issues: vec![format!("markdown: {n} unknown speaker placeholder(s)")],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::json::check_schema;

    fn speakers() -> Vec<SpeakerId> {
        vec![SpeakerId::new(1).unwrap(), SpeakerId::new(2).unwrap()]
    }

    fn sample() -> Value {
        json!({
            "summary": "{{S1}} and {{S2}} planned the release.",
            "topics": ["release"],
            "keywords": ["launch"],
            "tags": ["planning"],
            "decisions": [{"text": "Ship on Friday", "at_s": 12.5, "by_speaker": "S1", "by_name": null}],
            "actions": [{"text": "{{S2}} writes notes", "at_s": 30.0, "owner_speaker": "S2",
                         "owner_name": null, "due": "2026-11-20", "status": "open"}]
        })
    }

    #[test]
    fn schema_accepts_sample_and_rejects_extras() {
        let schema = enrichment_schema();
        assert!(check_schema(&sample(), &schema).is_ok());
        let mut extra = sample();
        extra["mood"] = json!("x");
        assert!(check_schema(&extra, &schema).is_err());
        let mut bad = sample();
        bad["actions"][0]["status"] = json!("maybe");
        assert!(check_schema(&bad, &schema).is_err());
    }

    #[test]
    fn accepts_valid_reply() {
        let s = speakers();
        let ctx = ReplyContext {
            speakers: &s,
            line_labels: &[],
            duration_s: 60.0,
        };
        let Verdict::Accept(r) = check_enrichment(&sample(), &ctx) else {
            panic!("expected accept");
        };
        assert_eq!(r.actions[0].status, ReplyStatus::Open);
        assert_eq!(r.summary, "{{S1}} and {{S2}} planned the release.");
    }

    #[test]
    fn unknown_speakers_are_flawed_and_cleared() {
        let mut v = sample();
        v["decisions"][0]["by_speaker"] = json!("S7");
        v["actions"][0]["text"] = json!("{{S9}} writes notes for {{S1}}");
        v["summary"] = json!("{{S3}} spoke");
        let s = speakers();
        let ctx = ReplyContext {
            speakers: &s,
            line_labels: &[],
            duration_s: 60.0,
        };
        let Verdict::Flawed { value, issues } = check_enrichment(&v, &ctx) else {
            panic!("expected flawed");
        };
        assert_eq!(value.decisions[0].by_speaker, None);
        assert_eq!(value.actions[0].text, "S9 writes notes for {{S1}}");
        assert_eq!(value.summary, "S3 spoke");
        assert_eq!(issues.len(), 3);
    }

    #[test]
    fn rejects_bad_times_dates_and_shapes() {
        let s = speakers();
        let ctx = ReplyContext {
            speakers: &s,
            line_labels: &[],
            duration_s: 60.0,
        };
        let mut v = sample();
        v["decisions"][0]["at_s"] = json!(500.0);
        assert!(matches!(check_enrichment(&v, &ctx), Verdict::Reject(_)));
        let mut v = sample();
        v["actions"][0]["due"] = json!("2026-02-30");
        assert!(matches!(check_enrichment(&v, &ctx), Verdict::Reject(_)));
        let mut v = sample();
        v["actions"][0]["text"] = json!("  ");
        assert!(matches!(check_enrichment(&v, &ctx), Verdict::Reject(_)));
        assert!(matches!(
            check_enrichment(&json!({"summary": 1}), &ctx),
            Verdict::Reject(_)
        ));
    }

    #[test]
    fn line_labels_are_names_never_speakers() {
        let s = speakers();
        let labels = vec!["P1".to_owned()];
        let ctx = ReplyContext {
            speakers: &s,
            line_labels: &labels,
            duration_s: 60.0,
        };
        // Sent labels are kept as placeholders and as names.
        let mut v = sample();
        v["summary"] = json!("{{P1}} and {{S1}} agreed; {{X}} and {{ P1 }} stay.");
        v["decisions"][0]["by_speaker"] = Value::Null;
        v["decisions"][0]["by_name"] = json!("P1");
        let Verdict::Accept(r) = check_enrichment(&v, &ctx) else {
            panic!("expected accept");
        };
        assert_eq!(
            r.summary,
            "{{P1}} and {{S1}} agreed; {{X}} and {{ P1 }} stay."
        );
        assert_eq!(r.decisions[0].by_name.as_deref(), Some("P1"));

        // A label as a speaker, or a label the prompt never sent, is cleared.
        let mut v = sample();
        v["summary"] = json!("{{P2}} spoke with {{P1}}; {{Q1}} too.");
        v["decisions"][0]["by_speaker"] = json!("P1");
        v["actions"][0]["owner_speaker"] = Value::Null;
        v["actions"][0]["owner_name"] = json!("P3");
        let Verdict::Flawed { value, issues } = check_enrichment(&v, &ctx) else {
            panic!("expected flawed");
        };
        assert_eq!(value.summary, "P2 spoke with {{P1}}; {{Q1}} too.");
        assert_eq!(value.decisions[0].by_speaker, None);
        assert_eq!(value.actions[0].owner_name, None);
        assert_eq!(issues.len(), 3, "{issues:?}");
    }

    #[test]
    fn dates() {
        assert!(valid_date("2028-02-29"));
        assert!(!valid_date("2026-02-29"));
        assert!(!valid_date("2026-13-01"));
        assert!(!valid_date("26-01-01"));
        assert!(!valid_date("2026-1-011"));
    }

    #[test]
    fn template_replies_are_one_markdown_document() {
        let schema = template_schema();
        check_schema(&json!({"markdown": "# Notes\n{{S1}} spoke."}), &schema).unwrap();
        assert!(check_schema(&json!({"markdown": "x", "extra": 1}), &schema).is_err());
        assert!(check_schema(&json!({}), &schema).is_err());
        let labels = vec!["P1".to_owned()];
        let ctx = ReplyContext {
            speakers: &speakers(),
            line_labels: &labels,
            duration_s: 60.0,
        };
        assert!(matches!(
            check_template(&json!({"markdown": "{{S1}} and {{P1}}"}), &ctx),
            Verdict::Accept(_)
        ));
        match check_template(
            &json!({"markdown": "{{S9}} met {{P4}} and {{Other}}"}),
            &ctx,
        ) {
            Verdict::Flawed { value, issues } => {
                assert_eq!(value.markdown, "S9 met P4 and {{Other}}");
                assert_eq!(issues.len(), 1);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            check_template(&json!({"markdown": "   "}), &ctx),
            Verdict::Reject(_)
        ));
    }
}
