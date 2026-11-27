//! Instructions and template definitions arrive on stdin (never argv):
//! `enrich instructions set --stdin`, `enrich … --instructions-stdin`,
//! `templates new|edit --stdin` and `templates run … --instructions-stdin`,
//! through the built binary.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use broscribe_core::index::Index;
use broscribe_core::queue::{JobId, Queue, TEMPLATE_KIND};
use broscribe_core::schema::{
    Meeting, ProcessingStatus, Segment, Source, Speaker, Track, TrackRole,
};
use broscribe_core::{Library, SpeakerId};
use serde_json::{Value, json};

/// One `done`, indexed meeting with two synthetic lines; its ID.
fn seed(lib_dir: &Path) -> String {
    let lib = Library::open(lib_dir).unwrap();
    let id = lib.new_meeting_id().unwrap();
    let started = chrono::DateTime::parse_from_rfc3339("2026-10-05T14:00:00-03:00").unwrap();
    let mut m = Meeting::new(id.clone(), Source::Import, started);
    m.title = Some("Budget sync".into());
    m.duration_s = 10.0;
    m.tracks = vec![Track {
        id: "mixed".into(),
        role: TrackRole::Mixed,
    }];
    for n in 1..=2 {
        m.speakers
            .push(Speaker::new(SpeakerId::new(n).unwrap(), "mixed"));
    }
    for (i, (spk, start, text)) in [
        (1, 0.0, "The budget is approved"),
        (2, 5.0, "Then we start in March"),
    ]
    .into_iter()
    .enumerate()
    {
        m.segments.push(Segment {
            speech_overlap: Vec::new(),
            id: i as u32,
            track: "mixed".into(),
            speaker: Some(SpeakerId::new(spk).unwrap()),
            start,
            end: start + 4.0,
            text: text.into(),
            words: Vec::new(),
        });
    }
    m.processing.status = ProcessingStatus::Done;
    lib.write(|tx| {
        tx.create_meeting_dir(&m)?;
        tx.save_meeting(&m)?;
        Index::open(tx.root())?.upsert_meeting(tx, &m)
    })
    .unwrap();
    id.as_str().to_owned()
}

/// `broscribe --json --library <tmp>/lib <args…>` with `stdin`; success and
/// the JSON document on stdout.
fn run(tmp: &Path, args: &[&str], stdin: &str) -> (bool, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_broscribe"))
        .arg("--json")
        .arg("--library")
        .arg(tmp.join("lib"))
        .args(args)
        .env("BROSCRIBE_DATA_DIR", tmp.join("data"))
        .env_remove("BROSCRIBE_LIBRARY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // A usage error can end the child before it reads stdin (broken pipe).
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (out.status.success(), value)
}

fn ok(tmp: &Path, args: &[&str], stdin: &str) -> Value {
    let (ok, v) = run(tmp, args, stdin);
    assert!(ok, "{args:?}: {v}");
    v
}

fn fails(tmp: &Path, args: &[&str], stdin: &str) -> String {
    let (ok, v) = run(tmp, args, stdin);
    assert!(!ok, "{args:?} succeeded: {v}");
    v["error"]["code"].as_str().unwrap_or_default().to_owned()
}

#[test]
fn saved_instructions_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    seed(&tmp.join("lib"));
    let show = ["enrich", "instructions", "show"];
    assert_eq!(ok(tmp, &show, ""), json!({ "instructions": null }));

    let set = ["enrich", "instructions", "set", "--stdin"];
    let saved = ok(tmp, &set, "Focus on budget risks.\nKeep it short.\n\n");
    assert_eq!(
        saved,
        json!({ "instructions": "Focus on budget risks.\nKeep it short." })
    );
    assert_eq!(ok(tmp, &show, ""), saved);
    let config = std::fs::read_to_string(tmp.join("data").join("config.toml")).unwrap();
    assert!(config.contains("Focus on budget risks."), "{config}");

    // Too long: refused, the saved text stays.
    assert_eq!(fails(tmp, &set, &"x".repeat(4_001)), "invalid_input");
    assert_eq!(ok(tmp, &show, ""), saved);
    // `set` needs --stdin: the text never goes in argv.
    assert_eq!(
        fails(tmp, &["enrich", "instructions", "set", "Focus"], ""),
        "invalid_input"
    );

    // Empty text clears, like `clear`.
    assert_eq!(ok(tmp, &set, "\n"), json!({ "instructions": null }));
    ok(tmp, &set, "Again");
    assert_eq!(
        ok(tmp, &["enrich", "instructions", "clear"], ""),
        json!({ "instructions": null })
    );
    assert_eq!(ok(tmp, &show, ""), json!({ "instructions": null }));
}

#[test]
fn run_instructions_reach_the_queued_job() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    let id = seed(&tmp.join("lib"));
    ok(tmp, &["providers", "enable", "claude-cli", "--accept"], "");
    let queue = Queue::open(tmp.join("lib")).unwrap();

    let v = ok(
        tmp,
        &[
            "enrich",
            &id,
            "--queue",
            "--provider",
            "claude-cli",
            "--instructions-stdin",
        ],
        "Write the summary in Portuguese.\n",
    );
    let job = queue
        .get(&JobId::parse(v["job_id"].as_str().unwrap()).unwrap())
        .unwrap();
    assert_eq!(
        job.input["instructions"],
        "Write the summary in Portuguese."
    );

    let v = ok(
        tmp,
        &[
            "templates",
            "run",
            &id,
            "bant",
            "--queue",
            "--provider",
            "claude-cli",
            "--instructions-stdin",
        ],
        "Only the budget.",
    );
    assert_eq!(v["queued"], true, "{v}");
    let job = queue
        .get(&JobId::parse(v["job_id"].as_str().unwrap()).unwrap())
        .unwrap();
    assert_eq!(job.kind, TEMPLATE_KIND);
    assert_eq!(job.input["instructions"], "Only the budget.");
    assert_eq!(job.input["template_id"], "bant");

    // Explicitly empty: none for this run (not the saved default).
    let v = ok(
        tmp,
        &[
            "templates",
            "run",
            &id,
            "meeting-notes",
            "--queue",
            "--provider",
            "claude-cli",
            "--instructions-stdin",
        ],
        "",
    );
    let job = queue
        .get(&JobId::parse(v["job_id"].as_str().unwrap()).unwrap())
        .unwrap();
    assert_eq!(job.input["instructions"], "");
}

#[test]
fn template_definitions_come_from_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    seed(&tmp.join("lib"));

    let list = ok(tmp, &["templates", "list"], "");
    let builtins: Vec<&str> = list["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        builtins.len(),
        5,
        "meeting-notes, meeting-outline, follow-up-email, bant, meddic: {list}"
    );

    let recap = r#"{"name": "Weekly recap", "description": "Short recap", "body": "List what changed this week."}"#;
    let made = ok(tmp, &["templates", "new", "weekly-recap", "--stdin"], recap);
    assert_eq!(made["template"]["id"], "weekly-recap");
    assert_eq!(made["template"]["builtin"], false);
    assert_eq!(made["template"]["auto"], false);
    assert_eq!(made["template"]["body"], "List what changed this week.");
    assert_eq!(ok(tmp, &["templates", "show", "weekly-recap"], ""), made);
    assert_eq!(
        fails(tmp, &["templates", "new", "weekly-recap", "--stdin"], recap),
        "already_exists"
    );
    assert_eq!(
        fails(tmp, &["templates", "new", "bant", "--stdin"], recap),
        "already_exists",
        "a built-in is changed with edit"
    );
    for (id, input) in [
        ("Bad_Id", recap),
        ("no-body", r#"{"name": "No body"}"#),
        ("extra", r#"{"name": "x", "body": "y", "auto": true}"#),
        ("not-json", "List what changed."),
    ] {
        assert_eq!(
            fails(tmp, &["templates", "new", id, "--stdin"], input),
            "invalid_input",
            "{id}"
        );
    }

    // Editing a built-in saves an override; reset restores it.
    let original = ok(tmp, &["templates", "show", "bant"], "")["template"].clone();
    let edited = ok(
        tmp,
        &["templates", "edit", "bant", "--stdin"],
        r#"{"body": "Only the budget."}"#,
    );
    assert_eq!(edited["template"]["overridden"], true);
    assert_eq!(edited["template"]["body"], "Only the budget.");
    assert_eq!(edited["template"]["name"], original["name"]);

    // Automatic runs live in the config; reset leaves them.
    assert_eq!(
        ok(tmp, &["templates", "auto", "add", "bant"], ""),
        json!({ "auto": ["bant"] })
    );
    assert_eq!(
        ok(tmp, &["templates", "auto", "add", "weekly-recap"], ""),
        json!({ "auto": ["bant", "weekly-recap"] })
    );
    assert_eq!(
        fails(tmp, &["templates", "auto", "add", "nowhere"], ""),
        "not_found"
    );
    let reset = ok(tmp, &["templates", "reset", "bant"], "");
    assert_eq!(reset["template"]["overridden"], false);
    assert_eq!(reset["template"]["body"], original["body"]);
    assert_eq!(reset["template"]["auto"], true);
    assert_eq!(
        fails(tmp, &["templates", "reset", "weekly-recap"], ""),
        "invalid_input"
    );

    // Deleting a user template also takes it out of the automatic list;
    // a pure built-in cannot be deleted.
    assert_eq!(
        ok(tmp, &["templates", "delete", "weekly-recap"], ""),
        json!({ "deleted": "weekly-recap" })
    );
    assert_eq!(
        fails(tmp, &["templates", "show", "weekly-recap"], ""),
        "not_found"
    );
    assert_eq!(
        fails(tmp, &["templates", "delete", "meddic"], ""),
        "invalid_input"
    );
    assert_eq!(
        ok(tmp, &["templates", "auto", "remove", "bant"], ""),
        json!({ "auto": [] })
    );
}

/// Definitions and saved instructions live in the data directory: changing
/// them through a mistyped `--library` never creates that library.
#[test]
fn template_definitions_never_create_the_library() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp = tmp.path();
    let recap = r#"{"name": "Weekly recap", "body": "List what changed this week."}"#;
    ok(tmp, &["templates", "new", "weekly-recap", "--stdin"], recap);
    ok(
        tmp,
        &["templates", "edit", "weekly-recap", "--stdin"],
        r#"{"body": "Only what changed."}"#,
    );
    ok(tmp, &["templates", "auto", "add", "weekly-recap"], "");
    assert_eq!(
        ok(
            tmp,
            &["enrich", "instructions", "set", "--stdin"],
            "Focus on decisions."
        ),
        json!({ "instructions": "Focus on decisions." })
    );
    assert_eq!(
        ok(tmp, &["templates", "delete", "weekly-recap"], ""),
        json!({ "deleted": "weekly-recap" })
    );
    assert_eq!(
        broscribe_core::config::Config::load(&tmp.join("data"))
            .unwrap()
            .templates
            .auto,
        Vec::<String>::new(),
        "delete also leaves the automatic list"
    );
    assert!(
        !tmp.join("lib").exists(),
        "a definition change created the library"
    );
}
