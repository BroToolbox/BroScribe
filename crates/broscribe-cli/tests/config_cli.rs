//! Config-only commands write only the data directory's config (a mistyped
//! `--library` is never created), a detection policy stays saved and is the
//! result when the running agent refuses to reload, and `queue status`
//! ranks every job from one snapshot. In process, with a temporary data
//! folder, a fake agent socket and synthetic jobs.
#![cfg(unix)]

use std::ffi::OsString;
use std::path::Path;

use broscribe_cli::{Deps, run};
use broscribe_core::Library;
use broscribe_core::config::Config;
use broscribe_core::queue::{JobId, Queue, VECTORS_KIND};
use broscribe_models::fakes::FakeModelProvider;
use broscribe_platform::DecoderChain;
use broscribe_platform::fakes::{
    FakeMicCapture, FakePaths, FakeProcessProbe, FakeProcessTreeControl, FakeSecretStore,
    FakeTrustStore,
};
use serde_json::{Value, json};

fn fixture(root: &Path) -> Deps {
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
        decoder: DecoderChain::new(vec![]),
        process: Box::new(FakeProcessProbe::new(std::process::id())),
        mic: Box::new(FakeMicCapture::new(16_000)),
        secrets: std::sync::Arc::new(FakeSecretStore::default()),
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

/// `broscribe --json <args>`: exit code, the JSON document on `out` and
/// the injected error stream.
fn command(deps: &Deps, args: &[&str]) -> (i32, Value, String) {
    let argv = ["broscribe", "--json"]
        .into_iter()
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    let (mut out, mut err) = (vec![], vec![]);
    let code = run(argv, deps, &mut out, &mut err);
    let value = serde_json::from_slice(&out)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out)));
    (code, value, String::from_utf8(err).unwrap())
}

#[test]
fn speaker_grouping_settings_share_presets_and_validate_custom_percentages() {
    let dir = tempfile::tempdir().unwrap();
    let deps = fixture(dir.path());
    let (code, standard, _) = command(&deps, &["settings", "speaker-grouping", "standard"]);
    assert_eq!(code, 0);
    assert_eq!(standard["preset"], "standard");
    assert_eq!(standard["suggest_at"].as_f64(), Some(50.0));
    assert_eq!(standard["auto_merge_at"].as_f64(), Some(80.0));
    let set_custom = |suggest, automatic| {
        command(
            &deps,
            &[
                "settings",
                "speaker-grouping",
                "custom",
                "--suggest-at",
                suggest,
                "--auto-merge-at",
                automatic,
            ],
        )
    };
    assert_ne!(set_custom("65", "60").0, 0);
    assert_eq!(
        command(&deps, &["settings", "speaker-grouping"]).1,
        standard
    );
    let (code, custom, _) = set_custom("62.5", "85");
    assert_eq!(code, 0);
    assert_eq!(custom["preset"], "custom");
    assert_eq!(custom["suggest_at"].as_f64(), Some(62.5));
    assert_eq!(custom["auto_merge_at"].as_f64(), Some(85.0));
    assert_eq!(command(&deps, &["settings", "speaker-grouping"]).1, custom);
}

#[test]
fn setup_deferral_is_reported_without_changing_completed_steps() {
    let dir = tempfile::tempdir().unwrap();
    let deps = fixture(dir.path());
    let (_, setup, _) = command(&deps, &["setup", "--lang", "en"]);
    assert_eq!(setup["deferred"], false);
    assert_eq!(setup["completed_steps"], json!(["languages"]));
    let (code, deferred, _) = command(&deps, &["setup", "defer"]);
    assert_eq!(code, 0);
    assert_eq!(deferred["deferred"], true);
    let (_, setup, _) = command(&deps, &["setup"]);
    let (_, doctor, _) = command(&deps, &["doctor"]);
    assert_eq!(setup["deferred"], true);
    assert_eq!(setup["completed_steps"], json!(["languages"]));
    assert_eq!(doctor["setup"]["deferred"], true);
    assert_eq!(doctor["setup"]["completed_steps"], json!(["languages"]));
    let (_, resume, _) = command(&deps, &["setup", "resume"]);
    assert_eq!(resume["deferred"], false);
    let (_, setup, _) = command(&deps, &["setup"]);
    assert_eq!(setup["completed_steps"], json!(["languages"]));
}

#[test]
fn setup_json_keeps_ids_and_completes_only_the_steps_taken() {
    let dir = tempfile::tempdir().unwrap();
    let deps = fixture(dir.path());
    let (code, setup, _) = command(&deps, &["setup", "--auto-language", "--profile", "default"]);
    assert_eq!(code, 0);
    assert_eq!(setup["language"], Value::Null);
    assert_eq!(setup["completed_steps"], json!(["languages"]));

    let (code, _, _) = command(&deps, &["setup", "recording-access"]);
    assert_ne!(code, 0, "recording access requires a running agent");
    let (code, setup, _) = command(&deps, &["setup", "summaries"]);
    assert_eq!(code, 0);
    assert_eq!(setup["completed_steps"], json!(["languages", "summaries"]));
    let (code, setup, _) = command(&deps, &["setup", "meeting-apps"]);
    assert_eq!(code, 0);
    assert_eq!(
        setup["completed_steps"],
        json!(["languages", "summaries", "meeting_apps"])
    );
    let (code, setup, _) = command(&deps, &["setup", "finish"]);
    assert_eq!(code, 0);
    assert_eq!(
        setup["completed_steps"],
        json!(["languages", "summaries", "meeting_apps", "assistant"])
    );
}

#[test]
fn text_size_is_reported_and_persisted_with_json() {
    let dir = tempfile::tempdir().unwrap();
    let deps = fixture(dir.path());
    let (code, initial, _) = command(&deps, &["settings", "text-size"]);
    assert_eq!(code, 0);
    assert_eq!(initial, json!({"reading_size": "standard", "scale": 1.0}));

    for (step, scale) in [
        ("small", 0.875),
        ("standard", 1.0),
        ("large", 1.25),
        ("larger", 1.5),
        ("largest", 2.0),
    ] {
        let expected = json!({"reading_size": step, "scale": scale});
        let (code, saved, _) = command(&deps, &["settings", "text-size", step]);
        assert_eq!(code, 0);
        assert_eq!(saved, expected);
        let (code, read, _) = command(&deps, &["settings", "text-size"]);
        assert_eq!(code, 0);
        assert_eq!(read, expected);
    }
}

#[test]
fn theme_defaults_to_system_and_is_persisted_with_json() {
    let dir = tempfile::tempdir().unwrap();
    let deps = fixture(dir.path());
    let (code, initial, _) = command(&deps, &["settings", "theme"]);
    assert_eq!(code, 0);
    assert_eq!(initial, json!({"theme": "system"}));
    for choice in ["dark", "light", "system"] {
        let (code, saved, _) = command(&deps, &["settings", "theme", choice]);
        assert_eq!(code, 0);
        assert_eq!(saved, json!({"theme": choice}));
        let (_, read, _) = command(&deps, &["settings", "theme"]);
        assert_eq!(read, json!({"theme": choice}));
    }
    let (code, _, _) = command(&deps, &["settings", "theme", "sepia"]);
    assert_ne!(code, 0, "only system, light and dark are offered");
}

#[test]
fn settings_preferences_and_window_state_have_json_parity() {
    let dir = tempfile::tempdir().unwrap();
    let deps = fixture(dir.path());
    let (_, import, _) = command(&deps, &["settings", "import-handling", "reference"]);
    let (_, consent, _) = command(&deps, &["settings", "consent-reminder", "on"]);
    assert_eq!(import["import_handling"], "reference");
    assert_eq!(consent["consent_reminder"], true);
    let (_, defaults, _) = command(&deps, &["settings", "window"]);
    assert_eq!(defaults, json!({"pane": "general", "window": null}));
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let position =
        json!({"mode": "windowed", "frame": {"x": 100, "y": 120, "width": 760, "height": 600}});
    let main = json!({"sidebar_visible": false, "sidebar_width": 214., "library_width": 326., "section": "people"});
    std::fs::write(
        data.join("ui-state.json"),
        json!({
            "settings_page": "calendars", "settings_window": position, "main_window": main
        })
        .to_string(),
    )
    .unwrap();
    let (_, settings, _) = command(&deps, &["settings", "window"]);
    assert_eq!(
        settings,
        json!({"pane": "calendars", "window": position, "main_window": main})
    );
}

#[test]
fn macos_status_item_and_compact_panel_preferences_round_trip_with_json() {
    let dir = tempfile::tempdir().unwrap();
    let deps = fixture(dir.path());
    assert_eq!(
        command(&deps, &["app", "status-item", "status"]).1["show_status_item"],
        true
    );
    assert_eq!(
        command(&deps, &["app", "panel-compact", "status"]).1["compact"],
        false
    );

    let (_, item, _) = command(&deps, &["app", "status-item", "off"]);
    let (_, panel, _) = command(&deps, &["app", "panel-compact", "on"]);
    assert_eq!(item["show_status_item"], false);
    assert_eq!(panel["compact"], true);
    assert!(panel["revision"].as_u64() > item["revision"].as_u64());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let saved = Config::load(&data).unwrap();
    assert!(!saved.show_status_item() && saved.panel.compact);

    assert_eq!(
        command(&deps, &["app", "panel-compact", "on"]).1["changed"],
        false
    );
    assert_eq!(
        command(&deps, &["app", "status-item", "on"]).1["show_status_item"],
        true
    );
    assert_eq!(
        command(&deps, &["app", "panel-compact", "off"]).1["compact"],
        false
    );
}

/// A fake agent answering one `expected` call with `reply`, one request
/// per connection (connections that send nothing are skipped).
fn fake_agent(
    deps: &Deps,
    expected: broscribe_ipc::Call,
    reply: Result<Value, broscribe_ipc::IpcError>,
) -> std::thread::JoinHandle<()> {
    use std::io::{BufRead, Write};
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let endpoint = broscribe_platform_native::ipc::Endpoint::new(&data);
    let loc = endpoint.socket_location().unwrap();
    broscribe_platform_native::ipc::ensure_private_dir(&loc.dir).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&loc.path).unwrap();
    std::thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            if std::io::BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap()
                == 0
            {
                continue;
            }
            let request = broscribe_ipc::Request::parse(&line).unwrap();
            assert_eq!(request.call, expected);
            let response = broscribe_ipc::Response {
                id: request.id,
                outcome: reply,
            };
            writeln!(stream, "{}", response.to_line().unwrap()).unwrap();
            return;
        }
    })
}

#[test]
fn missing_option_values_keep_json_errors_without_reinterpreting_literals() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    for args in [
        vec!["broscribe", "--library", "--json", "version"],
        vec![
            "broscribe",
            "record",
            "tag",
            "+synthetic",
            "--session",
            "--json",
        ],
    ] {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(
            args.into_iter().map(OsString::from).collect(),
            &deps,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 2);
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["error"]["code"], "invalid_input");
        assert!(err.is_empty(), "JSON refusals do not write human usage");
        assert!(!data.join("config.toml").exists());
    }
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(
        ["broscribe", "--library", "--", "--json"]
            .into_iter()
            .map(OsString::from)
            .collect(),
        &deps,
        &mut out,
        &mut err,
    );
    assert_eq!(code, 2);
    assert!(
        out.is_empty(),
        "literal --json does not request JSON output"
    );
    assert!(!err.is_empty());
}

#[test]
fn config_only_commands_never_create_a_mistyped_library() {
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let typo = tmp.path().join("typo-library");
    let t = typo.to_str().unwrap();
    for args in [
        vec!["--library", t, "detect", "policy", "zoom", "off"],
        vec!["--library", t, "mcp", "disable"],
        vec!["--library", t, "mcp", "enable"],
        vec![
            "--library",
            t,
            "providers",
            "enable",
            "claude-cli",
            "--accept",
        ],
        vec!["--library", t, "providers", "disable", "claude-cli"],
        vec!["--library", t, "templates", "auto", "add", "bant"],
        vec!["--library", t, "templates", "reset", "bant"],
        vec!["--library", t, "templates", "auto", "remove", "bant"],
        vec!["--library", t, "audio", "pin", "--clear"],
        vec!["--library", t, "enrich", "instructions", "clear"],
    ] {
        let (code, value, err) = command(&deps, &args);
        assert_eq!(code, 0, "{args:?}: {value} {err}");
        assert!(!typo.exists(), "{args:?} created the library");
    }
    let config = Config::load(&data).unwrap();
    assert_eq!(config.detect.policies["zoom"], "off");
    assert!(config.mcp_enabled());
    assert!(config.templates.auto.is_empty());
}

/// The policy is saved before the reload; a refusing agent leaves it saved
/// and reported, never a failed command.
#[test]
fn a_refused_detection_reload_keeps_the_saved_policy() {
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let deps = fixture(tmp.path());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let server = fake_agent(
        &deps,
        broscribe_ipc::Call::DetectReload,
        Err(broscribe_ipc::IpcError::new(
            "internal",
            "synthetic refusal",
        )),
    );
    let (code, value, err) = command(&deps, &["detect", "policy", "zoom", "off"]);
    server.join().unwrap();
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(
        value,
        json!({ "app": "zoom", "policy": "off", "daemon_reloaded": false })
    );
    assert!(
        err.contains("internal"),
        "the note names the refusal: {err}"
    );
    assert_eq!(Config::load(&data).unwrap().detect.policies["zoom"], "off");
}

/// Three waiting jobs and one failed for good: places in line 0, 1, 2 in
/// scheduler order, the failed one 0 and not runnable, as the core
/// snapshot ranks them.
#[test]
fn queue_status_ranks_every_job_from_one_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = fixture(tmp.path());
    let lib_dir = tmp.path().join("library");
    let lib = Library::open(&lib_dir).unwrap();
    let queue = Queue::open(&lib_dir).unwrap();
    let ids: Vec<JobId> = lib
        .write(|tx| {
            (0..4)
                .map(|n| queue.enqueue(tx, VECTORS_KIND, &format!("synthetic-{n}"), json!({})))
                .collect()
        })
        .unwrap();
    lib.write(|tx| queue.revoke(tx, &ids[1], deps.process.as_ref()))
        .unwrap();

    let (code, status, err) = command(
        &deps,
        &["--library", lib_dir.to_str().unwrap(), "queue", "status"],
    );
    assert_eq!(code, 0, "{status} {err}");
    let rows = status["jobs"].as_array().unwrap();
    assert_eq!(rows.len(), 4, "{status}");
    let ranks = queue.snapshot().unwrap().ranks();
    let mut waiting = Vec::new();
    for row in rows {
        let id = JobId::parse(row["id"].as_str().unwrap()).unwrap();
        let ahead = row["jobs_ahead"].as_u64().unwrap();
        assert_eq!(
            ahead,
            ranks.get(&id).copied().unwrap_or(0) as u64,
            "{status}"
        );
        if id == ids[1] {
            assert_eq!(row["state"], "failed", "{status}");
            assert_eq!(row["runnable"], false, "{status}");
            assert_eq!(ahead, 0, "{status}");
        } else {
            waiting.push(ahead);
        }
    }
    assert_eq!(waiting, [0, 1, 2], "{status}");
    assert_eq!(status["counts"], json!({ "failed": 1, "queued": 3 }));
}

#[test]
fn settings_lane_configuration_round_trips_through_json() {
    let root = tempfile::tempdir().unwrap();
    let deps = fixture(root.path());
    let folder = root.path().join("another-library");
    let folder = folder.to_str().unwrap();
    assert_eq!(
        command(&deps, &["settings", "library-folder", folder]).1["library_folder"],
        folder
    );
    assert_eq!(
        command(&deps, &["settings", "library-folder"]).1["library_folder"],
        folder
    );
    assert_eq!(
        command(&deps, &["settings", "default-language", "pt-BR"]).1["default_language"],
        "pt-BR"
    );
    assert_eq!(
        command(&deps, &["settings", "default-language", "auto"]).0,
        0
    );
    assert!(command(&deps, &["settings", "default-language"]).1["default_language"].is_null());
    assert_ne!(
        command(&deps, &["settings", "default-language", "not a language"]).0,
        0
    );

    assert_eq!(
        command(&deps, &["settings", "automatic-summaries", "off"]).0,
        0
    );
    assert_eq!(
        command(&deps, &["settings", "automatic-summaries"]).1["automatic_summaries"],
        false
    );

    let words = root.path().join("synthetic-words.txt");
    std::fs::write(&words, "Acorn release\n").unwrap();
    assert_eq!(
        command(
            &deps,
            &[
                "setup",
                "--non-interactive",
                "--words-file",
                words.to_str().unwrap()
            ]
        )
        .0,
        0
    );
    assert_eq!(
        command(&deps, &["settings", "word-list"]).1["words"],
        json!(["Acorn release"])
    );
}

#[test]
fn library_folder_write_reports_the_new_path_with_a_library_override() {
    let root = tempfile::tempdir().unwrap();
    let deps = fixture(root.path());
    let old = root.path().join("previous-library");
    let next = root.path().join("new-library");
    let (code, result, _) = command(
        &deps,
        &[
            "--library",
            old.to_str().unwrap(),
            "settings",
            "library-folder",
            next.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["library_folder"], next.to_str().unwrap());
    assert_eq!(
        command(&deps, &["settings", "library-folder"]).1["library_folder"],
        next.to_str().unwrap()
    );
    assert!(!old.exists());
}

#[test]
fn provider_configuration_does_not_enable_and_preference_requires_terms() {
    let root = tempfile::tempdir().unwrap();
    let deps = fixture(root.path());
    let configure = |id, address| {
        command(
            &deps,
            &[
                "providers",
                "configure",
                id,
                "--base-url",
                address,
                "--model",
                "synthetic-model",
            ],
        )
    };
    assert_ne!(configure("openai-http", "http://example.invalid/v1").0, 0);
    assert_ne!(
        configure("openai-http", "https://example.invalid/v1?private=value").0,
        0
    );
    assert_ne!(configure("claude-cli", "https://example.invalid/v1").0, 0);
    let (code, config, _) = configure("openai-http", "http://localhost:11434/v1/");
    assert_eq!(code, 0, "{config}");
    assert_eq!(config["base_url"], "http://localhost:11434/v1");
    let data_dir = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    let cfg = || Config::load(&data_dir).unwrap();
    assert!(!cfg().provider("openai-http").unwrap().is_consented());
    assert_ne!(command(&deps, &["providers", "prefer", "openai-http"]).0, 0);
    assert_eq!(
        command(&deps, &["providers", "enable", "openai-http", "--accept"]).0,
        0
    );
    assert_eq!(
        command(&deps, &["providers", "prefer", "openai-http"]).1["llm"],
        json!(["openai-http"])
    );
    let lookup = command(&deps, &["providers", "list"]).1;
    let entry = lookup["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "openai-http")
        .unwrap();
    assert_eq!(entry["summary_priority"], 0);
    assert_eq!(entry["model"], "synthetic-model");

    let secret = broscribe_models::llm::api_key_secret("openai-http");
    deps.secrets.set(&secret, "synthetic-key").unwrap();
    assert_ne!(configure("openai-http", "https://example.invalid/v1").0, 0);
    assert_eq!(
        deps.secrets.get(&secret).unwrap().as_deref(),
        Some("synthetic-key")
    );
    assert_eq!(
        command(&deps, &["providers", "delete-key", "openai-http"]).0,
        0
    );
    assert_eq!(configure("openai-http", "https://example.invalid/v1").0, 0);
    assert!(!cfg().provider("openai-http").unwrap().is_consented());

    assert_eq!(
        configure("openai-embeddings", "http://127.0.0.1:11434/v1").0,
        0
    );
    assert_eq!(
        command(
            &deps,
            &["providers", "enable", "openai-embeddings", "--accept"]
        )
        .0,
        0
    );
    let (code, model, _) = command(
        &deps,
        &["models", "use", "text_embedding", "openai-embeddings"],
    );
    assert_eq!(code, 0, "{model}");
    assert_eq!(cfg().profile.text_embedding, "openai-embeddings");
    assert_eq!(model["kind"], "provider");
}

#[test]
fn parity_library_switch_saves_the_new_root_when_offline() {
    let temp = tempfile::tempdir().unwrap();
    let deps = fixture(temp.path());
    let target = temp.path().join("synthetic-library-target");
    let (code, value, err) = command(&deps, &["library", "switch", target.to_str().unwrap()]);
    assert_eq!(code, 0, "{value} {err}");
    assert_eq!(value["app_switched"], false);
    assert_eq!(value["agent_switched"], false);
    let target = target.canonicalize().unwrap();
    assert_eq!(value["library_folder"], target.to_str().unwrap());
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    assert_eq!(Config::load(&data).unwrap().library, Some(target));
}

#[test]
fn offline_library_switch_fences_startup_and_refuses_a_held_instance() {
    use broscribe_core::lock::DaemonStartupLock;
    use broscribe_platform::{AgentInstanceProbe, fakes::FakeAgentInstanceProbe};

    struct CheckStartupLock(FakeAgentInstanceProbe);
    impl AgentInstanceProbe for CheckStartupLock {
        fn is_held(&self, data_dir: &Path) -> std::io::Result<bool> {
            assert!(DaemonStartupLock::try_acquire(data_dir).unwrap().is_none());
            self.0.is_held(data_dir)
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let mut deps = fixture(temp.path());
    deps.agent_instance = Box::new(CheckStartupLock(FakeAgentInstanceProbe {
        held: true.into(),
    }));
    let target = temp.path().join("synthetic-library-target");
    let args = [
        "library",
        "switch",
        target.to_str().unwrap(),
        "--timeout",
        "1",
    ];
    let data = broscribe_platform::resolve_paths(deps.paths.as_ref())
        .unwrap()
        .data_dir;
    std::fs::create_dir_all(&data).unwrap();
    let guard = DaemonStartupLock::try_acquire(&data).unwrap().unwrap();
    let (code, value, err) = command(&deps, &args);
    assert_eq!(code, 1, "{value} {err}");
    assert_eq!(value["error"]["code"], "busy");
    drop(guard);

    let (code, value, err) = command(&deps, &args);
    assert_eq!(code, 1, "{value} {err}");
    assert_eq!(value["error"]["code"], "agent_unreachable");
    assert!(!target.exists());
    assert!(Config::load(&data).unwrap().library.is_none());
}

#[test]
fn parity_job_fraction_is_tied_to_a_standalone_worker_lease() {
    let temp = tempfile::tempdir().unwrap();
    let deps = fixture(temp.path());
    let root = temp.path().join("synthetic-library");
    let lib = Library::open(&root).unwrap();
    let queue = Queue::open(&root).unwrap();
    let mid = "m-000001";
    let id = lib
        .write(|tx| queue.enqueue(tx, VECTORS_KIND, mid, json!({"meeting_id":mid})))
        .unwrap();
    let lease = queue
        .claim(&lib, &id, deps.process.as_ref())
        .unwrap()
        .unwrap();
    queue
        .report_progress(&id, deps.process.as_ref(), 0.42)
        .unwrap();
    let status = || {
        command(
            &deps,
            &["--library", root.to_str().unwrap(), "queue", "status"],
        )
        .1
    };
    assert_eq!(status()["jobs"][0]["fraction"], 0.42);
    assert_eq!(status()["jobs"][0]["meeting_id"], mid);
    lease.release(&lib).unwrap();
    let _new_lease = queue
        .claim(&lib, &id, deps.process.as_ref())
        .unwrap()
        .unwrap();
    assert!(status()["jobs"][0]["fraction"].is_null());
}

#[test]
fn parity_about_exposes_the_same_notices_file_as_the_app() {
    use broscribe_platform::about::{
        FakeAboutReader, InspectedInstallation, NOTICES_FILE, Packaging, SealState, SignatureFacts,
        SignatureKind, collect_about,
    };
    let temp = tempfile::tempdir().unwrap();
    let mut deps = fixture(temp.path());
    let notices = temp.path().join(NOTICES_FILE);
    std::fs::write(&notices, "Synthetic notices").unwrap();
    deps.agents.about = std::sync::Arc::new(FakeAboutReader::new(collect_about(
        "0.0.0",
        InspectedInstallation {
            packaging: Packaging::InstallFolder,
            resources: Some(temp.path().to_path_buf()),
            location_notes: vec![],
            bundle: None,
            translated: None,
            signature: SignatureFacts::without_notarization(
                SignatureKind::ManifestOnly,
                SealState::NotChecked,
            ),
        },
    )));
    let (code, value, _) = command(&deps, &["app", "about"]);
    assert_eq!(code, 0);
    assert_eq!(value["third_party_notices_path"], notices.to_str().unwrap());
}
