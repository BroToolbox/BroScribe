//! CLI ↔ MCP parity on a synthetic library: each MCP read tool and its CLI
//! command, given the same filters and an explicit limit, return the same
//! rows in the same order (compared by projection; each side keeps its own
//! JSON shape). Also the deliberate differences: the CLI `search` guard and
//! the default limits.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use broscribe_cli::mcp::serve_on;
use broscribe_cli::{AgentEnv, Deps, WallClock, run};
use broscribe_core::config::Config;
use broscribe_core::index::Index;
use broscribe_core::schema::{
    Action, ActionStatus, Centroid, Decision, Enrichment, Meeting, ProcessingStatus, Segment,
    Source, Speaker, StatusSource, TemplateResult, Track, TrackRole, enrichment_item_id,
};
use broscribe_core::{Library, MeetingId, SpeakerId};
use broscribe_models::fakes::FakeModelProvider;
use broscribe_platform::DecoderChain;
use broscribe_platform::fakes::{
    FakeAutostart, FakeBrowserLauncher, FakeMicCapture, FakePaths, FakeProcessProbe,
    FakeProcessTreeControl, FakeSecretStore, FakeTrustStore,
};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

/// More meetings than the default limit of 20.
const MEETINGS: usize = 23;

fn deps(root: &Path, clock: WallClock) -> Deps {
    Deps {
        clock,
        paths: Box::new(FakePaths::new(root)),
        updates: broscribe_cli::UpdateDeps::with_transport(
            std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                std::path::Path::new(&root).join("install"),
                "synthetic-test-target",
            )),
            std::sync::Arc::new(broscribe_updates::FakeTransport::default()),
        ),
        models: Box::new(FakeModelProvider::default()),
        decoder: DecoderChain::default(),
        process: Box::new(FakeProcessProbe::new(1)),
        mic: Box::new(FakeMicCapture::new(16_000)),
        secrets: std::sync::Arc::new(FakeSecretStore::default()),
        browser: std::sync::Arc::new(FakeBrowserLauncher::default()),
        agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
        autostart: Box::new(FakeAutostart::default()),
        agents: AgentEnv::isolated(root),
        // Synthetic containment and a trust store without roots: nothing
        // here starts a model process or makes an HTTPS request.
        execution: broscribe_models::ExecutionContext::new(
            std::sync::Arc::new(FakeProcessTreeControl::default()),
            std::sync::Arc::new(FakeTrustStore::default()),
        ),
    }
}

/// `MEETINGS` indexed, enriched meetings a day apart: every third has the
/// topic `pricing`, every other the tag `weekly`; each has two utterances,
/// one action (open or done) and one decision.
fn seed(lib_dir: &Path) -> Vec<MeetingId> {
    let lib = Library::open(lib_dir).unwrap();
    let model = Config::default().profile.speaker_embedding;
    let first = chrono::DateTime::parse_from_rfc3339("2026-09-01T10:00:00-03:00").unwrap();
    let mut ids = Vec::new();
    for i in 0..MEETINGS {
        let id = lib.new_meeting_id().unwrap();
        let started = first + chrono::Duration::days(i as i64);
        let mut m = Meeting::new(id.clone(), Source::Import, started);
        m.title = Some(if i % 3 == 0 {
            format!("Pricing review {i}")
        } else {
            format!("Team sync {i}")
        });
        m.duration_s = 20.0;
        if i % 2 == 0 {
            m.tags = vec!["weekly".into()];
        }
        m.tracks = vec![Track {
            id: "mixed".into(),
            role: TrackRole::Mixed,
        }];
        for n in 1..=2u32 {
            let mut s = Speaker::new(SpeakerId::new(n).unwrap(), "mixed");
            s.talk_s = 4.0;
            let hot: Vec<f32> = (0..16)
                .map(|k| if k == n as usize { 1.0 } else { 0.0 })
                .collect();
            s.centroid = Some(Centroid::new(model.clone(), &hot, 3));
            m.speakers.push(s);
        }
        let lines = [
            (
                1,
                0.0,
                4.0,
                format!("We should ship the pricing memo number {i}"),
            ),
            (
                2,
                5.0,
                9.0,
                format!("Agreed the roadmap looks good for item {i}"),
            ),
        ];
        for (k, (spk, start, end, text)) in lines.into_iter().enumerate() {
            m.segments.push(Segment {
                speech_overlap: Vec::new(),
                id: k as u32,
                track: "mixed".into(),
                speaker: Some(SpeakerId::new(spk).unwrap()),
                start,
                end,
                text,
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
            summary: "{{S1}} and {{S2}} met.".into(),
            instructions: None,
            topics: if i % 3 == 0 {
                vec!["pricing".into()]
            } else {
                vec!["planning".into()]
            },
            keywords: Vec::new(),
            tags: Vec::new(),
            decisions: vec![Decision {
                id: enrichment_item_id(&format!("Ship memo {i}"), 5.0),
                text: format!("{{{{S2}}}} agreed to ship memo {i}"),
                at_s: 5.0,
                by_speaker: Some(SpeakerId::new(2).unwrap()),
                by_name: None,
            }],
            actions: vec![Action {
                id: enrichment_item_id(&format!("Write memo {i}"), 0.0),
                text: format!("{{{{S1}}}} writes memo {i}"),
                owner_speaker: Some(SpeakerId::new(1).unwrap()),
                owner_name: None,
                due: None,
                at_s: 0.0,
                status: if i % 2 == 0 {
                    ActionStatus::Open
                } else {
                    ActionStatus::Done
                },
                status_source: StatusSource::Llm,
                status_updated_at: None,
                original_owner: None,
                due_user_set: false,
                stale: false,
            }],
        });
        // Model provenance: the vendor reported one (1), only a requested
        // alias is known (2), or the provider default without a report.
        let e = m.enrichment.as_mut().unwrap();
        match i {
            1 => {
                e.model = Some("fake-model-2026".into());
                e.model_reported = true;
            }
            2 => {
                e.model = Some("alias".into());
                e.requested_model = Some("alias".into());
            }
            _ => {}
        }
        // Template results: `bant` on every fourth meeting; every eighth
        // also has one of a template that no longer exists.
        let result = |template_id: &str, template_name: &str, markdown: String| TemplateResult {
            template_id: template_id.into(),
            template_name: template_name.into(),
            instructions_sha256: "0".repeat(64),
            provider: "fake".into(),
            model: None,
            requested_model: None,
            model_reported: false,
            created_at: started,
            revision: broscribe_core::index::meeting_window_revision(&m),
            attribution_revision: String::new(),
            line_people: Default::default(),
            stale: false,
            markdown,
        };
        let mut results = Vec::new();
        if i % 4 == 0 {
            results.push(result(
                "bant",
                "Budget check",
                format!("## Budget\n\n{{{{S1}}}} holds the budget for memo {i}."),
            ));
        }
        if i % 8 == 0 {
            results.push(result(
                "retired",
                "Old recap",
                format!("## Recap\n\nBudget recap {i} by {{{{S2}}}}."),
            ));
        }
        m.template_results = results;
        m.processing.status = ProcessingStatus::Done;
        lib.write(|tx| {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)?;
            Index::open(tx.root())?.upsert_meeting(tx, &m)
        })
        .unwrap();
        ids.push(id);
    }
    ids
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    deps: Deps,
    ids: Vec<MeetingId>,
    /// What the CLI and MCP clock reads, in Unix seconds.
    now: Arc<AtomicI64>,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let ids = seed(&root.join("library"));
        let now = Arc::new(AtomicI64::new(
            chrono::DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
                .unwrap()
                .timestamp(),
        ));
        let reads = Arc::clone(&now);
        let clock = WallClock::custom(Arc::new(move || {
            chrono::DateTime::from_timestamp(reads.load(Ordering::SeqCst), 0).unwrap()
        }));
        Self {
            deps: deps(&root, clock),
            _tmp: tmp,
            root,
            ids,
            now,
        }
    }

    /// Sets the clock both front ends read.
    fn set_now(&self, t: chrono::DateTime<chrono::Utc>) {
        self.now.store(t.timestamp(), Ordering::SeqCst);
    }

    fn lib(&self) -> PathBuf {
        self.root.join("library")
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    fn cli(&self, args: &[&str]) -> (i32, Value) {
        let mut argv: Vec<OsString> = vec![
            "broscribe".into(),
            "--json".into(),
            "--library".into(),
            self.lib().into(),
        ];
        argv.extend(args.iter().map(OsString::from));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv, &self.deps, &mut out, &mut err);
        let text = String::from_utf8(out).unwrap();
        (code, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn cli_ok(&self, args: &[&str]) -> Value {
        let (code, v) = self.cli(args);
        assert_eq!(code, 0, "{args:?}: {v}");
        v
    }

    /// Human (non-JSON) stdout of a successful command.
    fn cli_text(&self, args: &[&str]) -> String {
        let mut argv: Vec<OsString> =
            vec!["broscribe".into(), "--library".into(), self.lib().into()];
        argv.extend(args.iter().map(OsString::from));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv, &self.deps, &mut out, &mut err);
        assert_eq!(code, 0, "{args:?}: {}", String::from_utf8_lossy(&err));
        String::from_utf8(out).unwrap()
    }

    /// Runs every `(tool, arguments)` call against the in-process MCP server
    /// and returns the structured results in order.
    fn mcp(&self, calls: Vec<(&'static str, Value)>) -> Vec<Value> {
        self.mcp_results(calls)
            .into_iter()
            .map(|(failed, v)| {
                assert!(!failed, "{v}");
                v
            })
            .collect()
    }

    /// [`Self::mcp`], keeping failures: `(is_error, structured content)`.
    fn mcp_results(&self, calls: Vec<(&'static str, Value)>) -> Vec<(bool, Value)> {
        Config::update(&self.data_dir(), |config| {
            config.mcp.enabled = Some(true);
            Ok(())
        })
        .unwrap();
        let (server_io, client_io) = tokio::io::duplex(1 << 20);
        let client = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let client = ().serve(client_io).await.unwrap();
                    let mut results = Vec::new();
                    for (tool, args) in calls {
                        let r = client
                            .call_tool(
                                CallToolRequestParams::new(tool)
                                    .with_arguments(args.as_object().cloned().unwrap()),
                            )
                            .await
                            .unwrap();
                        results.push((
                            r.is_error == Some(true),
                            r.structured_content
                                .unwrap_or_else(|| panic!("{tool}: {:?}", r.content)),
                        ));
                    }
                    client.cancel().await.unwrap();
                    results
                })
        });
        let (reader, writer) = tokio::io::split(server_io);
        serve_on(&self.deps, self.lib(), self.data_dir(), reader, writer).unwrap();
        client.join().unwrap()
    }

    /// The error code of one MCP call that must fail.
    fn mcp_error(&self, tool: &'static str, args: Value) -> String {
        let (failed, v) = self.mcp_results(vec![(tool, args)]).remove(0);
        assert!(failed, "{tool} should fail: {v}");
        v["error"]["code"].as_str().unwrap().to_owned()
    }

    /// The error code of one CLI command that must fail.
    fn cli_error(&self, args: &[&str]) -> String {
        let (code, v) = self.cli(args);
        assert_eq!(code, 1, "{args:?}: {v}");
        v["error"]["code"].as_str().unwrap().to_owned()
    }
}

/// Every row of a paged read under `key`, page by page (`read` gets the
/// previous page's cursor) until the page without `next_cursor`. Each page
/// carries the index revision it was read at, the same for all of them on
/// an unchanged library, and only the last page may be short.
fn pages(key: &str, size: usize, mut read: impl FnMut(Option<&str>) -> Value) -> Vec<Value> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    let mut revision: Option<Value> = None;
    loop {
        let page = read(cursor.as_deref());
        let got = rows(&page, key);
        assert!(page["revision"]["instance"].is_string(), "{page}");
        match &revision {
            Some(r) => assert_eq!(&page["revision"], r, "{page}"),
            None => revision = Some(page["revision"].clone()),
        }
        all.extend(got.iter().cloned());
        assert!(all.len() <= 500, "paging ends");
        match page.get("next_cursor").and_then(Value::as_str) {
            Some(next) => {
                assert_eq!(got.len(), size, "only the last page is short: {page}");
                cursor = Some(next.to_owned());
            }
            None => {
                assert!(got.len() <= size, "{page}");
                return all;
            }
        }
    }
}

fn rows<'a>(v: &'a Value, key: &str) -> &'a Vec<Value> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("no {key} array: {v}"))
}

fn meeting_rows(v: &Value, key: &str) -> Vec<String> {
    rows(v, key)
        .iter()
        .map(|r| r["meeting_id"].as_str().unwrap().to_owned())
        .collect()
}

fn utterance_rows(v: &Value) -> Vec<(String, String, String, String)> {
    rows(v, "results")
        .iter()
        .map(|r| {
            (
                r["meeting_id"].as_str().unwrap().to_owned(),
                r["speaker_id"].as_str().unwrap().to_owned(),
                r["start"].to_string(),
                r["snippet"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn action_rows(v: &Value) -> Vec<(String, String, String)> {
    rows(v, "actions")
        .iter()
        .map(|r| {
            (
                r["meeting_id"].as_str().unwrap().to_owned(),
                r["action_id"].as_str().unwrap().to_owned(),
                r["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn decision_rows(v: &Value) -> Vec<(String, String)> {
    rows(v, "decisions")
        .iter()
        .map(|r| {
            (
                r["meeting_id"].as_str().unwrap().to_owned(),
                r["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn cli_and_mcp_read_the_same_rows() {
    let f = Fixture::new();
    let one = f.ids[3].as_str().to_owned();
    let mcp = f.mcp(vec![
        ("search_meetings", json!({ "limit": 50 })),
        ("search_meetings", json!({ "query": "pricing", "limit": 5 })),
        (
            "search_meetings",
            json!({ "tag": "weekly", "topic": "pricing", "since": "2026-09-05", "limit": 50 }),
        ),
        (
            "search_utterances",
            json!({ "query": "memo", "mode": "keyword", "limit": 7 }),
        ),
        (
            "search_utterances",
            json!({ "meeting_id": one, "limit": 50 }),
        ),
        (
            "search_utterances",
            json!({ "query": "roadmap", "mode": "keyword", "topic": "pricing", "until": "2026-09-20", "limit": 50 }),
        ),
        ("list_action_items", json!({ "status": "open", "limit": 50 })),
        (
            "list_action_items",
            json!({ "meeting_id": one, "query": "memo", "limit": 50 }),
        ),
        ("list_action_items", json!({ "since": "2026-09-10", "limit": 4 })),
        ("list_decisions", json!({ "topic": "pricing", "limit": 50 })),
        (
            "list_decisions",
            json!({ "meeting_id": one, "query": "ship", "limit": 50 }),
        ),
        ("list_decisions", json!({ "query": "agreed", "limit": 9 })),
    ]);

    let all = f.cli_ok(&["meetings", "--limit", "50"]);
    assert_eq!(
        meeting_rows(&all, "meetings"),
        meeting_rows(&mcp[0], "results")
    );
    assert_eq!(meeting_rows(&all, "meetings").len(), MEETINGS);
    let pricing = f.cli_ok(&["meetings", "pricing", "--limit", "5"]);
    assert_eq!(
        meeting_rows(&pricing, "meetings"),
        meeting_rows(&mcp[1], "results")
    );
    assert_eq!(meeting_rows(&pricing, "meetings").len(), 5);
    let filtered = f.cli_ok(&[
        "meetings",
        "--tag",
        "weekly",
        "--topic",
        "pricing",
        "--since",
        "2026-09-05",
        "--limit",
        "50",
    ]);
    assert_eq!(
        meeting_rows(&filtered, "meetings"),
        meeting_rows(&mcp[2], "results")
    );
    assert!(!meeting_rows(&filtered, "meetings").is_empty());

    let memo = f.cli_ok(&["search", "memo", "--mode", "keyword", "--limit", "7"]);
    assert_eq!(utterance_rows(&memo), utterance_rows(&mcp[3]));
    assert_eq!(utterance_rows(&memo).len(), 7);
    let in_meeting = f.cli_ok(&["search", "--meeting", &one, "--limit", "50"]);
    assert_eq!(utterance_rows(&in_meeting), utterance_rows(&mcp[4]));
    assert_eq!(utterance_rows(&in_meeting).len(), 2);
    assert!(utterance_rows(&in_meeting).iter().all(|r| r.0 == one));
    let roadmap = f.cli_ok(&[
        "search",
        "roadmap",
        "--mode",
        "keyword",
        "--topic",
        "pricing",
        "--until",
        "2026-09-20",
        "--limit",
        "50",
    ]);
    assert_eq!(utterance_rows(&roadmap), utterance_rows(&mcp[5]));
    assert!(!utterance_rows(&roadmap).is_empty());

    let open = f.cli_ok(&["actions", "--status", "open", "--limit", "50"]);
    assert_eq!(action_rows(&open), action_rows(&mcp[6]));
    assert_eq!(action_rows(&open).len(), MEETINGS.div_ceil(2));
    let in_meeting = f.cli_ok(&[
        "actions",
        "--meeting",
        &one,
        "--query",
        "memo",
        "--limit",
        "50",
    ]);
    assert_eq!(action_rows(&in_meeting), action_rows(&mcp[7]));
    assert_eq!(action_rows(&in_meeting).len(), 1);
    let recent = f.cli_ok(&["actions", "--since", "2026-09-10", "--limit", "4"]);
    assert_eq!(action_rows(&recent), action_rows(&mcp[8]));
    assert_eq!(action_rows(&recent).len(), 4);

    let topic = f.cli_ok(&["decisions", "--topic", "pricing", "--limit", "50"]);
    assert_eq!(decision_rows(&topic), decision_rows(&mcp[9]));
    assert_eq!(decision_rows(&topic).len(), MEETINGS.div_ceil(3));
    let in_meeting = f.cli_ok(&[
        "decisions",
        "--meeting",
        &one,
        "--query",
        "ship",
        "--limit",
        "50",
    ]);
    assert_eq!(decision_rows(&in_meeting), decision_rows(&mcp[10]));
    assert_eq!(decision_rows(&in_meeting).len(), 1);
    let text = f.cli_ok(&["decisions", "--query", "agreed", "--limit", "9"]);
    assert_eq!(decision_rows(&text), decision_rows(&mcp[11]));
    assert!(!decision_rows(&text).is_empty());

    // Each side keeps its own shape: the CLI has `query` and `timestamp`,
    // MCP `at` and `transcript_uri`; CLI meeting rows have no URI.
    assert_eq!(memo["query"], "memo");
    assert!(rows(&memo, "results")[0]["timestamp"].is_string());
    assert!(rows(&memo, "results")[0].get("transcript_uri").is_none());
    assert!(rows(&mcp[3], "results")[0]["at"].is_string());
    assert!(rows(&mcp[3], "results")[0]["transcript_uri"].is_string());
    assert!(rows(&all, "meetings")[0].get("transcript_uri").is_none());
    assert!(rows(&mcp[0], "results")[0]["transcript_uri"].is_string());
}

#[test]
fn show_and_get_meeting_project_the_same_enrichment() {
    let f = Fixture::new();
    // A meeting.json written before requested/reported models existed.
    let legacy = f.ids[0].as_str().to_owned();
    let path = f.cli_ok(&["show", &legacy])["path"]
        .as_str()
        .map(|d| Path::new(d).join("meeting.json"))
        .unwrap();
    let mut file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let e = file["enrichment"].as_object_mut().unwrap();
    e.remove("model_reported");
    e.remove("requested_model");
    e.insert("model".into(), "old-alias".into());
    std::fs::write(&path, serde_json::to_string_pretty(&file).unwrap()).unwrap();

    let ids: Vec<String> = f.ids[..3].iter().map(|id| id.as_str().to_owned()).collect();
    let mcp = f.mcp(
        ids.iter()
            .map(|id| ("get_meeting", json!({ "meeting_id": id })))
            .collect(),
    );
    let expected = [
        (json!("old-alias"), Value::Null, false),
        (json!("fake-model-2026"), Value::Null, true),
        (json!("alias"), json!("alias"), false),
    ];
    for (i, ((id, meeting), (model, requested, reported))) in
        ids.iter().zip(&mcp).zip(expected).enumerate()
    {
        let shown = f.cli_ok(&["show", id]);
        assert_eq!(shown["enrichment"], meeting["enrichment"], "{id}");
        let e = &shown["enrichment"];
        assert_eq!(e["provider"], "fake");
        assert_eq!(
            e["topics"],
            json!([if i == 0 { "pricing" } else { "planning" }])
        );
        assert_eq!(
            (&e["model"], &e["requested_model"], &e["model_reported"]),
            (&model, &requested, &json!(reported)),
            "{id}"
        );
        assert_eq!(e["stale"], false);
        assert!(e["created_at"].is_string());
    }
}

#[test]
fn speakers_matches_and_speaker_voice_matches_agree() {
    let f = Fixture::new();
    let model = Config::default().profile.speaker_embedding;
    std::fs::create_dir_all(f.data_dir()).unwrap();
    std::fs::write(
        f.data_dir().join("config.toml"),
        format!("[identity.{model:?}]\nauto_name = 0.8\nsuggest = 0.5\nmargin = 0.1\n"),
    )
    .unwrap();
    let first = f.ids[0].as_str().to_owned();
    let other = f.ids[1].as_str().to_owned();
    f.cli_ok(&["speakers", "name", &first, "S1", "Ana Example"]);
    f.cli_ok(&["speakers", "name", f.ids[2].as_str(), "S2", "Bia Example"]);

    let calls = [
        (&other, "S1", Some(2)),
        (&other, "S2", None),
        (&first, "S1", None),
    ];
    let mcp = f.mcp(
        calls
            .iter()
            .map(|(meeting, speaker, limit)| {
                let mut args = json!({ "meeting_id": meeting, "speaker_id": speaker });
                if let Some(n) = limit {
                    args["limit"] = json!(n);
                }
                ("speaker_voice_matches", args)
            })
            .collect(),
    );
    for ((meeting, speaker, limit), from_mcp) in calls.iter().zip(&mcp) {
        let limit = limit.map(|n: u32| n.to_string());
        let mut args = vec!["speakers", "matches", meeting.as_str(), speaker];
        if let Some(n) = &limit {
            args.extend(["--limit", n.as_str()]);
        }
        assert_eq!(&f.cli_ok(&args), from_mcp, "{args:?}");
    }

    let row = |v: &Value| {
        (
            v["name"].as_str().unwrap().to_owned(),
            v["likely"].as_bool().unwrap(),
            v["current_suggestion"].as_bool().unwrap(),
        )
    };
    let unnamed = &mcp[0];
    assert_eq!(
        (
            &unnamed["meeting_id"],
            &unnamed["speaker_id"],
            &unnamed["model"]
        ),
        (&json!(other), &json!("S1"), &json!(model))
    );
    let matches = rows(unnamed, "matches");
    assert_eq!(
        matches.iter().map(row).collect::<Vec<_>>(),
        [
            ("Ana Example".to_owned(), true, false),
            ("Bia Example".to_owned(), false, false)
        ]
    );
    assert!((matches[0]["score"].as_f64().unwrap() - 1.0).abs() < 1e-5);
    assert_eq!(rows(&mcp[1], "matches")[0]["name"], "Bia Example");
    // A confirmed speaker is compared without this meeting's samples: Ana's
    // only sample came from S1 itself, Bia's from another meeting.
    let confirmed = rows(&mcp[2], "matches");
    assert_eq!(
        confirmed.iter().map(row).collect::<Vec<_>>(),
        [("Bia Example".to_owned(), false, false)]
    );

    for bad in ["0", "11"] {
        let (code, v) = f.cli(&["speakers", "matches", &other, "S1", "--limit", bad]);
        assert_eq!(code, 1, "{v}");
        assert_eq!(v["error"]["code"], "invalid_input");
    }
    let (code, v) = f.cli(&["speakers", "matches", &other, "S9"]);
    assert_eq!((code, &v["error"]["code"]), (1, &json!("not_found")));
}

/// Adds `S3` to `meeting`: 2 s of talk, a voice `similarity` away from S1's
/// (one-hot on 1), and one line at 12 s.
fn add_split_speaker(lib_dir: &Path, meeting: &MeetingId, similarity: f32) {
    let lib = Library::open(lib_dir).unwrap();
    let model = Config::default().profile.speaker_embedding;
    let mut voice = vec![0.0f32; 16];
    voice[1] = similarity;
    voice[3] = (1.0 - similarity * similarity).sqrt();
    lib.write(|tx| {
        let mut m = tx.library().load_meeting(meeting)?;
        let mut s = Speaker::new(SpeakerId::new(3).unwrap(), "mixed");
        s.talk_s = 2.0;
        s.centroid = Some(Centroid::new(model.clone(), &voice, 3));
        m.speakers.push(s);
        m.segments.push(Segment {
            speech_overlap: Vec::new(),
            id: 2,
            track: "mixed".into(),
            speaker: Some(SpeakerId::new(3).unwrap()),
            start: 12.0,
            end: 14.0,
            text: "And the memo goes out on Friday".into(),
            words: Vec::new(),
        });
        tx.save_meeting(&m)?;
        Index::open(tx.root())?.upsert_meeting(tx, &m)
    })
    .unwrap();
}

/// `(segment, speaker shown, diarized speaker when different)` of the CLI
/// transcript and of MCP `get_transcript`.
fn shown_speakers(cli: &Value, mcp: &Value) -> (Vec<Value>, Vec<Value>) {
    let cli = rows(cli, "segments")
        .iter()
        .map(|s| json!([s["id"], s["speaker"], s["original_speaker"]]))
        .collect();
    let mcp = rows(mcp, "utterances")
        .iter()
        .map(|u| json!([u["segment_id"], u["speaker_id"], u["original_speaker_id"]]))
        .collect();
    (cli, mcp)
}

#[test]
fn speaker_merging_reads_the_same_on_cli_and_mcp() {
    let f = Fixture::new();
    let model = Config::default().profile.speaker_embedding;
    std::fs::create_dir_all(f.data_dir()).unwrap();
    std::fs::write(
        f.data_dir().join("config.toml"),
        format!("[identity.{model:?}]\nmerge = 0.5\nauto_merge = 0.95\n"),
    )
    .unwrap();
    let split = f.ids[0].as_str().to_owned();
    add_split_speaker(&f.lib(), &f.ids[0], 0.9);

    // Suggested, not merged: 0.9 is in [merge, auto_merge).
    let duplicates = f.cli_ok(&["speakers", "duplicates", &split]);
    let mcp = f.mcp(vec![(
        "speaker_duplicates",
        json!({ "meeting_id": split.as_str() }),
    )]);
    assert_eq!(duplicates, mcp[0]);
    assert_eq!(duplicates["merge"], json!(0.5));
    let pairs = rows(&duplicates, "duplicates");
    assert_eq!(pairs.len(), 1, "{duplicates}");
    assert_eq!(
        (&pairs[0]["a"], &pairs[0]["b"]),
        (&json!("S1"), &json!("S3"))
    );
    assert_eq!(pairs[0]["b_name"], "Speaker 3");
    assert!((pairs[0]["score"].as_f64().unwrap() - 0.9).abs() < 1e-4);

    // Merging into S3 keeps S1 (more talk time) and says so.
    let merged = f.cli_ok(&["speakers", "merge", &split, "S3", "S1"]);
    assert_eq!(merged["group"]["id"], "G1");
    assert_eq!(merged["group"]["kept"], "S1");
    assert_eq!(merged["group"]["members"], json!(["S3"]));
    assert_eq!(merged["group"]["origin"]["kind"], "user");
    assert_eq!(merged["requested_kept"], "S3");
    assert_eq!(merged["kept_swapped"], true);
    assert!(merged["message"].as_str().unwrap().contains("S1 is kept"));
    assert!(
        rows(&f.cli_ok(&["speakers", "duplicates", &split]), "duplicates").is_empty(),
        "merged speakers are no longer suggested"
    );

    // Transcript, meeting record: S3's line shows as S1, on both sides.
    let mcp = f.mcp(vec![
        ("get_transcript", json!({ "meeting_id": split.as_str() })),
        (
            "get_transcript",
            json!({ "meeting_id": split.as_str(), "speaker": "S3" }),
        ),
        ("get_meeting", json!({ "meeting_id": split.as_str() })),
    ]);
    let transcript = f.cli_ok(&["transcript", &split]);
    let (cli_rows, mcp_rows) = shown_speakers(&transcript, &mcp[0]);
    assert_eq!(cli_rows, mcp_rows);
    assert_eq!(
        cli_rows,
        [
            json!([0, "S1", null]),
            json!([1, "S2", null]),
            json!([2, "S1", "S3"])
        ]
    );
    assert_eq!(
        rows(&transcript, "segments")[2]["speaker_name"],
        "Speaker 1"
    );
    // A member's ID selects its own lines; the kept speaker's, the group's.
    let only_member = f.cli_ok(&["transcript", &split, "--speaker", "S3"]);
    assert_eq!(
        shown_speakers(&only_member, &mcp[1]),
        (vec![json!([2, "S1", "S3"])], vec![json!([2, "S1", "S3"])])
    );
    let group = f.cli_ok(&["transcript", &split, "--speaker", "S1"]);
    assert_eq!(rows(&group, "segments").len(), 2);
    let show = f.cli_ok(&["show", &split]);
    assert_eq!(show["speaker_groups"], mcp[2]["speaker_groups"]);
    assert_eq!(show["speakers"][2]["merged_into"], "S1");
    assert_eq!(mcp[2]["speakers"][2]["merged_into"], "S1");

    // Naming the member names its group's kept speaker.
    let named = f.cli_ok(&["speakers", "name", &split, "S3", "Ana Example"]);
    assert_eq!(named["speaker_id"], "S1");
    assert_eq!(named["redirected_from"], "S3");
    let lib = Library::open(f.lib()).unwrap();
    let m = lib.load_meeting(&f.ids[0]).unwrap();
    assert_eq!(
        m.speaker(SpeakerId::new(3).unwrap()).unwrap().person_id,
        None,
        "the member itself is untouched"
    );
    // Who said each line: the same provenance on both sides.
    let provenance = |rows: &[Value]| -> Vec<Value> {
        rows.iter()
            .map(|r| {
                json!([
                    r["person_id"],
                    r["person_name"],
                    r["attribution"],
                    r["assignment"]
                ])
            })
            .collect()
    };
    let cli_lines = f.cli_ok(&["transcript", &split]);
    let mcp_lines = f.mcp(vec![(
        "get_transcript",
        json!({ "meeting_id": split.as_str() }),
    )]);
    let cli_who = provenance(rows(&cli_lines, "segments"));
    assert_eq!(cli_who, provenance(rows(&mcp_lines[0], "utterances")));
    assert_eq!(cli_who[2][1], "Ana Example", "{cli_lines}");
    assert_eq!(cli_who[2][2], "speaker");
    assert_eq!(cli_who[2][3], "confirmed");

    // The member keeps a frozen name of its own (auto-named Bia): while it is
    // merged, Bia is not in the meeting and Ana's talk time is the group's.
    let bia =
        broscribe_core::schema::Person::new(broscribe_core::PersonId::random(), "Bia Example");
    lib.write(|tx| {
        tx.save_person(&bia)?;
        let mut index = Index::open(tx.root())?;
        index.upsert_person(tx, &bia)?;
        let mut m = tx.library().load_meeting(&f.ids[0])?;
        let s3 = m.speaker_mut(SpeakerId::new(3).unwrap()).unwrap();
        s3.assignment = broscribe_core::schema::Assignment::Auto;
        s3.person_id = Some(bia.id.clone());
        tx.save_meeting(&m)?;
        index.upsert_meeting(tx, &m)
    })
    .unwrap();
    let ana = f.cli_ok(&["people", "show", "Ana Example"]);
    let ana_meetings = rows(&ana, "meetings");
    assert_eq!(ana_meetings.len(), 1, "{ana}");
    assert_eq!(ana_meetings[0]["speakers"], json!(["S1"]));
    assert_eq!(ana_meetings[0]["talk_s"], json!(6.0), "S1 and S3 together");
    let shown = f.cli_ok(&["people", "show", bia.id.as_str()]);
    assert!(rows(&shown, "meetings").is_empty(), "{shown}");
    // MCP `get_person` is `people show`, read only, without vectors.
    let got = f.mcp(vec![
        ("get_person", json!({ "person": "ana example" })),
        ("get_person", json!({ "person": bia.id.as_str() })),
    ]);
    assert_eq!(got[0], ana);
    assert_eq!(got[1], shown);
    assert!(
        !got[0].to_string().contains("\"vector\""),
        "counts only: {}",
        got[0]
    );
    assert!(
        rows(&got[0], "voiceprints")
            .iter()
            .all(|v| v["count"].as_u64().is_some_and(|n| n > 0))
    );
    assert_eq!(
        f.mcp_error("get_person", json!({ "person": "Nobody Synthetic" })),
        "not_found"
    );
    let listed = f.mcp(vec![("list_people", json!({}))]);
    assert_eq!(f.cli_ok(&["people"])["people"], listed[0]["people"]);
    let person = |name: &str| {
        rows(&listed[0], "people")
            .iter()
            .find(|p| p["name"] == name)
            .unwrap_or_else(|| panic!("{name} not listed: {}", listed[0]))
            .clone()
    };
    let ana_row = person("Ana Example");
    assert_eq!(
        (&ana_row["meetings"], &ana_row["talk_s"]),
        (&json!(1), &ana_meetings[0]["talk_s"]),
        "people show and list_people agree"
    );
    assert_eq!(person("Bia Example")["meetings"], json!(0));

    // Unmerge remembers the pair: never merged or suggested again.
    let unmerged = f.cli_ok(&["speakers", "unmerge", &split, "g1"]);
    assert_eq!(unmerged["group"]["id"], "G1");
    assert_eq!(unmerged["not_same_added"], json!([["S1", "S3"]]));
    assert!(
        unmerged["message"]
            .as_str()
            .unwrap()
            .contains("won't be merged automatically again")
    );
    assert!(rows(&f.cli_ok(&["speakers", "duplicates", &split]), "duplicates").is_empty());
    let transcript = f.cli_ok(&["transcript", &split]);
    assert_eq!(rows(&transcript, "segments")[2]["speaker"], "S3");
    assert!(rows(&transcript, "segments")[2]["original_speaker"].is_null());
    let (code, v) = f.cli(&["speakers", "unmerge", &split, "G1"]);
    assert_eq!((code, &v["error"]["code"]), (1, &json!("not_found")));

    let not_same = f.cli_ok(&["speakers", "not-same", &split, "S2", "S1"]);
    assert_eq!(not_same["pair"], json!(["S1", "S2"]));
    assert_eq!(not_same["added"], true);
    let again = f.cli_ok(&["speakers", "not-same", &split, "S1", "S2"]);
    assert_eq!(again["added"], false);
    let (code, v) = f.cli(&["speakers", "merge", &split, "S1"]);
    assert_eq!((code, &v["error"]["code"]), (1, &json!("invalid_input")));
}

#[test]
fn automatic_merging_of_existing_meetings_is_idempotent() {
    let f = Fixture::new();
    let model = Config::default().profile.speaker_embedding;
    std::fs::create_dir_all(f.data_dir()).unwrap();
    std::fs::write(
        f.data_dir().join("config.toml"),
        format!("[identity.{model:?}]\nmerge = 0.5\nauto_merge = 0.95\n"),
    )
    .unwrap();
    add_split_speaker(&f.lib(), &f.ids[1], 0.99);
    add_split_speaker(&f.lib(), &f.ids[2], 0.9);

    let first = f.cli_ok(&["speakers", "merge", "--auto", "--all"]);
    assert_eq!(first["groups_added"], 1, "{first}");
    let meetings = rows(&first, "meetings");
    assert_eq!(meetings.len(), MEETINGS);
    let row = |id: &MeetingId| {
        meetings
            .iter()
            .find(|m| m["meeting_id"] == id.as_str())
            .unwrap()
            .clone()
    };
    let grouped = row(&f.ids[1]);
    assert_eq!(
        (
            &grouped["groups_added"],
            &grouped["speakers_before"],
            &grouped["speakers_after"]
        ),
        (&json!(1), &json!(3), &json!(2))
    );
    assert!(grouped.get("skipped").is_none());
    // Below auto_merge: only suggested.
    assert_eq!(row(&f.ids[2])["groups_added"], 0);
    let show = f.cli_ok(&["show", f.ids[1].as_str()]);
    assert_eq!(show["speaker_groups"][0]["origin"]["kind"], "auto");

    let second = f.cli_ok(&["speakers", "merge", "--auto", "--all"]);
    assert_eq!(second["groups_added"], 0, "{second}");
    let listed = f.cli_ok(&["speakers", "merge", "--auto", f.ids[1].as_str()]);
    assert_eq!(rows(&listed, "meetings").len(), 1);
    assert_eq!(listed["groups_added"], 0);

    // Unmerging an automatic group: the next automatic run leaves it apart.
    f.cli_ok(&["speakers", "unmerge", f.ids[1].as_str(), "G1"]);
    let third = f.cli_ok(&["speakers", "merge", "--auto", "--all"]);
    assert_eq!(third["groups_added"], 0, "{third}");

    for bad in [
        vec!["speakers", "merge", "--auto"],
        vec!["speakers", "merge", "--auto", "--all", f.ids[0].as_str()],
    ] {
        let (code, v) = f.cli(&bad);
        assert_eq!(
            (code, &v["error"]["code"]),
            (1, &json!("invalid_input")),
            "{bad:?}"
        );
    }
}

/// The review rows of meeting `id`: `speaker_id`s, in order.
fn review_speakers(review: &Value, id: &MeetingId) -> Vec<String> {
    rows(review, "speakers")
        .iter()
        .filter(|r| r["meeting_id"] == id.as_str())
        .map(|r| r["speaker_id"].as_str().unwrap().to_owned())
        .collect()
}

/// "Can't tell": samples, dismiss, the dismissed list, `show` and MCP
/// `get_meeting` agree, and a dismissed speaker auto-named later by
/// `identity re-resolve` stays listed as dismissed (R2.1).
#[test]
fn review_dismissal_reads_the_same_on_cli_and_mcp() {
    let f = Fixture::new();
    let model = Config::default().profile.speaker_embedding;
    let m0 = &f.ids[0];
    let id = m0.as_str().to_owned();

    let samples = f.cli_ok(&["speakers", "samples", &id, "S1"]);
    assert_eq!(samples["speaker_id"], "S1");
    let clips = rows(&samples, "samples");
    assert_eq!(clips.len(), 1);
    assert_eq!(
        (&clips[0]["start"], &clips[0]["end"]),
        (&json!(0.0), &json!(4.0))
    );
    assert_eq!(clips[0]["text"], "We should ship the pricing memo number 0");

    let review = f.cli_ok(&["speakers", "review"]);
    assert_eq!(review["dismissed"], 0);
    assert_eq!(review_speakers(&review, m0), ["S1", "S2"]);

    let out = f.cli_ok(&["speakers", "dismiss", &id, "S1"]);
    assert_eq!(
        (&out["dismissed"], &out["changed"]),
        (&json!(true), &json!(true))
    );
    let again = f.cli_ok(&["speakers", "dismiss", &id, "S1"]);
    assert_eq!(again["changed"], false);
    let review = f.cli_ok(&["speakers", "review"]);
    assert_eq!(review["dismissed"], 1);
    assert_eq!(review_speakers(&review, m0), ["S2"]);
    let listed = f.cli_ok(&["speakers", "review", "--dismissed"]);
    assert_eq!(listed["dismissed"], 1);
    assert_eq!(review_speakers(&listed, m0), ["S1"]);
    assert!(
        f.cli_text(&["speakers", "review", "--dismissed"])
            .contains("speakers restore")
    );

    let reported = |f: &Fixture| {
        let show = f.cli_ok(&["show", &id]);
        let mcp = f.mcp(vec![("get_meeting", json!({ "meeting_id": id.as_str() }))]);
        let flags = |v: &Value| -> Vec<Value> {
            rows(v, "speakers")
                .iter()
                .map(|s| s["review_dismissed"].clone())
                .collect()
        };
        assert_eq!(flags(&show), flags(&mcp[0]), "show and get_meeting agree");
        flags(&show)
    };
    assert_eq!(reported(&f), [json!(true), json!(false)]);

    // A known voice for S1's centroid, then thresholds that auto-name it.
    let lib = Library::open(f.lib()).unwrap();
    let ana =
        broscribe_core::schema::Person::new(broscribe_core::PersonId::random(), "Ana Example");
    let m = lib.load_meeting(m0).unwrap();
    let mut print = broscribe_core::ops::weak_sample(&m, SpeakerId::new(1).unwrap(), &ana.id)
        .unwrap()
        .unwrap();
    print.source.meeting_id = None;
    print.strength = broscribe_core::schema::Strength::Confirmed;
    lib.write(|tx| {
        tx.save_person(&ana)?;
        tx.upsert_voiceprints(&ana.id, vec![print])?;
        Index::open(tx.root())?.upsert_person(tx, &ana)
    })
    .unwrap();
    std::fs::create_dir_all(f.data_dir()).unwrap();
    std::fs::write(
        f.data_dir().join("config.toml"),
        format!("[identity.{model:?}]\nauto_name = 0.9\nsuggest = 0.5\nmargin = 0.1\n"),
    )
    .unwrap();
    let dry = f.cli_ok(&["identity", "re-resolve", "--dry-run"]);
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["total"]["auto_named"], json!(MEETINGS));
    assert_eq!(lib.load_meeting(m0).unwrap(), m, "a dry run writes nothing");
    let real = f.cli_ok(&["identity", "re-resolve"]);
    assert_eq!(real["total"], dry["total"]);
    assert_eq!(real["meetings"], dry["meetings"]);
    let m = lib.load_meeting(m0).unwrap();
    let s1 = m.speaker(SpeakerId::new(1).unwrap()).unwrap();
    assert_eq!(s1.assignment, broscribe_core::schema::Assignment::Auto);

    // Auto-named, it is no longer pending, and still dismissed.
    let review = f.cli_ok(&["speakers", "review"]);
    assert_eq!(review["dismissed"], 1);
    assert_eq!(review_speakers(&review, m0), ["S2"]);
    let listed = f.cli_ok(&["speakers", "review", "--dismissed"]);
    assert_eq!(review_speakers(&listed, m0), ["S1"]);
    assert_eq!(rows(&listed, "speakers")[0]["assignment"], "auto");
    assert_eq!(reported(&f), [json!(true), json!(false)]);
    // Idempotent.
    let again = f.cli_ok(&["identity", "re-resolve"]);
    assert_eq!(again["total"]["auto_named"], 0);

    let restored = f.cli_ok(&["speakers", "restore", &id, "S1"]);
    assert_eq!(
        (&restored["dismissed"], &restored["changed"]),
        (&json!(false), &json!(true))
    );
    assert_eq!(f.cli_ok(&["speakers", "review"])["dismissed"], 0);
    assert!(
        rows(
            &f.cli_ok(&["speakers", "review", "--dismissed"]),
            "speakers"
        )
        .is_empty()
    );
    assert_eq!(reported(&f), [json!(false), json!(false)]);
    assert!(
        f.cli_text(&["speakers", "review", "--dismissed"])
            .contains("No speakers dismissed")
    );
}

#[test]
fn search_guard_and_default_limits() {
    let f = Fixture::new();
    let one = f.ids[0].as_str().to_owned();

    // `search` still needs words or a filter; `--meeting` alone is one.
    let (code, v) = f.cli(&["search"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "invalid_input");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--meeting"),
        "{v}"
    );
    let (code, v) = f.cli(&["search", "   "]);
    assert_eq!(code, 1, "blank words are no words: {v}");
    let only = f.cli_ok(&["search", "--meeting", &one]);
    assert_eq!(utterance_rows(&only).len(), 2);
    let (code, v) = f.cli(&["search", "--meeting", "not-an-id"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "invalid_input");

    // Omitted limits: `meetings` and `search` give 20, `actions` and
    // `decisions` everything; the MCP tools give 20 each.
    let mcp = f.mcp(vec![
        ("search_meetings", json!({})),
        ("list_action_items", json!({})),
        ("list_decisions", json!({})),
    ]);
    assert_eq!(meeting_rows(&mcp[0], "results").len(), 20);
    assert_eq!(action_rows(&mcp[1]).len(), 20);
    assert_eq!(decision_rows(&mcp[2]).len(), 20);
    let meetings = f.cli_ok(&["meetings"]);
    assert_eq!(
        meeting_rows(&meetings, "meetings"),
        meeting_rows(&mcp[0], "results"),
        "newest first, like search_meetings"
    );
    assert_eq!(
        meeting_rows(&meetings, "meetings")[0],
        f.ids[MEETINGS - 1].as_str()
    );
    assert_eq!(action_rows(&f.cli_ok(&["actions"])).len(), MEETINGS);
    assert_eq!(decision_rows(&f.cli_ok(&["decisions"])).len(), MEETINGS);
    assert_eq!(utterance_rows(&f.cli_ok(&["search", "memo"])).len(), 20);
}

/// `base` with `--cursor <cursor>` when there is one.
fn with_cursor<'a>(base: &[&'a str], cursor: Option<&'a str>) -> Vec<&'a str> {
    let mut args = base.to_vec();
    if let Some(c) = cursor {
        args.extend(["--cursor", c]);
    }
    args
}

/// `meetings`/`search_meetings`, `search`/`search_utterances` and
/// `actions`/`list_action_items` page through the index's cursors: the pages
/// put together are the unpaged rows in the same order, on both sides, and a
/// cursor from one side continues on the other (both build the same query).
/// A cursor is refused once the index changed (`cursor_stale`) or when it
/// is not one (`invalid_cursor`); `limit` and `page_size` do not mix.
#[test]
fn cli_and_mcp_page_the_same_rows() {
    let f = Fixture::new();
    let mcp_page = |tool: &'static str, mut args: Value, cursor: Option<&str>| {
        if let Some(c) = cursor {
            args["cursor"] = c.into();
        }
        f.mcp(vec![(tool, args)]).remove(0)
    };
    let cli_page = |base: &[&str], cursor: Option<&str>| f.cli_ok(&with_cursor(base, cursor));
    let ids = |rows: &[Value]| -> Vec<String> {
        rows.iter()
            .map(|r| r["meeting_id"].as_str().unwrap().to_owned())
            .collect()
    };

    // Meetings: 23 rows in pages of 5.
    let unpaged = meeting_rows(&f.cli_ok(&["meetings", "--limit", "50"]), "meetings");
    assert_eq!(unpaged.len(), MEETINGS);
    let cli = pages("meetings", 5, |c| {
        cli_page(&["meetings", "--page-size", "5"], c)
    });
    let mcp = pages("results", 5, |c| {
        mcp_page("search_meetings", json!({ "page_size": 5 }), c)
    });
    assert_eq!(ids(&cli), unpaged);
    assert_eq!(ids(&mcp), unpaged);
    assert!(mcp.iter().all(|r| r["transcript_uri"].is_string()));
    let first = mcp_page("search_meetings", json!({ "page_size": 5 }), None);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let cli_second = cli_page(&["meetings", "--page-size", "5"], Some(&cursor));
    let mcp_second = mcp_page("search_meetings", json!({ "page_size": 5 }), Some(&cursor));
    assert_eq!(
        meeting_rows(&cli_second, "meetings"),
        meeting_rows(&mcp_second, "results")
    );
    assert_eq!(cli_second["next_cursor"], mcp_second["next_cursor"]);

    // Utterances (keyword) and actions, filtered.
    let unpaged =
        utterance_rows(&f.cli_ok(&["search", "memo", "--mode", "keyword", "--limit", "100"]));
    let cli = pages("results", 4, |c| {
        cli_page(
            &["search", "memo", "--mode", "keyword", "--page-size", "4"],
            c,
        )
    });
    let mcp = pages("results", 4, |c| {
        mcp_page(
            "search_utterances",
            json!({ "query": "memo", "mode": "keyword", "page_size": 4 }),
            c,
        )
    });
    let as_rows = |rows: Vec<Value>| utterance_rows(&json!({ "results": rows }));
    assert_eq!(as_rows(cli), unpaged);
    assert_eq!(as_rows(mcp), unpaged);
    let unpaged = action_rows(&f.cli_ok(&["actions", "--status", "open"]));
    assert!(!unpaged.is_empty());
    let cli = pages("actions", 3, |c| {
        cli_page(&["actions", "--status", "open", "--page-size", "3"], c)
    });
    let mcp = pages("actions", 3, |c| {
        mcp_page(
            "list_action_items",
            json!({ "status": "open", "page_size": 3 }),
            c,
        )
    });
    let as_actions = |rows: Vec<Value>| action_rows(&json!({ "actions": rows }));
    assert_eq!(as_actions(cli), unpaged);
    assert_eq!(as_actions(mcp), unpaged);

    // Refusals, with the same codes on both sides.
    assert_eq!(
        f.mcp_error("search_meetings", json!({ "page_size": 5, "limit": 5 })),
        "invalid_input"
    );
    assert_eq!(
        f.mcp_error("search_meetings", json!({ "cursor": cursor })),
        "invalid_input",
        "a cursor continues a paged read"
    );
    for size in [0, 501] {
        assert_eq!(
            f.mcp_error("list_action_items", json!({ "page_size": size })),
            "invalid_input"
        );
        assert_eq!(
            f.mcp_error("list_decisions", json!({ "limit": size })),
            "invalid_input",
            "results are bounded"
        );
    }
    assert_eq!(
        f.mcp_error(
            "search_meetings",
            json!({ "page_size": 5, "cursor": "not-a-cursor" })
        ),
        "invalid_cursor"
    );
    assert_eq!(
        f.cli_error(&["meetings", "--page-size", "5", "--cursor", "not-a-cursor"]),
        "invalid_cursor"
    );
    f.cli_ok(&["tag", f.ids[0].as_str(), "+paged"]);
    assert_eq!(
        f.mcp_error(
            "search_meetings",
            json!({ "page_size": 5, "cursor": cursor })
        ),
        "cursor_stale"
    );
    assert_eq!(
        f.cli_error(&["meetings", "--page-size", "5", "--cursor", &cursor]),
        "cursor_stale"
    );
    // Starting again reads the changed index.
    let again = pages("results", 5, |c| {
        mcp_page("search_meetings", json!({ "page_size": 5 }), c)
    });
    assert_eq!(again.len(), MEETINGS);
}

/// Every row of one paged read whose clock moves: the first page on the
/// CLI (`cli_first`) or MCP at `start`, then each next page on the other
/// front end 2 s after the page before, with the same filters and the
/// cursor the page before returned. `cli` is the CLI command (with
/// `--page-size`) and its rows' key, `mcp` the tool, its arguments (with
/// `page_size`) and its rows' key.
fn moving_pages(
    f: &Fixture,
    start: chrono::DateTime<chrono::Utc>,
    cli_first: bool,
    cli: (&[&str], &str),
    mcp: (&'static str, &Value, &str),
) -> Vec<Value> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    let mut on_cli = cli_first;
    let mut now = start;
    loop {
        f.set_now(now);
        let (page, key) = if on_cli {
            (f.cli_ok(&with_cursor(cli.0, cursor.as_deref())), cli.1)
        } else {
            let mut args = mcp.1.clone();
            if let Some(c) = &cursor {
                args["cursor"] = c.as_str().into();
            }
            (f.mcp(vec![(mcp.0, args)]).remove(0), mcp.2)
        };
        all.extend(rows(&page, key).iter().cloned());
        assert!(all.len() <= 500, "paging ends");
        match page.get("next_cursor").and_then(Value::as_str) {
            Some(next) => cursor = Some(next.to_owned()),
            None => return all,
        }
        on_cli = !on_cli;
        now += chrono::Duration::seconds(2);
    }
}

/// Relative dates page stably (CL19): the first page fixes the instant
/// `--since 30d` resolves at and its cursor carries it, so continuations
/// asked later, on either front end, read the same date window. Pages are
/// disjoint and together exactly the rows read at the first page's
/// instant, including a meeting that a window re-resolved 2 s later leaves
/// out. A changed filter is still `cursor_mismatch` and a changed index
/// `cursor_stale`; unpaged reads (decisions too) resolve at their own time.
#[test]
fn relative_dates_page_at_the_first_pages_instant() {
    let f = Fixture::new();
    // Meeting 10 started 2026-09-11T13:00:00Z: 30 days before `t` is 1 s
    // earlier, 30 days before `t` + 2 s is 1 s later.
    let t = chrono::DateTime::parse_from_rfc3339("2026-10-11T12:59:59Z")
        .unwrap()
        .to_utc();
    let later = |s: i64| t + chrono::Duration::seconds(s);
    let window: Vec<String> = f.ids[10..]
        .iter()
        .rev()
        .map(|id| id.as_str().to_owned())
        .collect();
    assert_eq!(window.len(), 13);
    let meetings_at = |now| {
        f.set_now(now);
        meeting_rows(
            &f.cli_ok(&["meetings", "--since", "30d", "--limit", "50"]),
            "meetings",
        )
    };
    assert_eq!(meetings_at(t), window);
    assert_eq!(meetings_at(later(2)), window[..12], "the window moves");

    let ids = |rows: &[Value]| -> Vec<String> {
        rows.iter()
            .map(|r| r["meeting_id"].as_str().unwrap().to_owned())
            .collect()
    };
    let meetings_cli: &[&str] = &["meetings", "--since", "30d", "--page-size", "4"];
    let meetings_mcp = json!({ "since": "30d", "page_size": 4 });
    let search_cli: &[&str] = &[
        "search",
        "memo",
        "--mode",
        "keyword",
        "--since",
        "30d",
        "--page-size",
        "4",
    ];
    let search_mcp = json!({ "query": "memo", "mode": "keyword", "since": "30d", "page_size": 4 });
    let actions_cli: &[&str] = &["actions", "--since", "30d", "--page-size", "4"];
    let actions_mcp = json!({ "since": "30d", "page_size": 4 });
    f.set_now(t);
    let utterances = utterance_rows(&f.cli_ok(&[
        "search", "memo", "--mode", "keyword", "--since", "30d", "--limit", "100",
    ]));
    let mut said_in: Vec<String> = utterances.iter().map(|u| u.0.clone()).collect();
    said_in.sort();
    let mut expected = window.clone();
    expected.sort();
    assert_eq!(said_in, expected, "one memo line per meeting in the window");
    for cli_first in [true, false] {
        let meetings = moving_pages(
            &f,
            t,
            cli_first,
            (meetings_cli, "meetings"),
            ("search_meetings", &meetings_mcp, "results"),
        );
        assert_eq!(ids(&meetings), window, "cli first: {cli_first}");
        let said = moving_pages(
            &f,
            t,
            cli_first,
            (search_cli, "results"),
            ("search_utterances", &search_mcp, "results"),
        );
        assert_eq!(
            utterance_rows(&json!({ "results": said })),
            utterances,
            "cli first: {cli_first}"
        );
        let actions = moving_pages(
            &f,
            t,
            cli_first,
            (actions_cli, "actions"),
            ("list_action_items", &actions_mcp, "actions"),
        );
        assert_eq!(ids(&actions), window, "cli first: {cli_first}");
    }

    // Decisions are not paged: each read resolves at its own time, the
    // same on both front ends.
    for (now, expected) in [(t, &window[..]), (later(2), &window[..12])] {
        f.set_now(now);
        let cli = f.cli_ok(&["decisions", "--since", "30d"]);
        let mcp = f
            .mcp(vec![(
                "list_decisions",
                json!({ "since": "30d", "limit": 50 }),
            )])
            .remove(0);
        assert_eq!(decision_rows(&cli), decision_rows(&mcp));
        let meetings: Vec<String> = decision_rows(&cli).into_iter().map(|d| d.0).collect();
        assert_eq!(meetings, expected);
    }

    // A day later the cursor still reads the first page's window.
    f.set_now(t);
    let first = f.cli_ok(meetings_cli);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    f.set_now(later(86_400));
    let second = f.mcp(vec![(
        "search_meetings",
        json!({ "since": "30d", "page_size": 4, "cursor": cursor }),
    )]);
    assert_eq!(meeting_rows(&second[0], "results"), window[4..8]);

    // Changed filters are another query, whatever the clock.
    f.set_now(later(2));
    for changed in [
        &["meetings", "--since", "31d", "--page-size", "4"][..],
        &[
            "meetings",
            "--since",
            "30d",
            "--tag",
            "weekly",
            "--page-size",
            "4",
        ],
        &["meetings", "--since", "2026-09-11", "--page-size", "4"],
        &["meetings", "--page-size", "4"],
        &["actions", "--since", "30d", "--page-size", "4"],
    ] {
        assert_eq!(
            f.cli_error(&with_cursor(changed, Some(&cursor))),
            "cursor_mismatch",
            "{changed:?}"
        );
    }
    assert_eq!(
        f.mcp_error(
            "search_meetings",
            json!({ "since": "29d", "page_size": 4, "cursor": cursor })
        ),
        "cursor_mismatch"
    );
    // A changed index is stale, not reread under the old window.
    f.cli_ok(&["tag", f.ids[12].as_str(), "+later"]);
    assert_eq!(
        f.cli_error(&with_cursor(meetings_cli, Some(&cursor))),
        "cursor_stale"
    );
    assert_eq!(
        f.mcp_error(
            "search_meetings",
            json!({ "since": "30d", "page_size": 4, "cursor": cursor })
        ),
        "cursor_stale"
    );
}

fn template_hit_rows(v: &Value) -> Vec<(String, String, String, String)> {
    rows(v, "results")
        .iter()
        .map(|r| {
            assert_eq!(r["kind"], "template", "{r}");
            (
                r["meeting_id"].as_str().unwrap().to_owned(),
                r["template_id"].as_str().unwrap().to_owned(),
                r["template_name"].as_str().unwrap().to_owned(),
                r["snippet"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn templates_search_and_export_read_the_same_on_cli_and_mcp() {
    let f = Fixture::new();
    let first = f.ids[0].as_str().to_owned();
    let plain = f.ids[1].as_str().to_owned();
    let mcp = f.mcp(vec![
        ("list_templates", json!({})),
        ("list_templates", json!({ "meeting_id": first })),
        (
            "get_template_result",
            json!({ "meeting_id": first, "template_id": "bant" }),
        ),
        (
            "get_template_result",
            json!({ "meeting_id": first, "template_id": "retired" }),
        ),
        (
            "search_templates",
            json!({ "query": "budget", "limit": 50 }),
        ),
        ("search_templates", json!({ "query": "recap", "limit": 2 })),
        ("export_meeting", json!({ "meeting_id": first })),
        (
            "export_meeting",
            json!({
                "meeting_id": first,
                "include": ["transcript", "template:bant", "template:meddic"],
                "format": "txt",
                "timestamps": false,
            }),
        ),
        (
            "export_meeting",
            json!({ "meeting_id": plain, "format": "json" }),
        ),
    ]);

    // Definitions and a meeting's results, including one whose template
    // was deleted (R1.4): the same document on both sides.
    assert_eq!(f.cli_ok(&["templates", "list"]), mcp[0]);
    assert_eq!(
        f.cli_ok(&["templates", "list", "--meeting", &first]),
        mcp[1]
    );
    assert_eq!(
        rows(&mcp[1], "results")
            .iter()
            .map(|r| (r["template_id"].clone(), r["defined"].clone()))
            .collect::<Vec<_>>(),
        [
            (json!("bant"), json!(true)),
            (json!("retired"), json!(false))
        ]
    );
    assert_eq!(f.cli_ok(&["templates", "result", &first, "bant"]), mcp[2]);
    assert_eq!(
        mcp[2]["markdown"],
        "## Budget\n\nSpeaker 1 holds the budget for memo 0."
    );
    assert_eq!(
        f.cli_ok(&["templates", "result", &first, "retired"]),
        mcp[3]
    );
    let (code, v) = f.cli(&["templates", "result", &plain, "bant"]);
    assert_eq!((code, &v["error"]["code"]), (1, &json!("not_found")));

    // Keyword search in template results.
    let budget = f.cli_ok(&["search", "budget", "--in", "template", "--limit", "50"]);
    assert_eq!(template_hit_rows(&budget), template_hit_rows(&mcp[4]));
    assert_eq!(template_hit_rows(&budget).len(), 6 + 3);
    assert_eq!(budget["in"], "template");
    assert_eq!(budget["mode"], "keyword");
    let recap = f.cli_ok(&[
        "search", "recap", "--in", "template", "--mode", "keyword", "--limit", "2",
    ]);
    assert_eq!(template_hit_rows(&recap), template_hit_rows(&mcp[5]));
    assert_eq!(template_hit_rows(&recap).len(), 2);
    for bad in [
        vec!["search", "budget", "--in", "template", "--mode", "semantic"],
        vec!["search", "budget", "--in", "template", "--mode", "hybrid"],
        vec!["search", "budget", "--in", "template", "--meeting", &first],
        vec!["search", "budget", "--in", "template", "--tag", "weekly"],
        vec!["search", "--in", "template"],
    ] {
        let (code, v) = f.cli(&bad);
        assert_eq!(
            (code, &v["error"]["code"]),
            (1, &json!("invalid_input")),
            "{bad:?}: {v}"
        );
    }

    // Export: the same text through stdout, `--json` and a 0600 file.
    let full = f.cli_ok(&["export", &first]);
    assert_eq!(full["content"], mcp[6]["content"]);
    assert_eq!(full["sections"], mcp[6]["sections"]);
    assert_eq!(full["omitted"], json!([]));
    let text = mcp[6]["content"].as_str().unwrap();
    for part in [
        "Speaker 1 holds the budget for memo 0.",
        "Budget recap 0 by Speaker 2.",
        "We should ship the pricing memo number 0",
        "| Status | Action | Owner | Due | At |",
    ] {
        assert!(text.contains(part), "{part:?} in {text}");
    }
    assert!(!text.contains("{{"), "{text}");
    let human = f.cli_text(&["export", &first]);
    assert_eq!(human.trim_end(), text.trim_end());

    let partial = f.cli_ok(&[
        "export",
        &first,
        "--include",
        "transcript,template:bant,template:meddic",
        "--format",
        "txt",
        "--no-timestamps",
    ]);
    assert_eq!(partial["content"], mcp[7]["content"]);
    // Sections follow the document's order, not the order asked for.
    assert_eq!(partial["sections"], json!(["template:bant", "transcript"]));
    assert_eq!(partial["omitted"], json!(["template:meddic"]));
    let partial_text = partial["content"].as_str().unwrap();
    let timed = f.cli_ok(&[
        "export",
        &first,
        "--include",
        "transcript,template:bant",
        "--format",
        "txt",
    ]);
    assert_ne!(
        timed["content"], partial["content"],
        "timestamps make a difference"
    );
    assert!(!partial_text.contains("Budget recap"), "{partial_text}");

    let json_doc = f.cli_ok(&["export", &plain, "--format", "json"]);
    assert_eq!(json_doc["content"], mcp[8]["content"]);
    let parsed: Value = serde_json::from_str(json_doc["content"].as_str().unwrap()).unwrap();
    assert!(parsed.is_object(), "{parsed}");

    let file = f.root.join("exports").join("meeting.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let written = f.cli_ok(&["export", &first, "--out", file.to_str().unwrap()]);
    assert_eq!(written["path"], file.to_str().unwrap());
    assert!(written.get("content").is_none());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let (code, v) = f.cli(&["export", &first, "--include", "nonsense"]);
    assert_eq!((code, &v["error"]["code"]), (1, &json!("invalid_input")));
}

#[test]
fn titling_reads_the_same_on_cli_and_mcp_and_only_the_cli_deletes() {
    let f = Fixture::new();
    let (a, b) = (f.ids[0].to_string(), f.ids[1].to_string());
    // The CLI positional title and MCP set_meeting_title give one result shape.
    let cli = f.cli_ok(&["title", &a, "Synthetic renamed one"]);
    let mcp = f.mcp(vec![(
        "set_meeting_title",
        json!({ "meeting_id": b, "title": "Synthetic renamed two" }),
    )]);
    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(keys(&cli), keys(&mcp[0]));
    assert_eq!(cli["title"], "Synthetic renamed one");
    assert_eq!(mcp[0]["title"], "Synthetic renamed two");
    let found = f.cli_ok(&["meetings", "renamed", "--limit", "5"]);
    let mut found_ids = meeting_rows(&found, "meetings");
    found_ids.sort();
    let mut expected_ids = vec![a.clone(), b.clone()];
    expected_ids.sort();
    assert_eq!(found_ids, expected_ids, "{found}");
    let (code, v) = f.cli(&["title", &a, "  "]);
    assert_eq!((code, &v["error"]["code"]), (1, &json!("invalid_input")));
    let (code, _) = f.cli(&["title", &a]);
    assert_ne!(code, 0, "a title or --stdin is required");

    // Delete: a plan changes nothing; --yes removes the meeting; neither
    // flag is refused by the grammar; MCP offers no delete tool.
    let (code, _) = f.cli(&["delete", &a]);
    assert_ne!(code, 0);
    let plan = f.cli_ok(&["delete", &a, "--dry-run"]);
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["state"], "planned");
    assert_eq!(f.cli(&["show", &a]).0, 0, "a plan deletes nothing");
    let deleted = f.cli_ok(&["delete", &a, "--yes"]);
    assert_eq!(deleted["dry_run"], false);
    assert_eq!(deleted["state"], "deleted");
    let (code, v) = f.cli(&["show", &a]);
    assert_eq!((code, &v["error"]["code"]), (1, &json!("not_found")));
    let still = f.cli_ok(&["show", &b]);
    assert_eq!(still["title"], "Synthetic renamed two");
}

#[test]
fn meeting_order_pages_agree_between_cli_and_mcp() {
    let f = Fixture::new();
    let first = f.cli_ok(&["meetings", "--order", "oldest", "--page-size", "2"]);
    assert_eq!(first["meetings"][0]["meeting_id"], f.ids[0].as_str());
    assert_eq!(first["meetings"][1]["meeting_id"], f.ids[1].as_str());
    let cursor = first["next_cursor"].as_str().unwrap();
    let mcp = f.mcp(vec![(
        "search_meetings",
        json!({
            "order": "oldest", "page_size": 2, "cursor": cursor,
        }),
    )]);
    assert_eq!(mcp[0]["results"][0]["meeting_id"], f.ids[2].as_str());
    let (_, refused) = f.cli(&[
        "meetings",
        "--order",
        "newest",
        "--page-size",
        "2",
        "--cursor",
        cursor,
    ]);
    assert_eq!(refused["error"]["code"], "cursor_mismatch");
    let ranked = f.cli_ok(&["meetings", "pricing", "--order", "newest", "--limit", "10"]);
    assert_eq!(
        ranked,
        f.cli_ok(&["meetings", "pricing", "--order", "oldest", "--limit", "10"])
    );
}

#[test]
fn review_note_revision_refuses_a_replacement_and_checks_the_shown_note() {
    use broscribe_core::schema::ReviewSource;
    let f = Fixture::new();
    let lib = Library::open(f.lib()).unwrap();
    let id = f.ids[0].as_str();
    let replace = |text: &str| {
        lib.write(|tx| {
            let mut meeting = tx.load_meeting(&f.ids[0])?;
            meeting.set_review_notes(ReviewSource::Reprocess, [text.to_owned()]);
            tx.save_meeting(&meeting)
        })
        .unwrap()
    };
    replace("Check the first synthetic note.");
    let shown = f.cli_ok(&["meeting", "review-notes", id]);
    let revision = shown["review_notes"][0]["revision"].as_str().unwrap();
    replace("Check the replacement synthetic note.");
    let (_, refusal) = f.cli(&[
        "meeting",
        "review-notes",
        id,
        "--dismiss",
        "1",
        "--expect",
        revision,
    ]);
    assert_eq!(refusal["error"]["code"], "review_note_changed");
    let shown = f.mcp(vec![("get_meeting", json!({"meeting_id": id}))]);
    assert_eq!(
        shown[0]["review_notes"][0]["text"],
        "Check the replacement synthetic note."
    );
    assert_eq!(
        f.mcp(vec![(
            "check_review_note",
            json!({
                "meeting_id": id,
                "position": 1,
                "revision": shown[0]["review_note_revisions"][0],
            })
        )])[0]["checked"],
        1
    );
    assert_eq!(
        f.cli_ok(&["meeting", "review-notes", id])["review_notes"],
        json!([])
    );
    replace("Check a later synthetic note.");
    assert_eq!(
        f.cli_ok(&["meeting", "review-notes", id, "--dismiss", "1"])["checked"],
        1
    );
}

#[test]
fn export_includes_recording_bookmarks_and_notes_in_cli_and_mcp() {
    use broscribe_core::schema::{Bookmark, Note};
    let f = Fixture::new();
    let lib = Library::open(f.lib()).unwrap();
    let id = f.ids[0].as_str();
    lib.write(|tx| {
        let mut meeting = tx.load_meeting(&f.ids[0])?;
        meeting.bookmarks.push(Bookmark {
            at_s: 4.0,
            label: Some("Synthetic bookmark".into()),
        });
        meeting.notes.push(Note {
            at_s: 8.0,
            text: "Synthetic note".into(),
        });
        tx.save_meeting(&meeting)
    })
    .unwrap();
    let cli = f.cli_ok(&["export", id, "--include", "notes"]);
    let mcp = f.mcp(vec![(
        "export_meeting",
        json!({"meeting_id": id, "include": ["notes"]}),
    )]);
    assert_eq!(cli["sections"], json!(["notes"]));
    assert_eq!(cli["content"], mcp[0]["content"]);
    let text = cli["content"].as_str().unwrap();
    assert!(
        text.contains("00:04 · Bookmark · Synthetic bookmark"),
        "{text}"
    );
    assert!(text.contains("00:08 · Note · Synthetic note"), "{text}");
    let out = f.cli_ok(&["export", id, "--include", "notes", "--format", "json"]);
    let content: Value = serde_json::from_str(out["content"].as_str().unwrap()).unwrap();
    assert_eq!(content["bookmarks"][0]["at_s"], 4.0);
    assert_eq!(content["notes"][0]["text"], "Synthetic note");
}

#[test]
fn add_person_and_edit_actions_are_available_in_cli_and_mcp() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let f = Fixture::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_broscribe"))
        .env("BROSCRIBE_DATA_DIR", f.data_dir())
        .arg("--library")
        .arg(f.lib())
        .args(["--json", "people", "add", "--spec-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(br#"{"name":"Carla Fiction","emails":["carla@example.test"],"notes":"Synthetic contact"}"#).unwrap();
    let output = child.wait_with_output().unwrap();
    let added: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{added}");
    let owner = added["person"]["id"].as_str().unwrap();
    let lib = Library::open(f.lib()).unwrap();
    let owner_id = broscribe_core::PersonId::parse(owner).unwrap();
    let (person, notes) = lib.load_person_with_notes(&owner_id).unwrap();
    assert_eq!(person.emails, ["carla@example.test"]);
    assert_eq!(notes.trim(), "Synthetic contact");
    assert!(!lib.person_dir(&owner_id).join("voiceprints.json").exists());
    let other = f.mcp(vec![("add_person", json!({"name":"Dara Fiction"}))]);
    assert_eq!(other[0]["person"]["name"], "Dara Fiction");

    let id = f.ids[0].as_str();
    let meeting = lib.load_meeting(&f.ids[0]).unwrap();
    let action = meeting.enrichment.unwrap().actions[0].id.clone();
    let edited = f.cli_ok(&[
        "actions",
        "set",
        id,
        &action,
        "--owner",
        owner,
        "--due",
        "2027-02-03",
    ]);
    assert_eq!(edited["action"]["due"], "2027-02-03");
    let actions = f.cli_ok(&["actions", "--meeting", id, "--owner", owner]);
    assert_eq!(actions["actions"][0]["owner"], "Carla Fiction");
    assert_eq!(
        f.cli_ok(&["actions", "set-status", id, &action, "dropped"])["action"]["status"],
        "dropped"
    );
    assert_eq!(
        f.cli_ok(&["actions", "set-status", id, &action, "open"])["action"]["status"],
        "open"
    );
    let changed = f.mcp(vec![
        (
            "edit_action",
            json!({"meeting_id": id, "action_id": action, "clear_due": true, "clear_owner": true}),
        ),
        (
            "update_action_status",
            json!({"meeting_id": id, "action_id": action, "status": "done"}),
        ),
    ]);
    assert_eq!(changed[0]["action"]["due"], Value::Null);
    assert_eq!(changed[0]["action"]["owner_name"], Value::Null);
    assert_eq!(changed[1]["action"]["status"], "done");
}

#[test]
fn speaker_naming_refuses_a_stale_suggestion_in_cli_and_mcp() {
    use broscribe_core::schema::{Assignment, Person, Suggestion};
    let f = Fixture::new();
    let lib = Library::open(f.lib()).unwrap();
    let id = &f.ids[0];
    let proposed = lib.new_person_id();
    let change_suggestion = |score| {
        lib.write(|tx| {
            tx.save_person(&Person::new(proposed.clone(), "Robin Synthetic"))?;
            let mut meeting = tx.load_meeting(id)?;
            meeting.speakers[0].assignment = Assignment::Suggested;
            meeting.speakers[0].suggestion = Some(Suggestion {
                person_id: proposed.clone(),
                score,
            });
            tx.save_meeting(&meeting)
        })
        .unwrap();
    };
    change_suggestion(0.7);
    let displayed = f.cli_ok(&["show", id.as_str()]);
    let revision = displayed["speakers"][0]["revision"].as_str().unwrap();
    let mcp = f.mcp(vec![("get_meeting", json!({"meeting_id": id}))]);
    assert_eq!(mcp[0]["speakers"][0]["revision"], revision);
    change_suggestion(0.85);
    let (code, refused) = f.cli(&[
        "speakers",
        "name",
        id.as_str(),
        "S1",
        proposed.as_str(),
        "--expect",
        revision,
    ]);
    assert_eq!(code, 1, "{refused}");
    assert_eq!(refused["error"]["code"], "speaker_changed");
    let name = |revision: &str| json!({"meeting_id": id, "speaker_id": "S1", "person": proposed, "revision": revision});
    assert_eq!(
        f.mcp_error("name_speaker", name(revision)),
        "speaker_changed"
    );
    let current = f.cli_ok(&["show", id.as_str()]);
    assert_eq!(current["speakers"][0]["assignment"], "suggested");
    assert_eq!(
        f.mcp(vec![(
            "name_speaker",
            name(current["speakers"][0]["revision"].as_str().unwrap())
        )])[0]["person_id"],
        proposed.as_str()
    );
    let current = f.cli_ok(&["show", id.as_str()]);
    let revision = current["speakers"][0]["revision"].as_str().unwrap();
    assert_eq!(
        f.cli_ok(&[
            "speakers",
            "name",
            id.as_str(),
            "S1",
            proposed.as_str(),
            "--expect",
            revision
        ])["person_id"],
        proposed.as_str()
    );
}

#[test]
fn cli_export_refuses_to_replace_a_file_inside_the_library() {
    let f = Fixture::new();
    let id = f.ids[0].as_str();
    let inside = f.lib().join("synthetic.md");
    std::fs::write(&inside, "keep this").unwrap();
    let (code, value) = f.cli(&["export", id, "--out", inside.to_str().unwrap()]);
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "invalid_input");
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "keep this");
    let outside = f.root.join("synthetic.md");
    f.cli_ok(&["export", id, "--out", outside.to_str().unwrap()]);
    assert!(
        std::fs::read_to_string(&outside)
            .unwrap()
            .contains("pricing memo")
    );
}

#[test]
fn people_not_same_resolves_names_aliases_and_email() {
    use broscribe_core::{ops::PersonPair, schema::Person};
    let f = Fixture::new();
    let lib = Library::open(f.lib()).unwrap();
    let mut robin = Person::new(lib.new_person_id(), "Robin Synthetic");
    robin.aliases.push("Robin Agenda".into());
    let mut casey = Person::new(lib.new_person_id(), "Casey Synthetic");
    casey.emails.push("casey@example.invalid".into());
    lib.write(|tx| {
        tx.save_person(&robin)?;
        tx.save_person(&casey)
    })
    .unwrap();
    let pair = PersonPair::new(robin.id, casey.id);
    for reference in ["Robin Synthetic", "Robin Agenda"] {
        let result = f.cli_ok(&["people", "not-same", reference, "CASEY@example.invalid"]);
        assert_eq!(result["separate"], json!(pair));
    }
}

#[test]
fn mcp_speaker_rejection_runs_retention_after_the_decision() {
    use broscribe_core::config::Retention;
    use broscribe_core::schema::{Handling, Person, Recording, RecordingState};
    let f = Fixture::new();
    let lib = Library::open(f.lib()).unwrap();
    let id = &f.ids[0];
    let person = lib.new_person_id();
    let dir = lib.meeting_dir(id).unwrap();
    let audio = dir.join("audio.wav");
    std::fs::write(&audio, b"synthetic").unwrap();
    lib.write(|tx| {
        tx.save_person(&Person::new(person.clone(), "Robin Synthetic"))?;
        let mut meeting = tx.load_meeting(id)?;
        meeting.recording = Some(Recording {
            state: RecordingState::Available,
            handling: Handling::Copy,
            path: "audio.wav".into(),
            original_path: None,
            sha256: "ab".repeat(32),
            source_sha256: None,
            sources: Vec::new(),
            bytes: 9,
            retention_override: None,
            delete_after: None,
        });
        tx.save_meeting(&meeting)
    })
    .unwrap();
    Config::update(&f.data_dir(), |config| {
        config.retention = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        Ok(())
    })
    .unwrap();
    let result = f.mcp(vec![(
        "reject_speaker",
        json!({"meeting_id": id, "speaker_id":"S1", "person":person}),
    )]);
    assert_eq!(
        result[0]["retention"]["deleted"][0]["meeting_id"],
        id.as_str()
    );
    assert!(!audio.exists());
}

#[test]
fn edit_session_naming_runs_retention_after_the_decision() {
    use broscribe_core::{config::Retention, schema::Person};
    use std::io::Write;
    use std::process::{Command, Stdio};
    let f = Fixture::new();
    let lib = Library::open(f.lib()).unwrap();
    let id = &f.ids[0];
    let person = lib.new_person_id();
    lib.write(|tx| tx.save_person(&Person::new(person.clone(), "Robin Synthetic")))
        .unwrap();
    let enrollment = lib.person_dir(&person).join("enrollment");
    std::fs::create_dir(&enrollment).unwrap();
    let reading = enrollment.join("reading.wav");
    std::fs::write(&reading, b"synthetic reading").unwrap();
    Config::update(&f.data_dir(), |config| {
        config.retention = Retention::DeleteAfterDays { days: 0 };
        Ok(())
    })
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_broscribe"))
        .env("BROSCRIBE_DATA_DIR", f.data_dir())
        .arg("--library")
        .arg(f.lib())
        .args(["--json", "identity", "edit-session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "{}",
        json!({"action":"name","meeting":id,"speaker":"S1","person":person})
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{response}");
    assert_eq!(
        response["result"]["person_id"],
        person.as_str(),
        "{response}"
    );
    assert!(!reading.exists());
}

#[test]
fn cancel_fences_reused_jobs_and_restore_consumes_just_the_previous_result() {
    use broscribe_core::queue::{ENRICH_KIND, JobState, Queue, TEMPLATE_KIND, template_job_key};
    let f = Fixture::new();
    let lib = Library::open(f.lib()).unwrap();
    let id = &f.ids[0];
    let queue = Queue::open(lib.root()).unwrap();
    let (_, shown, template_date) = lib
        .write(|tx| {
            let mut m = tx.load_meeting(id)?;
            let base = m.enrichment.clone().unwrap();
            let mut second = base.clone();
            second.created_at += chrono::Duration::seconds(1);
            second.summary = "Second synthetic summary.".into();
            m.publish_enrichment(second.clone());
            let mut third = second.clone();
            third.created_at += chrono::Duration::seconds(1);
            third.summary = "Third synthetic summary.".into();
            m.publish_enrichment(third.clone());
            let mut result = m.template_results[0].clone();
            let previous_id = result.template_id.clone();
            result.created_at += chrono::Duration::seconds(1);
            m.publish_template_result(result.clone());
            tx.save_meeting(&m)?;
            Ok((
                base.created_at,
                third.created_at,
                (previous_id, result.created_at),
            ))
        })
        .unwrap();
    let enqueue = || {
        lib.write(|tx| {
            queue.restart_finished(tx, ENRICH_KIND, id.as_str(), json!({ "meeting_id": id }))
        })
        .unwrap()
    };
    let job = enqueue();
    let first = queue.get(&job).unwrap().generation_run.unwrap();
    assert!(f.cli_ok(&["queue", "cancel", job.as_str(), "--run", &first])["cancelled"] == true);
    assert_eq!(enqueue(), job);
    let running = queue
        .claim(&lib, &job, &FakeProcessProbe::new(1))
        .unwrap()
        .unwrap();
    let (_, wrong) = f.cli(&["queue", "cancel", job.as_str(), "--run", &first]);
    assert_eq!(wrong["error"]["code"], "generation_changed");
    let template = lib
        .write(|tx| {
            let template = &template_date.0;
            queue.enqueue(
                tx,
                TEMPLATE_KIND,
                &template_job_key(id, template),
                json!({ "meeting_id": id, "template_id": template }),
            )
        })
        .unwrap();
    let status = || {
        f.mcp(vec![("generation_status", json!({"meeting_id": id}))])
            .remove(0)
    };
    let generation = status();
    let jobs = generation["jobs"].as_array().unwrap();
    let current = jobs
        .iter()
        .find(|row| row["job_id"] == job.as_str())
        .unwrap();
    let queued = jobs
        .iter()
        .find(|row| row["job_id"] == template.as_str())
        .unwrap();
    assert_eq!(current["state"], "running");
    assert_eq!(queued["state"], "queued");
    assert_eq!(queued["template_id"], template_date.0);
    let queue_status = f.cli_ok(&["queue", "status"]);
    for row in jobs {
        let same = queue_status["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == row["job_id"])
            .unwrap();
        for key in ["state", "run", "jobs_ahead"] {
            assert_eq!(row[key], same[key]);
        }
    }
    let (_, busy) = f.cli(&[
        "enrich",
        "restore",
        id.as_str(),
        "--expect",
        &shown.to_rfc3339(),
    ]);
    assert_eq!(busy["error"]["code"], "generation_in_progress");
    assert_eq!(
        f.mcp(vec![(
            "cancel_generation",
            json!({ "job_id": current["job_id"], "run": current["run"] })
        )])[0]["cancelled"],
        true
    );
    assert!(
        status()["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["job_id"] == job.as_str() && row["state"] == "failed")
    );
    let template_run = queue
        .claim(&lib, &template, &FakeProcessProbe::new(1))
        .unwrap()
        .unwrap();
    template_run
        .commit(&lib, JobState::Done, |_, _| Ok(()))
        .unwrap();
    template_run.release(&lib).unwrap();
    assert!(
        status()["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["job_id"] == template.as_str() && row["state"] == "done")
    );
    assert!(
        running
            .commit::<()>(&lib, JobState::Done, |_, _| panic!(
                "revoked worker must not publish"
            ))
            .is_err()
    );
    assert_eq!(
        lib.load_meeting(id).unwrap().enrichment.unwrap().created_at,
        shown
    );

    // A published canonical result is terminal even if its queue
    // record was left running by a write failure.
    assert_eq!(enqueue(), job);
    let published = queue.get(&job).unwrap().generation_run.unwrap();
    lib.write(|tx| {
        let mut meeting = tx.load_meeting(id)?;
        meeting
            .published_generation_runs
            .insert(job.to_string(), published.clone());
        tx.save_meeting(&meeting)
    })
    .unwrap();
    let (_, too_late) = f.cli(&["queue", "cancel", job.as_str(), "--run", &published]);
    assert_eq!(too_late["error"]["code"], "generation_finished");
    let lease = queue
        .claim(&lib, &job, &FakeProcessProbe::new(1))
        .unwrap()
        .unwrap();
    lease.commit(&lib, JobState::Done, |_, _| Ok(())).unwrap();
    lease.release(&lib).unwrap();

    let template_job = lib
        .write(|tx| {
            queue.restart_finished(
                tx,
                TEMPLATE_KIND,
                &template_job_key(id, &template_date.0),
                json!({ "meeting_id": id }),
            )
        })
        .unwrap();
    assert_eq!(
        f.cli_ok(&["queue", "cancel", template_job.as_str()])["cancelled"],
        true
    );
    let previous_date = lib
        .load_meeting(id)
        .unwrap()
        .previous_enrichment
        .unwrap()
        .created_at;
    let (_, wrong) = f.cli(&[
        "enrich",
        "restore",
        id.as_str(),
        "--expect",
        &previous_date.to_rfc3339(),
    ]);
    assert_eq!(wrong["error"]["code"], "generation_changed");
    let restored = f.cli_ok(&[
        "enrich",
        "restore",
        id.as_str(),
        "--expect",
        &shown.to_rfc3339(),
    ]);
    assert_eq!(restored["created_at"], previous_date.to_rfc3339());
    let current = lib.load_meeting(id).unwrap();
    assert_eq!(
        current.enrichment.unwrap().summary,
        "Second synthetic summary."
    );
    assert!(current.previous_enrichment.is_none());
    let (_, repeat) = f.cli(&[
        "enrich",
        "restore",
        id.as_str(),
        "--expect",
        &previous_date.to_rfc3339(),
    ]);
    assert_eq!(repeat["error"]["code"], "no_previous_result");
    let restored = f.mcp(vec![(
        "restore_generation",
        json!({
            "meeting_id": id, "template_id": template_date.0, "expected": template_date.1,
        }),
    )]);
    assert_eq!(restored[0]["restored"], true);
    let current = lib.load_meeting(id).unwrap();
    assert!(
        !current
            .previous_template_results
            .contains_key(&template_date.0)
    );
}
