use std::collections::BTreeMap;

use chrono::NaiveDate;

use super::*;
use crate::schema::tests::sample_meeting;
use crate::schema::{Decision, Enrichment, Person, StatusSource};
use crate::{PersonId, SpeakerId};

const ANA: &str = "p-a1a1a1";
const BO: &str = "p-b2b2b2";
const CY: &str = "p-c3c3c3";

fn pid(s: &str) -> PersonId {
    PersonId::parse(s).unwrap()
}

fn people() -> PeopleLookup {
    PeopleLookup::new([
        Person::new(pid(ANA), "Ana Example"),
        Person::new(pid(BO), "Bo Invented"),
        Person::new(pid(CY), "Cy Made-Up"),
    ])
}

fn result(id: &str, name: &str, markdown: &str, p1: &str, stale: bool) -> TemplateResult {
    TemplateResult {
        template_id: id.into(),
        template_name: name.into(),
        instructions_sha256: "0".repeat(64),
        provider: "fake".into(),
        model: Some("model-x".into()),
        requested_model: None,
        model_reported: true,
        created_at: crate::schema::now(),
        revision: String::new(),
        attribution_revision: String::new(),
        line_people: BTreeMap::from([("P1".to_owned(), pid(p1))]),
        stale,
        markdown: markdown.into(),
    }
}

/// The sample meeting (speaker 1 is Ana; speaker 2 unnamed) with an
/// enrichment whose `P1` is Bo and two template results whose `P1` are Bo
/// and Cy respectively.
fn meeting() -> Meeting {
    let mut m = sample_meeting();
    m.enrichment = Some(Enrichment {
        provider: "fake".into(),
        model: None,
        requested_model: None,
        model_reported: false,
        created_at: crate::schema::now(),
        stale: false,
        revision: String::new(),
        attribution_revision: String::new(),
        line_people: BTreeMap::from([("P1".to_owned(), pid(BO))]),
        summary: "{{S1}} opened; {{P1}} agreed.".into(),
        topics: vec![],
        keywords: vec![],
        tags: vec![],
        decisions: vec![Decision {
            id: "d1".into(),
            text: "Ship the beta".into(),
            at_s: 30.0,
            by_speaker: Some(SpeakerId::new(1).unwrap()),
            by_name: None,
        }],
        actions: vec![
            Action {
                id: "a1".into(),
                text: "Draft notes | share them".into(),
                owner_speaker: None,
                owner_name: Some("P1".into()),
                due: NaiveDate::from_ymd_opt(2026, 11, 20),
                at_s: 61.0,
                status: ActionStatus::Open,
                status_source: StatusSource::Llm,
                status_updated_at: None,
                original_owner: None,
                due_user_set: false,
                stale: false,
            },
            Action {
                id: "a2".into(),
                text: "Book a room".into(),
                owner_speaker: Some(SpeakerId::new(2).unwrap()),
                owner_name: None,
                due: None,
                at_s: 90.0,
                status: ActionStatus::Done,
                status_source: StatusSource::User,
                status_updated_at: None,
                original_owner: None,
                due_user_set: false,
                stale: true,
            },
        ],
        instructions: None,
    });
    m.template_results = vec![
        result(
            "notes",
            "Meeting notes",
            "Lead: {{S1}}. Budget: {{P1}}.",
            BO,
            false,
        ),
        result("bant", "BANT insights", "Authority: {{P1}}.", CY, true),
    ];
    m
}

fn all() -> Vec<ExportSection> {
    available_sections(&meeting())
}

#[test]
fn sections_parse_and_name() {
    for s in [
        "transcript",
        "summary",
        "decisions",
        "actions",
        "notes",
        "template:bant",
    ] {
        assert_eq!(ExportSection::parse(s).unwrap().name(), s);
    }
    for bad in ["bookmarks", "template:", "template:Bad Id", ""] {
        assert_eq!(
            ExportSection::parse(bad).unwrap_err().code(),
            "invalid_input"
        );
    }
    assert_eq!(ExportFormat::parse("MD").unwrap(), ExportFormat::Markdown);
    assert_eq!(ExportFormat::parse("txt").unwrap().extension(), "txt");
    assert_eq!(ExportFormat::parse("json").unwrap(), ExportFormat::Json);
    assert_eq!(
        ExportFormat::parse("pdf").unwrap_err().code(),
        "invalid_input"
    );
    assert_eq!(
        all().iter().map(ExportSection::name).collect::<Vec<_>>(),
        [
            "summary",
            "decisions",
            "actions",
            "template:notes",
            "template:bant",
            "transcript"
        ]
    );
}

#[test]
fn markdown_export_substitutes_names_per_result() {
    let doc = render_export(&meeting(), &people(), &all(), ExportFormat::Markdown, true).unwrap();
    let md = &doc.content;
    assert!(md.starts_with("# Roadmap review\n"), "{md}");
    assert!(md.contains("## Summary\n\nAna Example opened; Bo Invented agreed."));
    assert!(md.contains("- [00:00:30] Ship the beta (Ana Example)"));
    // Each result's P1 resolves through its own map.
    assert!(md.contains("## Meeting notes\n\nLead: Ana Example. Budget: Bo Invented."));
    assert!(
        md.contains(
            "## BANT insights\n\n*(predates transcript changes)*\n\nAuthority: Cy Made-Up."
        )
    );
    // Only the stale result is marked.
    assert_eq!(md.matches(STALE_MARK).count(), 1);
    assert!(md.contains("[00:00:01] Ana Example: Hello there. Shall we start?"));
    assert!(!md.contains("{{"), "{md}");
    assert!(doc.omitted.is_empty());
    assert_eq!(doc.sections.len(), 6);
}

#[test]
fn actions_render_as_a_table() {
    let table = actions_table_markdown(&meeting(), &people());
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines[0], "| Status | Action | Owner | Due | At |");
    assert_eq!(lines[1], "|---|---|---|---|---|");
    assert_eq!(
        lines[2],
        "| Open | Draft notes \\| share them | Bo Invented | 2026-11-20 | 00:01:01 |"
    );
    assert_eq!(
        lines[3],
        "| Done | Book a room (no longer in the latest summary) | Speaker 2 |  | 00:01:30 |"
    );
    let mut bare = meeting();
    bare.enrichment = None;
    assert_eq!(actions_table_markdown(&bare, &people()), "");
}

#[test]
fn timestamps_can_be_left_out() {
    let doc = render_export(&meeting(), &people(), &all(), ExportFormat::Markdown, false).unwrap();
    assert!(doc.content.contains("| Status | Action | Owner | Due |\n"));
    assert!(
        doc.content
            .contains("\nAna Example: Hello there. Shall we start?")
    );
    assert!(doc.content.contains("- Ship the beta (Ana Example)"));
    assert!(!doc.content.contains("[00:00:01]"));
}

#[test]
fn text_and_json_exports() {
    let txt = render_export(&meeting(), &people(), &all(), ExportFormat::Text, true)
        .unwrap()
        .content;
    assert!(txt.contains("SUMMARY\n=======\n\nAna Example opened"));
    assert!(
        txt.contains("- Draft notes | share them (Open, Bo Invented, due 2026-11-20, at 00:01:01)")
    );
    assert!(txt.contains("BANT INSIGHTS\n=============\n\n(predates transcript changes)"));
    assert!(!txt.contains("| Status"));

    let json = render_export(&meeting(), &people(), &all(), ExportFormat::Json, true)
        .unwrap()
        .content;
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["meeting"]["title"], "Roadmap review");
    assert_eq!(
        v["summary"]["text"],
        "Ana Example opened; Bo Invented agreed."
    );
    assert_eq!(v["summary"]["stale"], false);
    assert_eq!(v["actions"]["items"][0]["owner"], "Bo Invented");
    assert_eq!(v["actions"]["items"][1]["no_longer_in_summary"], true);
    assert_eq!(
        v["templates"][0]["markdown"],
        "Lead: Ana Example. Budget: Bo Invented."
    );
    assert_eq!(v["templates"][1]["markdown"], "Authority: Cy Made-Up.");
    assert_eq!(v["templates"][1]["stale"], true);
    assert_eq!(v["transcript"][0]["speaker"], "Ana Example");
    assert_eq!(v["transcript"][0]["start_s"], 1.0);
    let bare = render_export(&meeting(), &people(), &all(), ExportFormat::Json, false)
        .unwrap()
        .content;
    let v: serde_json::Value = serde_json::from_str(&bare).unwrap();
    assert!(v["transcript"][0].get("start_s").is_none());
}

#[test]
fn a_stale_enrichment_marks_its_sections() {
    let mut m = meeting();
    m.enrichment.as_mut().unwrap().stale = true;
    let md = render_export(&m, &people(), &all(), ExportFormat::Markdown, true)
        .unwrap()
        .content;
    // Summary, decisions, actions and the stale template.
    assert_eq!(md.matches(STALE_MARK).count(), 4);
}

#[test]
fn absent_sections_are_omitted_and_reported() {
    // A template result without any enrichment still exports, names resolved.
    let mut m = meeting();
    m.enrichment = None;
    let wanted = [
        ExportSection::Summary,
        ExportSection::Template("notes".into()),
        ExportSection::Template("gone".into()),
        ExportSection::Template("notes".into()),
    ];
    let doc = render_export(&m, &people(), &wanted, ExportFormat::Markdown, true).unwrap();
    assert_eq!(doc.sections, ["template:notes"]);
    assert_eq!(doc.omitted, ["summary", "template:gone"]);
    assert!(
        doc.content
            .contains("Lead: Ana Example. Budget: Bo Invented.")
    );
    assert!(!doc.content.contains("## Summary"));
}

#[cfg(unix)]
#[test]
fn exports_are_written_owner_only_and_atomically() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meeting.md");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    write_export(&path, "new text").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new text");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    // No temporary file is left behind.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn parity_export_includes_detected_app_and_the_summarys_own_instructions() {
    let mut m = meeting();
    m.app = Some("synthetic-app".into());
    m.tags = vec!["indexed".into()];
    let e = m.enrichment.as_mut().unwrap();
    e.instructions = Some("Include only synthetic examples.".into());
    e.tags = vec!["generated".into()];
    let doc = render_export(
        &m,
        &people(),
        &[ExportSection::Summary],
        ExportFormat::Json,
        false,
    )
    .unwrap();
    let v: Value = serde_json::from_str(&doc.content).unwrap();
    assert_eq!(v["meeting"]["app"], "synthetic-app");
    assert_eq!(v["meeting"]["summary_tags"][0], "generated");
    assert_eq!(v["meeting"]["user_tags"][0], "indexed");
    assert_eq!(
        v["summary"]["instructions"],
        "Include only synthetic examples."
    );
    // Documents for people keep their shape; the instructions are data in JSON.
    let md = render_export(
        &m,
        &people(),
        &[ExportSection::Summary],
        ExportFormat::Markdown,
        false,
    )
    .unwrap();
    assert!(!md.content.contains("Include only synthetic examples"));
}
