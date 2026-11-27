//! `--query-stdin` for `search`, `meetings`, `actions` and `decisions`: the
//! query text and names arrive on stdin (the app's and the agent skill's
//! path, so they never appear in argv).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use broscribe_core::index::Index;
use broscribe_core::schema::{
    Action, ActionStatus, Decision, Enrichment, Meeting, ProcessingStatus, Segment, Source,
    Speaker, StatusSource, Track, TrackRole, enrichment_item_id,
};
use broscribe_core::{Library, SpeakerId};
use serde_json::Value;

/// One `done`, indexed, enriched meeting with two synthetic lines, one
/// action item and one decision.
fn seed(lib_dir: &Path) {
    let lib = Library::open(lib_dir).unwrap();
    let id = lib.new_meeting_id().unwrap();
    let started = chrono::DateTime::parse_from_rfc3339("2026-10-05T14:00:00-03:00").unwrap();
    let mut m = Meeting::new(id, Source::Import, started);
    m.title = Some("Pricing sync".into());
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
        (1, 0.0, "We should ship the pricing memo"),
        (2, 5.0, "Agreed the roadmap looks good"),
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
    m.enrichment = Some(Enrichment {
        provider: "fake".into(),
        model: None,
        requested_model: None,
        model_reported: false,
        created_at: started,
        stale: false,
        revision: broscribe_core::index::meeting_window_revision(&m),
        attribution_revision: String::new(),
        line_people: Default::default(),
        summary: "{{S1}} met {{S2}}.".into(),
        instructions: None,
        topics: vec!["pricing".into()],
        keywords: Vec::new(),
        tags: Vec::new(),
        decisions: vec![Decision {
            id: enrichment_item_id("Ship the memo", 5.0),
            text: "{{S2}} agreed to ship the memo".into(),
            at_s: 5.0,
            by_speaker: Some(SpeakerId::new(2).unwrap()),
            by_name: None,
        }],
        actions: vec![Action {
            id: enrichment_item_id("Write the memo", 0.0),
            text: "{{S1}} writes the memo".into(),
            owner_speaker: Some(SpeakerId::new(1).unwrap()),
            owner_name: None,
            due: None,
            at_s: 0.0,
            status: ActionStatus::Open,
            status_source: StatusSource::Llm,
            status_updated_at: None,
            original_owner: None,
            due_user_set: false,
            stale: false,
        }],
    });
    m.processing.status = ProcessingStatus::Done;
    lib.write(|tx| {
        tx.create_meeting_dir(&m)?;
        tx.save_meeting(&m)?;
        Index::open(tx.root())?.upsert_meeting(tx, &m)
    })
    .unwrap();
}

fn search(tmp: &Path, args: &[&str], stdin: &str) -> (bool, Value) {
    command(tmp, "search", args, stdin)
}

fn command(tmp: &Path, name: &str, args: &[&str], stdin: &str) -> (bool, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_broscribe"))
        .arg("--json")
        .arg("--library")
        .arg(tmp.join("lib"))
        .arg(name)
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

#[test]
fn search_reads_query_and_person_from_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    seed(&tmp.path().join("lib"));
    let (ok, v) = search(
        tmp.path(),
        &["--query-stdin", "--mode", "keyword"],
        r#"{"query": "pricing memo"}"#,
    );
    assert!(ok, "{v}");
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{v}");
    assert_eq!(results[0]["segment_id"], 0, "{v}");
    assert_eq!(v["query"], "pricing memo", "{v}");

    // Unknown keys are refused.
    let (ok, v) = search(
        tmp.path(),
        &["--query-stdin"],
        r#"{"query": "pricing", "extra": 1}"#,
    );
    assert!(!ok);
    assert_eq!(v["error"]["code"], "invalid_input", "{v}");

    // Neither the positional query nor --person may be combined with it.
    let (ok, _) = search(tmp.path(), &["--query-stdin", "pricing"], "{}");
    assert!(!ok);
    let (ok, _) = search(tmp.path(), &["--query-stdin", "--person", "x"], "{}");
    assert!(!ok);
}

#[test]
fn meetings_actions_and_decisions_read_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    seed(&tmp.path().join("lib"));
    let tmp = tmp.path();
    let count = |v: &Value, key: &str| v[key].as_array().unwrap_or_else(|| panic!("{v}")).len();

    let (ok, v) = command(
        tmp,
        "meetings",
        &["--query-stdin"],
        r#"{"query": "pricing"}"#,
    );
    assert!(ok, "{v}");
    assert_eq!(count(&v, "meetings"), 1);
    assert_eq!(v["meetings"][0]["title"], "Pricing sync");
    let (ok, v) = command(
        tmp,
        "meetings",
        &["--query-stdin"],
        r#"{"query": "picnic"}"#,
    );
    assert!(ok, "{v}");
    assert_eq!(count(&v, "meetings"), 0);
    let (ok, v) = command(tmp, "meetings", &["--query-stdin"], "{}");
    assert!(ok, "{v}");
    assert_eq!(count(&v, "meetings"), 1, "no filters: the recent meetings");

    let (ok, v) = command(tmp, "actions", &["--query-stdin"], r#"{"query": "memo"}"#);
    assert!(ok, "{v}");
    assert_eq!(count(&v, "actions"), 1);
    let (ok, v) = command(tmp, "actions", &["--query-stdin"], r#"{"query": "picnic"}"#);
    assert!(ok, "{v}");
    assert_eq!(count(&v, "actions"), 0);
    // The owner comes from stdin too, and is resolved like --owner.
    let (ok, v) = command(
        tmp,
        "actions",
        &["--query-stdin"],
        r#"{"owner": "Nobody Invented"}"#,
    );
    assert!(!ok);
    assert_eq!(v["error"]["code"], "not_found", "{v}");

    let (ok, v) = command(tmp, "decisions", &["--query-stdin"], r#"{"query": "ship"}"#);
    assert!(ok, "{v}");
    assert_eq!(count(&v, "decisions"), 1);
    let (ok, v) = command(
        tmp,
        "decisions",
        &["--query-stdin"],
        r#"{"query": "picnic"}"#,
    );
    assert!(ok, "{v}");
    assert_eq!(count(&v, "decisions"), 0);

    // Unknown fields are refused: `person` is not a decisions field, `owner`
    // not a meetings field.
    for (name, input) in [
        ("meetings", r#"{"owner": "x"}"#),
        ("actions", r#"{"person": "x"}"#),
        ("decisions", r#"{"owner": "x"}"#),
    ] {
        let (ok, v) = command(tmp, name, &["--query-stdin"], input);
        assert!(!ok, "{name}");
        assert_eq!(v["error"]["code"], "invalid_input", "{name}: {v}");
    }
    // The argv forms conflict with --query-stdin.
    for (name, args) in [
        ("meetings", &["--query-stdin", "pricing"][..]),
        ("meetings", &["--query-stdin", "--person", "x"][..]),
        ("actions", &["--query-stdin", "--query", "x"][..]),
        ("actions", &["--query-stdin", "--owner", "me"][..]),
        ("decisions", &["--query-stdin", "--query", "x"][..]),
    ] {
        let (ok, _) = command(tmp, name, args, "{}");
        assert!(!ok, "{name} {args:?}");
    }
}

#[test]
fn transcript_find_reads_literal_text_from_stdin() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("lib");
    seed(&root);
    let lib = Library::open_existing(&root).unwrap();
    let mut meeting = lib
        .load_meeting_at(&lib.list_meeting_dirs().unwrap()[0])
        .unwrap();
    meeting.segments[1].text = "Draft the café notes.".into();
    lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
    let args = [meeting.id.as_str(), "--find-stdin"];
    let (ok, report) = command(temp.path(), "transcript", &args, "CAFE");
    assert!(ok, "{report}");
    assert_eq!(report["matches"][0]["segment_ids"], serde_json::json!([1]));
    assert_eq!(report["matches"][0]["start"], 5.0);
    assert_eq!(report["matches"][0]["text"], "Draft the café notes.");
    let (ok, report) = command(temp.path(), "transcript", &args, "MISSING");
    assert!(ok);
    assert_eq!(report["matches"], serde_json::json!([]));
}

/// `transcript attribute --fingerprints-stdin`: the app's view of the lines
/// and the person arrive on stdin (no name in argv); a fingerprint the
/// transcript no longer has is refused with nothing written.
#[test]
fn transcript_attribute_reads_fingerprints_from_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    seed(&tmp.path().join("lib"));
    let tmp = tmp.path();
    let lib = Library::open_existing(tmp.join("lib")).unwrap();
    let load = || {
        lib.load_meeting_at(&lib.list_meeting_dirs().unwrap()[0])
            .unwrap()
    };
    let m = load();
    let id = m.id.as_str().to_owned();
    let fingerprint =
        serde_json::to_value(broscribe_core::ops::segment_fingerprint(&m.segments[1])).unwrap();
    let args = ["attribute", id.as_str(), "--fingerprints-stdin"];
    let input = |fp: &Value, person: Option<&str>| {
        let mut v = serde_json::json!({ "fingerprints": [fp] });
        if let Some(p) = person {
            v["person"] = Value::from(p);
        }
        v.to_string()
    };

    // Stale: the text changed since the caller read it.
    let mut stale = fingerprint.clone();
    stale["text_sha256"] = Value::from("0".repeat(64));
    let (ok, v) = command(
        tmp,
        "transcript",
        &args,
        &input(&stale, Some("Ivo Invented")),
    );
    assert!(!ok);
    assert_eq!(v["error"]["code"], "invalid_input", "{v}");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("reload"),
        "{v}"
    );
    assert!(load().utterance_people.is_empty(), "nothing written");

    // Unknown fields are refused; exactly one of stdin `person` or --clear.
    let with_clear = ["attribute", id.as_str(), "--fingerprints-stdin", "--clear"];
    for (argv, stdin) in [
        (
            &args[..],
            format!(r#"{{"fingerprints":[{fingerprint}],"person":"Ivo Invented","extra":1}}"#),
        ),
        (
            &args[..],
            r#"{"fingerprints":[{"track":"mixed","start":5.0,"end":9.0,"text_sha256":"x","id":1}],"person":"Ivo Invented"}"#
                .to_owned(),
        ),
        (&args[..], input(&fingerprint, None)),
        (&with_clear[..], input(&fingerprint, Some("Ivo Invented"))),
    ] {
        let (ok, v) = command(tmp, "transcript", argv, &stdin);
        assert!(!ok, "{argv:?} {stdin}");
        assert_eq!(v["error"]["code"], "invalid_input", "{stdin}: {v}");
    }
    // --segment and --person conflict with stdin (usage errors).
    for argv in [
        &[
            "attribute",
            id.as_str(),
            "--fingerprints-stdin",
            "--segment",
            "1",
            "--clear",
        ][..],
        &[
            "attribute",
            id.as_str(),
            "--fingerprints-stdin",
            "--person",
            "Ivo Invented",
        ][..],
    ] {
        let (ok, _) = command(
            tmp,
            "transcript",
            argv,
            &input(&fingerprint, Some("Ivo Invented")),
        );
        assert!(!ok, "{argv:?}");
    }
    assert!(load().utterance_people.is_empty());

    let (ok, v) = command(
        tmp,
        "transcript",
        &args,
        &input(&fingerprint, Some("Ivo Invented")),
    );
    assert!(ok, "{v}");
    assert_eq!(v["meeting_id"], id.as_str());
    assert_eq!(v["segments"], serde_json::json!([1]));
    let person_id = v["person_id"].as_str().unwrap().to_owned();
    let (ok, t) = command(tmp, "transcript", &[id.as_str()], "");
    assert!(ok, "{t}");
    assert_eq!(t["segments"][1]["attribution"], "line");
    assert_eq!(t["segments"][1]["person_id"], person_id.as_str());
    assert_eq!(t["segments"][1]["person_name"], "Ivo Invented");
    assert_eq!(t["segments"][0]["attribution"], "speaker");

    // --clear with only fingerprints on stdin removes the correction.
    let (ok, v) = command(tmp, "transcript", &with_clear, &input(&fingerprint, None));
    assert!(ok, "{v}");
    assert_eq!(v["person_id"], Value::Null);
    assert!(load().utterance_people.is_empty());
}

/// `"fence"` on stdin: a line named from a view taken before the speakers
/// were regrouped is refused (`speakers_regrouped`) with nothing written.
#[test]
fn transcript_attribute_refuses_a_regrouped_view() {
    let tmp = tempfile::tempdir().unwrap();
    seed(&tmp.path().join("lib"));
    let tmp = tmp.path();
    let lib = Library::open_existing(tmp.join("lib")).unwrap();
    let dir = lib.list_meeting_dirs().unwrap()[0].clone();
    let seen = lib.load_meeting_at(&dir).unwrap();
    let id = seen.id.as_str().to_owned();
    let input = serde_json::json!({
        "fingerprints": [broscribe_core::ops::segment_fingerprint(&seen.segments[1])],
        "person": "Ivo Invented",
        "fence": broscribe_core::ops::SpeakerFence::of(&seen),
    })
    .to_string();
    // Merged elsewhere after the view was read.
    lib.write(|tx| {
        let mut m = tx.library().load_meeting(&seen.id)?;
        m.speaker_groups
            .push(broscribe_core::schema::SpeakerGroup::new(
                SpeakerId::new(1).unwrap(),
                [SpeakerId::new(2).unwrap()],
                broscribe_core::schema::GroupOrigin::User,
            ));
        tx.save_meeting(&m)
    })
    .unwrap();
    let args = ["attribute", id.as_str(), "--fingerprints-stdin"];
    let (ok, v) = command(tmp, "transcript", &args, &input);
    assert!(!ok);
    assert_eq!(v["error"]["code"], "speakers_regrouped", "{v}");
    let after = lib.load_meeting_at(&dir).unwrap();
    assert!(after.utterance_people.is_empty(), "nothing written");
    assert!(lib.list_people().unwrap().is_empty(), "no person created");

    // The current view goes through.
    let mut input: Value = serde_json::from_str(&input).unwrap();
    input["fence"] = serde_json::to_value(broscribe_core::ops::SpeakerFence::of(&after)).unwrap();
    let (ok, v) = command(tmp, "transcript", &args, &input.to_string());
    assert!(ok, "{v}");
    assert_eq!(v["segments"], serde_json::json!([1]));
}
