//! AI agent access: `agents status|install|uninstall` (the
//! `~/.local/bin/broscribe` link and the BroScribe skill) and
//! `mcp enable|disable|status` (the `[mcp] enabled` gate plus registration
//! with detected agent CLIs through their own `mcp add` / `mcp remove`).
//!
//! The link exists only on Unix: elsewhere its state is `unsupported` and
//! install/uninstall report it skipped. Ownership rules: the link is ours
//! only when it is a symlink whose target, canonicalized, is this CLI or a
//! `broscribe` inside
//! `…/BroScribe.app/Contents/MacOS/`; a skill is ours only when its
//! `SKILL.md` carries [`MARKER`] on a line of its own. Anything else is
//! reported and never touched. Vendor commands run with a cleared,
//! allow-listed environment, a temporary working folder and a timeout; argv
//! holds only BroScribe's name and this CLI's path.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use broscribe_core::config::Config;
use broscribe_core::{Error, Result};
use broscribe_models::ExecutionContext;
use broscribe_models::llm::detect::agent_child_path;
use broscribe_models::llm::process::{RunSpec, Sandbox, child_env, run};
use broscribe_platform::about::LocationNote;
use serde_json::{Value, json};

use crate::{AgentEnv, Ctx};

/// Agent CLIs BroScribe registers with, in report order.
const AGENT_NAMES: [&str; 3] = ["claude", "codex", "gemini"];
/// The line that marks a `SKILL.md` as written by BroScribe.
pub(crate) const MARKER: &str = "<!-- broscribe-skill: managed -->";
const SKILL_TEMPLATE: &str = include_str!("../../assets/skill/SKILL.md");
/// Time allowed for one vendor `mcp add` / `mcp remove`.
const VENDOR_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest vendor message kept in a result.
const MESSAGE_MAX: usize = 300;

/// A detected agent CLI.
struct Agent {
    name: &'static str,
    path: PathBuf,
}

fn detect(env: &AgentEnv) -> Vec<Agent> {
    AGENT_NAMES
        .into_iter()
        .filter_map(|name| {
            Some(Agent {
                name,
                path: env.search.find(name)?,
            })
        })
        .collect()
}

fn agents_json(agents: &[Agent]) -> Value {
    agents
        .iter()
        .map(|a| json!({ "name": a.name, "path": text(&a.path) }))
        .collect()
}

/// A path for JSON and messages (lossy: JSON strings are UTF-8).
fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn home(env: &AgentEnv) -> Result<&Path> {
    env.home
        .as_deref()
        .ok_or_else(|| Error::NotConfigured("the home folder is unknown (HOME is not set)".into()))
}

// ---- the link ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkState {
    Missing,
    Ours,
    Other,
    /// Not a Unix system: no `~/.local/bin` link is made or read.
    Unsupported,
}

impl LinkState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Ours => "ours",
            Self::Other => "other",
            Self::Unsupported => "unsupported",
        }
    }
}

/// What an item's install or uninstall did when it did not fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemOutcome {
    /// Done, or already as asked.
    Done,
    /// Not available on this system; nothing was changed.
    Unsupported,
}

/// One `results` row of `agents install|uninstall`: `{item, path, ok,
/// error}`, plus `skipped: "unsupported"` (with `ok: true`) for an item
/// this system does not have.
fn item_row(item: &str, path: &Path, r: Result<ItemOutcome>) -> Value {
    match r {
        Ok(ItemOutcome::Done) => json!({
            "item": item, "path": text(path), "ok": true, "error": null,
        }),
        Ok(ItemOutcome::Unsupported) => json!({
            "item": item, "path": text(path), "ok": true, "skipped": "unsupported", "error": null,
        }),
        Err(e) => json!({
            "item": item, "path": text(path), "ok": false, "error": e.to_string(),
            "error_code": e.code(),
        }),
    }
}

/// The human line of a [`item_row`] row.
fn write_item(out: &mut dyn std::io::Write, row: &Value) -> std::io::Result<()> {
    let item = row["item"].as_str().unwrap_or_default();
    match (row["error"].as_str(), row["skipped"].as_str()) {
        (Some(e), _) => writeln!(out, "{item}: {e}"),
        (None, Some(_)) => writeln!(out, "{item}: not available on this system"),
        (None, None) => writeln!(out, "{item}: ok"),
    }
}

fn link_path(home: &Path) -> PathBuf {
    home.join(".local").join("bin").join("broscribe")
}

/// A `broscribe` file directly inside `…/BroScribe.app/Contents/MacOS/`.
fn is_bundled_cli(path: &Path) -> bool {
    let mut parts = path.components().rev().map(|c| c.as_os_str());
    path.is_file()
        && parts.next() == Some(OsStr::new("broscribe"))
        && parts.next() == Some(OsStr::new("MacOS"))
        && parts.next() == Some(OsStr::new("Contents"))
        && parts.next() == Some(OsStr::new("BroScribe.app"))
}

fn link_state(link: &Path, cli: &Path) -> LinkState {
    if !cfg!(unix) {
        return LinkState::Unsupported;
    }
    match std::fs::symlink_metadata(link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => LinkState::Missing,
        Ok(m) if m.file_type().is_symlink() => match std::fs::canonicalize(link) {
            Ok(target)
                if is_bundled_cli(&target)
                    || std::fs::canonicalize(cli).is_ok_and(|c| c == target) =>
            {
                LinkState::Ours
            }
            _ => LinkState::Other,
        },
        _ => LinkState::Other,
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "linking the CLI needs a Unix system",
    ))
}

/// Creates the link, or points our existing link at this CLI (atomically,
/// through a temporary link renamed over it); skipped off Unix.
fn install_link(link: &Path, cli: &Path) -> Result<ItemOutcome> {
    if !cfg!(unix) {
        return Ok(ItemOutcome::Unsupported);
    }
    if !cli.is_file() {
        return Err(Error::NotFound(format!(
            "this CLI at {} is not a file",
            cli.display()
        )));
    }
    match link_state(link, cli) {
        LinkState::Unsupported => return Ok(ItemOutcome::Unsupported),
        LinkState::Other => Err(Error::AlreadyExists(format!(
            "{} exists and is not BroScribe's; left alone",
            link.display()
        ))),
        LinkState::Ours if std::fs::read_link(link).is_ok_and(|t| t == cli) => Ok(()),
        LinkState::Ours => replace_link(link, cli, temp_link_names(link)),
        LinkState::Missing => {
            let dir = link.parent().unwrap_or(Path::new("."));
            std::fs::create_dir_all(dir).map_err(|e| Error::io_at(dir, e))?;
            symlink(cli, link).map_err(|e| Error::io_at(link, e))
        }
    }
    .map(|()| ItemOutcome::Done)
}

/// Hidden names beside `link` for its replacement, unique to this call.
fn temp_link_names(link: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    (0..8u32).map(move |n| link.with_file_name(format!(".broscribe.tmp-{pid}-{nanos:08x}-{n}")))
}

/// Points `link` at `cli` by creating a symlink under the first free name of
/// `candidates` and renaming it over `link`. Creating a symlink never
/// replaces anything, so a name already taken (by anyone) is skipped, and
/// only the entry created here is ever removed.
fn replace_link(
    link: &Path,
    cli: &Path,
    candidates: impl IntoIterator<Item = PathBuf>,
) -> Result<()> {
    for tmp in candidates {
        match symlink(cli, &tmp) {
            Ok(()) => {
                return std::fs::rename(&tmp, link).map_err(|e| {
                    let _ = std::fs::remove_file(&tmp);
                    Error::io_at(link, e)
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::io_at(&tmp, e)),
        }
    }
    Err(Error::AlreadyExists(format!(
        "no free temporary name beside {}; nothing changed",
        link.display()
    )))
}

/// Removes our link; skipped off Unix.
fn uninstall_link(link: &Path, cli: &Path) -> Result<ItemOutcome> {
    if !cfg!(unix) {
        return Ok(ItemOutcome::Unsupported);
    }
    if link_state(link, cli) == LinkState::Ours {
        std::fs::remove_file(link).map_err(|e| Error::io_at(link, e))?;
    }
    Ok(ItemOutcome::Done)
}

// ---- the skill ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SkillState {
    Missing,
    Current,
    Outdated,
    Foreign,
}

impl SkillState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Current => "current",
            Self::Outdated => "outdated",
            Self::Foreign => "foreign",
        }
    }

    fn ours(self) -> bool {
        matches!(self, Self::Current | Self::Outdated)
    }
}

/// `'…'`-quotes `s` for a POSIX shell when needed.
pub(super) fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@,".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The skill with this CLI's path; the same path always renders the same text.
pub(crate) fn render_skill(cli: &Path) -> String {
    SKILL_TEMPLATE.replace("{{cli}}", &shell_quote(&cli.to_string_lossy()))
}

/// Skill folders: `~/.agents/skills/broscribe` always (Codex and the Agent
/// Skills standard); `~/.claude/skills/broscribe` when Claude Code is
/// detected or a skill is already there.
fn skill_targets(home: &Path, agents: &[Agent]) -> Vec<(&'static str, PathBuf)> {
    let mut targets = vec![(
        "agents",
        home.join(".agents").join("skills").join("broscribe"),
    )];
    let claude = home.join(".claude").join("skills").join("broscribe");
    if agents.iter().any(|a| a.name == "claude") || std::fs::symlink_metadata(&claude).is_ok() {
        targets.push(("claude", claude));
    }
    targets
}

fn has_marker(text: &str) -> bool {
    text.lines().any(|l| l.trim() == MARKER)
}

fn skill_state(dir: &Path, rendered: &str) -> SkillState {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SkillState::Missing,
        Ok(m) if m.is_dir() => {}
        _ => return SkillState::Foreign,
    }
    let file = dir.join("SKILL.md");
    match std::fs::symlink_metadata(&file) {
        Ok(m) if m.is_file() => match std::fs::read_to_string(&file) {
            Ok(text) if text == rendered => SkillState::Current,
            Ok(text) if has_marker(&text) => SkillState::Outdated,
            _ => SkillState::Foreign,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // An empty folder holds nothing to keep.
            if std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none()) {
                SkillState::Missing
            } else {
                SkillState::Foreign
            }
        }
        _ => SkillState::Foreign,
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| Error::io_at(path, e))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn install_skill(dir: &Path, rendered: &str) -> Result<()> {
    match skill_state(dir, rendered) {
        SkillState::Foreign => Err(Error::AlreadyExists(format!(
            "{} holds a skill BroScribe did not write; left alone",
            dir.display()
        ))),
        SkillState::Current => Ok(()),
        SkillState::Missing | SkillState::Outdated => {
            std::fs::create_dir_all(dir).map_err(|e| Error::io_at(dir, e))?;
            set_mode(dir, 0o755)?;
            let file = dir.join("SKILL.md");
            broscribe_core::fsutil::write_atomic(&file, rendered.as_bytes())?;
            set_mode(&file, 0o644)
        }
    }
}

fn uninstall_skill(dir: &Path, rendered: &str) -> Result<()> {
    if skill_state(dir, rendered).ours() {
        let file = dir.join("SKILL.md");
        std::fs::remove_file(&file).map_err(|e| Error::io_at(&file, e))?;
        // Only an empty folder goes; anything the user added stays.
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

// ---- agents commands ----

fn status_value(ctx: &Ctx<'_>) -> Result<Value> {
    let env = &ctx.deps.agents;
    let home = home(env)?;
    let agents = detect(env);
    let link = link_path(home);
    let rendered = render_skill(&env.cli);
    let skills: Vec<Value> = skill_targets(home, &agents)
        .into_iter()
        .map(|(target, dir)| {
            json!({
                "target": target,
                "path": text(&dir.join("SKILL.md")),
                "state": skill_state(&dir, &rendered).as_str(),
            })
        })
        .collect();
    Ok(json!({
        "cli": {
            "path": text(&env.cli),
            "link": { "path": text(&link), "state": link_state(&link, &env.cli).as_str() },
        },
        "agents": agents_json(&agents),
        "skills": skills,
        "mcp": {
            "enabled": ctx.config()?.mcp_enabled(),
            "last": load_last(ctx.data_dir(), &ctx.background_notes())?,
        },
    }))
}

fn write_status(out: &mut dyn std::io::Write, status: &Value) -> std::io::Result<()> {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_owned();
    writeln!(out, "CLI: {}", text(&status["cli"]["path"]))?;
    let link = &status["cli"]["link"];
    if link["state"] == LinkState::Unsupported.as_str() {
        writeln!(out, "Link: not available on this system")?;
    } else {
        writeln!(
            out,
            "Link {}: {}",
            text(&link["path"]),
            text(&link["state"])
        )?;
    }
    for s in status["skills"].as_array().into_iter().flatten() {
        writeln!(out, "Skill {}: {}", text(&s["path"]), text(&s["state"]))?;
    }
    let agents = status["agents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if agents.is_empty() {
        writeln!(out, "No agent CLI found (claude, codex, gemini).")?;
    }
    for a in agents {
        writeln!(out, "Agent {}: {}", text(&a["name"]), text(&a["path"]))?;
    }
    let mcp = if status["mcp"]["enabled"] == true {
        "allowed"
    } else {
        "off"
    };
    writeln!(out, "MCP access: {mcp}")?;
    let last = serde_json::from_value(status["mcp"]["last"].clone()).ok();
    write_last(out, last.as_ref())
}

pub(super) fn status(ctx: &mut Ctx<'_>) -> Result<()> {
    let value = status_value(ctx)?;
    ctx.emit(value.clone(), |out| write_status(out, &value))
}

/// Refuses to register this CLI where the platform says its location does
/// not last (a copy macOS translocated, a read-only volume, a temporary
/// folder): a link, skill or MCP entry would point at a path that
/// disappears. Runs before anything is written; a location that cannot be
/// read refuses too. Status, uninstall and disable never call it.
fn check_location(env: &AgentEnv) -> Result<()> {
    let notes = env.about.read()?.location_notes;
    let problem = notes.iter().find_map(|note| match note {
        LocationNote::Translocated => Some("a copy macOS translocated from a download"),
        LocationNote::ReadOnlyVolume => Some("a read-only volume"),
        LocationNote::Temporary => Some("a temporary folder"),
        LocationNote::NetworkVolume => None,
    });
    match problem {
        Some(problem) => Err(Error::Unsupported(format!(
            "BroScribe runs from {problem}, so agents could not find it later; install it (Applications on macOS), open it from there and try again. Nothing was registered."
        ))),
        None => Ok(()),
    }
}

pub(super) fn change_agent_tools(env: &AgentEnv, install: bool) -> Result<Vec<Value>> {
    let home = home(env)?;
    if install {
        check_location(env)?;
    }
    let agents = detect(env);
    let rendered = render_skill(&env.cli);
    let link = link_path(home);
    let mut results = Vec::new();
    let r = if install {
        install_link(&link, &env.cli)
    } else {
        uninstall_link(&link, &env.cli)
    };
    results.push(item_row("link", &link, r));
    for (target, dir) in skill_targets(home, &agents) {
        let r = if install {
            install_skill(&dir, &rendered)
        } else {
            uninstall_skill(&dir, &rendered)
        };
        let r = r.map(|()| ItemOutcome::Done);
        results.push(item_row(
            &format!("skill:{target}"),
            &dir.join("SKILL.md"),
            r,
        ));
    }
    Ok(results)
}

pub(super) fn unregister_all(deps: &crate::Deps) -> Vec<(String, std::result::Result<(), String>)> {
    detect(&deps.agents)
        .iter()
        .map(|agent| {
            (
                agent.name.to_owned(),
                unregister(&deps.agents, &deps.execution, agent),
            )
        })
        .collect()
}

/// `agents install` (`install`) or `agents uninstall`: one result per item;
/// an item's failure is reported beside the others, never aborting them.
/// Install first checks this CLI's location ([`check_location`]).
pub(super) fn install(ctx: &mut Ctx<'_>, install: bool) -> Result<()> {
    let results = change_agent_tools(&ctx.deps.agents, install)?;
    // The items above already changed the disk: a status that cannot be
    // read (an unreadable config or registration record) is reported beside
    // them, never in their place.
    let status = status_value(ctx);
    let mut value = json!({ "results": results });
    match &status {
        Ok(status) => value["status"] = status.clone(),
        Err(e) => {
            value["status_error"] = json!({ "code": e.code(), "message": e.to_string() });
        }
    }
    ctx.emit(value, |out| {
        for r in &results {
            write_item(out, r)?;
        }
        match &status {
            Ok(status) => write_status(out, status),
            Err(e) => writeln!(out, "Status unavailable: {e}"),
        }
    })
}

// ---- MCP registration ----

fn add_args(agent: &str, cli: &str) -> Vec<String> {
    let args: &[&str] = match agent {
        "claude" => &[
            "mcp",
            "add",
            "--scope",
            "user",
            "broscribe",
            "--",
            cli,
            "mcp",
        ],
        "codex" => &["mcp", "add", "broscribe", "--", cli, "mcp"],
        _ => &["mcp", "add", "--scope", "user", "broscribe", cli, "mcp"],
    };
    args.iter().map(|s| (*s).to_owned()).collect()
}

fn remove_args(agent: &str) -> Vec<String> {
    let args: &[&str] = match agent {
        "codex" => &["mcp", "remove", "broscribe"],
        _ => &["mcp", "remove", "--scope", "user", "broscribe"],
    };
    args.iter().map(|s| (*s).to_owned()).collect()
}

/// A failed vendor command: its first output line (stderr first), capped.
struct VendorFailure {
    message: String,
    /// Lowercased stdout and stderr, to recognise known outcomes.
    output: String,
}

/// One vendor command, its process tree contained by `execution` (ended
/// with the call on success, failure and timeout).
fn vendor(
    env: &AgentEnv,
    execution: &ExecutionContext,
    program: &Path,
    args: &[String],
) -> std::result::Result<(), VendorFailure> {
    let fail = |message: String| VendorFailure {
        output: message.to_lowercase(),
        message,
    };
    let sandbox = Sandbox::new().map_err(|e| fail(e.to_string()))?;
    // Script launchers (`#!/usr/bin/env node`, Windows `.cmd` shims) find
    // their runtime beside the CLI or in the usual install folders.
    let vars = child_env(
        env.vars.iter().cloned(),
        &agent_child_path(program, &env.search),
    );
    let out = run(
        &RunSpec {
            program,
            args,
            cwd: sandbox.path(),
            env: &vars,
            stdin: None,
            timeout: VENDOR_TIMEOUT,
        },
        execution,
    )
    .map_err(|e| fail(e.to_string()))?;
    if out.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first = out
        .stderr_head
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty());
    let mut message = match first {
        Some(line) => line.to_owned(),
        None => format!("exited with {}", out.status),
    };
    if message.len() > MESSAGE_MAX {
        let mut end = MESSAGE_MAX;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push('…');
    }
    Err(VendorFailure {
        output: format!("{}\n{stdout}", out.stderr_head).to_lowercase(),
        message,
    })
}

/// `mcp add`; when the vendor reports an existing entry, `mcp remove` then
/// `mcp add` once, so the entry records this CLI's current path.
fn register(
    env: &AgentEnv,
    execution: &ExecutionContext,
    agent: &Agent,
) -> std::result::Result<(), String> {
    let cli = env
        .cli
        .to_str()
        .ok_or_else(|| format!("this CLI's path is not UTF-8: {}", env.cli.display()))?;
    let add = add_args(agent.name, cli);
    match vendor(env, execution, &agent.path, &add) {
        Ok(()) => Ok(()),
        Err(f) if f.output.contains("already exists") => {
            vendor(env, execution, &agent.path, &remove_args(agent.name)).map_err(|f| f.message)?;
            vendor(env, execution, &agent.path, &add).map_err(|f| f.message)
        }
        Err(f) => Err(f.message),
    }
}

/// `mcp remove`; an entry that is not there counts as removed.
fn unregister(
    env: &AgentEnv,
    execution: &ExecutionContext,
    agent: &Agent,
) -> std::result::Result<(), String> {
    match vendor(env, execution, &agent.path, &remove_args(agent.name)) {
        Ok(()) => Ok(()),
        Err(f)
            if ["not found", "does not exist", "no mcp server"]
                .iter()
                .any(|s| f.output.contains(s)) =>
        {
            Ok(())
        }
        Err(f) => Err(f.message),
    }
}

/// File in the data folder holding the result of the last `mcp
/// enable|disable`.
const LAST_FILE: &str = "mcp-registrations.json";

/// The result of the last `mcp enable|disable` (`last` in `mcp status` and
/// `agents status`): what each vendor command answered then, not what each
/// agent's own config holds now. Holds no transcript text.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct LastRegistration {
    /// `enable` or `disable`.
    action: String,
    /// RFC 3339, UTC.
    at: String,
    agents: Vec<RegistrationResult>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RegistrationResult {
    name: String,
    ok: bool,
    error: Option<String>,
}

/// The saved last result; `None` before the first `mcp enable|disable`. An
/// unreadable record (corrupt, or from an incompatible version) is only a
/// past report, so it reads as absent with a warning instead of failing the
/// status commands; the next `mcp enable|disable` replaces it.
fn load_last(data_dir: &Path, notes: &crate::BackgroundNotes) -> Result<Option<LastRegistration>> {
    match broscribe_core::fsutil::read_json(&data_dir.join(LAST_FILE)) {
        Ok(last) => Ok(Some(last)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(Error::Schema(e)) => {
            notes.push(format!("warning: ignoring an unreadable {LAST_FILE}: {e}"));
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Replaces the saved last result atomically; the file is mode 0600 from
/// before its first byte is written.
fn save_last(data_dir: &Path, last: &LastRegistration) -> Result<()> {
    std::fs::create_dir_all(data_dir).map_err(|e| Error::io_at(data_dir, e))?;
    let path = data_dir.join(LAST_FILE);
    let mut bytes = serde_json::to_vec_pretty(last)
        .map_err(|e| Error::Internal(format!("serialising {}: {e}", path.display())))?;
    bytes.push(b'\n');
    broscribe_core::fsutil::write_atomic_with_mode(&path, &bytes, Some(0o600))
}

fn write_last(
    out: &mut dyn std::io::Write,
    last: Option<&LastRegistration>,
) -> std::io::Result<()> {
    let Some(last) = last else {
        return Ok(());
    };
    writeln!(
        out,
        "Last registration result ({}, {}):",
        last.action, last.at
    )?;
    if last.agents.is_empty() {
        writeln!(out, "  no agent CLI was found")?;
    }
    for a in &last.agents {
        match &a.error {
            None => writeln!(out, "  {}: ok", a.name)?,
            Some(e) => writeln!(out, "  {}: failed: {e}", a.name)?,
        }
    }
    Ok(())
}

/// `mcp enable` / `mcp disable`: saves the flag in the data directory's
/// config (authoritative: a vendor failure never rolls it back; no library
/// is opened or created), then (re)registers with or unregisters from
/// every detected agent CLI, and saves the per-agent results as the last
/// registration result. Enable first checks this CLI's location
/// ([`check_location`]): an unsuitable one changes nothing.
pub(super) fn mcp_switch(ctx: &mut Ctx<'_>, enable: bool) -> Result<()> {
    if enable {
        check_location(&ctx.deps.agents)?;
    }
    let (config, ()) = Config::update(ctx.data_dir(), |c| {
        c.mcp.enabled = Some(enable);
        Ok(())
    })?;
    let enabled = config.mcp_enabled();
    let deps = ctx.deps;
    let env = &deps.agents;
    let results: Vec<RegistrationResult> = detect(env)
        .iter()
        .map(|agent| {
            let r = if enable {
                register(env, &deps.execution, agent)
            } else {
                unregister(env, &deps.execution, agent)
            };
            RegistrationResult {
                name: agent.name.to_owned(),
                ok: r.is_ok(),
                error: r.err(),
            }
        })
        .collect();
    let last = LastRegistration {
        action: if enable { "enable" } else { "disable" }.to_owned(),
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        agents: results,
    };
    // The commands already ran: report them even if the record is not saved.
    if let Err(e) = save_last(ctx.data_dir(), &last) {
        ctx.note(format!(
            "warning: the registration result was not saved: {e}"
        ));
    }
    let results = &last.agents;
    let value = json!({ "enabled": enabled, "at": last.at, "agents": crate::to_value(results)? });
    ctx.emit(value, |out| {
        writeln!(
            out,
            "MCP access: {}",
            if enabled { "allowed" } else { "off" }
        )?;
        if results.is_empty() {
            writeln!(out, "No agent CLI found (claude, codex, gemini).")?;
        }
        let verb = if enable { "registered" } else { "unregistered" };
        for r in results {
            match &r.error {
                None => writeln!(out, "{}: {verb}", r.name)?,
                Some(e) => writeln!(out, "{}: {e}", r.name)?,
            }
        }
        Ok(())
    })
}

/// `mcp status`: the flag, the detected agent CLIs and the last registration
/// result. Registration in each agent's own config is not read back.
pub(super) fn mcp_status(ctx: &mut Ctx<'_>) -> Result<()> {
    let enabled = ctx.config()?.mcp_enabled();
    let agents = detect(&ctx.deps.agents);
    let last = load_last(ctx.data_dir(), &ctx.background_notes())?;
    let value = json!({ "enabled": enabled, "agents": agents_json(&agents), "last": last });
    ctx.emit(value, |out| {
        writeln!(
            out,
            "MCP access: {}",
            if enabled { "allowed" } else { "off" }
        )?;
        for a in &agents {
            writeln!(out, "Agent {}: {}", a.name, a.path.display())?;
        }
        write_last(out, last.as_ref())
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::Cli;

    /// Splits a command line the way a POSIX shell would for the simple
    /// quoting the skill uses (`'…'`, `"…"`), stopping at `<`, `>` or `|`.
    fn shell_words(line: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut in_word = false;
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                '\'' | '"' => {
                    in_word = true;
                    for q in chars.by_ref() {
                        if q == c {
                            break;
                        }
                        word.push(q);
                    }
                }
                '<' | '>' | '|' => break,
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                c => {
                    in_word = true;
                    word.push(c);
                }
            }
        }
        if in_word {
            words.push(word);
        }
        words
    }

    #[test]
    fn every_skill_command_parses() {
        let mut in_block = false;
        let mut checked = 0;
        for line in SKILL_TEMPLATE.lines() {
            if line.trim_start().starts_with("```") {
                in_block = !in_block;
                continue;
            }
            if !in_block || !line.trim_start().starts_with("broscribe ") {
                continue;
            }
            let words = shell_words(line);
            if let Err(e) = Cli::try_parse_from(&words) {
                panic!("skill line does not parse: {line}\n{e}");
            }
            assert!(words.iter().any(|w| w == "--json"), "{line} lacks --json");
            checked += 1;
        }
        assert!(!in_block, "unclosed code block");
        assert!(checked >= 15, "only {checked} commands found");
    }

    #[test]
    fn skill_frontmatter_marker_and_render() {
        assert!(SKILL_TEMPLATE.starts_with("---\nname: broscribe\ndescription: "));
        assert!(has_marker(SKILL_TEMPLATE));
        let a = render_skill(Path::new(
            "/Applications/BroScribe.app/Contents/MacOS/broscribe",
        ));
        assert_eq!(
            a,
            render_skill(Path::new(
                "/Applications/BroScribe.app/Contents/MacOS/broscribe"
            ))
        );
        assert!(a.contains("`/Applications/BroScribe.app/Contents/MacOS/broscribe`"));
        assert!(!a.contains("{{cli}}"));
        let spaced = render_skill(Path::new("/Users/x/My Apps/broscribe"));
        assert!(spaced.contains("`'/Users/x/My Apps/broscribe'`"));
    }

    #[test]
    fn bundled_cli_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let macos = tmp.path().join("BroScribe.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join("broscribe"), b"").unwrap();
        std::fs::write(macos.join("other"), b"").unwrap();
        assert!(is_bundled_cli(&macos.join("broscribe")));
        assert!(!is_bundled_cli(&macos.join("other")));
        assert!(!is_bundled_cli(&macos.join("missing")));
    }

    #[test]
    fn item_rows_and_their_lines() {
        let path = Path::new("/synthetic/home/.local/bin/broscribe");
        let line = |row: &Value| {
            let mut out = Vec::new();
            write_item(&mut out, row).unwrap();
            String::from_utf8(out).unwrap()
        };
        let done = item_row("link", path, Ok(ItemOutcome::Done));
        assert_eq!(
            done,
            json!({"item": "link", "path": "/synthetic/home/.local/bin/broscribe",
                   "ok": true, "error": null})
        );
        assert_eq!(line(&done), "link: ok\n");
        let skipped = item_row("link", path, Ok(ItemOutcome::Unsupported));
        assert_eq!(
            skipped,
            json!({"item": "link", "path": "/synthetic/home/.local/bin/broscribe",
                   "ok": true, "skipped": "unsupported", "error": null})
        );
        assert_eq!(line(&skipped), "link: not available on this system\n");
        let failed = item_row(
            "skill:claude",
            path,
            Err(Error::AlreadyExists("synthetic".into())),
        );
        assert_eq!(failed["ok"], false);
        assert!(failed.get("skipped").is_none());
        let message = failed["error"].as_str().unwrap();
        assert_eq!(line(&failed), format!("skill:claude: {message}\n"));

        let status = |state: LinkState| {
            let mut out = Vec::new();
            let value = json!({"cli": {"path": "/x/broscribe",
                "link": {"path": "/l/broscribe", "state": state.as_str()}}});
            write_status(&mut out, &value).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert!(status(LinkState::Unsupported).contains("Link: not available on this system\n"));
        assert!(status(LinkState::Missing).contains("Link /l/broscribe: missing\n"));
    }

    /// Off Unix the link is neither read nor made: status says so and
    /// install/uninstall skip it, even beside a real file at its path.
    #[cfg(not(unix))]
    #[test]
    fn the_link_is_unsupported_off_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = tmp.path().join("broscribe.exe");
        std::fs::write(&cli, b"").unwrap();
        let link = tmp.path().join(".local").join("bin").join("broscribe");
        assert_eq!(link_state(&link, &cli), LinkState::Unsupported);
        assert_eq!(install_link(&link, &cli).unwrap(), ItemOutcome::Unsupported);
        assert!(link.symlink_metadata().is_err(), "nothing was created");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::write(&link, b"someone else's").unwrap();
        assert_eq!(link_state(&link, &cli), LinkState::Unsupported);
        assert_eq!(
            uninstall_link(&link, &cli).unwrap(),
            ItemOutcome::Unsupported
        );
        assert_eq!(std::fs::read(&link).unwrap(), b"someone else's");
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_link_never_touches_taken_temporary_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let (old, cli) = (dir.join("old-cli"), dir.join("new-cli"));
        std::fs::write(&old, b"").unwrap();
        std::fs::write(&cli, b"").unwrap();
        let link = dir.join("broscribe");
        std::os::unix::fs::symlink(&old, &link).unwrap();
        // Someone else's file and dangling symlink under the first names.
        let taken = [dir.join("t0"), dir.join("t1")];
        std::fs::write(&taken[0], b"keep me").unwrap();
        std::os::unix::fs::symlink(dir.join("nowhere"), &taken[1]).unwrap();
        let free = dir.join("t2");
        let candidates = || taken.iter().cloned().chain([free.clone()]);

        replace_link(&link, &cli, candidates()).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), cli);
        assert_eq!(std::fs::read(&taken[0]).unwrap(), b"keep me");
        assert_eq!(std::fs::read_link(&taken[1]).unwrap(), dir.join("nowhere"));
        assert!(free.symlink_metadata().is_err(), "renamed over the link");

        // Every name taken: nothing changes and nothing is removed.
        let err = replace_link(&link, &old, taken.iter().cloned()).unwrap_err();
        assert_eq!(err.code(), "already_exists");
        assert_eq!(std::fs::read_link(&link).unwrap(), cli);
        assert_eq!(std::fs::read(&taken[0]).unwrap(), b"keep me");
        assert!(taken[1].symlink_metadata().is_ok());

        // The real names are unique per call and hidden beside the link.
        let names: Vec<PathBuf> = temp_link_names(&link).collect();
        assert!(names.len() > 1);
        assert!(names.iter().all(|n| n.parent() == Some(dir)));
        assert_ne!(names[0], names[1]);
        assert_ne!(
            names[0],
            link.with_file_name(format!(".broscribe.tmp-{}", std::process::id()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn last_registration_round_trips_privately() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        assert!(
            load_last(&data, &crate::BackgroundNotes::new())
                .unwrap()
                .is_none()
        );
        let last = LastRegistration {
            action: "enable".into(),
            at: "2026-11-15T12:00:00Z".into(),
            agents: vec![RegistrationResult {
                name: "codex".into(),
                ok: false,
                error: Some("synthetic failure".into()),
            }],
        };
        save_last(&data, &last).unwrap();
        let path = data.join(LAST_FILE);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let back = load_last(&data, &crate::BackgroundNotes::new())
            .unwrap()
            .unwrap();
        assert_eq!(back.action, "enable");
        assert_eq!(back.agents[0].error.as_deref(), Some("synthetic failure"));
        // Only the record is left in the folder.
        let names: Vec<_> = std::fs::read_dir(&data).unwrap().flatten().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn an_unreadable_last_registration_reads_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(LAST_FILE), b"{not json").unwrap();
        assert!(
            load_last(tmp.path(), &crate::BackgroundNotes::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn vendor_argv() {
        let cli = "/x/broscribe";
        assert_eq!(
            add_args("claude", cli).join(" "),
            "mcp add --scope user broscribe -- /x/broscribe mcp"
        );
        assert_eq!(
            add_args("codex", cli).join(" "),
            "mcp add broscribe -- /x/broscribe mcp"
        );
        assert_eq!(
            add_args("gemini", cli).join(" "),
            "mcp add --scope user broscribe /x/broscribe mcp"
        );
        assert_eq!(
            remove_args("claude").join(" "),
            "mcp remove --scope user broscribe"
        );
        assert_eq!(remove_args("codex").join(" "), "mcp remove broscribe");
        assert_eq!(
            remove_args("gemini").join(" "),
            "mcp remove --scope user broscribe"
        );
    }
}
