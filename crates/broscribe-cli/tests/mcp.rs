//! The MCP server in process: a client and the server over an in-memory
//! duplex, on a temporary library holding one synthetic meeting.

use std::path::Path;

use broscribe_cli::Deps;
use broscribe_cli::mcp::serve_on;
use broscribe_core::config::Config;
use broscribe_core::index::Index;
use broscribe_core::schema::{
    Action, ActionStatus, Centroid, Decision, Enrichment, Meeting, ProcessingStatus, ReviewNote,
    ReviewSource, Segment, Source, Speaker, StatusSource, TemplateResult, Track, TrackRole,
    enrichment_item_id,
};
use broscribe_core::{Library, MeetingId, SpeakerId};
use broscribe_models::fakes::FakeModelProvider;
use broscribe_platform::DecoderChain;
use broscribe_platform::fakes::{
    FakePaths, FakeProcessProbe, FakeProcessTreeControl, FakeTrustStore,
};
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ReadResourceRequestParams, ResourceContents,
};
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};

type Client = RunningService<RoleClient, ()>;

fn deps(data_dir: &Path) -> Deps {
    Deps {
        paths: Box::new(FakePaths::new(data_dir)),
        updates: broscribe_cli::UpdateDeps::with_transport(
            std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                std::path::Path::new(&data_dir).join("install"),
                "synthetic-test-target",
            )),
            std::sync::Arc::new(broscribe_updates::FakeTransport::default()),
        ),
        models: Box::new(FakeModelProvider::default()),
        decoder: DecoderChain::default(),
        process: Box::new(FakeProcessProbe::new(1)),
        mic: Box::new(broscribe_platform::fakes::FakeMicCapture::new(16_000)),
        secrets: std::sync::Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
        browser: std::sync::Arc::new(broscribe_platform::fakes::FakeBrowserLauncher::default()),
        agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
        autostart: Box::new(broscribe_platform::fakes::FakeAutostart::default()),
        agents: broscribe_cli::AgentEnv::isolated(data_dir),
        // Synthetic containment; no roots, so only loopback HTTP works.
        execution: broscribe_models::ExecutionContext::new(
            std::sync::Arc::new(FakeProcessTreeControl::default()),
            std::sync::Arc::new(FakeTrustStore::default()),
        ),
        clock: broscribe_cli::system_clock(),
    }
}

fn opt_in(data: &Path) {
    Config::update(data, |config| {
        config.mcp.enabled = Some(true);
        Ok(())
    })
    .unwrap();
}

fn unit_vector(dim: usize, hot: usize) -> Vec<f32> {
    (0..dim).map(|i| if i == hot { 1.0 } else { 0.0 }).collect()
}

/// One `done` meeting with two speakers (centroids of the default embedding
/// model, no recording), written through core and indexed.
fn seed(lib_dir: &Path) -> MeetingId {
    let lib = Library::open(lib_dir).unwrap();
    let id = lib.new_meeting_id().unwrap();
    let started = chrono::DateTime::parse_from_rfc3339("2026-10-05T14:00:00-03:00").unwrap();
    let mut m = Meeting::new(id.clone(), Source::Import, started);
    m.title = Some("Pricing sync".into());
    m.duration_s = 20.0;
    m.tracks = vec![Track {
        id: "mixed".into(),
        role: TrackRole::Mixed,
    }];
    let model = Config::default().profile.speaker_embedding;
    for n in 1..=2 {
        let mut s = Speaker::new(SpeakerId::new(n).unwrap(), "mixed");
        s.talk_s = 4.0;
        s.centroid = Some(Centroid::new(
            model.clone(),
            &unit_vector(16, n as usize),
            3,
        ));
        m.speakers.push(s);
    }
    let lines = [
        (1, 0.0, 4.0, "We should ship the pricing memo"),
        (2, 5.0, 9.0, "Agreed the roadmap looks good"),
    ];
    for (i, (spk, start, end, text)) in lines.into_iter().enumerate() {
        m.segments.push(Segment {
            speech_overlap: Vec::new(),
            id: i as u32,
            track: "mixed".into(),
            speaker: Some(SpeakerId::new(spk).unwrap()),
            start,
            end,
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
        summary: "{{S1}} discussed pricing with {{S2}}.".into(),
        instructions: None,
        topics: vec!["pricing".into()],
        keywords: vec!["memo".into()],
        tags: vec!["decision".into()],
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
    m.template_results = vec![TemplateResult {
        template_id: "bant".into(),
        template_name: "Budget check".into(),
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
        markdown: "## Budget\n\n{{S1}} owns the memo budget.".into(),
    }];
    m.review_notes = vec![ReviewNote {
        source: ReviewSource::Reprocess,
        text: "old S9: confirmed but matches no single new speaker".into(),
    }];
    m.processing.status = ProcessingStatus::Done;
    lib.write(|tx| {
        tx.create_meeting_dir(&m)?;
        tx.save_meeting(&m)?;
        Index::open(tx.root())?.upsert_meeting(tx, &m)
    })
    .unwrap();
    id
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    let args = args.as_object().cloned().unwrap_or_default();
    client
        .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(args))
        .await
        .unwrap()
}

/// The structured content of a successful call.
async fn ok(client: &Client, tool: &str, args: Value) -> Value {
    let r = call(client, tool, args).await;
    assert_ne!(r.is_error, Some(true), "{tool} failed: {:?}", r.content);
    r.structured_content.expect("structured content")
}

async fn script(client: Client, id: MeetingId, data_dir: std::path::PathBuf) {
    let id = id.as_str();
    let initial = ok(&client, "get_meeting", json!({ "meeting_id": id })).await;
    assert_eq!(initial["tags"], json!(["decision"]));
    assert_eq!(
        initial["review_notes"],
        json!([{
            "source": "reprocess",
            "text": "old S9: confirmed but matches no single new speaker",
        }])
    );
    assert_eq!(
        initial["enrichment"],
        json!({
            "provider": "fake",
            "instructions": null,
            "tags": ["decision"],
            "topics": ["pricing"],
            "model": null,
            "requested_model": null,
            "model_reported": false,
            "created_at": "2026-10-05T14:00:00-03:00",
            "can_restore": false,
            "stale": false,
        })
    );

    let mut tools: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    tools.sort();
    for previous_tool in [
        "add_person",
        "cancel_generation",
        "check_review_note",
        "dismiss_speaker_review",
        "edit_action",
        "edit_person",
        "export_meeting",
        "forget_person",
        "forget_voice",
        "generation_status",
        "get_diagnostics",
        "get_live_preview",
        "get_meeting",
        "get_person",
        "get_template_result",
        "get_transcript",
        "keep_identification",
        "list_action_items",
        "list_decisions",
        "list_people",
        "list_speaker_reviews",
        "list_templates",
        "name_speaker",
        "people_review",
        "recheck_identifications",
        "reject_speaker",
        "restore_generation",
        "restore_speaker_review",
        "search_meetings",
        "search_templates",
        "search_utterances",
        "set_meeting_title",
        "speaker_duplicates",
        "speaker_voice_matches",
        "tag_meeting",
        "undo_live_speaker_name",
        "update_action_status",
    ] {
        assert!(
            tools.iter().any(|name| name == previous_tool),
            "{previous_tool}"
        );
    }
    let review = ok(&client, "people_review", json!({})).await;
    let groups = review["identification"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    assert!(groups.iter().all(|group| {
        group["meeting_count"] == 1 && group["occurrences"][0]["meeting_id"] == id
    }));
    assert_eq!(review["duplicates"], json!([]));
    assert!(review["pending_merge"].is_null());

    let checks = ok(&client, "recheck_identifications", json!({})).await;
    assert_eq!(
        checks,
        json!({"alerts": [], "compared": 0, "unreadable_meetings": 0})
    );

    // Templates: definitions, this meeting's result, keyword search, export.
    let listed = ok(&client, "list_templates", json!({ "meeting_id": id })).await;
    let defined: Vec<&str> = listed["templates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    for builtin in [
        "meeting-notes",
        "meeting-outline",
        "follow-up-email",
        "bant",
        "meddic",
    ] {
        assert!(defined.contains(&builtin), "{builtin} in {defined:?}");
    }
    assert!(
        listed["templates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["builtin"] == true && t["auto"] == false && t["body"].is_string())
    );
    assert_eq!(
        listed["results"],
        json!([{
            "template_id": "bant",
            "template_name": "Budget check",
            "created_at": "2026-10-05T14:00:00-03:00",
            "stale": false,
            "defined": true,
        }])
    );
    let result = ok(
        &client,
        "get_template_result",
        json!({ "meeting_id": id, "template_id": "bant" }),
    )
    .await;
    assert_eq!(
        result["markdown"],
        "## Budget\n\nSpeaker 1 owns the memo budget."
    );
    assert_eq!(result["provider"], "fake");
    assert_eq!(result["stale"], false);
    let missing = call(
        &client,
        "get_template_result",
        json!({ "meeting_id": id, "template_id": "meddic" }),
    )
    .await;
    assert_eq!(missing.is_error, Some(true));
    assert_eq!(
        missing.structured_content.unwrap()["error"]["code"],
        "not_found"
    );
    let found = ok(&client, "search_templates", json!({ "query": "budget" })).await;
    let hits = found["results"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "{found}");
    assert_eq!(hits[0]["meeting_id"], id);
    assert_eq!(hits[0]["kind"], "template");
    assert_eq!(hits[0]["template_id"], "bant");
    assert_eq!(hits[0]["template_name"], "Budget check");
    let none = ok(&client, "search_templates", json!({ "query": "picnic" })).await;
    assert!(none["results"].as_array().unwrap().is_empty());
    let exported = ok(&client, "export_meeting", json!({ "meeting_id": id })).await;
    let text = exported["content"].as_str().unwrap();
    assert!(text.contains("Speaker 1 owns the memo budget."), "{text}");
    assert!(
        text.contains("| Status | Action | Owner | Due | At |"),
        "{text}"
    );
    assert!(text.contains("We should ship the pricing memo"), "{text}");
    assert!(!text.contains("{{"), "{text}");
    assert_eq!(exported["format"], "md");
    assert!(
        exported["sections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "template:bant"),
        "{exported}"
    );
    let partial = ok(
        &client,
        "export_meeting",
        json!({ "meeting_id": id, "include": ["summary", "template:meddic"], "format": "txt" }),
    )
    .await;
    assert_eq!(partial["sections"], json!(["summary"]));
    assert_eq!(partial["omitted"], json!(["template:meddic"]));
    assert!(
        !partial["content"]
            .as_str()
            .unwrap()
            .contains("pricing memo\n"),
        "no transcript when not asked for"
    );

    let found = ok(&client, "search_utterances", json!({ "query": "pricing" })).await;
    let hits = found["results"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["meeting_id"], id);
    assert_eq!(hits[0]["speaker_id"], "S1");
    assert_eq!(found["mode"], "keyword");
    assert!(
        found["note"].is_string(),
        "unavailable vectors are explained"
    );
    let keyword = ok(
        &client,
        "search_utterances",
        json!({ "query": "pricing", "mode": "keyword", "topic": "pricing" }),
    )
    .await;
    assert_eq!(keyword["mode"], "keyword");
    assert!(keyword["note"].is_null());
    assert_eq!(keyword["results"].as_array().unwrap().len(), 1);
    let empty = ok(
        &client,
        "search_utterances",
        json!({ "query": "  \t ", "topic": "pricing" }),
    )
    .await;
    assert_eq!(empty["results"].as_array().unwrap().len(), 2);
    assert_eq!(empty["mode"], "keyword");
    assert!(empty["note"].is_null());
    let unrelated = ok(
        &client,
        "search_utterances",
        json!({ "query": "pricing", "mode": "keyword", "topic": "picnic" }),
    )
    .await;
    assert!(unrelated["results"].as_array().unwrap().is_empty());
    let unrelated = ok(&client, "search_meetings", json!({ "topic": "picnic" })).await;
    assert!(unrelated["results"].as_array().unwrap().is_empty());
    let semantic = call(
        &client,
        "search_utterances",
        json!({ "query": "pricing", "mode": "semantic" }),
    )
    .await;
    assert_eq!(
        semantic.is_error,
        Some(true),
        "no installed embedding model"
    );
    let hybrid = call(
        &client,
        "search_utterances",
        json!({ "query": "pricing", "mode": "hybrid" }),
    )
    .await;
    assert_eq!(hybrid.is_error, Some(true));
    assert_eq!(hits[0]["speaker_name"], "Speaker 1");
    assert_eq!(hits[0]["at"], "00:00:00");
    assert!(hits[0]["started_at"].is_string());
    assert!(hits[0]["snippet"].as_str().unwrap().contains("**pricing**"));

    let t = ok(&client, "get_transcript", json!({ "meeting_id": id })).await;
    let lines = t["utterances"].as_array().unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["speaker"], "Speaker 1");
    assert_eq!(lines[1]["speaker_id"], "S2");
    assert_eq!(lines[1]["at"], "00:00:05");
    assert_eq!(lines[1]["meeting_id"], id);
    for line in lines {
        assert_eq!(line["person_id"], Value::Null, "{line}");
        assert_eq!(line["person_name"], Value::Null, "{line}");
        assert_eq!(line["attribution"], "speaker", "{line}");
    }
    let only_s2 = ok(
        &client,
        "get_transcript",
        json!({ "meeting_id": id, "speaker": "Speaker 2" }),
    )
    .await;
    assert_eq!(only_s2["utterances"].as_array().unwrap().len(), 1);

    let templates = client.list_resource_templates(None).await.unwrap();
    assert_eq!(
        templates.resource_templates[0].uri_template,
        "meeting://{id}/transcript.md"
    );
    let uri = format!("meeting://{id}/transcript.md");
    let md = resource_text(&client, &uri).await;
    assert!(md.contains("## Transcript"), "{md}");
    assert!(
        md.contains("Speaker 1: We should ship the pricing memo"),
        "{md}"
    );

    let named = ok(
        &client,
        "name_speaker",
        json!({ "meeting_id": id, "speaker_id": "S1", "person": "Ana Example" }),
    )
    .await;
    assert_eq!(named["meeting_id"], id);
    assert_eq!(named["speaker_id"], "S1");
    assert_eq!(named["person_name"], "Ana Example");
    assert_eq!(named["created"], true);
    assert_eq!(named["voiceprint_added"], true);
    // Saved now; the recheck of other speakers is queued (no agent in
    // tests), never computed or applied by the tool.
    assert_eq!(named["learning"]["source_meeting"], id);
    assert!(named["learning"]["job_id"].is_string(), "{named}");
    assert_eq!(named["agent_notified"], false);
    assert!(named.get("retro_candidates").is_none(), "{named}");

    let t = ok(&client, "get_transcript", json!({ "meeting_id": id })).await;
    assert_eq!(t["utterances"][0]["speaker"], "Ana Example");
    assert_eq!(t["utterances"][0]["person_name"], "Ana Example");
    assert!(t["utterances"][0]["person_id"].is_string());
    assert_eq!(t["utterances"][0]["attribution"], "speaker");
    let found = ok(
        &client,
        "search_utterances",
        json!({ "query": "pricing", "person": "Ana Example" }),
    )
    .await;
    assert_eq!(found["results"][0]["speaker_name"], "Ana Example");
    assert!(
        resource_text(&client, &uri)
            .await
            .contains("Ana Example: We should ship")
    );
    let people = ok(&client, "list_people", json!({})).await;
    assert_eq!(named["retention"]["checked"], 1);
    // Every person carries their stored hue, as the CLI shows it.
    for person in people["people"].as_array().unwrap() {
        assert!(
            (1..=7).contains(&person["hue"].as_u64().unwrap()),
            "{person}"
        );
    }
    let actions = ok(
        &client,
        "list_action_items",
        json!({ "owner": "Ana Example", "status": "open", "meeting_id": id }),
    )
    .await;
    assert_eq!(actions["actions"].as_array().unwrap().len(), 1);
    let action = &actions["actions"][0];
    assert_eq!(action["owner"], "Ana Example");
    assert_eq!(action["text"], "Ana Example writes the memo");
    let action_id = action["action_id"].as_str().unwrap();
    let updated = ok(
        &client,
        "update_action_status",
        json!({ "meeting_id": id, "action_id": action_id, "status": "done" }),
    )
    .await;
    assert_eq!(updated["action"]["status"], "done");
    assert_eq!(updated["action"]["status_source"], "user");
    assert!(updated["action"]["status_updated_at"].is_string());
    let open = ok(&client, "list_action_items", json!({ "status": "open" })).await;
    assert!(open["actions"].as_array().unwrap().is_empty());
    let done = ok(
        &client,
        "list_action_items",
        json!({ "query": "Ana", "status": "done" }),
    )
    .await;
    assert_eq!(done["actions"].as_array().unwrap().len(), 1);
    let decisions = ok(
        &client,
        "list_decisions",
        json!({ "topic": "pricing", "meeting_id": id, "since": "2026-10-01" }),
    )
    .await;
    assert_eq!(decisions["decisions"].as_array().unwrap().len(), 1);
    assert_eq!(decisions["decisions"][0]["by"], "Speaker 2");
    let unrelated = ok(&client, "list_decisions", json!({ "topic": "picnic" })).await;
    assert!(unrelated["decisions"].as_array().unwrap().is_empty());
    assert_eq!(people["people"][0]["name"], "Ana Example");

    let tagged = ok(
        &client,
        "tag_meeting",
        json!({ "meeting_id": id, "add": ["decision"] }),
    )
    .await;
    assert_eq!(tagged["tags"], json!(["decision"]));
    let meetings = ok(&client, "search_meetings", json!({ "tag": "decision" })).await;
    assert_eq!(meetings["results"][0]["meeting_id"], id);
    let meeting = ok(&client, "get_meeting", json!({ "meeting_id": id })).await;
    // Renaming: the user's title, findable by search; an empty title is refused.
    let renamed = ok(
        &client,
        "set_meeting_title",
        json!({ "meeting_id": id, "title": "  Synthetic renamed sync " }),
    )
    .await;
    assert_eq!(renamed["meeting_id"], id);
    assert_eq!(renamed["title"], "Synthetic renamed sync");
    assert!(renamed["session_id"].is_null());
    let found = ok(&client, "search_meetings", json!({ "query": "renamed" })).await;
    assert_eq!(found["results"][0]["meeting_id"], id);
    let empty = call(
        &client,
        "set_meeting_title",
        json!({ "meeting_id": id, "title": "  " }),
    )
    .await;
    assert_eq!(empty.is_error, Some(true));
    // The live preview comes from the recording agent only.
    let live = call(&client, "get_live_preview", json!({})).await;
    assert_eq!(live.is_error, Some(true));
    let text = &live.content[0].as_text().unwrap().text;
    assert!(text.starts_with("daemon_unavailable:"), "{text}");
    assert_eq!(meeting["speakers"][0]["name"], "Ana Example");
    assert_eq!(meeting["speakers"][0]["assignment"], "confirmed");
    let person_id = people["people"][0]["id"].as_str().unwrap();
    let photo = broscribe_integrations::account_dir(
        &data_dir,
        broscribe_integrations::IntegrationId::Google,
    )
    .join("photos")
    .join(format!("{person_id}.img"));
    std::fs::create_dir_all(photo.parent().unwrap()).unwrap();
    std::fs::write(&photo, b"synthetic-photo").unwrap();
    let forgotten = ok(
        &client,
        "forget_person",
        json!({ "person": "Ana Example", "confirm": true }),
    )
    .await;
    assert!(forgotten["person_id"].is_string());
    assert_eq!(
        forgotten["photos"],
        json!({ "status": "complete" }),
        "{forgotten}"
    );
    assert!(forgotten.get("photos_error").is_none(), "{forgotten}");
    let remaining = ok(&client, "list_people", json!({})).await;
    assert!(remaining["people"].as_array().unwrap().is_empty());
    assert!(!photo.exists());

    let missing = call(&client, "get_meeting", json!({ "meeting_id": "m-000000" })).await;
    assert_eq!(missing.is_error, Some(true));
    let text = &missing.content[0].as_text().unwrap().text;
    assert!(text.starts_with("not_found:"), "{text}");
    assert_eq!(
        missing.structured_content.unwrap()["error"]["code"],
        "not_found"
    );

    client.cancel().await.unwrap();
}

async fn resource_text(client: &Client, uri: &str) -> String {
    let read = client
        .read_resource(ReadResourceRequestParams::new(uri))
        .await
        .unwrap();
    match &read.contents[0] {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn tools_and_resource_over_duplex() {
    let tmp = tempfile::tempdir().unwrap();
    let lib_dir = tmp.path().join("library");
    let data_dir = tmp.path().join("data");
    let id = seed(&lib_dir);

    opt_in(&data_dir);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let client_data_dir = data_dir.clone();
    let client = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = ().serve(client_io).await.unwrap();
                script(client, id, client_data_dir).await;
            });
    });
    let (reader, writer) = tokio::io::split(server_io);
    serve_on(&deps(&data_dir), lib_dir, data_dir.clone(), reader, writer).unwrap();
    client.join().unwrap();
}

#[test]
fn maintenance_mcp_retains_admission_and_rejects_migrating_reads_without_bootstrap() {
    use broscribe_core::maintenance::{Mode, ModeChange};
    let tmp = tempfile::tempdir().unwrap();
    let library = tmp.path().join("library");
    let data = tmp.path().join("data");
    let id = seed(&library);
    std::fs::remove_dir_all(library.join(".index")).unwrap();
    let change = ModeChange::acquire(&data).unwrap();
    change.set(Mode::Maintenance).unwrap();
    drop(change);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let client_data = data.clone();
    let client = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = ().serve(client_io).await.unwrap();
                assert!(
                    ModeChange::acquire(&client_data).is_err(),
                    "idle MCP must exclude offline changes"
                );
                assert_eq!(
                    ok(&client, "get_meeting", json!({"meeting_id":id})).await["title"],
                    "Pricing sync"
                );
                let transcript = ok(&client, "get_transcript", json!({"meeting_id":id})).await;
                assert_eq!(
                    transcript["utterances"][0]["text"],
                    "We should ship the pricing memo"
                );
                assert_eq!(
                    ok(
                        &client,
                        "get_template_result",
                        json!({"meeting_id":id,"template_id":"bant"})
                    )
                    .await["template_name"],
                    "Budget check"
                );
                assert!(ok(&client, "list_templates", json!({})).await["templates"].is_array());
                assert!(
                    ok(&client, "export_meeting", json!({"meeting_id":id})).await["content"]
                        .as_str()
                        .unwrap()
                        .contains("pricing memo")
                );
                for (tool, arguments) in [
                    ("search_meetings", json!({})),
                    (
                        "search_utterances",
                        json!({"query":"pricing","mode":"keyword"}),
                    ),
                    ("list_people", json!({})),
                    ("get_person", json!({"person":"synthetic"})),
                    ("speaker_duplicates", json!({"meeting_id":id})),
                    ("list_action_items", json!({})),
                    ("list_decisions", json!({})),
                    ("search_templates", json!({"query":"budget"})),
                    ("get_live_preview", json!({})),
                    ("enroll_voice", json!({"microphone":true})),
                    ("enroll_voice", json!({"microphone":"not a boolean"})),
                    ("add_words", json!({"words":["multiple\nlines"]})),
                    ("preview_import", json!({})),
                    ("forget_person", json!({"person":"synthetic"})),
                    ("forget_voice", json!({"person":"synthetic"})),
                    ("reject_speaker", json!({"meeting_id":id,"speaker_id":"S1"})),
                    (
                        "name_speaker",
                        json!({"meeting_id":id,"speaker_id":"S1","person":"synthetic"}),
                    ),
                    (
                        "set_meeting_title",
                        json!({"meeting_id":id,"title":"changed"}),
                    ),
                    (
                        "update_action_status",
                        json!({"meeting_id":id,"action_id":"synthetic","status":"done"}),
                    ),
                    (
                        "speaker_voice_matches",
                        json!({"meeting_id":id,"speaker_id":"S1"}),
                    ),
                    ("get_diagnostics", json!({})),
                    ("tag_meeting", json!({"meeting_id":id,"add":["changed"]})),
                ] {
                    let result = call(&client, tool, arguments).await;
                    assert_eq!(result.is_error, Some(true), "{tool}");
                    assert_eq!(
                        result.structured_content.unwrap()["error"]["code"],
                        "maintenance",
                        "{tool}"
                    );
                }
                let text = resource_text(&client, &format!("meeting://{id}/transcript.md")).await;
                assert!(text.contains("pricing memo"));
                client.cancel().await.unwrap();
            });
    });
    let (reader, writer) = tokio::io::split(server_io);
    opt_in(&data);
    serve_on(&deps(&data), library.clone(), data.clone(), reader, writer).unwrap();
    client.join().unwrap();
    assert!(!library.join(".index").exists());
    assert!(!data.join("models").exists());
    ModeChange::acquire(&data)
        .unwrap()
        .set(Mode::Normal)
        .unwrap();
}

/// `broscribe --json --library <lib> <args…>` in process.
fn cli(deps: &Deps, lib: &Path, args: &[&str]) -> (i32, Value) {
    cli_on(deps, Some(lib), args)
}

fn cli_on(deps: &Deps, lib: Option<&Path>, args: &[&str]) -> (i32, Value) {
    let mut argv: Vec<std::ffi::OsString> = vec!["broscribe".into(), "--json".into()];
    if let Some(lib) = lib {
        argv.push("--library".into());
        argv.push(lib.into());
    }
    argv.extend(args.iter().map(std::ffi::OsString::from));
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = broscribe_cli::run(argv, deps, &mut out, &mut err);
    (code, serde_json::from_slice(&out).unwrap_or(Value::Null))
}

/// A line corrected with `transcript attribute` shows its person in the CLI
/// transcript (JSON and text), MCP `get_transcript` and the
/// `meeting://…/transcript.md` resource; clearing it restores the speaker.
#[test]
fn corrected_line_reaches_cli_mcp_and_resource() {
    let tmp = tempfile::tempdir().unwrap();
    let lib_dir = tmp.path().join("library");
    let data_dir = tmp.path().join("data");
    let id = seed(&lib_dir);
    let deps = deps(&data_dir);

    let (code, v) = cli(
        &deps,
        &lib_dir,
        &[
            "transcript",
            "attribute",
            id.as_str(),
            "--segment",
            "1",
            "--person",
            "Bea Example",
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["meeting_id"], id.as_str());
    assert_eq!(v["segments"], json!([1]));
    let person_id = v["person_id"].as_str().unwrap().to_owned();

    let (code, t) = cli(&deps, &lib_dir, &["transcript", id.as_str()]);
    assert_eq!(code, 0, "{t}");
    let segs = t["segments"].as_array().unwrap();
    assert_eq!(segs[0]["attribution"], "speaker");
    assert_eq!(segs[0]["person_id"], Value::Null);
    assert_eq!(segs[1]["attribution"], "line");
    assert_eq!(segs[1]["person_id"], person_id.as_str());
    assert_eq!(segs[1]["person_name"], "Bea Example");
    assert_eq!(segs[1]["speaker"], "S2", "the speaker is unchanged");
    assert_eq!(segs[1]["speaker_name"], "Speaker 2");
    let (code, only) = cli(
        &deps,
        &lib_dir,
        &["transcript", id.as_str(), "--speaker", "Bea Example"],
    );
    assert_eq!(code, 0, "{only}");
    assert_eq!(only["segments"].as_array().unwrap().len(), 1);
    let mut argv: Vec<std::ffi::OsString> = vec!["broscribe".into(), "--library".into()];
    argv.push(lib_dir.clone().into());
    argv.extend(["transcript".into(), id.as_str().into()]);
    let mut out = Vec::new();
    assert_eq!(
        broscribe_cli::run(argv, &deps, &mut out, &mut Vec::new()),
        0
    );
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("] Bea Example: Agreed the roadmap"), "{text}");
    assert!(text.contains("] Speaker 1: We should ship"), "{text}");

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let mcp_id = id.clone();
    let client = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = ().serve(client_io).await.unwrap();
                let id = mcp_id.as_str();
                let t = ok(&client, "get_transcript", json!({ "meeting_id": id })).await;
                let lines = t["utterances"].as_array().unwrap();
                assert_eq!(lines[0]["attribution"], "speaker");
                assert_eq!(lines[1]["attribution"], "line");
                assert_eq!(lines[1]["person_id"], person_id.as_str());
                assert_eq!(lines[1]["person_name"], "Bea Example");
                assert_eq!(lines[1]["speaker_id"], "S2");
                let only = ok(
                    &client,
                    "get_transcript",
                    json!({ "meeting_id": id, "speaker": "bea example" }),
                )
                .await;
                assert_eq!(only["utterances"].as_array().unwrap().len(), 1);
                let by_id = ok(
                    &client,
                    "get_transcript",
                    json!({ "meeting_id": id, "speaker": "S2" }),
                )
                .await;
                assert_eq!(by_id["utterances"].as_array().unwrap().len(), 1);
                let md = resource_text(&client, &format!("meeting://{id}/transcript.md")).await;
                assert!(md.contains("Bea Example: Agreed the roadmap"), "{md}");
                client.cancel().await.unwrap();
            });
    });
    let (reader, writer) = tokio::io::split(server_io);
    opt_in(&data_dir);
    serve_on(&deps, lib_dir.clone(), data_dir.clone(), reader, writer).unwrap();
    client.join().unwrap();

    let (code, v) = cli(
        &deps,
        &lib_dir,
        &[
            "transcript",
            "attribute",
            id.as_str(),
            "--segment",
            "1",
            "--clear",
        ],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["person_id"], Value::Null);
    let (_, t) = cli(&deps, &lib_dir, &["transcript", id.as_str()]);
    assert_eq!(t["segments"][1]["attribution"], "speaker");
    assert_eq!(t["segments"][1]["person_name"], Value::Null);
}

/// Renaming a person who appears in a meeting only through a line
/// correction re-renders that meeting's `meeting.md`.
#[test]
fn renaming_an_overlay_only_person_rerenders_meeting_md() {
    let tmp = tempfile::tempdir().unwrap();
    let lib_dir = tmp.path().join("library");
    let id = seed(&lib_dir);
    let deps = deps(&tmp.path().join("data"));
    let attribute = [
        "transcript",
        "attribute",
        id.as_str(),
        "--segment",
        "1",
        "--person",
        "Pia Invented",
    ];
    let (code, v) = cli(&deps, &lib_dir, &attribute);
    assert_eq!(code, 0, "{v}");
    let lib = Library::open_existing(&lib_dir).unwrap();
    let md_path = lib.list_meeting_dirs().unwrap()[0].join(broscribe_core::paths::MEETING_MD);
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.contains("Pia Invented: Agreed the roadmap"), "{md}");

    let (code, v) = cli(
        &deps,
        &lib_dir,
        &["people", "edit", "Pia Invented", "--name", "Pia Renamed"],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["meetings_rerendered"], 1, "{v}");
    let md = std::fs::read_to_string(&md_path).unwrap();
    assert!(md.contains("Pia Renamed: Agreed the roadmap"), "{md}");
    assert!(!md.contains("Pia Invented"), "{md}");
}

/// A loopback `/v1/embeddings` server answering every request with one
/// small vector per input; returns its base URL and the texts it embedded.
fn embedding_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    embedding_server_gated(None)
}

/// Holds the first embedding request: `arrived` is signalled when it came
/// in, and it is answered once `release` is.
struct Gate {
    arrived: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

/// [`embedding_server`], its first request held by `gate`.
fn embedding_server_gated(
    mut gate: Option<Gate>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!(
        "http://127.0.0.1:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let texts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = texts.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            let inputs: Vec<String> = request["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap().to_owned())
                .collect();
            let data: Vec<Value> = (0..inputs.len())
                .map(|i| json!({ "index": i, "embedding": [1.0, 0.5, i as f64 + 1.0] }))
                .collect();
            log.lock().unwrap().extend(inputs);
            if let Some(gate) = gate.take() {
                let _ = gate.arrived.send(());
                let _ = gate.release.recv();
            }
            let reply = json!({ "data": data }).to_string();
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .as_bytes(),
            );
        }
    });
    (base, texts)
}

/// The seeded meeting's two lines as one window (one speaker, no correction).
const MERGED: &str = "We should ship the pricing memo Agreed the roadmap looks good";

/// Semantic search through the loopback embedding server at `base`, with
/// the provider enabled and consented (synthetic settings).
fn use_embedding_server(data: &Path, base: String) {
    Config::update(data, |config| {
        config.profile.text_embedding = "openai-embeddings".into();
        config.providers.insert(
            "openai-embeddings".into(),
            broscribe_core::config::ProviderConfig {
                enabled: true,
                consented_at: Some(broscribe_core::schema::now()),
                model: Some("fake-embed".into()),
                base_url: Some(base),
            },
        );
        Ok(())
    })
    .unwrap();
}

/// The seeded meeting with both lines said by S1, semantic search through
/// the fake embedding server, and line 1 corrected to "Pia Invented" (which
/// splits the window). Returns the CLI deps, the data folder, the meeting
/// and the server's log.
fn split_window_library(
    tmp: &Path,
) -> (
    Deps,
    std::path::PathBuf,
    MeetingId,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    let lib_dir = tmp.join("library");
    let id = seed(&lib_dir);
    let lib = Library::open_existing(&lib_dir).unwrap();
    let mut m = lib.load_meeting(&id).unwrap();
    m.segments[1].speaker = Some(SpeakerId::new(1).unwrap());
    lib.write(|tx| {
        tx.save_meeting(&m)?;
        Index::open(tx.root())?.upsert_meeting(tx, &m)
    })
    .unwrap();
    let (base, log) = embedding_server();
    let deps = deps(tmp);
    let data = tmp.join("data");
    use_embedding_server(&data, base);
    let (code, v) = cli(
        &deps,
        &lib_dir,
        &[
            "transcript",
            "attribute",
            id.as_str(),
            "--segment",
            "1",
            "--person",
            "Pia Invented",
        ],
    );
    assert_eq!(code, 0, "{v}");
    let embedded = log.lock().unwrap().clone();
    assert!(
        embedded
            .iter()
            .any(|t| t == "Agreed the roadmap looks good"),
        "the split windows were embedded: {embedded:?}"
    );
    assert!(!embedded.iter().any(|t| t == MERGED), "{embedded:?}");
    (deps, data, id, log)
}

/// Pending windows of `id` under the configured embedder's key, which names
/// its server origin (vectors from another server never mix).
fn pending(lib_dir: &Path, data: &Path, id: &MeetingId) -> usize {
    let config = Config::load(data).unwrap();
    let base = config.providers["openai-embeddings"]
        .base_url
        .clone()
        .unwrap();
    let key = format!(
        "openai-embeddings:fake-embed@{}",
        broscribe_models::llm::http::normalized_origin(&base).unwrap()
    );
    Index::open_readonly(lib_dir)
        .unwrap()
        .pending_windows(&key, Some(id))
        .unwrap()
        .len()
}

/// Forgetting a person removes their line corrections, which merges the
/// windows they split; the CLI embeds the merged window right away.
#[test]
fn cli_forget_embeds_windows_it_merges() {
    let tmp = tempfile::tempdir().unwrap();
    let (deps, data, id, log) = split_window_library(tmp.path());
    let lib_dir = tmp.path().join("library");
    let (code, v) = cli(
        &deps,
        &lib_dir,
        &["people", "forget", "Pia Invented", "--yes"],
    );
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["meetings_changed"], json!([id.as_str()]), "{v}");
    assert!(log.lock().unwrap().iter().any(|t| t == MERGED));
    assert_eq!(pending(&lib_dir, &data, &id), 0);
    let (code, found) = cli(
        &deps,
        &lib_dir,
        &[
            "search",
            "roadmap",
            "--mode",
            "semantic",
            "--meeting",
            id.as_str(),
        ],
    );
    assert_eq!(code, 0, "{found}");
    assert_eq!(found["mode"], "semantic", "{found}");
    assert!(!found["results"].as_array().unwrap().is_empty(), "{found}");
}

/// The same through MCP `forget_person`.
#[test]
fn mcp_forget_embeds_windows_it_merges() {
    let tmp = tempfile::tempdir().unwrap();
    let (deps, data, id, log) = split_window_library(tmp.path());
    let lib_dir = tmp.path().join("library");
    opt_in(&data);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let client = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = ().serve(client_io).await.unwrap();
                let v = ok(
                    &client,
                    "forget_person",
                    json!({ "person": "Pia Invented", "confirm": true }),
                )
                .await;
                client.cancel().await.unwrap();
                v
            })
    });
    let (reader, writer) = tokio::io::split(server_io);
    serve_on(&deps, lib_dir.clone(), data.clone(), reader, writer).unwrap();
    let v = client.join().unwrap();
    assert!(v.get("vectors_error").is_none(), "{v}");
    assert_eq!(v["meetings_changed"], json!([id.as_str()]), "{v}");
    assert!(log.lock().unwrap().iter().any(|t| t == MERGED));
    assert_eq!(pending(&lib_dir, &data, &id), 0);
}

fn assert_disabled(r: &CallToolResult) {
    assert_eq!(r.is_error, Some(true));
    let text = &r.content[0].as_text().unwrap().text;
    assert!(text.starts_with("mcp_disabled:"), "{text}");
    assert_eq!(
        r.structured_content.as_ref().unwrap()["error"]["code"],
        "mcp_disabled"
    );
}

#[test]
fn disabling_mcp_stops_a_running_server() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let lib_dir = root.join("library");
    // `FakePaths::new(root)` keeps settings in `root/data`.
    let data_dir = root.join("data");
    let (code, state) = cli(&deps(&root), &lib_dir, &["mcp", "status"]);
    assert_eq!(code, 0);
    assert_eq!(state["enabled"], false);
    let id = seed(&lib_dir);

    opt_in(&data_dir);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let client_root = root.clone();
    let client_lib = lib_dir.clone();
    let client = std::thread::spawn(move || {
        let deps = deps(&client_root);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = ().serve(client_io).await.unwrap();
                let meeting = json!({ "meeting_id": id.as_str() });
                let uri = format!("meeting://{}/transcript.md", id.as_str());
                ok(&client, "get_meeting", meeting.clone()).await;

                let (code, off) = cli(&deps, &client_lib, &["mcp", "disable"]);
                assert_eq!(code, 0, "{off}");
                assert_eq!(off["enabled"], false);
                assert_eq!(off["agents"], json!([]));
                assert!(chrono::DateTime::parse_from_rfc3339(off["at"].as_str().unwrap()).is_ok());
                assert_disabled(&call(&client, "get_meeting", meeting.clone()).await);
                for (tool, input) in [
                    ("enroll_voice", json!({"microphone":true})),
                    ("enroll_voice", json!({"microphone":"not a boolean"})),
                    ("add_words", json!({"words":["multiple\nlines"]})),
                    ("preview_import", json!({})),
                ] {
                    assert_disabled(&call(&client, tool, input).await);
                }
                assert_disabled(
                    &call(&client, "search_utterances", json!({ "query": "pricing" })).await,
                );
                assert_disabled(&call(&client, "list_people", json!({})).await);
                assert_disabled(
                    &call(
                        &client,
                        "speaker_voice_matches",
                        json!({ "meeting_id": id.as_str(), "speaker_id": "S1" }),
                    )
                    .await,
                );
                assert_disabled(
                    &call(
                        &client,
                        "speaker_duplicates",
                        json!({ "meeting_id": id.as_str() }),
                    )
                    .await,
                );
                assert_disabled(&call(&client, "list_templates", json!({})).await);
                assert_disabled(
                    &call(&client, "search_templates", json!({ "query": "memo" })).await,
                );
                assert_disabled(
                    &call(
                        &client,
                        "get_template_result",
                        json!({ "meeting_id": id.as_str(), "template_id": "bant" }),
                    )
                    .await,
                );
                assert_disabled(&call(&client, "export_meeting", meeting.clone()).await);
                assert_disabled(
                    &call(
                        &client,
                        "tag_meeting",
                        json!({ "meeting_id": id.as_str(), "add": ["x"] }),
                    )
                    .await,
                );
                let refused = client
                    .read_resource(ReadResourceRequestParams::new(uri.clone()))
                    .await
                    .unwrap_err();
                assert!(
                    format!("{refused:?}").contains("mcp_disabled"),
                    "{refused:?}"
                );
                let (_, status) = cli(&deps, &client_lib, &["mcp", "status"]);
                assert_eq!(status["enabled"], false);

                let (code, on) = cli(&deps, &client_lib, &["mcp", "enable"]);
                assert_eq!(code, 0, "{on}");
                assert_eq!(on["enabled"], true);
                let back = ok(&client, "get_meeting", meeting).await;
                assert_eq!(
                    back["tags"],
                    json!(["decision"]),
                    "the refused tag was not applied"
                );
                assert!(resource_text(&client, &uri).await.contains("## Transcript"));
                client.cancel().await.unwrap();
            });
    });
    let (reader, writer) = tokio::io::split(server_io);
    let server_deps = deps(&root);
    serve_on(
        &server_deps,
        lib_dir.clone(),
        data_dir.clone(),
        reader,
        writer,
    )
    .unwrap();
    client.join().unwrap();

    // Disabled: refused at startup, before the library is opened.
    let (code, _) = cli(&server_deps, &lib_dir, &["mcp", "disable"]);
    assert_eq!(code, 0);
    let (server_io, _client_io) = tokio::io::duplex(1024);
    let (reader, writer) = tokio::io::split(server_io);
    let refused = serve_on(
        &server_deps,
        root.join("no-library"),
        data_dir,
        reader,
        writer,
    )
    .unwrap_err();
    assert_eq!(refused.code(), "mcp_disabled");
    let (code, v) = cli(&server_deps, &lib_dir, &["mcp"]);
    assert_eq!(code, 1);
    assert_eq!(v["error"]["code"], "mcp_disabled");
}

/// A client speaking raw JSON-RPC (newline-delimited) to the server, so a
/// test controls request IDs, progress tokens and cancellations exactly.
struct RawClient {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    /// Every message the server sent, in order.
    seen: Vec<Value>,
}

impl RawClient {
    async fn connect(io: tokio::io::DuplexStream) -> Self {
        use tokio::io::AsyncBufReadExt;
        let (reader, writer) = tokio::io::split(io);
        let mut client = Self {
            writer,
            lines: tokio::io::BufReader::new(reader).lines(),
            seen: Vec::new(),
        };
        client
            .send(json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "synthetic-client", "version": "0.0.0" },
                },
            }))
            .await;
        assert!(client.answer(0).await.get("result").is_some());
        client
            .send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        client
    }

    async fn send(&mut self, message: Value) {
        use tokio::io::AsyncWriteExt;
        self.writer
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        self.writer.flush().await.unwrap();
    }

    /// `tools/call` with request `id`, asking for progress under `token`.
    async fn call(&mut self, id: i64, tool: &str, arguments: Value, token: Option<&str>) {
        let mut params = json!({ "name": tool, "arguments": arguments });
        if let Some(token) = token {
            params["_meta"] = json!({ "progressToken": token });
        }
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params }))
            .await;
    }

    async fn cancel(&mut self, id: i64) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": id, "reason": "synthetic cancellation" },
        }))
        .await;
    }

    /// Reads messages until the answer to request `id`.
    async fn answer(&mut self, id: i64) -> Value {
        self.until(|m| m["id"] == id && m.get("method").is_none())
            .await
    }

    /// Reads messages until one `wanted` accepts, and returns it.
    async fn until(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
        loop {
            let line =
                tokio::time::timeout(std::time::Duration::from_secs(30), self.lines.next_line())
                    .await
                    .expect("the server answers in time")
                    .unwrap()
                    .expect("the server is still connected");
            let message: Value = serde_json::from_str(&line).unwrap();
            self.seen.push(message.clone());
            if wanted(&message) {
                return message;
            }
        }
    }

    /// `(progress, message)` of every progress notification for `token`.
    fn progress(&self, token: &str) -> Vec<(f64, String)> {
        self.seen
            .iter()
            .filter(|m| m["method"] == "notifications/progress")
            .filter(|m| m["params"]["progressToken"] == token)
            .map(|m| {
                assert_eq!(m["params"]["total"], json!(2.0), "{m}");
                (
                    m["params"]["progress"].as_f64().unwrap(),
                    m["params"]["message"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
}

/// The structured result of a successful `tools/call` answer.
fn structured(answer: &Value) -> &Value {
    assert_ne!(answer["result"]["isError"], json!(true), "{answer}");
    &answer["result"]["structuredContent"]
}

/// A semantic search waiting on its text model (a held loopback embedding
/// request) neither delays later requests nor answers after the client
/// cancelled it: the keyword search after it is answered at once with its
/// own progress phases, the cancelled request stops at its first phase and
/// is never answered (not even once its embedding finishes), and neither a
/// later request on this connection nor another client's is affected by the
/// cancellation (or by a stale one for a request already answered).
#[test]
fn progress_and_cancellation_follow_each_request() {
    let tmp = tempfile::tempdir().unwrap();
    let lib_dir = tmp.path().join("library");
    let data_dir = tmp.path().join("data");
    let id = seed(&lib_dir);
    let (arrived_tx, arrived) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let (base, embedded) = embedding_server_gated(Some(Gate {
        arrived: arrived_tx,
        release: release_rx,
    }));
    use_embedding_server(&data_dir, base);

    // A second client on the same library, started once the first one has
    // cancelled its request.
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    let (other_io, other_client_io) = tokio::io::duplex(64 * 1024);
    let other_id = id.clone();
    let other = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = ().serve(other_client_io).await.unwrap();
                go_rx
                    .recv_timeout(std::time::Duration::from_secs(60))
                    .expect("the first client cancelled its request");
                let t = ok(
                    &client,
                    "get_transcript",
                    json!({ "meeting_id": other_id.as_str() }),
                )
                .await;
                client.cancel().await.unwrap();
                t
            })
    });
    let other_lib = lib_dir.clone();
    opt_in(&data_dir);
    let other_data = data_dir.clone();
    let other_server = std::thread::spawn(move || {
        let (reader, writer) = tokio::io::split(other_io);
        serve_on(
            &deps(&other_data),
            other_lib,
            other_data.clone(),
            reader,
            writer,
        )
    });

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let meeting = id.clone();
    let client = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let mut c = RawClient::connect(client_io).await;
                let meeting = json!({ "meeting_id": meeting.as_str() });
                c.call(
                    1,
                    "search_utterances",
                    json!({ "query": "roadmap", "mode": "semantic" }),
                    Some("slow"),
                )
                .await;
                arrived
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .expect("the semantic search reached the text model");

                // Not queued behind the held search.
                c.call(
                    2,
                    "search_utterances",
                    json!({ "query": "pricing", "mode": "keyword" }),
                    Some("fast"),
                )
                .await;
                let fast = c.answer(2).await;
                assert_eq!(structured(&fast)["mode"], "keyword");
                assert_eq!(structured(&fast)["results"].as_array().unwrap().len(), 1);
                assert_eq!(
                    c.progress("fast"),
                    [
                        (0.0, "preparing the query".to_owned()),
                        (1.0, "reading the index".to_owned()),
                    ]
                );

                c.cancel(1).await;
                c.call(3, "get_meeting", meeting.clone(), None).await;
                assert_eq!(
                    structured(&c.answer(3).await)["meeting_id"],
                    meeting["meeting_id"]
                );
                go.send(()).unwrap();
                // A stale cancellation of an answered request changes nothing.
                c.cancel(2).await;
                c.call(4, "get_transcript", meeting.clone(), Some("after"))
                    .await;
                let lines = c.answer(4).await;
                assert_eq!(
                    structured(&lines)["utterances"].as_array().unwrap().len(),
                    2
                );

                // The held embedding finishes; the cancelled search stays
                // unanswered and never reads the index.
                release.send(()).unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                c.call(5, "list_people", json!({}), None).await;
                c.answer(5).await;
                assert!(
                    !c.seen
                        .iter()
                        .any(|m| m["id"] == 1 && m.get("method").is_none()),
                    "a cancelled request is never answered: {:?}",
                    c.seen
                );
                assert_eq!(
                    c.progress("slow"),
                    [(0.0, "preparing the query".to_owned())]
                );
                assert!(
                    c.progress("after").is_empty(),
                    "get_transcript has no phases"
                );
            })
    });
    let (reader, writer) = tokio::io::split(server_io);
    serve_on(&deps(&data_dir), lib_dir, data_dir.clone(), reader, writer).unwrap();
    client.join().unwrap();

    let other = other.join().unwrap();
    assert_eq!(other["utterances"].as_array().unwrap().len(), 2);
    other_server.join().unwrap().unwrap();
    assert_eq!(
        *embedded.lock().unwrap(),
        ["roadmap"],
        "only the cancelled search embedded its query"
    );
}

/// `search_utterances` arguments for one page of `roadmap`.
fn roadmap_page(mode: Option<&str>, cursor: Option<&str>) -> Value {
    let mut args = json!({ "query": "roadmap", "page_size": 1 });
    if let Some(mode) = mode {
        args["mode"] = mode.into();
    }
    if let Some(cursor) = cursor {
        args["cursor"] = cursor.into();
    }
    args
}

/// A continued semantic or hybrid page keeps the mode its cursor was issued
/// for: naming that mode, or none, continues with the kept query vector
/// (the query is not embedded again); naming the other mode is
/// `cursor_mismatch`, never a page silently ranked in the cursor's mode.
#[test]
fn continued_search_pages_keep_their_mode() {
    let tmp = tempfile::tempdir().unwrap();
    let lib_dir = tmp.path().join("library");
    let data_dir = tmp.path().join("data");
    seed(&lib_dir);
    let (base, embedded) = embedding_server();
    use_embedding_server(&data_dir, base);
    let deps = deps(tmp.path());
    let (code, rebuilt) = cli(&deps, &lib_dir, &["index", "rebuild"]);
    assert_eq!(code, 0, "{rebuilt}");
    assert!(rebuilt["vectors_error"].is_null(), "{rebuilt}");

    opt_in(&data_dir);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let client = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = ().serve(client_io).await.unwrap();
                for (mode, other) in [("semantic", "hybrid"), ("hybrid", "semantic")] {
                    let first =
                        ok(&client, "search_utterances", roadmap_page(Some(mode), None)).await;
                    assert_eq!(first["mode"], mode, "{first}");
                    assert_eq!(first["results"].as_array().unwrap().len(), 1, "{first}");
                    let cursor = first["next_cursor"]
                        .as_str()
                        .expect("two windows: a next page")
                        .to_owned();

                    let refused = call(
                        &client,
                        "search_utterances",
                        roadmap_page(Some(other), Some(&cursor)),
                    )
                    .await;
                    assert_eq!(refused.is_error, Some(true), "{mode} → {other}");
                    assert_eq!(
                        refused.structured_content.unwrap()["error"]["code"],
                        "cursor_mismatch",
                        "{mode} → {other}"
                    );

                    let same = ok(
                        &client,
                        "search_utterances",
                        roadmap_page(Some(mode), Some(&cursor)),
                    )
                    .await;
                    let omitted = ok(
                        &client,
                        "search_utterances",
                        roadmap_page(None, Some(&cursor)),
                    )
                    .await;
                    assert_eq!(same["mode"], mode, "{same}");
                    assert_eq!(omitted["mode"], mode, "{omitted}");
                    assert_eq!(same["results"], omitted["results"]);
                    assert_eq!(same["results"].as_array().unwrap().len(), 1, "{same}");
                    assert_ne!(same["results"], first["results"]);
                }
                client.cancel().await.unwrap();
            });
    });
    let (reader, writer) = tokio::io::split(server_io);
    serve_on(&deps, lib_dir, data_dir, reader, writer).unwrap();
    client.join().unwrap();
    assert_eq!(
        embedded
            .lock()
            .unwrap()
            .iter()
            .filter(|t| *t == "roadmap")
            .count(),
        2,
        "each first page embedded the query once; its continuations reused that vector"
    );
}

/// A cancellation after the write committed stops nothing that follows it:
/// the forget's search-vector follow-up (which starts only after the
/// commit) is held at the embedding server while the client cancels and a
/// later request proves the cancellation was handled; once released, the
/// window the forget merged is still embedded and the person stays
/// forgotten.
#[test]
fn a_write_cancelled_after_its_commit_still_runs_its_follow_ups() {
    let tmp = tempfile::tempdir().unwrap();
    let (deps, data, id, _) = split_window_library(tmp.path());
    let lib_dir = tmp.path().join("library");
    let (arrived_tx, arrived) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let (base, embedded) = embedding_server_gated(Some(Gate {
        arrived: arrived_tx,
        release: release_rx,
    }));
    use_embedding_server(&data, base);
    assert_ne!(pending(&lib_dir, &data, &id), 0);

    opt_in(&data);
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let meeting = id.clone();
    let client = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let mut c = RawClient::connect(client_io).await;
                c.call(
                    1,
                    "forget_person",
                    json!({ "person": "Pia Invented", "confirm": true }),
                    None,
                )
                .await;
                arrived
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .expect("the committed forget's follow-up reached the text model");
                c.cancel(1).await;
                c.call(
                    2,
                    "get_meeting",
                    json!({ "meeting_id": meeting.as_str() }),
                    None,
                )
                .await;
                structured(&c.answer(2).await);
                release.send(()).unwrap();
            });
    });
    let (reader, writer) = tokio::io::split(server_io);
    serve_on(&deps, lib_dir.clone(), data.clone(), reader, writer).unwrap();
    client.join().unwrap();

    assert!(embedded.lock().unwrap().iter().any(|t| t == MERGED));
    assert_eq!(pending(&lib_dir, &data, &id), 0);
    let (code, person) = cli(&deps, &lib_dir, &["people", "show", "Pia Invented"]);
    assert_ne!(code, 0, "{person}");
    assert_eq!(person["error"]["code"], "not_found", "{person}");
}

fn with_parity_client<F, Fut>(test: F)
where
    F: FnOnce(Client, std::path::PathBuf, MeetingId) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let data = root.join("data");
    let library = root.join("library");
    let id = seed(&library);
    opt_in(&data);

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let client_root = root.clone();
    let client = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = ().serve(client_io).await.unwrap();
                test(client, client_root, id).await;
            });
    });
    let (reader, writer) = tokio::io::split(server_io);
    serve_on(&deps(&root), library, data, reader, writer).unwrap();
    client.join().unwrap();
}

async fn same_cli(
    client: &Client,
    root: &Path,
    tool: &str,
    input: Value,
    command: &[&str],
) -> Value {
    let expected = cli_on(&deps(root), None, command).1;
    assert!(!expected.is_null(), "{command:?}");
    let reply = call(client, tool, input).await;
    let answer = reply
        .structured_content
        .unwrap_or_else(|| panic!("{tool}: {:?}", reply.content));
    assert_eq!(
        reply.is_error == Some(true),
        expected.get("error").is_some(),
        "{tool}: {answer}"
    );
    assert_eq!(answer, expected, "{tool} / {command:?}");
    answer
}

#[test]
fn mcp_command_reads_use_cli_rows_and_error_codes() {
    with_parity_client(|client, root, id| async move {
        let id = id.as_str();
        let me = json!({"meeting_id":id});
        let person = json!({"person":"Nobody Invented"});
        let empty = || json!({});
        let cases: &[(&str, Value, &[&str])] = &[
            (
                "preview_identity_reresolution",
                empty(),
                &["identity", "re-resolve", "--dry-run"],
            ),
            ("get_recording_status", empty(), &["record", "status"]),
            ("get_daemon_status", empty(), &["daemon", "status"]),
            ("list_recording_prompts", empty(), &["record", "prompts"]),
            ("get_queue_status", empty(), &["queue", "status"]),
            ("get_identity_status", empty(), &["identity", "status"]),
            (
                "list_live_speaker_reviews",
                me.clone(),
                &["speakers", "live-review", "--meeting", id],
            ),
            (
                "get_speaker_samples",
                json!({"meeting_id":id,"speaker_id":"S1"}),
                &["speakers", "samples", id, "S1"],
            ),
            (
                "list_person_samples",
                person,
                &["people", "samples", "Nobody Invented"],
            ),
            ("get_library_filters", empty(), &["library", "filters"]),
            ("check_library", empty(), &["library", "check"]),
            (
                "get_meeting_timeline",
                me.clone(),
                &["meeting", "timeline", id],
            ),
            (
                "list_meeting_review_notes",
                me.clone(),
                &["meeting", "review-notes", id],
            ),
            ("show_meeting_record", me, &["show", id]),
            ("get_about", empty(), &["app", "about"]),
            ("get_version", empty(), &["version"]),
            (
                "get_maintenance_status",
                empty(),
                &["maintenance", "status"],
            ),
            ("get_app_permissions", empty(), &["app", "permissions"]),
            ("get_detection_status", empty(), &["detect", "status"]),
            ("list_audio_devices", empty(), &["audio", "devices"]),
            ("get_microphone_pin", empty(), &["audio", "pin-status"]),
            (
                "get_audio_check_status",
                empty(),
                &["audio", "check-status"],
            ),
            ("list_models", empty(), &["models", "list"]),
            ("check_models", empty(), &["models", "doctor"]),
            (
                "get_voice_enrollment_status",
                empty(),
                &["setup", "voice-status"],
            ),
            ("get_setup_status", empty(), &["setup", "--non-interactive"]),
            (
                "get_voice_passage",
                json!({"language":"en"}),
                &["setup", "voice", "--show-passage", "--lang", "en"],
            ),
            ("list_providers", empty(), &["providers", "list"]),
            (
                "list_provider_models",
                json!({"id":"synthetic-missing"}),
                &["providers", "models", "synthetic-missing"],
            ),
            (
                "get_summary_instructions",
                empty(),
                &["enrich", "instructions", "show"],
            ),
            (
                "get_template_definition",
                json!({"template_id":"bant"}),
                &["templates", "show", "bant"],
            ),
            (
                "get_calendar_status",
                json!({"integration":"google"}),
                &["integrations", "status", "google"],
            ),
            (
                "list_calendar_accounts",
                json!({"integration":"microsoft-graph"}),
                &["integrations", "accounts", "microsoft-graph"],
            ),
            (
                "list_calendars",
                json!({"integration":"microsoft-graph"}),
                &["integrations", "calendars", "microsoft-graph"],
            ),
            (
                "get_calendar_login_attempt",
                json!({"integration":"google","attempt":"synthetic"}),
                &[
                    "integrations",
                    "login-attempt",
                    "google",
                    "--attempt",
                    "synthetic",
                ],
            ),
            (
                "search_directory",
                json!({"query":"Nobody Invented"}),
                &["people", "lookup", "Nobody Invented"],
            ),
            ("get_agent_tools_status", empty(), &["agents", "status"]),
            ("get_mcp_status", empty(), &["mcp", "status"]),
            (
                "get_app_setting",
                json!({"setting":"live_identity"}),
                &["app", "live-identity", "status"],
            ),
            ("get_text_size", empty(), &["settings", "text-size"]),
            ("get_theme", empty(), &["settings", "theme"]),
            (
                "get_speaker_grouping",
                empty(),
                &["settings", "speaker-grouping"],
            ),
            (
                "get_default_language",
                empty(),
                &["settings", "default-language"],
            ),
            ("get_word_list", empty(), &["settings", "word-list"]),
            ("get_window_settings", empty(), &["settings", "window"]),
            (
                "get_import_handling",
                empty(),
                &["settings", "import-handling"],
            ),
            (
                "get_consent_reminder",
                empty(),
                &["settings", "consent-reminder"],
            ),
            (
                "get_automatic_summaries",
                empty(),
                &["settings", "automatic-summaries"],
            ),
            (
                "search_people",
                json!({"query":"Nobody Invented"}),
                &["people", "list", "Nobody Invented"],
            ),
            (
                "list_library_meetings",
                json!({"query":"roadmap","include_transcripts":true}),
                &["meetings", "roadmap", "--include-transcripts"],
            ),
            ("poll_daemon_events", empty(), &["daemon", "watch"]),
        ];
        for (tool, input, command) in cases {
            same_cli(&client, &root, tool, input.clone(), command).await;
        }

        let missing = root.join("missing.wav");
        same_cli(
            &client,
            &root,
            "preview_import",
            json!({"path":missing}),
            &["import", "--dry-run", missing.to_str().unwrap()],
        )
        .await;
        same_cli(
            &client,
            &root,
            "preview_index_repair",
            empty(),
            &["library", "repair", "--only", "index", "--dry-run"],
        )
        .await;
        client.cancel().await.unwrap();
    });
}

#[test]
fn mcp_commands_keep_session_write_boundaries() {
    with_parity_client(|client, root, id| async move {
        let id = id.as_str();
        let session = || json!({"session_id":"s-synthetic"});
        for (tool, input, command) in [
            ("start_recording", json!({}), vec!["record", "start"]),
            (
                "pause_recording",
                session(),
                vec!["record", "pause", "--session", "s-synthetic"],
            ),
            (
                "resume_recording",
                session(),
                vec!["record", "resume", "--session", "s-synthetic"],
            ),
            (
                "stop_recording",
                session(),
                vec!["record", "stop", "--session", "s-synthetic"],
            ),
            (
                "process_recording_now",
                session(),
                vec!["record", "process-now", "--session", "s-synthetic"],
            ),
            (
                "bookmark_recording",
                session(),
                vec!["record", "bookmark", "--session", "s-synthetic"],
            ),
            (
                "note_recording",
                json!({"session_id":"s-synthetic","text":"synthetic"}),
                vec!["record", "note", "--session", "s-synthetic", "synthetic"],
            ),
            (
                "tag_recording",
                json!({"session_id":"s-synthetic","changes":["+tag"]}),
                vec!["record", "tag", "--session", "s-synthetic", "+tag"],
            ),
            (
                "set_recording_language",
                json!({"session_id":"s-synthetic","language":"en"}),
                vec!["record", "lang", "--session", "s-synthetic", "en"],
            ),
            (
                "set_recording_title",
                json!({"session_id":"s-synthetic","title":"Synthetic"}),
                vec!["record", "title", "--session", "s-synthetic", "Synthetic"],
            ),
            (
                "set_recording_participant",
                json!({"session_id":"s-synthetic","action":"confirm","person":"Synthetic Name"}),
                vec![
                    "record",
                    "participant",
                    "confirm",
                    "Synthetic Name",
                    "--session",
                    "s-synthetic",
                ],
            ),
            (
                "set_recording_auto_stop",
                json!({"session_id":"s-synthetic","enabled":true}),
                vec!["record", "auto-stop", "--session", "s-synthetic", "--on"],
            ),
            (
                "retry_recording",
                session(),
                vec!["record", "retry", "--session", "s-synthetic"],
            ),
            (
                "answer_recording_prompt",
                json!({"prompt_id":"synthetic","action":"ignore"}),
                vec!["record", "answer", "synthetic", "ignore"],
            ),
            (
                "attribute_live_turn",
                json!({"session_id":"s-synthetic","turn_id":"mic-0","revision":1,"epoch":0,"evidence":"ev-synthetic","clear":true}),
                vec![
                    "record",
                    "live-attribute",
                    "--session",
                    "s-synthetic",
                    "--turn",
                    "mic-0",
                    "--revision",
                    "1",
                    "--epoch",
                    "0",
                    "--evidence",
                    "ev-synthetic",
                    "--clear",
                ],
            ),
            ("show_app", json!({}), vec!["app", "show"]),
            (
                "answer_agent_notice",
                json!({"notice_id":"synthetic","action":"dismiss"}),
                vec!["daemon", "notice", "synthetic", "dismiss"],
            ),
            (
                "dismiss_live_speaker_review",
                json!({"meeting_id":id,"decision":1,"review_key":"synthetic"}),
                vec![
                    "speakers",
                    "live-dismiss",
                    id,
                    "--decision",
                    "1",
                    "--review-key",
                    "synthetic",
                ],
            ),
        ] {
            same_cli(&client, &root, tool, input, &command).await;
        }

        let changed = ok(
            &client,
            "edit_meeting_session",
            json!({"edit":{"action":"rename","meeting":id,"title":"Updated title"}}),
        )
        .await;
        assert_eq!(changed["can_undo"], true);
        assert_eq!(
            cli(&deps(&root), &root.join("library"), &["show", id]).1["title"],
            "Updated title"
        );
        let undone = ok(
            &client,
            "edit_meeting_session",
            json!({"edit":{"action":"undo"}}),
        )
        .await;
        assert_eq!(
            undone,
            json!({"can_undo":false,"can_redo":true,"result":"undone"})
        );
        assert_eq!(
            cli(&deps(&root), &root.join("library"), &["show", id]).1["title"],
            "Pricing sync"
        );
        client.cancel().await.unwrap();
    });
}

#[test]
fn mcp_forget_previews_match_cli_and_leave_the_profile_and_reading() {
    with_parity_client(|client, root, _| async move {
        let person = ok(&client, "add_person", json!({"name":"Pia Invented"})).await;
        let id = person["person"]["id"].as_str().unwrap();
        let library = Library::open_existing(root.join("library")).unwrap();
        let reading = library
            .person_dir(&broscribe_core::PersonId::parse(id).unwrap())
            .join("enrollment")
            .join(format!("{}.wav", "a".repeat(64)));
        std::fs::create_dir_all(reading.parent().unwrap()).unwrap();
        std::fs::write(&reading, b"synthetic").unwrap();

        for (tool, name, args) in [
            ("forget_person", "forget", json!({"person":id})),
            (
                "forget_voice",
                "forget-voice",
                json!({"person":id,"confirm":false}),
            ),
        ] {
            let value = same_cli(
                &client,
                &root,
                tool,
                args,
                &["people", name, id, "--dry-run"],
            )
            .await;
            assert_eq!(value["dry_run"], true);
            assert_eq!(value["plan"]["kept_readings"], 1);
            assert!(reading.is_file());
            assert_eq!(
                ok(&client, "get_person", json!({"person":id})).await["id"],
                id
            );
        }
        ok(&client, "forget_voice", json!({"person":id,"confirm":true})).await;
        assert!(!reading.exists());
        assert!(
            ok(&client, "get_person", json!({"person":id}))
                .await
                .is_object()
        );
        client.cancel().await.unwrap();
    });
}

#[test]
fn mcp_writes_share_cli_validation_and_persist_expected_changes() {
    with_parity_client(|client, root, id| async move {
        let id = id.as_str();
        let library = root.join("library");
        let data = root.join("data");
        let empty = || json!({});
        assert_eq!(
            cli(&deps(&root), &library, &["app", "live-identity", "off"]).0,
            0
        );
        for (tool, args, command) in [
            (
                "set_text_size",
                json!({"size":"larger"}),
                vec!["settings", "text-size", "larger"],
            ),
            (
                "set_theme",
                json!({"theme":"dark"}),
                vec!["settings", "theme", "dark"],
            ),
            (
                "set_default_language",
                json!({"language":"pt-BR"}),
                vec!["settings", "default-language", "pt-BR"],
            ),
            (
                "set_speaker_grouping",
                json!({"preset":{"kind":"conservative"}}),
                vec!["settings", "speaker-grouping", "conservative"],
            ),
            (
                "set_import_handling",
                json!({"handling":"reference"}),
                vec!["settings", "import-handling", "reference"],
            ),
            (
                "enable_consent_reminder",
                empty(),
                vec!["settings", "consent-reminder", "on"],
            ),
            (
                "set_automatic_summaries",
                json!({"enabled":false}),
                vec!["settings", "automatic-summaries", "off"],
            ),
            (
                "set_app_setting",
                json!({"setting":"live_identity","enabled":false}),
                vec!["app", "live-identity", "off"],
            ),
            (
                "set_microphone_pin",
                json!({"clear":true}),
                vec!["audio", "pin", "--clear"],
            ),
            (
                "complete_setup_step",
                json!({"step":"meeting_apps"}),
                vec!["setup", "meeting-apps"],
            ),
            (
                "set_setup_deferred",
                json!({"enabled":true}),
                vec!["setup", "defer"],
            ),
            (
                "clear_summary_instructions",
                empty(),
                vec!["enrich", "instructions", "clear"],
            ),
            (
                "pull_model",
                json!({"id":"missing-synthetic-model"}),
                vec!["models", "pull", "missing-synthetic-model"],
            ),
            (
                "use_model",
                json!({"role":"asr","id":"missing-synthetic-model"}),
                vec!["models", "use", "asr", "missing-synthetic-model"],
            ),
            (
                "configure_model_profile",
                json!({"profile":"missing-synthetic-profile"}),
                vec![
                    "setup",
                    "--non-interactive",
                    "--profile",
                    "missing-synthetic-profile",
                ],
            ),
            (
                "configure_provider",
                json!({"id":"openai-http","base_url":"http://localhost:11434/v1","model":"fake"}),
                vec![
                    "providers",
                    "configure",
                    "openai-http",
                    "--base-url",
                    "http://localhost:11434/v1",
                    "--model",
                    "fake",
                ],
            ),
            (
                "disable_provider",
                json!({"id":"openai-http"}),
                vec!["providers", "disable", "openai-http"],
            ),
            (
                "prefer_provider",
                json!({"id":"openai-http"}),
                vec!["providers", "prefer", "openai-http"],
            ),
            (
                "test_provider",
                json!({"id":"openai-http"}),
                vec!["providers", "test", "openai-http"],
            ),
            (
                "cache_person_photo",
                json!({"person":"Nobody Invented"}),
                vec!["people", "photo", "Nobody Invented"],
            ),
            (
                "connect_system_calendar",
                empty(),
                vec!["integrations", "login", "system-calendar"],
            ),
            (
                "select_system_calendars",
                json!({"calendar_ids":["synthetic"]}),
                vec![
                    "integrations",
                    "setup",
                    "system-calendar",
                    "--calendar-id",
                    "synthetic",
                ],
            ),
            (
                "sync_calendar_account",
                json!({"account":"not-an-id"}),
                vec![
                    "integrations",
                    "sync-account",
                    "microsoft-graph",
                    "--account",
                    "not-an-id",
                ],
            ),
            (
                "cancel_calendar_login",
                json!({"integration":"google","attempt":"synthetic"}),
                vec![
                    "integrations",
                    "cancel-login",
                    "google",
                    "--attempt",
                    "synthetic",
                ],
            ),
            ("start_daemon", empty(), vec!["daemon", "start"]),
            ("install_daemon", empty(), vec!["daemon", "install"]),
            ("stop_daemon", empty(), vec!["daemon", "stop"]),
            (
                "setup_recording_access",
                empty(),
                vec!["setup", "recording-access"],
            ),
            (
                "request_app_permission",
                json!({"permission":"accessibility"}),
                vec!["app", "permissions", "--request", "accessibility"],
            ),
            (
                "check_recording_file",
                json!({"meeting_id":id}),
                vec!["meeting", "check-recording", id],
            ),
            (
                "prepare_meeting_audio",
                json!({"meeting_id":id}),
                vec!["meeting", "prepare-audio", id],
            ),
        ] {
            same_cli(&client, &root, tool, args, &command).await;
        }

        let instructions = "Use the synthetic agenda.";
        ok(
            &client,
            "set_summary_instructions",
            json!({"instructions":instructions}),
        )
        .await;
        let saved = same_cli(
            &client,
            &root,
            "get_summary_instructions",
            empty(),
            &["enrich", "instructions", "show"],
        )
        .await;
        assert!(saved.to_string().contains(instructions));

        ok(&client, "create_template", json!({"template_id":"synthetic","name":"Synthetic review","body":"Summarise the synthetic agenda."})).await;
        let original = same_cli(
            &client,
            &root,
            "get_template_definition",
            json!({"template_id":"synthetic"}),
            &["templates", "show", "synthetic"],
        )
        .await;
        ok(
            &client,
            "edit_template",
            json!({"template_id":"synthetic","name":"Updated synthetic review"}),
        )
        .await;
        let template = same_cli(
            &client,
            &root,
            "get_template_definition",
            json!({"template_id":"synthetic"}),
            &["templates", "show", "synthetic"],
        )
        .await;
        assert_ne!(original, template);
        same_cli(
            &client,
            &root,
            "set_template_automatic",
            json!({"template_id":"synthetic","enabled":true}),
            &["templates", "auto", "add", "synthetic"],
        )
        .await;

        let person = ok(&client, "add_person", json!({"name":"Pia Invented"})).await;
        let pid = person["person"]["id"].as_str().unwrap();
        let edit = ok(
            &client,
            "edit_person",
            json!({"person_id":pid,"add_aliases":["Synthetic Alias"],"notes":"Example notes."}),
        )
        .await;
        let undo_id = edit["undo_id"].as_str().unwrap();
        let page = same_cli(
            &client,
            &root,
            "get_person",
            json!({"person":pid}),
            &["people", "show", pid],
        )
        .await;
        assert_eq!(page["aliases"], json!(["Synthetic Alias"]));
        ok(
            &client,
            "undo_person_edit",
            json!({"person":pid,"undo_id":undo_id}),
        )
        .await;
        let page = same_cli(
            &client,
            &root,
            "get_person",
            json!({"person":pid}),
            &["people", "show", pid],
        )
        .await;
        assert_eq!(page["notes"], "");

        let out = root.join("synthetic-export.json");
        let cli_out = root.join("cli-synthetic-export.json");
        let args = json!({"meeting_id":id,"out":out,"format":"json"});
        let exported = ok(&client, "export_meeting_file", args.clone()).await;
        assert_eq!(exported["path"], out.to_str().unwrap());
        assert_eq!(
            cli(
                &deps(&root),
                &library,
                &[
                    "export",
                    id,
                    "--format",
                    "json",
                    "--out",
                    cli_out.to_str().unwrap()
                ]
            )
            .0,
            0
        );
        assert_eq!(
            std::fs::read(&out).unwrap(),
            std::fs::read(cli_out).unwrap()
        );
        assert_eq!(
            call(&client, "export_meeting_file", args).await.is_error,
            Some(true)
        );

        Config::update(&data, |config| {
            config.mcp.enabled = Some(false);
            Ok(())
        })
        .unwrap();
        let blocked = call(&client, "set_default_language", json!({"language":"en"})).await;
        assert_eq!(
            blocked.structured_content.unwrap()["error"]["code"],
            "mcp_disabled"
        );
        assert_eq!(
            Config::load(&data).unwrap().default_language.as_deref(),
            Some("pt-BR")
        );
        client.cancel().await.unwrap();
    });
}

fn cli_with_input(root: &Path, command: &[&str], input: &str) -> Value {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_broscribe"))
        .env("BROSCRIBE_DATA_DIR", root.join("data"))
        .arg("--library")
        .arg(root.join("library"))
        .arg("--json")
        .args(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    serde_json::from_slice(&result.stdout).unwrap_or_else(|e| panic!("{command:?}: {e}"))
}

#[test]
fn mcp_file_and_setup_commands_use_cli_inputs_and_replies() {
    with_parity_client(|client, root, meeting| async move {
        let id = meeting.as_str();
        let library = root.join("library");
        let missing = root.join("missing.wav");
        let missing_path = missing.to_str().unwrap();
        let output = root.join("clip.wav");
        let output_path = output.to_str().unwrap();
        let empty = || json!({});
        let invalid_name = "Bad\nName";
        for (tool, args, command) in [
            ("get_setup_diagnostics", empty(), vec!["doctor"]),
            (
                "dump_reader_structure",
                json!({"app":"dev.broscribe.synthetic.absent"}),
                vec!["readers", "dump", "--app", "dev.broscribe.synthetic.absent"],
            ),
            (
                "queue_reprocess_meeting",
                json!({"meeting_id":"invalid"}),
                vec!["reprocess", "invalid", "--queue"],
            ),
            (
                "import_recording",
                json!({"path":missing}),
                vec!["import", missing_path],
            ),
            (
                "repair_library_index",
                empty(),
                vec!["library", "repair", "--only", "index", "--yes"],
            ),
            (
                "export_meeting_clip",
                json!({"meeting_id":id,"from":"0","to":"1","out":output}),
                vec![
                    "meeting",
                    "clip",
                    id,
                    "--from",
                    "0",
                    "--to",
                    "1",
                    "--out",
                    output_path,
                ],
            ),
            (
                "export_person_clip",
                json!({"person":"Nobody Invented","sample_id":"synthetic","out":output}),
                vec![
                    "people",
                    "clip",
                    "Nobody Invented",
                    "synthetic",
                    "--out",
                    output_path,
                ],
            ),
            (
                "set_self_person",
                json!({"name":invalid_name}),
                vec!["setup", "--non-interactive", "--name", invalid_name],
            ),
            ("check_microphone", empty(), vec!["setup", "mic-test"]),
            (
                "configure_google_calendar",
                json!({"client_id":""}),
                vec!["integrations", "setup", "google", "--client-id", ""],
            ),
            (
                "select_calendars",
                json!({"integration":"microsoft-graph","generation":1,"selection_generation":1,"targets":[]}),
                vec![
                    "integrations",
                    "select-calendars",
                    "microsoft-graph",
                    "--generation",
                    "1",
                    "--selection-generation",
                    "1",
                    "--spec-stdin",
                ],
            ),
            (
                "remove_selected_calendar",
                json!({"account":"not-an-id","generation":1,"selection_generation":1,"target":{"kind":"own","calendar_id":"synthetic"}}),
                vec![
                    "integrations",
                    "remove-calendar",
                    "microsoft-graph",
                    "--account",
                    "not-an-id",
                    "--generation",
                    "1",
                    "--selection-generation",
                    "1",
                    "--spec-stdin",
                ],
            ),
            (
                "sync_calendars",
                json!({"integration":"system-calendar"}),
                vec!["integrations", "sync", "system-calendar"],
            ),
            (
                "apply_directory_email",
                json!({"person":"Nobody Invented","email":"invented@example.invalid","account_generation":"invalid"}),
                vec![
                    "people",
                    "edit",
                    "Nobody Invented",
                    "--email",
                    "invented@example.invalid",
                    "--expect-generation",
                    "invalid",
                ],
            ),
        ] {
            same_cli(&client, &root, tool, args, &command).await;
        }

        for (tool, command) in [
            ("get_library_disk_usage", vec!["library", "disk-usage"]),
            ("get_setup_model_check", vec!["setup", "model-check"]),
        ] {
            let mut answer = ok(&client, tool, empty()).await;
            let mut expected = cli(&deps(&root), &library, &command).1;
            answer.as_object_mut().unwrap().remove("free_bytes");
            expected.as_object_mut().unwrap().remove("free_bytes");
            assert_eq!(answer, expected, "{tool}");
        }

        // Stdin-backed inputs are handled by the MCP request and never read
        // from the JSON-RPC transport.
        let found = ok(
            &client,
            "find_transcript",
            json!({"meeting_id":id,"query":"roadmap"}),
        )
        .await;
        assert_eq!(
            found,
            cli_with_input(&root, &["transcript", id, "--find-stdin"], "roadmap")
        );
        let m = Library::open_existing(&library)
            .unwrap()
            .load_meeting(&meeting)
            .unwrap();
        let fingerprint = broscribe_core::ops::segment_fingerprint(&m.segments[1]);
        let named = ok(
            &client,
            "attribute_transcript_lines",
            json!({"meeting_id":id,"fingerprints":[fingerprint],"person":"Pia Invented"}),
        )
        .await;
        assert!(named.is_object());
        let cleared = ok(
            &client,
            "attribute_transcript_lines",
            json!({"meeting_id":id,"fingerprints":[fingerprint],"clear":true}),
        )
        .await;
        assert!(cleared.is_object());
        assert_eq!(
            cli_with_input(
                &root,
                &[
                    "transcript",
                    "attribute",
                    id,
                    "--fingerprints-stdin",
                    "--clear"
                ],
                &json!({"fingerprints":[fingerprint]}).to_string()
            )["meeting_id"],
            cleared["meeting_id"]
        );

        let audio_error = call(
            &client,
            "run_audio_check",
            json!({"echo_handling":"full_aec","save":false}),
        )
        .await;
        assert_eq!(
            audio_error.structured_content.unwrap()["error"]["code"],
            cli(
                &deps(&root),
                &library,
                &["audio-check", "--echo-handling", "full_aec"]
            )
            .1["error"]["code"]
        );
        // Compare install and then uninstall to the same operation with a
        // second isolated root, because their outputs include paths.
        let install = call(&client, "install_agent_tools", empty()).await;
        let install_cli = cli(&deps(&root), &library, &["agents", "install"]);
        let actual = install.structured_content.unwrap();
        if install.is_error == Some(true) {
            assert_eq!(actual["error"]["code"], install_cli.1["error"]["code"]);
        } else {
            assert!(actual.is_object());
        }

        let plan = ok(
            &client,
            "enroll_voice",
            json!({"meeting_id":id,"speaker_id":"S1"}),
        )
        .await;
        assert_eq!(plan["confirmed"], false);
        assert_eq!(plan["plan"]["source"]["kind"], "meeting");

        let switched = root.join("next-library");
        Library::open(&switched).unwrap();
        Config::update(&root.join("data"), |config| {
            config.library = Some(switched);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            ok(&client, "get_library_folder", empty()).await["library_folder"],
            library.canonicalize().unwrap().to_str().unwrap()
        );
        client.cancel().await.unwrap();
    });
}

#[test]
fn mcp_uses_bounded_inline_contact_words_and_manifest_inputs() {
    with_parity_client(|client, root, _| async move {
        let source = root.join("inputs");
        std::fs::create_dir(&source).unwrap();
        let text_file = source.join("synthetic-private-filename.txt");
        let content = "Synthetic words and contacts withheld";
        std::fs::write(&text_file, content).unwrap();
        let recording = source.join("synthetic.wav");
        std::fs::write(&recording, []).unwrap();

        for (tool, args) in [
            ("add_words", json!({"path":text_file})),
            ("import_contacts", json!({"path":text_file})),
            ("enroll_voice", json!({"path":text_file,"confirm":true})),
            ("try_setup_transcription", json!({"path":text_file})),
            ("preview_import", json!({"manifest":text_file})),
            ("import_recording", json!({"manifest":text_file})),
            ("import_recording", json!({"path":text_file})),
            ("add_words", json!({"words":vec!["hint"; 1001]})),
            ("add_words", json!({"words":["x".repeat(257)]})),
            ("add_words", json!({"words":["two\nlines"]})),
            (
                "import_contacts",
                json!({"contacts":vec![json!({"name":"Example"}); 1001]}),
            ),
            (
                "import_contacts",
                json!({"contacts":[{"name":"x".repeat(257)}]}),
            ),
            (
                "import_contacts",
                json!({"contacts":[{"name":"Example","emails":vec!["a@example.invalid"; 17]}]}),
            ),
            (
                "import_contacts",
                json!({"contacts":[{"name":"Unsaved Example"},{"name":"Bad\nName"}]}),
            ),
        ] {
            let result = call(&client, tool, args).await;
            assert_eq!(result.is_error, Some(true), "{tool}: {result:?}");
            assert!(
                !format!("{result:?}").contains(content),
                "{tool}: {result:?}"
            );
        }
        assert_eq!(
            ok(&client, "get_word_list", json!({})).await["words"],
            json!([])
        );
        assert_eq!(
            ok(&client, "list_people", json!({})).await["people"],
            json!([])
        );

        let words = ok(
            &client,
            "add_words",
            json!({"words":["Acorn release","  acorn release  ","# heading"]}),
        )
        .await;
        assert_eq!(words["words_added"], 1);
        assert_eq!(
            ok(&client, "get_word_list", json!({})).await["words"],
            json!(["Acorn release"])
        );
        let people = ok(
            &client,
            "import_contacts",
            json!({"contacts":[{"name":"Robin Example","emails":["robin@example.invalid"]}]}),
        )
        .await;
        assert_eq!(
            people["people"],
            json!({"contacts":1,"created":1,"updated":0})
        );

        let preview = ok(&client, "preview_import", json!({"path":source})).await;
        assert_eq!(preview["recordings"], json!([recording]));
        assert_eq!(preview["skipped_count"], 1);
        assert!(preview.get("skipped").is_none());
        assert!(!preview.to_string().contains("synthetic-private-filename"));

        let manifest = json!({
            "format":"broscribe-import-manifest","version":1,
            "meetings":[{"audio_path":recording},{"audio_path":text_file}],
        });
        let preview = ok(&client, "preview_import", json!({"manifest":manifest})).await;
        assert_eq!(preview["kind"], "manifest");
        assert_eq!(preview["recordings"], json!([recording]));
        assert_eq!(preview["skipped_count"], 1);
        assert!(!preview.to_string().contains("synthetic-private-filename"));
        assert_eq!(std::fs::read_to_string(text_file).unwrap(), content);
        client.cancel().await.unwrap();
    });
}

#[test]
fn mcp_cannot_select_an_unverified_model_through_a_profile_or_live_companion() {
    with_parity_client(|client, root, _| async move {
        let data = root.join("data");
        let registry = data.join(broscribe_models::registry::USER_REGISTRY_FILE);
        let before = Config::load(&data).unwrap().profile;

        let block = |id: &str, roles: &str| {
            format!(
                r#"
[[model]]
id = "{id}"
roles = [{roles}]
runtime = "onnx"
license = "Synthetic unverified licence"
license_verified = false
"#
            )
        };
        std::fs::write(
            &registry,
            block("whisper-large-v3-turbo", r#""asr", "live""#),
        )
        .unwrap();
        let pull = call(
            &client,
            "pull_model",
            json!({"id":"whisper-large-v3-turbo","allow_unverified_license":true}),
        )
        .await;
        assert_eq!(pull.is_error, Some(true), "{pull:?}");
        let profile = call(
            &client,
            "configure_model_profile",
            json!({"profile":"multilingual"}),
        )
        .await;
        assert_eq!(profile.is_error, Some(true), "{profile:?}");
        assert!(
            profile.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("not verified"),
            "{profile:?}"
        );

        std::fs::write(&registry, block("parakeet-tdt-0.6b-v3", r#""asr", "live""#)).unwrap();
        let use_model = call(
            &client,
            "use_model",
            json!({"role":"asr","id":"whisper-large-v3-turbo"}),
        )
        .await;
        assert_eq!(use_model.is_error, Some(true), "{use_model:?}");
        assert!(
            use_model.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("not verified"),
            "{use_model:?}"
        );
        assert_eq!(Config::load(&data).unwrap().profile, before);
        client.cancel().await.unwrap();
    });
}

#[test]
fn mcp_refuses_confirmed_and_default_move_imports() {
    use broscribe_core::schema::Handling;
    with_parity_client(|client, root, _| async move {
        let data = root.join("data");
        let path = root.join("synthetic.wav");
        std::fs::write(&path, b"synthetic fixture").unwrap();
        for (tool, args) in [
            ("set_import_handling", json!({"handling":"move"})),
            ("import_recording", json!({"path":path,"handling":"move"})),
            ("preview_import", json!({"path":path,"handling":"move"})),
        ] {
            let result = call(&client, tool, args).await;
            assert_eq!(result.is_error, Some(true), "{tool}: {result:?}");
            let text = &result.content[0].as_text().unwrap().text;
            assert!(
                text.contains("copy") && text.contains("reference"),
                "{text}"
            );
        }
        Config::update(&data, |config| {
            config.import_handling = Handling::Move;
            Ok(())
        })
        .unwrap();
        let manifest = json!({
            "format":"broscribe-import-manifest","version":1,
            "meetings":[{"audio_path":path}],
        });
        for (tool, args) in [
            ("import_recording", json!({"path":path})),
            ("import_recording", json!({"manifest":manifest})),
            ("preview_import", json!({"path":path})),
        ] {
            let blocked = call(&client, tool, args).await;
            assert_eq!(blocked.is_error, Some(true), "{blocked:?}");
            assert!(
                blocked.content[0]
                    .as_text()
                    .unwrap()
                    .text
                    .contains("Copy or Reference")
            );
        }
        for handling in ["copy", "reference"] {
            let value = ok(
                &client,
                "preview_import",
                json!({"path":path,"handling":handling}),
            )
            .await;
            assert_eq!(value["count"], 1);
        }
        assert_eq!(std::fs::read(path).unwrap(), b"synthetic fixture");
        client.cancel().await.unwrap();
    });
}

#[test]
fn mcp_exports_refuse_hidden_mismatched_and_internal_destinations() {
    with_parity_client(|client, root, id| async move {
        let data = root.join("data");
        let library = root.join("library");
        let blocked = vec![
            root.join(".bashrc"),
            root.join(".visible-extension.md"),
            root.join("wrong.txt"),
            library.join("internal.md"),
            data.join("internal.md"),
        ];
        #[cfg(unix)]
        let blocked = {
            let mut blocked = blocked;
            let alias = root.join("data-alias");
            std::os::unix::fs::symlink(&data, &alias).unwrap();
            blocked.push(alias.join("internal.md"));
            blocked
        };
        for out in blocked {
            let result = call(
                &client,
                "export_meeting_file",
                json!({"meeting_id":id,"format":"md","out":out}),
            )
            .await;
            assert_eq!(
                result.structured_content.unwrap()["error"]["code"],
                "invalid_input",
                "{out:?}"
            );
            assert!(!out.exists(), "{out:?}");
        }
        let stdout = call(
            &client,
            "export_meeting_file",
            json!({"meeting_id":id,"format":"md","out":"-"}),
        )
        .await;
        assert_eq!(
            stdout.structured_content.unwrap()["error"]["code"],
            "invalid_input"
        );
        for out in [
            root.join(".clip.wav"),
            root.join("clip.txt"),
            data.join("clip.wav"),
            library.join("clip.wav"),
        ] {
            let result = call(
                &client,
                "export_meeting_clip",
                json!({"meeting_id":id,"from":"0","to":"1","out":out}),
            )
            .await;
            assert_eq!(
                result.structured_content.unwrap()["error"]["code"],
                "invalid_input",
                "{out:?}"
            );
            assert!(!out.exists(), "{out:?}");
        }
        client.cancel().await.unwrap();
    });
}

#[test]
fn mcp_meeting_edit_history_keeps_more_than_the_cli_limit() {
    with_parity_client(|client, _, id| async move {
        for index in 0..21 {
            ok(
                &client,
                "edit_meeting_session",
                json!({"edit":{
                    "action":"rename","meeting":id,"title":format!("Synthetic edit {index}"),
                }}),
            )
            .await;
        }
        for index in 0..21 {
            let outcome = ok(
                &client,
                "edit_meeting_session",
                json!({"edit":{"action":"undo"}}),
            )
            .await;
            assert_eq!(outcome["can_undo"], index < 20, "{index}: {outcome}");
        }
        assert_eq!(
            ok(&client, "get_meeting", json!({"meeting_id":id})).await["title"],
            "Pricing sync"
        );
        client.cancel().await.unwrap();
    });
}

#[test]
fn mcp_agent_commands_and_events_refuse_a_different_library() {
    use broscribe_ipc::{Call, PROTOCOL_VERSION, Request, Response};
    use broscribe_platform_native::ipc::Listener;
    use std::time::Duration;

    with_parity_client(|client, root, _| async move {
        let other = root.join("different-library");
        Library::open(&other).unwrap();
        let data = root.join("data");
        Config::update(&data, |config| {
            config.library = Some(other.clone());
            Ok(())
        })
        .unwrap();
        let mut listener = Listener::bind(&data).unwrap();
        let active = json!({"session_id":"s-synthetic"});
        let refused = [
            ("get_live_preview", active.clone()),
            ("poll_daemon_events", json!({})),
            ("get_recording_status", json!({})),
            ("list_recording_prompts", json!({})),
            ("start_recording", json!({})),
            ("pause_recording", active.clone()),
            ("resume_recording", active.clone()),
            ("stop_recording", active.clone()),
            ("process_recording_now", active.clone()),
            ("bookmark_recording", active.clone()),
            (
                "note_recording",
                json!({"session_id":"s-synthetic","text":"Example"}),
            ),
            (
                "set_recording_language",
                json!({"session_id":"s-synthetic","language":"en"}),
            ),
            ("tag_recording", active.clone()),
            (
                "set_recording_participant",
                json!({"session_id":"s-synthetic","action":"confirm","person":"Example"}),
            ),
            (
                "set_recording_title",
                json!({"session_id":"s-synthetic","title":"Example"}),
            ),
            (
                "set_recording_auto_stop",
                json!({"session_id":"s-synthetic","enabled":true}),
            ),
            ("retry_recording", active.clone()),
            (
                "answer_recording_prompt",
                json!({"prompt_id":"example","action":"record"}),
            ),
            (
                "attribute_live_turn",
                json!({"session_id":"s-synthetic","turn_id":"turn","revision":1,"epoch":1,"evidence":"evidence","clear":true}),
            ),
            (
                "answer_agent_notice",
                json!({"notice_id":"example","action":"record"}),
            ),
        ];
        let refused_count = refused.len();
        let server = tokio::spawn(async move {
            let state = |library: &Path| {
                json!({
                    "pid":123,"version":"synthetic","protocol":PROTOCOL_VERSION,
                    "library":library,"recording":null,"jobs":{"running":null,"waiting":0,"deferred":0},
                    "preview":{"state":"off"},
                })
            };
            for _ in 0..refused_count {
                let connection = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
                let (mut read, mut write) = connection.into_split();
                let request = Request::parse(&read.read_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(request.call, Call::Status);
                write
                    .write_line(
                        Response {
                            id: request.id,
                            outcome: Ok(state(&other)),
                        }
                        .to_line()
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                assert!(
                    read.read_line().await.unwrap().is_none(),
                    "foreign agent received a follow-up"
                );
            }
            let connection = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let (mut read, mut write) = connection.into_split();
            let request = Request::parse(&read.read_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(request.call, Call::Status);
            let same = data.parent().unwrap().join("library");
            write
                .write_line(
                    Response {
                        id: request.id,
                        outcome: Ok(state(&same)),
                    }
                    .to_line()
                    .unwrap(),
                )
                .await
                .unwrap();
            let request = Request::parse(
                &read
                    .read_line()
                    .await
                    .unwrap()
                    .expect("record.start on verified connection"),
            )
            .unwrap();
            assert!(matches!(request.call, Call::RecordStart(_)));
            write
                .write_line(
                    Response {
                        id: request.id,
                        outcome: Ok(json!({"started":true})),
                    }
                    .to_line()
                    .unwrap(),
                )
                .await
                .unwrap();
        });
        for (tool, input) in refused {
            let result = call(&client, tool, input).await;
            assert_eq!(result.is_error, Some(true), "{tool}");
            assert_eq!(
                result.structured_content.unwrap()["error"]["code"],
                "library_changed",
                "{tool}"
            );
            assert!(
                result.content[0]
                    .as_text()
                    .unwrap()
                    .text
                    .contains("Reconnect")
            );
        }
        assert_eq!(
            ok(&client, "start_recording", json!({})).await["started"],
            true
        );
        server.await.unwrap();
        client.cancel().await.unwrap();
    });
}
