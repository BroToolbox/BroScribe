//! `agents status|install|uninstall`, `mcp enable|disable|status` and
//! `app open-at-login` in process, with a temporary home, fake agent CLIs
//! (shell scripts that log their arguments) and an empty environment: the
//! user's real home and agent registrations are never touched.
#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use broscribe_cli::{AgentEnv, Deps, run};
use broscribe_models::fakes::FakeModelProvider;
use broscribe_models::llm::SearchPaths;
use broscribe_platform::DecoderChain;
use broscribe_platform::about::{
    FakeAboutReader, InspectedInstallation, LocationNote, Packaging, PlatformAboutReader,
    SealState, SignatureFacts, SignatureKind, collect_about,
};
use broscribe_platform::fakes::{
    FakeAutostart, FakeBrowserLauncher, FakeMicCapture, FakePaths, FakeProcessProbe,
    FakeProcessTreeControl, FakeSecretStore, FakeTrustStore,
};
use serde_json::{Value, json};

const MARKER: &str = "<!-- broscribe-skill: managed -->";

/// The platform's About facts of a build running from where `notes` say.
fn about(notes: Vec<LocationNote>) -> Arc<dyn PlatformAboutReader> {
    Arc::new(FakeAboutReader::new(collect_about(
        "0.0.0-synthetic",
        InspectedInstallation {
            packaging: Packaging::Unbundled,
            resources: None,
            location_notes: notes,
            bundle: None,
            signature: SignatureFacts::without_notarization(
                SignatureKind::ManifestOnly,
                SealState::NotChecked,
            ),
            translated: None,
        },
    )))
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    deps: Deps,
}

#[test]
fn app_about_json_reports_platform_facts_and_missing_values_without_paths() {
    let mut fixture = Fixture::new(&[]);
    fixture.deps.agents.about = about(vec![LocationNote::Temporary]);
    let report = fixture.json(&["app", "about"]);
    assert_eq!(report["app_version"], "0.0.0-synthetic");
    assert_eq!(report["architecture"]["arch"], std::env::consts::ARCH);
    assert!(report["architecture"]["translated"].is_null());
    assert_eq!(report["packaging"], "unbundled");
    assert_eq!(report["location_notes"], json!(["temporary"]));
    assert_eq!(report["build"]["state"], "missing");
    assert_eq!(report["signature"]["notarization"], "not_applicable");
    assert_eq!(report["licences"]["state"], "missing");
    assert_eq!(report["mismatches"], json!([]));
    assert!(!report.to_string().contains(fixture.root.to_str().unwrap()));
}

impl Fixture {
    /// A home, this CLI at `root/bin/broscribe` and fake agents in
    /// `root/agents` (only the named ones).
    fn new(agents: &[(&str, &str)]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Canonical, as `AgentEnv::from_process` makes it (macOS temp
        // folders sit behind a symlink).
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("broscribe"), b"#!/bin/sh\n").unwrap();
        let agent_dir = root.join("agents");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        for (name, body) in agents {
            let path = agent_dir.join(name);
            let log = root.join(format!("{name}.log"));
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nstate='{}'\n{body}\n",
                log.display(),
                root.join(format!("{name}.registered")).display(),
            );
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let deps = Deps {
            clock: broscribe_cli::system_clock(),
            paths: Box::new(FakePaths::new(&root)),
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
            mic: Box::new(FakeMicCapture::new(16_000)),
            secrets: std::sync::Arc::new(FakeSecretStore::default()),
            browser: std::sync::Arc::new(FakeBrowserLauncher::default()),
            agent_instance: Box::new(broscribe_platform::fakes::FakeAgentInstanceProbe::default()),
            autostart: Box::new(FakeAutostart::default()),
            agents: AgentEnv {
                cli: bin.join("broscribe"),
                home: Some(root.join("home")),
                search: SearchPaths::new(vec![agent_dir]),
                vars: Vec::new(),
                // An installed build: no location note.
                about: about(Vec::new()),
                uninstall: Arc::new(broscribe_platform::uninstall::FakeDesktopUninstall::default()),
            },
            // The fake agent CLIs run through synthetic process-tree
            // control; no trust roots, so nothing reaches HTTPS.
            execution: broscribe_models::ExecutionContext::new(
                std::sync::Arc::new(FakeProcessTreeControl::default()),
                std::sync::Arc::new(FakeTrustStore::default()),
            ),
        };
        Self {
            _tmp: tmp,
            root,
            deps,
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn cli(&self) -> PathBuf {
        self.root.join("bin").join("broscribe")
    }

    fn link(&self) -> PathBuf {
        self.home().join(".local/bin/broscribe")
    }

    fn skill(&self, target: &str) -> PathBuf {
        self.home()
            .join(format!(".{target}/skills/broscribe/SKILL.md"))
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut argv: Vec<OsString> = vec![
            "broscribe".into(),
            "--json".into(),
            "--library".into(),
            self.root.join("library").into(),
        ];
        argv.extend(args.iter().map(OsString::from));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv, &self.deps, &mut out, &mut err);
        let v: Value = serde_json::from_slice(&out).unwrap_or(Value::Null);
        assert_eq!(code, 0, "{args:?}: {v} {}", String::from_utf8_lossy(&err));
        v
    }

    fn log(&self, agent: &str) -> Vec<String> {
        std::fs::read_to_string(self.root.join(format!("{agent}.log")))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// `--json <args>` that must fail with exit code 1; the error envelope.
    fn fails(&self, args: &[&str]) -> Value {
        let mut argv: Vec<OsString> = vec!["broscribe".into(), "--json".into()];
        argv.extend(args.iter().map(OsString::from));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv, &self.deps, &mut out, &mut err);
        let v: Value = serde_json::from_slice(&out).unwrap_or(Value::Null);
        assert_eq!(code, 1, "{args:?}: {v} {}", String::from_utf8_lossy(&err));
        v
    }
}

#[test]
fn corrupt_registration_warns_through_the_injected_error_stream() {
    let f = Fixture::new(&[]);
    let data = broscribe_platform::resolve_paths(f.deps.paths.as_ref())
        .unwrap()
        .data_dir;
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("mcp-registrations.json"), b"{not json").unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(
        ["broscribe", "--json", "mcp", "status"]
            .map(OsString::from)
            .into(),
        &f.deps,
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0);
    let status: Value = serde_json::from_slice(&out).unwrap();
    assert!(status["last"].is_null());
    assert!(
        String::from_utf8(err)
            .unwrap()
            .contains("mcp-registrations.json"),
        "the injected error stream must receive the warning"
    );
}

fn results(v: &Value) -> Vec<(String, bool)> {
    v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["item"].as_str().unwrap().to_owned(),
                r["ok"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn skill_states(status: &Value) -> Vec<(String, String)> {
    status["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["target"].as_str().unwrap().to_owned(),
                s["state"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_owned(), b.to_owned())
}

#[test]
fn install_and_uninstall_link_and_skills() {
    let f = Fixture::new(&[("claude", "exit 0"), ("codex", "exit 0")]);
    let status = f.json(&["agents", "status"]);
    assert_eq!(status["cli"]["path"], f.cli().to_str().unwrap());
    assert_eq!(status["cli"]["link"]["path"], f.link().to_str().unwrap());
    assert_eq!(status["cli"]["link"]["state"], "missing");
    let names: Vec<&str> = status["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["claude", "codex"]);
    assert_eq!(
        skill_states(&status),
        [pair("agents", "missing"), pair("claude", "missing")]
    );
    // No registration has run yet, so there is no last result.
    assert_eq!(status["mcp"], json!({ "enabled": false, "last": null }));

    let installed = f.json(&["agents", "install"]);
    assert_eq!(
        results(&installed),
        [
            ("link".to_owned(), true),
            ("skill:agents".to_owned(), true),
            ("skill:claude".to_owned(), true),
        ]
    );
    assert_eq!(installed["status"]["cli"]["link"]["state"], "ours");
    assert_eq!(std::fs::read_link(f.link()).unwrap(), f.cli());
    assert_eq!(
        skill_states(&installed["status"]),
        [pair("agents", "current"), pair("claude", "current")]
    );
    let text = std::fs::read_to_string(f.skill("agents")).unwrap();
    assert!(text.starts_with("---\nname: broscribe\n"));
    assert!(text.lines().any(|l| l == MARKER));
    assert!(text.contains(f.cli().to_str().unwrap()));
    assert!(!text.contains("{{cli}}"));
    assert_eq!(text, std::fs::read_to_string(f.skill("claude")).unwrap());
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&f.skill("agents")), 0o644);
    assert_eq!(mode(f.skill("agents").parent().unwrap()), 0o755);

    // Installing again changes nothing; an outdated skill of ours is
    // rewritten.
    std::fs::write(
        f.skill("agents"),
        format!("---\nname: broscribe\n---\n{MARKER}\nold\n"),
    )
    .unwrap();
    assert_eq!(
        skill_states(&f.json(&["agents", "status"])),
        [pair("agents", "outdated"), pair("claude", "current")]
    );
    let again = f.json(&["agents", "install"]);
    assert!(results(&again).iter().all(|(_, ok)| *ok));
    assert_eq!(std::fs::read_to_string(f.skill("agents")).unwrap(), text);

    let removed = f.json(&["agents", "uninstall"]);
    assert!(results(&removed).iter().all(|(_, ok)| *ok));
    assert_eq!(removed["status"]["cli"]["link"]["state"], "missing");
    assert!(f.link().symlink_metadata().is_err());
    assert!(!f.skill("agents").parent().unwrap().exists());
    assert!(!f.skill("claude").parent().unwrap().exists());
    // The `.claude` target stays listed while Claude Code is detected.
    assert_eq!(
        skill_states(&removed["status"]),
        [pair("agents", "missing"), pair("claude", "missing")]
    );
}

#[test]
fn foreign_link_and_skill_are_left_alone() {
    let f = Fixture::new(&[("claude", "exit 0")]);
    // Something else at the link path and a skill BroScribe did not write.
    std::fs::create_dir_all(f.link().parent().unwrap()).unwrap();
    std::fs::write(f.link(), b"someone else's script").unwrap();
    std::fs::create_dir_all(f.skill("claude").parent().unwrap()).unwrap();
    std::fs::write(
        f.skill("claude"),
        "---\nname: broscribe\n---\nhand-written\n",
    )
    .unwrap();
    let status = f.json(&["agents", "status"]);
    assert_eq!(status["cli"]["link"]["state"], "other");
    assert_eq!(
        skill_states(&status),
        [pair("agents", "missing"), pair("claude", "foreign")]
    );

    let installed = f.json(&["agents", "install"]);
    assert_eq!(
        results(&installed),
        [
            ("link".to_owned(), false),
            ("skill:agents".to_owned(), true),
            ("skill:claude".to_owned(), false),
        ]
    );
    let errors: Vec<&str> = installed["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["error"].as_str())
        .collect();
    assert!(
        errors.iter().all(|e| e.contains("left alone")),
        "{errors:?}"
    );
    assert_eq!(std::fs::read(f.link()).unwrap(), b"someone else's script");
    assert!(
        std::fs::read_to_string(f.skill("claude"))
            .unwrap()
            .contains("hand-written")
    );

    let removed = f.json(&["agents", "uninstall"]);
    assert!(results(&removed).iter().all(|(_, ok)| *ok));
    assert_eq!(std::fs::read(f.link()).unwrap(), b"someone else's script");
    assert!(f.skill("claude").exists());
    assert!(!f.skill("agents").exists());

    // A symlink to another program is not ours either; one into a
    // BroScribe app bundle is.
    std::fs::remove_file(f.link()).unwrap();
    let other = f.root.join("other-tool");
    std::fs::write(&other, b"").unwrap();
    std::os::unix::fs::symlink(&other, f.link()).unwrap();
    assert_eq!(
        f.json(&["agents", "status"])["cli"]["link"]["state"],
        "other"
    );
    std::fs::remove_file(f.link()).unwrap();
    let bundled = f.root.join("Apps/BroScribe.app/Contents/MacOS");
    std::fs::create_dir_all(&bundled).unwrap();
    std::fs::write(bundled.join("broscribe"), b"").unwrap();
    std::os::unix::fs::symlink(bundled.join("broscribe"), f.link()).unwrap();
    assert_eq!(
        f.json(&["agents", "status"])["cli"]["link"]["state"],
        "ours"
    );
    // Install points our link at the running CLI, through a fresh temporary
    // name: a file at the old fixed temporary name survives.
    let old_tmp = f
        .link()
        .with_file_name(format!(".broscribe.tmp-{}", std::process::id()));
    std::fs::write(&old_tmp, b"unrelated file").unwrap();
    let installed = f.json(&["agents", "install"]);
    assert_eq!(results(&installed)[0], ("link".to_owned(), true));
    assert_eq!(std::fs::read_link(f.link()).unwrap(), f.cli());
    assert_eq!(std::fs::read(&old_tmp).unwrap(), b"unrelated file");
    let leftovers: Vec<String> = std::fs::read_dir(f.link().parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        leftovers.len(),
        2,
        "the link and the unrelated file: {leftovers:?}"
    );
}

/// A fake vendor that refuses a second `mcp add` as an existing entry.
const STATEFUL: &str = r#"case "$2" in
add) if [ -e "$state" ]; then echo "MCP server broscribe already exists in user config" >&2; exit 1; fi; : > "$state" ;;
remove) if [ -e "$state" ]; then /bin/rm "$state"; else echo "No MCP server named broscribe found" >&2; exit 1; fi ;;
esac"#;

#[test]
fn mcp_enable_and_disable_register_with_detected_agents() {
    let f = Fixture::new(&[
        ("claude", STATEFUL),
        ("codex", STATEFUL),
        ("gemini", "echo 'boom: config unreadable' >&2\nexit 3"),
    ]);
    // Claude already has an entry (for example from an older install).
    std::fs::write(f.root.join("claude.registered"), b"").unwrap();
    let cli = f.cli();
    let cli = cli.to_str().unwrap();

    let on = f.json(&["mcp", "enable"]);
    assert_eq!(on["enabled"], true);
    assert_eq!(
        on["agents"],
        json!([
            { "name": "claude", "ok": true, "error": null },
            { "name": "codex", "ok": true, "error": null },
            { "name": "gemini", "ok": false, "error": "boom: config unreadable" },
        ])
    );
    // Existing entry: add, remove, add again with the current path.
    assert_eq!(
        f.log("claude"),
        [
            format!("mcp add --scope user broscribe -- {cli} mcp"),
            "mcp remove --scope user broscribe".to_owned(),
            format!("mcp add --scope user broscribe -- {cli} mcp"),
        ]
    );
    assert_eq!(f.log("codex"), [format!("mcp add broscribe -- {cli} mcp")]);
    assert_eq!(
        f.log("gemini"),
        [format!("mcp add --scope user broscribe {cli} mcp")]
    );

    // Enabling again re-runs registration.
    let again = f.json(&["mcp", "enable"]);
    assert_eq!(
        again["agents"][1],
        json!({ "name": "codex", "ok": true, "error": null })
    );
    // Cumulative log: the first enable's add, then add (exists), remove, add.
    assert_eq!(
        f.log("codex")[1..],
        [
            format!("mcp add broscribe -- {cli} mcp"),
            "mcp remove broscribe".to_owned(),
            format!("mcp add broscribe -- {cli} mcp"),
        ]
    );

    let status = f.json(&["mcp", "status"]);
    assert_eq!(status["enabled"], true);
    let agents = status["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 3);
    assert_eq!(
        agents[0]["path"],
        f.root.join("agents/claude").to_str().unwrap()
    );

    let off = f.json(&["mcp", "disable"]);
    assert_eq!(off["enabled"], false);
    assert_eq!(
        off["agents"][0],
        json!({ "name": "claude", "ok": true, "error": null })
    );
    assert_eq!(
        off["agents"][1],
        json!({ "name": "codex", "ok": true, "error": null })
    );
    // The vendor failure is reported; the flag stays off regardless.
    assert_eq!(off["agents"][2]["ok"], false);
    assert_eq!(f.log("codex").last().unwrap(), "mcp remove broscribe");
    assert_eq!(
        f.log("gemini").last().unwrap(),
        "mcp remove --scope user broscribe"
    );
    // Removing an entry that is not there counts as removed.
    let off = f.json(&["mcp", "disable"]);
    assert_eq!(off["agents"][0]["ok"], true);
    assert_eq!(off["agents"][1]["ok"], true);
    assert_eq!(f.json(&["mcp", "status"])["enabled"], false);
    assert_eq!(f.json(&["agents", "status"])["mcp"]["enabled"], false);
}

#[test]
fn the_last_registration_result_is_kept_between_invocations() {
    // Codex fails while `<state>.fail` exists.
    let f = Fixture::new(&[
        ("claude", "exit 0"),
        (
            "codex",
            "if [ -e \"$state.fail\" ]; then echo 'synthetic vendor outage' >&2; exit 2; fi",
        ),
    ]);
    assert_eq!(f.json(&["mcp", "status"])["last"], Value::Null);
    assert_eq!(f.json(&["agents", "status"])["mcp"]["last"], Value::Null);
    let fail = f.root.join("codex.registered.fail");
    std::fs::write(&fail, b"").unwrap();

    let on = f.json(&["mcp", "enable"]);
    assert_eq!(on["agents"][1]["ok"], false);
    // A separate invocation reports the saved result.
    let status = f.json(&["mcp", "status"]);
    let last = &status["last"];
    assert_eq!(last["action"], "enable");
    assert!(
        chrono::DateTime::parse_from_rfc3339(last["at"].as_str().unwrap()).is_ok(),
        "{last}"
    );
    assert_eq!(
        last["agents"],
        json!([
            { "name": "claude", "ok": true, "error": null },
            { "name": "codex", "ok": false, "error": "synthetic vendor outage" },
        ])
    );
    // Live state is still only the flag and the detected agents.
    assert_eq!(status["enabled"], true);
    assert_eq!(status["agents"].as_array().unwrap().len(), 2);
    assert_eq!(f.json(&["agents", "status"])["mcp"]["last"], *last);
    let file = f.root.join("data/mcp-registrations.json");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // A later successful enable replaces it; a disable records its action.
    std::fs::remove_file(&fail).unwrap();
    f.json(&["mcp", "enable"]);
    let last = f.json(&["mcp", "status"])["last"].clone();
    assert_eq!(last["action"], "enable");
    assert_eq!(
        last["agents"][1],
        json!({ "name": "codex", "ok": true, "error": null })
    );
    f.json(&["mcp", "disable"]);
    let last = f.json(&["agents", "status"])["mcp"]["last"].clone();
    assert_eq!(last["action"], "disable");
    assert!(
        last["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["ok"] == true)
    );
}

#[test]
fn vendors_get_only_allow_listed_environment() {
    let mut f = Fixture::new(&[("codex", "export -p > \"$state\"")]);
    f.deps.agents.vars = vec![
        ("HOME".into(), f.home().into()),
        ("CODEX_HOME".into(), f.root.join("codex-home").into()),
        ("BROSCRIBE_LIBRARY".into(), "/elsewhere".into()),
        ("SECRET_TOKEN".into(), "synthetic".into()),
    ];
    f.json(&["mcp", "enable"]);
    let env = std::fs::read_to_string(f.root.join("codex.registered")).unwrap();
    // `export -p` quotes values differently per shell.
    assert!(env.contains(" HOME="), "{env}");
    assert!(env.contains(&f.home().display().to_string()), "{env}");
    assert!(env.contains("CODEX_HOME="), "{env}");
    assert!(!env.contains("BROSCRIBE_LIBRARY"));
    assert!(!env.contains("SECRET_TOKEN"));
}

/// The switch reports the setting as saved, whether this command changed
/// it and the config revision: an unchanged switch writes nothing, and a
/// setting saved meanwhile by another command survives the switch.
#[test]
fn open_at_login_switch() {
    let f = Fixture::new(&[]);
    let status = f.json(&["app", "open-at-login", "status"]);
    assert_eq!(status["open_at_login"], true);
    assert_eq!(status["changed"], false);
    let unset = status["revision"].as_u64().unwrap();

    let off = f.json(&["app", "open-at-login", "off"]);
    assert_eq!(off["open_at_login"], false);
    assert_eq!(off["changed"], true);
    let saved = off["revision"].as_u64().unwrap();
    assert!(saved > unset, "{off}");

    let again = f.json(&["app", "open-at-login", "off"]);
    assert_eq!(again["open_at_login"], false);
    assert_eq!(again["changed"], false);
    assert_eq!(again["revision"], saved);

    let size = f.json(&["app", "reading-size", "large"]);
    assert_eq!(size["changed"], true);
    let status = f.json(&["app", "open-at-login", "status"]);
    assert_eq!(status["open_at_login"], false);
    assert_eq!(status["changed"], false);
    assert_eq!(status["revision"], size["revision"]);

    let on = f.json(&["app", "open-at-login", "on"]);
    assert_eq!(on["open_at_login"], true);
    assert_eq!(on["changed"], true);
    assert!(on["revision"].as_u64().unwrap() > size["revision"].as_u64().unwrap());
    assert_eq!(f.json(&["app", "reading-size"])["reading_size"], "large");
}

#[test]
fn hide_controls_from_screen_sharing_switch_persists_and_reports_json() {
    let f = Fixture::new(&[]);
    let state = |value| f.json(&["app", "hide-controls-from-screen-sharing", value]);
    assert_eq!(state("status")["hide_controls_from_screen_sharing"], false);
    assert_eq!(state("on")["changed"], true);
    assert_eq!(state("status")["hide_controls_from_screen_sharing"], true);
    assert_eq!(state("on")["changed"], false);
    assert_eq!(state("off")["hide_controls_from_screen_sharing"], false);
}

/// A build the platform places in a translocated copy, on a read-only
/// volume or in a temporary folder (or whose location cannot be read) never
/// registers itself: no link, skill, vendor command, MCP flag or record is
/// written. Status, uninstall and disable stay available, a network volume
/// is not a reason to refuse, and the `--library` the fixture names is
/// never created by these commands.
#[test]
fn an_unsuitable_location_registers_nothing() {
    let readers: [Arc<dyn PlatformAboutReader>; 4] = [
        about(vec![LocationNote::Translocated]),
        about(vec![LocationNote::ReadOnlyVolume]),
        about(vec![LocationNote::NetworkVolume, LocationNote::Temporary]),
        Arc::new(FakeAboutReader::failing("synthetic: executable not found")),
    ];
    for (n, reader) in readers.into_iter().enumerate() {
        let mut f = Fixture::new(&[("claude", STATEFUL), ("codex", STATEFUL)]);
        f.deps.agents.about = reader;
        let data = broscribe_platform::resolve_paths(f.deps.paths.as_ref())
            .unwrap()
            .data_dir;
        for args in [["agents", "install"], ["mcp", "enable"]] {
            let v = f.fails(&args);
            if n < 3 {
                assert_eq!(v["error"]["code"], "unsupported", "{args:?}: {v}");
            }
        }
        assert!(f.link().symlink_metadata().is_err(), "case {n}: link");
        assert!(!f.home().join(".agents").exists(), "case {n}: skill");
        assert!(!f.home().join(".claude").exists(), "case {n}: skill");
        assert!(f.log("claude").is_empty() && f.log("codex").is_empty());
        assert!(!data.join("mcp-registrations.json").exists(), "case {n}");
        assert_eq!(
            broscribe_core::config::Config::load(&data)
                .unwrap()
                .mcp
                .enabled,
            None,
            "case {n}: the MCP flag"
        );

        f.json(&["agents", "status"]);
        f.json(&["mcp", "status"]);
        f.json(&["agents", "uninstall"]);
        assert_eq!(f.json(&["mcp", "disable"])["enabled"], false);
        assert_eq!(f.log("codex"), ["mcp remove broscribe"]);
        assert!(!f.root.join("library").exists(), "case {n}: library");
    }

    let mut f = Fixture::new(&[("codex", STATEFUL)]);
    f.deps.agents.about = about(vec![LocationNote::NetworkVolume]);
    let installed = f.json(&["agents", "install"]);
    assert!(results(&installed).iter().all(|(_, ok)| *ok), "{installed}");
    assert_eq!(std::fs::read_link(f.link()).unwrap(), f.cli());
    assert_eq!(f.json(&["mcp", "enable"])["agents"][0]["ok"], true);
    assert!(!f.root.join("library").exists());
}
