//! `broscribe diagnostics` and MCP `get_diagnostics` on synthetic fixtures:
//! real counts from meetings, the queue, the index and the recording
//! journals; one collector for both; unique markers in every sensitive
//! field never reach either report; missing and corrupt inputs are
//! reported, never created.

use std::path::{Path, PathBuf};

use broscribe_capture::journal::{
    self, ChunkEntry, MANIFEST_VERSION, SessionManifest, SessionMeta, SessionState, TrackJournal,
};
use broscribe_cli::Deps;
use broscribe_cli::mcp::serve_on;
use broscribe_core::config::Config;
use broscribe_core::index::Index;
use broscribe_core::queue::{JOB_JSON, JobId, JobState, Queue};
use broscribe_core::schema::{
    Handling, Meeting, Note, Participant, ProcessingStatus, Recording, RecordingState, Segment,
    Source, Track, TrackRole,
};
use broscribe_core::{Library, SpeakerId};
use broscribe_dsp::{Alignment, EchoHandling, EchoReport};
use broscribe_models::fakes::FakeModelProvider;
use broscribe_platform::DecoderChain;
use broscribe_platform::fakes::{
    FakePaths, FakeProcessProbe, FakeProcessTreeControl, FakeTrustStore,
};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

/// In every sensitive field, path and identifier of the fixtures.
const MARK: &str = "Zq7SECRET";

/// `root/data` is the CLI's data folder (FakePaths).
fn deps(root: &Path) -> Deps {
    Deps {
        clock: broscribe_cli::system_clock(),
        paths: Box::new(FakePaths::new(root)),
        updates: broscribe_cli::UpdateDeps::with_transport(
            std::sync::Arc::new(broscribe_platform::update::FakeUpdatePlatform::new(
                root.join("install"),
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
        agents: broscribe_cli::AgentEnv::isolated(root),
        execution: broscribe_models::ExecutionContext::new(
            std::sync::Arc::new(FakeProcessTreeControl::default()),
            std::sync::Arc::new(FakeTrustStore::default()),
        ),
    }
}

/// `broscribe [--json] [--library <lib>] diagnostics`: exit code and stdout.
fn diagnostics(deps: &Deps, lib: Option<&Path>, json: bool) -> (i32, String) {
    let mut argv: Vec<std::ffi::OsString> = vec!["broscribe".into()];
    if json {
        argv.push("--json".into());
    }
    if let Some(lib) = lib {
        argv.push("--library".into());
        argv.push(lib.into());
    }
    argv.push("diagnostics".into());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = broscribe_cli::run(argv, deps, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap())
}

fn report(deps: &Deps, lib: Option<&Path>) -> Value {
    let (code, out) = diagnostics(deps, lib, true);
    assert_eq!(code, 0, "{out}");
    serde_json::from_str(&out).unwrap()
}

/// Every file and folder under `root` with its size: what a read must not
/// change. SQLite's own `-wal`/`-shm` companions of the index (which a
/// read-only connection may leave) are not BroScribe files.
fn tree(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with("-wal") || name.ends_with("-shm") {
                continue;
            }
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            }
            out.push((entry.path(), if meta.is_dir() { 0 } else { meta.len() }));
        }
    }
    out.sort();
    out
}

fn secret(what: &str) -> String {
    format!("{MARK}-{what}")
}

/// A recorded meeting with every sensitive field marked, two tracks,
/// marked model IDs, a recorded error and a degraded echo report.
fn recorded_meeting(lib: &Library) -> Meeting {
    let started = chrono::DateTime::parse_from_rfc3339("2026-10-05T14:00:00-03:00").unwrap();
    let mut m = Meeting::new(lib.new_meeting_id().unwrap(), Source::Recording, started);
    m.title = Some(secret("title"));
    m.source_id = Some(secret("source-id"));
    m.app = Some(secret("app"));
    m.setup = Some(secret("setup"));
    m.tags = vec![secret("tag")];
    m.hotwords = vec![secret("hotword")];
    m.notes = vec![Note {
        at_s: 1.0,
        text: secret("note"),
    }];
    m.participants = vec![Participant {
        name: Some(secret("participant")),
        email: Some(format!("{MARK}@example.invalid")),
        ..Participant::default()
    }];
    m.tracks = vec![
        Track {
            id: secret("mic-track"),
            role: TrackRole::Mic,
        },
        Track {
            id: secret("system-track"),
            role: TrackRole::System,
        },
    ];
    m.segments = vec![Segment {
        speech_overlap: Vec::new(),
        id: 0,
        track: secret("mic-track"),
        speaker: Some(SpeakerId::new(1).unwrap()),
        start: 0.0,
        end: 2.0,
        text: secret("transcript"),
        words: Vec::new(),
    }];
    m.recording = Some(Recording {
        state: RecordingState::Available,
        handling: Handling::Copy,
        path: PathBuf::from(secret("audio.m4a")),
        original_path: Some(PathBuf::from(format!("/{MARK}/original.m4a"))),
        sha256: secret("sha"),
        bytes: 1,
        source_sha256: None,
        sources: Vec::new(),
        retention_override: None,
        delete_after: None,
    });
    m.models.vad = Some(secret("vad-model"));
    m.models.asr = Some(secret("asr-model"));
    m.models.diarization = Some(secret("diarization-model"));
    m.models.speaker_embedding = Some(secret("embedding-model"));
    m.processing.status = ProcessingStatus::Done;
    m.processing.duration_ms = Some(1_234);
    m.processing.error = Some(format!("{MARK}: provider stderr at /{MARK}/path"));
    m.processing.echo_report = Some(
        serde_json::to_value(EchoReport {
            requested: EchoHandling::FullAec,
            applied: EchoHandling::ResidueGating,
            degraded: true,
            alignment: Alignment::zero(),
            erle_db: None,
            gated_fraction: 0.25,
            system_active_fraction: 0.5,
            reason: Some(secret("echo-reason")),
        })
        .unwrap(),
    );
    m
}

/// An imported single-track meeting on the profile's ASR model, failed,
/// with an echo report that is not one.
fn imported_meeting(lib: &Library) -> Meeting {
    let started = chrono::DateTime::parse_from_rfc3339("2026-10-06T09:00:00-03:00").unwrap();
    let mut m = Meeting::new(lib.new_meeting_id().unwrap(), Source::Import, started);
    m.title = Some(secret("imported-title"));
    m.tracks = vec![Track {
        id: "mixed".into(),
        role: TrackRole::Mixed,
    }];
    m.models.asr = Some(Config::default().profile.asr);
    m.processing.status = ProcessingStatus::Failed;
    m.processing.echo_report = Some(json!({ "reason": secret("invalid-echo") }));
    m
}

/// The library: two meetings (indexed), one corrupt `meeting.json`, two
/// jobs (one failed with attempts left, one of a kind this version does not
/// run) and an unreadable job record.
fn seed_library(root: &Path) {
    let lib = Library::open(root).unwrap();
    let recorded = recorded_meeting(&lib);
    let imported = imported_meeting(&lib);
    lib.write(|tx| {
        for m in [&recorded, &imported] {
            tx.create_meeting_dir(m)?;
            tx.save_meeting(m)?;
            Index::open(tx.root())?.upsert_meeting(tx, m)?;
        }
        let queue = Queue::open(tx.root())?;
        let failed = queue.enqueue(
            tx,
            "import",
            &secret("job-key"),
            json!({ "path": format!("/{MARK}/input.m4a") }),
        )?;
        queue.enqueue(tx, &secret("kind"), &secret("other-key"), json!({}))?;
        let mut job = queue.get(&failed)?;
        job.state = JobState::Failed;
        job.attempts = 1;
        job.error = Some(secret("job-error"));
        std::fs::write(
            queue.job_dir(&failed).join(JOB_JSON),
            serde_json::to_vec(&job).unwrap(),
        )
        .unwrap();
        let unreadable = queue.job_dir(&JobId::for_key("import", &secret("corrupt")));
        std::fs::create_dir_all(&unreadable).unwrap();
        std::fs::write(unreadable.join(JOB_JSON), secret("not json")).unwrap();
        Ok(())
    })
    .unwrap();
    let corrupt = root
        .join("2026")
        .join("10")
        .join(secret("corrupt-m-abcdef"));
    std::fs::create_dir_all(&corrupt).unwrap();
    std::fs::write(corrupt.join("meeting.json"), secret("{ not json")).unwrap();
}

/// One stopped journal with marked metadata, a capture gap, a recovered and
/// an open chunk and dropped system frames; one unreadable manifest.
fn seed_journals(data_dir: &Path) {
    let root = broscribe_core::lifecycle::journal_root(data_dir);
    let chunk = |seq, frames, after_gap, recovered| ChunkEntry {
        seq,
        segment: 0,
        host_start_ns: seq * 5_000_000_000,
        frames,
        after_gap,
        recovered,
    };
    let segment = journal::Segment {
        index: 0,
        generation: 0,
        device: Some(secret("device")),
        device_id: Some(secret("device-id")),
        sample_rate: 48_000,
        started_ns: 0,
    };
    let manifest = SessionManifest {
        version: MANIFEST_VERSION,
        session_id: secret("session"),
        state: SessionState::Stopped,
        owner_pid: 1,
        started_at: chrono::Utc::now(),
        stopped_at: Some(chrono::Utc::now()),
        stopped_ns: Some(15_000_000_000),
        stop_reason: Some(secret("stop-reason")),
        chunk_seconds: 5.0,
        meta: SessionMeta {
            app: Some(secret("journal-app")),
            title: Some(secret("journal-title")),
            tags: vec![secret("journal-tag")],
            pinned_mic: Some(secret("pinned-mic")),
            ..SessionMeta::default()
        },
        mic: TrackJournal {
            segments: vec![segment.clone()],
            chunks: vec![
                chunk(0, Some(240_000), false, false),
                chunk(1, Some(240_000), true, true),
                chunk(2, None, false, false),
            ],
            dropped_frames: 0,
        },
        system: TrackJournal {
            segments: vec![segment],
            chunks: vec![chunk(0, Some(240_000), false, false)],
            dropped_frames: 120,
        },
        pauses: Vec::new(),
        languages: Vec::new(),
        bookmarks: vec![journal::Bookmark {
            at_ns: 1,
            label: Some(secret("bookmark")),
        }],
        notes: vec![journal::Note {
            at_ns: 1,
            text: secret("journal-note"),
        }],
        participants: vec![journal::Participant {
            person_ref: secret("person-ref"),
            name: Some(secret("journal-participant")),
            email: Some(format!("{MARK}-journal@example.invalid")),
            evidence: vec![secret("evidence")],
            removed: false,
        }],
    };
    let dir = root.join(secret("session"));
    std::fs::create_dir_all(&dir).unwrap();
    journal::write_manifest(&dir, &manifest).unwrap();
    let corrupt = root.join(secret("corrupt-session"));
    std::fs::create_dir_all(&corrupt).unwrap();
    std::fs::write(corrupt.join(journal::MANIFEST_FILE), secret("{")).unwrap();
}

/// The MCP server's `get_diagnostics` answer (structured content) and the
/// whole tool result as JSON text.
fn mcp_diagnostics(deps: &Deps, lib: PathBuf, data_dir: PathBuf) -> (Value, String) {
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    let client = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let client = ().serve(client_io).await.unwrap();
                let tools = client.list_all_tools().await.unwrap();
                let tool = tools
                    .iter()
                    .find(|t| t.name == "get_diagnostics")
                    .expect("get_diagnostics is listed");
                assert_eq!(
                    tool.annotations.as_ref().and_then(|a| a.read_only_hint),
                    Some(true)
                );
                let result = client
                    .call_tool(CallToolRequestParams::new("get_diagnostics"))
                    .await
                    .unwrap();
                assert_ne!(result.is_error, Some(true), "{:?}", result.content);
                let text = serde_json::to_string(&result).unwrap();
                client.cancel().await.unwrap();
                (result.structured_content.expect("structured content"), text)
            })
    });
    let (reader, writer) = tokio::io::split(server_io);
    serve_on(deps, lib, data_dir, reader, writer).unwrap();
    client.join().unwrap()
}

#[test]
fn counts_are_real_identical_over_cli_and_mcp_and_carry_no_private_data() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(secret("root"));
    let lib_dir = root.join(secret("library"));
    let data_dir = root.join("data");
    seed_library(&lib_dir);
    seed_journals(&data_dir);
    let deps = deps(&root);
    // An initialized profile already has its stable admission inode.
    drop(
        broscribe_core::maintenance::ModeLease::acquire(&data_dir, std::time::Duration::ZERO)
            .unwrap(),
    );
    Config::update(&data_dir, |config| {
        config.mcp.enabled = Some(true);
        Ok(())
    })
    .unwrap();
    let before = tree(&root);

    let v = report(&deps, Some(&lib_dir));
    assert_eq!(v["schema"], 1);
    assert_eq!(v["software"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["software"]["protocol"], broscribe_ipc::PROTOCOL_VERSION);
    assert_eq!(v["config"]["state"], "written");

    let lib = &v["library"];
    assert_eq!(lib["availability"], "available", "{v:#}");
    let m = &lib["meetings"];
    assert_eq!(m["availability"], "available");
    assert_eq!(m["folders"], 3);
    assert_eq!(m["readable"], 2);
    assert_eq!(m["unreadable"], json!({ "schema": 1 }));
    assert_eq!(m["status"]["done"], 1);
    assert_eq!(m["status"]["failed"], 1);
    assert_eq!(m["source"], json!({ "import": 1, "recording": 1 }));
    assert_eq!(m["recording"]["none"], 1);
    assert_eq!(m["recording"]["available"], 1);
    assert_eq!(m["recording"]["copied"], 1);
    assert_eq!(m["tracks"]["mic"], 1);
    assert_eq!(m["tracks"]["system"], 1);
    assert_eq!(m["tracks"]["mixed"], 1);
    assert_eq!(m["tracks"]["multitrack_meetings"], 1);
    assert_eq!(
        m["processing"],
        json!({ "duration_recorded": 1, "duration_not_recorded": 1, "with_error": 1 })
    );
    assert_eq!(
        m["models"]["vad"],
        json!({ "recorded": 1, "not_recorded": 1, "distinct": 1, "current_profile": 0 })
    );
    assert_eq!(
        m["models"]["asr"],
        json!({ "recorded": 2, "not_recorded": 0, "distinct": 2, "current_profile": 1 })
    );
    let echo = &m["echo"];
    assert_eq!(echo["reports"], 2);
    assert_eq!(echo["invalid"], 1);
    assert_eq!(echo["not_recorded"], 0);
    assert_eq!(echo["degraded"], 1);
    assert_eq!(echo["requested"]["full_aec"], 1);
    assert_eq!(echo["applied"]["residue_gating"], 1);
    // Saved meetings keep no gap counts: unknown, never 0.
    assert_eq!(m["history"]["capture_gaps"], "not_recorded");
    assert_eq!(m["history"]["dropped_frames"], "not_recorded");

    assert_eq!(lib["index"]["availability"], "available");
    assert_eq!(lib["index"]["database"], true);

    let q = &lib["queue"];
    assert_eq!(q["availability"], "available", "{q:#}");
    assert_eq!(q["jobs"], 2);
    assert_eq!(q["invalid"], 1);
    assert_eq!(q["state"]["failed"], 1);
    assert_eq!(q["state"]["queued"], 1);
    assert_eq!(q["retry_pending"], 1);
    assert_eq!(q["failed_final"], 0);
    assert_eq!(q["retried"], 1);
    assert_eq!(q["kinds"], json!({ "import": 1, "other": 1 }));

    let j = &v["journals"];
    assert_eq!(j["availability"], "available", "{j:#}");
    assert_eq!(j["sessions"], 1);
    assert_eq!(j["unreadable"], json!({ "schema": 1 }));
    assert_eq!(j["stopped"], 1);
    assert_eq!(j["mic"]["chunks"], 3);
    assert_eq!(j["mic"]["gaps"], 1);
    assert_eq!(j["mic"]["recovered_chunks"], 1);
    assert_eq!(j["mic"]["open_chunks"], 1);
    assert_eq!(j["mic"]["sessions_with_gaps"], 1);
    assert_eq!(j["system"]["dropped_frames"], 120);
    assert_eq!(j["system"]["sessions_with_dropped_frames"], 1);
    assert_eq!(j["system"]["gaps"], 0);

    // Nothing is installed in the synthetic data folder: every profile
    // role is reported missing, without loading anything.
    let roles = v["models"]["roles"].as_array().unwrap();
    assert!(!roles.is_empty());
    assert!(roles.iter().all(|r| r["state"] == "missing"), "{roles:?}");

    assert_eq!(v["agent"]["availability"], "not_running", "{v:#}");
    let names: Vec<&str> = v["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "encoder",
            "playback",
            "microphone",
            "system_audio",
            "meeting_readers",
            "agent_ipc",
            "window_titles"
        ]
    );
    for c in v["capabilities"].as_array().unwrap() {
        // Nothing is opened: runtime is never claimed without an agent.
        assert_eq!(c["runtime"], "not_checked", "{c}");
    }
    let mic = &v["capabilities"][2];
    assert_eq!(mic["permission"], "unknown");
    assert_eq!(mic["availability"], "unknown");

    let json_text = serde_json::to_string(&v).unwrap();
    assert!(!json_text.contains(MARK), "{json_text}");
    let (code, human) = diagnostics(&deps, Some(&lib_dir), false);
    assert_eq!(code, 0, "{human}");
    assert!(human.contains("BroScribe"), "{human}");
    assert!(!human.contains(MARK), "{human}");

    // MCP runs the same collector on the same library and data folder.
    let (mcp, mcp_text) = mcp_diagnostics(&deps, lib_dir.clone(), data_dir.clone());
    assert!(!mcp_text.contains(MARK), "{mcp_text}");
    assert_eq!(mcp, v);

    assert_eq!(tree(&root), before, "a read changed files");
}

#[test]
fn missing_and_unreadable_inputs_are_reported_never_created() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let deps = deps(&root);
    let lib_dir = root.join("no-library");

    let v = report(&deps, Some(&lib_dir));
    assert_eq!(v["library"]["availability"], "absent");
    assert_eq!(v["journals"]["availability"], "absent");
    assert_eq!(v["config"]["state"], "defaults");
    assert_eq!(v["agent"]["availability"], "not_running");
    assert!(
        !lib_dir.exists(),
        "diagnostics must not bootstrap its input library"
    );
    let data_dir = root.join("data");
    let root_entries: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(root_entries.as_slice(), std::slice::from_ref(&data_dir));
    let mut control_entries: Vec<_> = std::fs::read_dir(&data_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    control_entries.sort();
    assert_eq!(
        control_entries,
        [
            data_dir.join(broscribe_core::maintenance::PROFILE_MARKER),
            data_dir.join(broscribe_core::maintenance::LOCK_FILE),
        ],
        "only profile admission metadata may be created"
    );

    // Unreadable settings: the library they name is unknown (no default
    // library is guessed), and so is the profile.
    std::fs::create_dir_all(root.join("library").join("people")).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(Config::path(&data_dir), "[unterminated").unwrap();
    let v = report(&deps, None);
    assert_eq!(v["config"]["state"], "unreadable");
    assert!(v["config"]["code"].is_string());
    assert_eq!(
        v["library"],
        json!({ "availability": "unknown", "reason": "config_unreadable" })
    );
    assert_eq!(
        v["models"],
        json!({ "availability": "unknown", "reason": "config_unreadable" })
    );

    // An explicit library is still read; its absent queue and empty index
    // stay absent.
    let v = report(&deps, Some(&root.join("library")));
    assert_eq!(v["library"]["availability"], "available");
    assert_eq!(v["library"]["queue"]["availability"], "absent");
    assert_eq!(v["library"]["meetings"]["folders"], 0);
    assert_eq!(v["library"]["index"]["database"], false);
    assert!(!root.join("library").join(".queue").exists());
    assert!(!root.join("library").join(".index").exists());
}
